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
    GestureClick, GestureDrag, GestureZoom, Label, Overlay, Picture, ScrolledWindow, Spinner,
};
use crate::ui::faces::FaceBox;
use photon_core::models::Image;
use photon_import::thumbnails::{thumb_path, ThumbSize, ThumbnailGenerator};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Largest zoom, as a multiple of 1:1.
pub const MAX_ZOOM: f64 = 8.0;
/// One wheel notch or +/− keypress zooms by this factor.
pub const ZOOM_STEP: f64 = 1.25;
/// Zoomed to a face, the view is this many times the face's size: room for
/// the hair and chin, with the eyes large enough to judge focus.
const FACE_ROOM: f64 = 2.2;

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
    /// Says why the full-resolution render isn't shown, when it failed.
    note: Label,
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
    /// A face to zoom to once the viewport has its size.
    pending_face: Cell<Option<FaceBox>>,
    /// Whether double-click toggles Fit ↔ 1:1 (Survey uses it to open a photo).
    double_click_zoom: Cell<bool>,
    sync_queued: Cell<bool>,
    on_change: Rc<dyn Fn(&PhotoView)>,
    /// Called whenever what is on screen moves: zoom or pan (for Compare,
    /// which keeps two views in step).
    on_moved: std::cell::RefCell<Option<Rc<dyn Fn(&Rc<PhotoView>)>>>,
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

        let note = Label::new(None);
        note.add_css_class("osd");
        note.add_css_class("photon-viewer-note");
        note.set_halign(Align::Start);
        note.set_valign(Align::End);
        note.set_margin_start(12);
        note.set_margin_bottom(12);
        note.set_wrap(true);
        note.set_max_width_chars(60);
        note.set_visible(false);

        let root = Overlay::new();
        root.set_child(Some(&scrolled));
        root.add_overlay(&spinner);
        root.add_overlay(&note);

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
            note,
            image: image.clone(),
            size: Cell::new(size),
            size_real: Cell::new(size.is_some()),
            zoom: Cell::new(zoom),
            has_full: Cell::new(false),
            full_requested: Cell::new(false),
            pending_center: Cell::new(pending_center),
            pointer: Cell::new(None),
            pending_face: Cell::new(None),
            double_click_zoom: Cell::new(true),
            sync_queued: Cell::new(false),
            on_change,
            on_moved: Default::default(),
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

    pub fn image(&self) -> &Image {
        &self.image
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
        let zoom = self.clamped(zoom);
        if zoom == self.zoom.get() {
            return;
        }
        let anchor = anchor.unwrap_or_else(|| self.viewport_center());
        let frac = self.frac_at(anchor);
        self.show_at(zoom, frac, anchor);
    }

    /// Zoom in on `face`, centred in the view. Before the view is laid out,
    /// this happens once it is.
    pub fn zoom_to_face(self: &Rc<Self>, face: FaceBox) {
        let (vw, vh) = self.viewport();
        let Some((w, h)) = self.size.get() else { return };
        if vw <= 0.0 || vh <= 0.0 {
            self.pending_face.set(Some(face));
            return;
        }
        self.pending_face.set(None);
        let (fw, fh) = ((face.size.0 * w).max(1.0), (face.size.1 * h).max(1.0));
        let scale = (vw / (fw * FACE_ROOM)).min(vh / (fh * FACE_ROOM)) * self.device_scale();
        self.show_at(self.clamped(Zoom::Scale(scale)), face.center, self.viewport_center());
    }

    /// Whether double-click toggles Fit ↔ 1:1 (on by default).
    pub fn set_double_click_zoom(&self, on: bool) {
        self.double_click_zoom.set(on);
    }

    /// Whether the view is currently zoomed past Fit (`Zoom::Scale`).
    pub fn is_detail_zoom(&self) -> bool {
        matches!(self.zoom.get(), Zoom::Scale(_))
    }

    /// `zoom` within what the view shows: no smaller than Fit, no larger than [`MAX_ZOOM`].
    fn clamped(&self, zoom: Zoom) -> Zoom {
        match zoom {
            // Smaller than Fit only leaves empty space around the photo.
            Zoom::Scale(s) if s <= self.fit_scale() * 1.001 => Zoom::Fit,
            Zoom::Scale(s) => Zoom::Scale(s.min(MAX_ZOOM.max(self.fit_scale()))),
            Zoom::Fit => Zoom::Fit,
        }
    }

    /// Show the photo at `zoom` with its point `frac` at viewport point `anchor`.
    fn show_at(self: &Rc<Self>, zoom: Zoom, frac: (f64, f64), anchor: (f64, f64)) {
        self.pending_center.set(None);
        self.zoom.set(zoom);
        if matches!(zoom, Zoom::Fit) && !self.has_full.get() {
            self.spinner.set_visible(false);
        }
        self.apply_size();
        self.scroll_to(frac, anchor);
        self.ensure_detail();
        (self.on_change)(self);
        self.moved();
    }

    /// Be told whenever the view zooms or pans.
    pub fn connect_moved(&self, f: impl Fn(&Rc<PhotoView>) + 'static) {
        *self.on_moved.borrow_mut() = Some(Rc::new(f));
    }

    fn moved(self: &Rc<Self>) {
        let f = self.on_moved.borrow().clone();
        if let Some(f) = f {
            f(self);
        }
    }

    /// Show the same part of the photo, at the same zoom, as `state` (from
    /// another view's [`state`](Self::state)).
    pub fn follow(self: &Rc<Self>, state: ViewState) {
        if self.size.get().is_none() {
            return;
        }
        if state.zoom != self.zoom.get() {
            self.pending_center.set(None);
            self.zoom.set(state.zoom);
            self.apply_size();
            self.ensure_detail();
            (self.on_change)(self);
        }
        if let Zoom::Scale(_) = state.zoom {
            self.scroll_to(state.center, self.viewport_center());
        }
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
        if let Some(face) = self.pending_face.get() {
            let (vw, vh) = self.viewport();
            if vw > 0.0 && vh > 0.0 {
                self.zoom_to_face(face);
            }
        }
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
            let weak = Rc::downgrade(self);
            adj.connect_value_changed(move |_| with(&weak, |v| v.moved()));
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
                with(&weak, |v| {
                    if v.double_click_zoom.get() {
                        v.toggle(Some((x, y)));
                    }
                });
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

    /// Whether the current zoom level shows more pixels than the loaded preview.
    fn needs_detail_pixels(&self) -> bool {
        if self.has_full.get() {
            return false;
        }
        let Zoom::Scale(s) = self.zoom.get() else { return false };
        let Some((w, _)) = self.size.get() else { return false };
        let have = self.picture.paintable().map_or(0, |p| p.intrinsic_width()) as f64;
        !(self.size_real.get() && have + 1.0 >= (w * s).min(w))
    }

    /// Load the full-resolution render if the zoom shows more pixels than
    /// the preview has.
    fn ensure_detail(self: &Rc<Self>) {
        if self.full_requested.get() || !self.needs_detail_pixels() {
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

impl PhotoView {
    /// The real pixels can't be had (e.g. a RAW variant the decoder doesn't
    /// support): say so, rather than silently showing the preview enlarged.
    fn show_render_failure(&self, reason: &str) {
        self.spinner.set_visible(false);
        let shown = if self.image.format.is_some_and(|f| f.is_raw()) {
            "the camera's embedded preview"
        } else {
            "a preview"
        };
        self.note.set_text(&format!("Full resolution unavailable — showing {shown}"));
        self.note.set_tooltip_text(Some(reason));
        self.note.set_visible(true);
    }
}

fn with(weak: &Weak<PhotoView>, f: impl FnOnce(&Rc<PhotoView>)) {
    if let Some(view) = weak.upgrade() {
        f(&view);
    }
}

// ── Full-resolution renders ─────────────────────────────

/// Full-resolution renders of the current photo plus neighboring photos
/// (next/previous) while zoomed in, so stepping back and forth while checking
/// focus doesn't render them again. (A 20 MP render is ~60 MB.)
const FULL_RES_CACHE: usize = 4;

/// Maximum concurrent full-resolution renders. Developing a RAW file uses all
/// CPU cores via Rayon and ~300 MB of temporary buffers; capping at 2 keeps
/// Survey face-zoom (`F` across 9 RAWs) from spiking memory by ~2.7 GB.
const MAX_CONCURRENT_FULL_RES: usize = 2;

struct InFlight {
    id: u64,
    views: Vec<Weak<PhotoView>>,
}

#[derive(Default)]
struct RenderSemaphore {
    active: usize,
    high_waiters: std::collections::VecDeque<async_channel::Sender<()>>,
    low_waiters: std::collections::VecDeque<async_channel::Sender<()>>,
}

struct RenderPermit;

impl Drop for RenderPermit {
    fn drop(&mut self) {
        RENDER_SEM.with(|sem| {
            let mut sem = sem.borrow_mut();
            while let Some(tx) = sem
                .high_waiters
                .pop_front()
                .or_else(|| sem.low_waiters.pop_front())
            {
                if tx.try_send(()).is_ok() {
                    return;
                }
            }
            sem.active = sem.active.saturating_sub(1);
        });
    }
}

async fn acquire_render_permit(preload: bool) -> RenderPermit {
    let rx = RENDER_SEM.with(|sem| {
        let mut sem = sem.borrow_mut();
        if sem.active < MAX_CONCURRENT_FULL_RES {
            sem.active += 1;
            None
        } else {
            let (tx, rx) = async_channel::bounded(1);
            if preload {
                sem.low_waiters.push_back(tx);
            } else {
                sem.high_waiters.push_back(tx);
            }
            Some(rx)
        }
    });
    if let Some(rx) = rx {
        let _ = rx.recv().await;
    }
    RenderPermit
}

thread_local! {
    static FULL_RES: RefCell<std::collections::VecDeque<(String, gdk::Texture)>> = Default::default();
    static IN_FLIGHT: RefCell<std::collections::HashMap<String, InFlight>> = Default::default();
    static NEXT_FLIGHT_ID: Cell<u64> = const { Cell::new(1) };
    static WANTED_PRELOADS: RefCell<Vec<String>> = Default::default();
    static RENDER_SEM: RefCell<RenderSemaphore> = Default::default();
}

pub fn invalidate_full_res(hash: &str) {
    // Whatever changes the pixels (a rotation) moves the faces and grid tile too.
    crate::ui::faces::invalidate(hash);
    crate::ui::timeline::invalidate_texture(hash);
    FULL_RES.with(|c| c.borrow_mut().retain(|(h, _)| h != hash));
    IN_FLIGHT.with(|m| {
        m.borrow_mut().remove(hash);
    });
    WANTED_PRELOADS.with(|w| w.borrow_mut().retain(|h| h != hash));
}

fn has_cached_full_res(hash: &str) -> bool {
    FULL_RES.with(|c| c.borrow().iter().any(|(h, _)| h == hash))
}

fn cached_full_res(hash: &str) -> Option<gdk::Texture> {
    FULL_RES.with(|c| {
        let mut cache = c.borrow_mut();
        let idx = cache.iter().position(|(h, _)| h == hash)?;
        let item = cache.remove(idx)?;
        let tex = item.1.clone();
        cache.push_front(item);
        Some(tex)
    })
}

/// Cancel any queued background neighbor preloads that haven't started yet.
pub fn cancel_preloads() {
    WANTED_PRELOADS.with(|w| w.borrow_mut().clear());
}

/// Preload full-resolution renders for `neighbors` (typically next and previous
/// photos) in the background at lower priority than active views.
pub fn preload_neighbors(
    db: &photon_core::db::Database,
    neighbors: &[&photon_core::models::TimelineItem],
) {
    let mut wanted = Vec::with_capacity(neighbors.len());
    let mut to_fetch = Vec::new();
    for item in neighbors {
        if item.is_video {
            continue;
        }
        if !wanted.contains(&item.hash) {
            wanted.push(item.hash.clone());
        }
        let already = has_cached_full_res(&item.hash)
            || IN_FLIGHT.with(|m| m.borrow().contains_key(&item.hash));
        if !already {
            to_fetch.push(item.id);
        }
    }
    WANTED_PRELOADS.with(|w| *w.borrow_mut() = wanted);
    if to_fetch.is_empty() {
        return;
    }
    let Ok(conn) = db.conn() else { return };
    for id in to_fetch {
        if let Ok(Some(img)) = photon_core::db::queries::get_image(&conn, id) {
            preload_full_resolution(img);
        }
    }
}

fn next_flight_id() -> u64 {
    NEXT_FLIGHT_ID.with(|c| {
        let id = c.get();
        c.set(id + 1);
        id
    })
}

fn preload_full_resolution(image: Image) {
    if image.format.is_some_and(|f| f.is_video() || f == photon_core::models::ImageFormat::Gif) {
        return;
    }
    if has_cached_full_res(&image.hash) {
        return;
    }
    let flight_id = IN_FLIGHT.with(|m| {
        let mut map = m.borrow_mut();
        if map.contains_key(&image.hash) {
            return None;
        }
        let id = next_flight_id();
        map.insert(
            image.hash.clone(),
            InFlight {
                id,
                views: Vec::new(),
            },
        );
        Some(id)
    });
    if let Some(flight_id) = flight_id {
        spawn_render(image, flight_id, true);
    }
}

/// Render the photo's real pixels — RAW files developed from the sensor
/// data — on a worker thread, and show them unless the view has gone.
fn load_full_resolution(view: &Rc<PhotoView>) {
    if let Some(texture) = cached_full_res(&view.image.hash) {
        view.show_texture(&texture, true);
        return;
    }
    view.spinner.set_visible(true);
    let weak = Rc::downgrade(view);
    let flight_id = IN_FLIGHT.with(|m| {
        let mut map = m.borrow_mut();
        if let Some(entry) = map.get_mut(&view.image.hash) {
            entry.views.push(weak);
            return None;
        }
        let id = next_flight_id();
        map.insert(
            view.image.hash.clone(),
            InFlight {
                id,
                views: vec![weak],
            },
        );
        Some(id)
    });
    if let Some(flight_id) = flight_id {
        spawn_render(view.image.clone(), flight_id, false);
    }
}

fn spawn_render(image: Image, flight_id: u64, preload: bool) {
    glib::spawn_future_local(async move {
        let permit = acquire_render_permit(preload).await;
        let hash = image.hash.clone();

        // Check whether this render is still needed after waiting for a permit.
        let still_needed = IN_FLIGHT.with(|m| {
            let mut map = m.borrow_mut();
            let Some(entry) = map.get(&hash) else {
                return false;
            };
            if entry.id != flight_id {
                return false;
            }
            let any_view_needs = entry
                .views
                .iter()
                .filter_map(Weak::upgrade)
                .any(|v| v.needs_detail_pixels());
            let wanted_preload = WANTED_PRELOADS.with(|w| w.borrow().iter().any(|h| h == &hash));
            if any_view_needs || wanted_preload {
                true
            } else {
                if let Some(removed) = map.remove(&hash) {
                    for w in removed.views {
                        with(&w, |v| {
                            v.full_requested.set(false);
                            v.spinner.set_visible(false);
                        });
                    }
                }
                false
            }
        });
        if !still_needed {
            drop(permit);
            return;
        }

        let rendered =
            gio::spawn_blocking(move || photon_import::thumbnails::full_resolution(&image)).await;
        drop(permit);

        let flight = IN_FLIGHT.with(|m| {
            let mut map = m.borrow_mut();
            if map.get(&hash).is_some_and(|e| e.id == flight_id) {
                map.remove(&hash)
            } else {
                None
            }
        });
        let Some(flight) = flight else {
            // Invalidated (e.g. rotated) while the worker was running: discard.
            return;
        };

        let rgb = match rendered {
            Ok(Ok(rgb)) => rgb,
            Ok(Err(e)) => {
                log::warn!("Full-resolution render failed: {e:#}");
                for w in &flight.views {
                    with(w, |v| v.show_render_failure(&format!("{e:#}")));
                }
                return;
            }
            Err(_) => {
                log::warn!("Full-resolution render panicked");
                for w in &flight.views {
                    with(w, |v| v.show_render_failure("the renderer crashed"));
                }
                return;
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

        let has_active_view = flight.views.iter().any(|w| w.upgrade().is_some());
        FULL_RES.with(|c| {
            let mut cache = c.borrow_mut();
            cache.retain(|(h, _)| h != &hash);
            if has_active_view || cache.is_empty() {
                cache.push_front((hash, texture.clone()));
            } else {
                // Keep the currently viewed photo at index 0 so preloading both
                // neighbors never evicts the photo on screen.
                cache.insert(1, (hash, texture.clone()));
            }
            cache.truncate(FULL_RES_CACHE);
        });
        for w in &flight.views {
            with(w, |v| v.show_texture(&texture, true));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_permit_prioritizes_active_views_over_preloads_and_caps_concurrency() {
        glib::MainContext::default().block_on(async {
            let p1 = acquire_render_permit(false).await;
            let p2 = acquire_render_permit(false).await;
            assert_eq!(RENDER_SEM.with(|s| s.borrow().active), 2);

            let order = Rc::new(RefCell::new(Vec::new()));

            let o_low = order.clone();
            let low_task = glib::spawn_future_local(async move {
                let _p = acquire_render_permit(true).await;
                o_low.borrow_mut().push("preload");
            });

            let o_high = order.clone();
            let high_task = glib::spawn_future_local(async move {
                let p = acquire_render_permit(false).await;
                o_high.borrow_mut().push("active");
                p
            });

            // Let both tasks register in the semaphore queues.
            glib::timeout_future(std::time::Duration::from_millis(5)).await;
            assert!(order.borrow().is_empty(), "neither waiter may run while 2 permits are held");

            // Releasing 1 permit while 2 waiters are queued must wake the active
            // view (`high_task`) even though `low_task` was queued first.
            drop(p1);
            let p_high = high_task.await.unwrap();
            glib::timeout_future(std::time::Duration::from_millis(5)).await;
            assert_eq!(&*order.borrow(), &["active"], "preload must wait while 2 permits are held");

            drop(p_high);
            drop(p2);
            let _ = low_task.await;

            assert_eq!(&*order.borrow(), &["active", "preload"]);
            assert_eq!(RENDER_SEM.with(|s| s.borrow().active), 0);
        });
    }
}
