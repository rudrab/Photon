//! The zoomable photo in the 1-up viewer.
//!
//! One `ScrolledWindow` holds the `Picture` at every zoom level: in Fit the
//! picture has no size request and `ContentFit::Contain` scales it to the
//! viewport; zoomed in, its size request is the photo's full-resolution size
//! times the zoom and the viewport scrolls. The zoom is continuous (slider,
//! Ctrl+wheel, pinch, +/−) and anchored at the pointer, so the point under
//! it stays put.
//!
//! The Grid and Large previews are shown first. The full-resolution render
//! is loaded only once the zoom needs more pixels than the preview has.

use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Align, EventControllerMotion, EventControllerScroll, EventControllerScrollFlags,
    GestureClick, GestureDrag, GestureZoom, Overlay, Picture, ScrolledWindow, Spinner,
};
use photon_core::models::Image;
use photon_import::thumbnails::{thumb_path, ThumbSize, ThumbnailGenerator};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Largest zoom, as a multiple of 1:1.
pub const MAX_ZOOM: f64 = 8.0;
/// One wheel notch or +/− keypress zooms by this factor.
pub const ZOOM_STEP: f64 = 1.25;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Zoom {
    /// The whole photo, as large as the viewport allows.
    Fit,
    /// Screen pixels per photo pixel: 1.0 is 1:1.
    Scale(f64),
}

/// What to keep when the same photo is shown again (after a rating change).
#[derive(Clone, Copy)]
pub struct ViewState {
    zoom: Zoom,
    /// The point of the photo at the viewport's centre, as fractions of its size.
    center: (f64, f64),
}

pub struct PhotoView {
    root: Overlay,
    scrolled: ScrolledWindow,
    picture: Picture,
    spinner: Spinner,
    image: Image,
    /// The photo's full-resolution size in pixels, upright. From the catalog
    /// until the render arrives; from a preview if the catalog has none.
    size: Cell<Option<(f64, f64)>>,
    /// Whether `size` is the photo's (not a preview's).
    size_real: Cell<bool>,
    zoom: Cell<Zoom>,
    has_full: Cell<bool>,
    full_requested: Cell<bool>,
    /// A restored view's centre, applied once the viewport has its size.
    pending_center: Cell<Option<(f64, f64)>>,
    pointer: Cell<Option<(f64, f64)>>,
    sync_queued: Cell<bool>,
    on_change: Rc<dyn Fn(&PhotoView)>,
}

impl PhotoView {
    /// Build the view for `image`. `on_change` runs whenever the zoom (or the
    /// Fit scale) changes, to update the zoom controls.
    pub fn new(
        image: &Image,
        cache_dir: &Path,
        restore: Option<ViewState>,
        on_change: Rc<dyn Fn(&PhotoView)>,
    ) -> Rc<Self> {
        let picture = Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_halign(Align::Fill);
        picture.set_valign(Align::Fill);

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .kinetic_scrolling(true)
            .child(&picture)
            .build();

        let spinner = Spinner::builder().spinning(true).halign(Align::End).valign(Align::Start).build();
        spinner.set_margin_top(12);
        spinner.set_margin_end(12);
        spinner.set_size_request(24, 24);
        spinner.set_tooltip_text(Some("Rendering at full resolution…"));
        spinner.set_visible(false);

        let root = Overlay::new();
        root.set_child(Some(&scrolled));
        root.add_overlay(&spinner);

        let size = match (image.width, image.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                // The catalog has the sensor-orientation size.
                let upright = if matches!(image.orientation, Some(5..=8)) { (h, w) } else { (w, h) };
                Some((upright.0 as f64, upright.1 as f64))
            }
            _ => None,
        };

        let (zoom, pending_center) = match restore {
            Some(ViewState { zoom: z @ Zoom::Scale(_), center }) => (z, Some(center)),
            _ => (Zoom::Fit, None),
        };

        let view = Rc::new(Self {
            root,
            scrolled,
            picture,
            spinner,
            image: image.clone(),
            size: Cell::new(size),
            size_real: Cell::new(size.is_some()),
            zoom: Cell::new(zoom),
            has_full: Cell::new(false),
            full_requested: Cell::new(false),
            pending_center: Cell::new(pending_center),
            pointer: Cell::new(None),
            sync_queued: Cell::new(false),
            on_change,
        });
        view.connect_events();
        view.load_previews(cache_dir);
        view.apply_size();
        view.ensure_detail();
        view
    }

    pub fn widget(&self) -> &Overlay {
        &self.root
    }

    pub fn zoom(&self) -> Zoom {
        self.zoom.get()
    }

    pub fn state(&self) -> ViewState {
        ViewState { zoom: self.zoom.get(), center: self.frac_at(self.viewport_center()) }
    }

    /// The zoom that shows the whole photo in the viewport.
    pub fn fit_scale(&self) -> f64 {
        let (vw, vh) = self.viewport();
        match self.size.get() {
            Some((w, h)) if vw > 0.0 && vh > 0.0 => (vw / w).min(vh / h) * self.device_scale(),
            _ => 1.0,
        }
    }

    /// The zoom being shown, Fit included.
    pub fn scale(&self) -> f64 {
        match self.zoom.get() {
            Zoom::Fit => self.fit_scale(),
            Zoom::Scale(s) => s,
        }
    }

    /// Zoom to `zoom`, keeping the photo point under `anchor` (viewport
    /// coordinates; the centre if `None`) where it is.
    pub fn set_zoom(self: &Rc<Self>, zoom: Zoom, anchor: Option<(f64, f64)>) {
        if self.size.get().is_none() {
            return;
        }
        let zoom = match zoom {
            // Smaller than Fit only leaves empty space around the photo.
            Zoom::Scale(s) if s <= self.fit_scale() * 1.001 => Zoom::Fit,
            Zoom::Scale(s) => Zoom::Scale(s.min(MAX_ZOOM.max(self.fit_scale()))),
            Zoom::Fit => Zoom::Fit,
        };
        if zoom == self.zoom.get() {
            return;
        }
        let anchor = anchor.unwrap_or_else(|| self.viewport_center());
        let frac = self.frac_at(anchor);
        self.pending_center.set(None);
        self.zoom.set(zoom);
        self.apply_size();
        self.scroll_to(frac, anchor);
        self.ensure_detail();
        (self.on_change)(self);
    }

    /// Multiply the zoom by `factor`.
    pub fn zoom_by(self: &Rc<Self>, factor: f64, anchor: Option<(f64, f64)>) {
        self.set_zoom(Zoom::Scale(self.scale() * factor), anchor);
    }

    /// Fit ↔ 1:1, centred on `anchor`.
    pub fn toggle(self: &Rc<Self>, anchor: Option<(f64, f64)>) {
        match self.zoom.get() {
            Zoom::Fit => self.set_zoom(Zoom::Scale(1.0), anchor),
            Zoom::Scale(_) => self.set_zoom(Zoom::Fit, anchor),
        }
    }

    // ── Geometry ────────────────────────────────────────

    fn device_scale(&self) -> f64 {
        self.scrolled.scale_factor().max(1) as f64
    }

    fn viewport(&self) -> (f64, f64) {
        let (h, v) = (self.scrolled.hadjustment().page_size(), self.scrolled.vadjustment().page_size());
        let w = if h > 0.0 { h } else { self.scrolled.width() as f64 };
        let hh = if v > 0.0 { v } else { self.scrolled.height() as f64 };
        (w, hh)
    }

    fn viewport_center(&self) -> (f64, f64) {
        let (w, h) = self.viewport();
        (w / 2.0, h / 2.0)
    }

    /// The photo's size on screen, in logical pixels.
    fn displayed(&self) -> (f64, f64) {
        let Some((w, h)) = self.size.get() else { return (0.0, 0.0) };
        let s = self.scale() / self.device_scale();
        (w * s, h * s)
    }

    /// The photo point under viewport point `(x, y)`, as fractions of its size.
    fn frac_at(&self, (x, y): (f64, f64)) -> (f64, f64) {
        let (dw, dh) = self.displayed();
        let (vw, vh) = self.viewport();
        let hadj = self.scrolled.hadjustment();
        let vadj = self.scrolled.vadjustment();
        // Smaller than the viewport, the photo is centred in it.
        let fx = if dw > 0.0 { (hadj.value() + x - ((vw - dw) / 2.0).max(0.0)) / dw } else { 0.5 };
        let fy = if dh > 0.0 { (vadj.value() + y - ((vh - dh) / 2.0).max(0.0)) / dh } else { 0.5 };
        (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0))
    }

    /// Scroll so the photo point `frac` is at viewport point `(x, y)`.
    fn scroll_to(&self, (fx, fy): (f64, f64), (x, y): (f64, f64)) {
        let (dw, dh) = self.displayed();
        let (vw, vh) = self.viewport();
        for (adj, f, a, d, page) in
            [(self.scrolled.hadjustment(), fx, x, dw, vw), (self.scrolled.vadjustment(), fy, y, dh, vh)]
        {
            // The viewport only takes the new size at the next layout, and it
            // keeps the value it finds then, so set the range now.
            adj.set_upper(d.max(page));
            adj.set_value(f * d + ((page - d) / 2.0).max(0.0) - a);
        }
    }

    fn apply_size(&self) {
        match self.zoom.get() {
            Zoom::Fit => {
                self.picture.set_size_request(-1, -1);
                self.scrolled.set_cursor_from_name(None);
            }
            Zoom::Scale(_) => {
                let (w, h) = self.displayed();
                self.picture.set_size_request(w.round() as i32, h.round() as i32);
                let (vw, vh) = self.viewport();
                let pannable = w > vw + 0.5 || h > vh + 0.5;
                self.scrolled.set_cursor_from_name(pannable.then_some("grab"));
            }
        }
    }

    /// Run `on_change` after the current layout pass: the viewport reports
    /// its new size during allocation, when the zoom controls can't resize.
    fn queue_sync(self: &Rc<Self>) {
        if self.sync_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(view) = weak.upgrade() {
                view.sync_queued.set(false);
                if let Zoom::Scale(_) = view.zoom.get() {
                    view.apply_size(); // the grab cursor depends on the viewport
                }
                (view.on_change)(&view);
            }
        });
    }

    fn viewport_changed(self: &Rc<Self>) {
        if let Some(center) = self.pending_center.get() {
            let (dw, dh) = self.displayed();
            let (hadj, vadj) = (self.scrolled.hadjustment(), self.scrolled.vadjustment());
            let laid_out = hadj.page_size() > 0.0
                && vadj.page_size() > 0.0
                && hadj.upper() + 1.0 >= dw
                && vadj.upper() + 1.0 >= dh;
            if laid_out {
                self.pending_center.set(None);
                self.scroll_to(center, self.viewport_center());
            }
        }
        self.queue_sync();
    }

    // ── Input ───────────────────────────────────────────

    fn connect_events(self: &Rc<Self>) {
        for adj in [self.scrolled.hadjustment(), self.scrolled.vadjustment()] {
            let weak = Rc::downgrade(self);
            adj.connect_changed(move |_| with(&weak, |v| v.viewport_changed()));
        }
        let weak = Rc::downgrade(self);
        self.scrolled.connect_scale_factor_notify(move |_| {
            with(&weak, |v| {
                v.apply_size();
                v.queue_sync();
            })
        });

        let motion = EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| with(&weak, |v| v.pointer.set(Some((x, y)))));
        let weak = Rc::downgrade(self);
        motion.connect_leave(move |_| with(&weak, |v| v.pointer.set(None)));
        self.scrolled.add_controller(motion);

        // Ctrl+wheel zooms at the pointer. Captured, so the scrolled window
        // doesn't also scroll.
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |ctrl, _, dy| {
            if !ctrl.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            with(&weak, |v| {
                // A touchpad scrolls in pixels, a wheel in notches.
                let factor = match ctrl.unit() {
                    gdk::ScrollUnit::Surface => 1.01f64.powf(-dy),
                    _ => ZOOM_STEP.powf(-dy),
                };
                v.zoom_by(factor, v.pointer.get());
            });
            glib::Propagation::Stop
        });
        self.scrolled.add_controller(scroll);

        let pinch = GestureZoom::new();
        let start = Rc::new(Cell::new(1.0));
        let weak = Rc::downgrade(self);
        let s = start.clone();
        pinch.connect_begin(move |_, _| with(&weak, |v| s.set(v.scale())));
        let weak = Rc::downgrade(self);
        pinch.connect_scale_changed(move |g, factor| {
            with(&weak, |v| v.set_zoom(Zoom::Scale(start.get() * factor), g.bounding_box_center()))
        });
        self.scrolled.add_controller(pinch);

        // Drag to pan when zoomed in.
        let drag = GestureDrag::new();
        let origin = Rc::new(Cell::new((0.0, 0.0)));
        let weak = Rc::downgrade(self);
        let o = origin.clone();
        drag.connect_drag_begin(move |_, _, _| {
            with(&weak, |v| {
                o.set((v.scrolled.hadjustment().value(), v.scrolled.vadjustment().value()));
                if let Zoom::Scale(_) = v.zoom.get() {
                    v.scrolled.set_cursor_from_name(Some("grabbing"));
                }
            })
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            with(&weak, |v| {
                if let Zoom::Scale(_) = v.zoom.get() {
                    let (x, y) = origin.get();
                    v.scrolled.hadjustment().set_value(x - dx);
                    v.scrolled.vadjustment().set_value(y - dy);
                }
            })
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| with(&weak, |v| v.apply_size()));
        self.scrolled.add_controller(drag);

        // Double-click: Fit ↔ 1:1 at the clicked point.
        let click = GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, n_press, x, y| {
            if n_press == 2 {
                with(&weak, |v| v.toggle(Some((x, y))));
            }
        });
        self.scrolled.add_controller(click);
    }

    // ── Pixels ──────────────────────────────────────────

    /// The Grid preview at once, then the Large one; the cached full
    /// resolution render instead, if there is one.
    fn load_previews(self: &Rc<Self>, cache_dir: &Path) {
        if self.image.format == Some(photon_core::models::ImageFormat::Gif) {
            // Decoded straight from the file: already full resolution.
            match gdk::Texture::from_filename(&self.image.path) {
                Ok(texture) => self.show_texture(&texture, true),
                Err(e) => log::warn!("Loading {}: {e}", self.image.path.display()),
            }
            return;
        }
        if let Some(texture) = cached_full_res(&self.image.hash) {
            self.show_texture(&texture, true);
            return;
        }

        let grid = thumb_path(cache_dir, ThumbSize::Grid, &self.image.hash);
        if grid.exists() {
            match gdk::Texture::from_filename(&grid) {
                Ok(texture) => self.show_texture(&texture, false),
                Err(e) => log::warn!("Loading {}: {e}", grid.display()),
            }
        }

        let large = thumb_path(cache_dir, ThumbSize::Large, &self.image.hash);
        let generator = ThumbnailGenerator::new(cache_dir.to_path_buf());
        let image = self.image.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let loaded = gio::spawn_blocking(move || -> anyhow::Result<gdk::Texture> {
                let path: PathBuf = if large.exists() { large } else { generator.ensure(&image, ThumbSize::Large)? };
                Ok(gdk::Texture::from_filename(&path)?)
            })
            .await;
            match loaded {
                Ok(Ok(texture)) => with(&weak, |v| v.show_texture(&texture, false)),
                Ok(Err(e)) => log::warn!("Large preview failed: {e:#}"),
                Err(_) => log::warn!("Large preview worker panicked"),
            }
        });
    }

    /// Load the full-resolution render if the zoom shows more pixels than
    /// the preview has.
    fn ensure_detail(self: &Rc<Self>) {
        if self.full_requested.get() || self.has_full.get() {
            return;
        }
        let Zoom::Scale(s) = self.zoom.get() else { return };
        let Some((w, _)) = self.size.get() else { return };
        let have = self.picture.paintable().map_or(0, |p| p.intrinsic_width()) as f64;
        if self.size_real.get() && have + 1.0 >= (w * s).min(w) {
            return;
        }
        self.full_requested.set(true);
        load_full_resolution(self);
    }

    /// Show `texture`. A preview never replaces the full-resolution render,
    /// whichever arrives last.
    fn show_texture(self: &Rc<Self>, texture: &gdk::Texture, full: bool) {
        if self.has_full.get() && !full {
            return;
        }
        self.picture.set_paintable(Some(texture));
        if full {
            self.has_full.set(true);
            self.spinner.set_visible(false);
        }
        if full || self.size.get().is_none() {
            self.set_size((texture.width() as f64, texture.height() as f64), full);
        }
        self.ensure_detail();
    }

    /// The photo's real size is known (or better known): keep the same point
    /// at the centre and the same zoom.
    fn set_size(self: &Rc<Self>, size: (f64, f64), real: bool) {
        let center = self.viewport_center();
        let frac = self.frac_at(center);
        let changed = self.size.get() != Some(size);
        self.size.set(Some(size));
        self.size_real.set(real);
        if changed {
            self.apply_size();
            if let (Zoom::Scale(_), None) = (self.zoom.get(), self.pending_center.get()) {
                self.scroll_to(frac, center);
            }
            (self.on_change)(self);
        }
    }
}

fn with(weak: &Weak<PhotoView>, f: impl FnOnce(&Rc<PhotoView>)) {
    if let Some(view) = weak.upgrade() {
        f(&view);
    }
}

// ── Full-resolution renders ─────────────────────────────

/// Full-resolution renders of the last few photos zoomed into, so stepping
/// back and forth while checking focus doesn't render them again.
/// (A 20 MP render is ~60 MB.)
const FULL_RES_CACHE: usize = 3;

thread_local! {
    static FULL_RES: RefCell<std::collections::VecDeque<(String, gdk::Texture)>> = Default::default();
}

pub fn invalidate_full_res(hash: &str) {
    FULL_RES.with(|c| c.borrow_mut().retain(|(h, _)| h != hash));
}

fn cached_full_res(hash: &str) -> Option<gdk::Texture> {
    FULL_RES.with(|c| c.borrow().iter().find(|(h, _)| h == hash).map(|(_, t)| t.clone()))
}

/// Render the photo's real pixels — RAW files developed from the sensor
/// data — on a worker thread, and show them unless the view has gone.
fn load_full_resolution(view: &Rc<PhotoView>) {
    if let Some(texture) = cached_full_res(&view.image.hash) {
        view.show_texture(&texture, true);
        return;
    }
    view.spinner.set_visible(true);
    let image = view.image.clone();
    let weak = Rc::downgrade(view);
    glib::spawn_future_local(async move {
        let hash = image.hash.clone();
        let rendered = gio::spawn_blocking(move || photon_import::thumbnails::full_resolution(&image)).await;
        let rgb = match rendered {
            Ok(Ok(rgb)) => rgb,
            Ok(Err(e)) => {
                with(&weak, |v| v.spinner.set_visible(false));
                return log::warn!("Full-resolution render failed: {e:#}");
            }
            Err(_) => {
                with(&weak, |v| v.spinner.set_visible(false));
                return log::warn!("Full-resolution render panicked");
            }
        };
        let (w, h) = rgb.dimensions();
        let texture: gdk::Texture = gdk::MemoryTexture::new(
            w as i32,
            h as i32,
            gdk::MemoryFormat::R8g8b8,
            &glib::Bytes::from_owned(rgb.into_raw()),
            w as usize * 3,
        )
        .upcast();
        FULL_RES.with(|c| {
            let mut cache = c.borrow_mut();
            cache.push_front((hash, texture.clone()));
            cache.truncate(FULL_RES_CACHE);
        });
        with(&weak, |v| v.show_texture(&texture, true));
    });
}
