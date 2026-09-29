//! Compare: two photos side by side at full resolution, zoom and pan in step
//! (Lightroom's Compare view).
//!
//! The left photo is the *select*, the right one the *candidate*. ← → bring
//! in the previous or next photo of the timeline as candidate; ↑ (or "Make
//! Select") keeps the candidate: it becomes the select and the next photo
//! the candidate — survival of the sharpest through a burst. Click a pane (or
//! Tab) to make it active: P X U, 1–5 0, 6–9 mark the active photo, and the
//! zoom buttons act on it (the other follows).

use crate::ui::mark::{self, MarkButton, QualityGauge};
use crate::ui::photo_view::{PhotoView, Zoom, ZOOM_STEP};
use gtk4::prelude::*;
use gtk4::{gdk, glib, ActionBar, Align, Box as GtkBox, Button, EventControllerKey, GestureClick, Label, Orientation};
use photon_core::db::{queries, Database};
use photon_core::models::TimelineItem;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

/// What Compare and Survey need from the window.
#[derive(Clone)]
pub struct ViewContext {
    pub db: Database,
    pub cache_dir: PathBuf,
    pub notify: Rc<dyn Fn(&str)>,
    pub undo: Option<Rc<RefCell<crate::ui::undo::UndoManager>>>,
    /// Back to the library.
    pub on_back: Rc<dyn Fn()>,
}

/// One side: a header (role, file name, marks, quality) over the photo.
struct Pane {
    root: GtkBox,
    name: Label,
    mark: MarkButton,
    quality: QualityGauge,
    holder: GtkBox,
    view: RefCell<Option<Rc<PhotoView>>>,
    item_id: Cell<i64>,
}

struct Compare {
    ctx: ViewContext,
    photos: Rc<Vec<TimelineItem>>,
    panes: [Rc<Pane>; 2],
    select: Cell<usize>,
    candidate: Cell<usize>,
    /// 0 = the select (left), 1 = the candidate (right).
    active: Cell<usize>,
    /// Set while one view follows the other, so it doesn't echo back.
    syncing: Rc<Cell<bool>>,
    counter: Label,
}

pub fn build_compare(ctx: ViewContext, photos: Rc<Vec<TimelineItem>>, select: usize, candidate: usize) -> GtkBox {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.set_vexpand(true);
    root.set_hexpand(true);
    root.set_focusable(true);

    let body = GtkBox::new(Orientation::Horizontal, 8);
    body.set_homogeneous(true);
    body.set_vexpand(true);
    body.set_margin_top(8);
    body.set_margin_start(8);
    body.set_margin_end(8);
    body.set_margin_bottom(8);
    root.append(&body);

    let counter = Label::new(None);
    counter.add_css_class("photon-badge");
    let this = Rc::new_cyclic(|weak: &Weak<Compare>| {
        let make_pane = |side: usize| {
            let pane_root = GtkBox::new(Orientation::Vertical, 6);
            pane_root.add_css_class("photon-compare-pane");
            let header = GtkBox::new(Orientation::Horizontal, 8);
            header.set_margin_start(6);
            header.set_margin_end(6);
            header.set_margin_top(4);
            let role = Label::new(Some(if side == 0 { "Select" } else { "Candidate" }));
            role.add_css_class("caption-heading");
            let name = Label::new(None);
            name.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
            name.set_hexpand(true);
            name.set_xalign(0.0);
            let w = weak.clone();
            let mark = mark::mark_button(move |m| {
                if let Some(c) = w.upgrade() {
                    let (r, f, col) = match m {
                        mark::Mark::Rating(r) => (Some(r), None, None),
                        mark::Mark::Flag(f) => (None, Some(f), None),
                        mark::Mark::Color(c) => (None, None, Some(c)),
                    };
                    c.save_marks(side, r, f, col);
                }
            });
            let quality = mark::quality_gauge();
            header.append(&role);
            header.append(&name);
            header.append(&mark.button);
            header.append(&quality.widget);
            let holder = GtkBox::new(Orientation::Vertical, 0);
            holder.add_css_class("photon-photo-backdrop");
            holder.set_vexpand(true);
            holder.set_hexpand(true);
            pane_root.append(&header);
            pane_root.append(&holder);

            let click = GestureClick::new();
            click.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let w = weak.clone();
            click.connect_pressed(move |_, _, _, _| {
                if let Some(c) = w.upgrade() {
                    c.set_active(side);
                }
            });
            pane_root.add_controller(click);

            Rc::new(Pane {
                root: pane_root,
                name,
                mark,
                quality,
                holder,
                view: RefCell::new(None),
                item_id: Cell::new(0),
            })
        };
        Compare {
            ctx: ctx.clone(),
            photos: photos.clone(),
            panes: [make_pane(0), make_pane(1)],
            select: Cell::new(select),
            candidate: Cell::new(candidate),
            active: Cell::new(1),
            syncing: Rc::new(Cell::new(false)),
            counter: counter.clone(),
        }
    });
    body.append(&this.panes[0].root);
    body.append(&this.panes[1].root);

    // ── Bottom bar (the same groups as the other bars) ──
    let bar = ActionBar::new();
    bar.add_css_class("photon-bottom-bar");
    let group = || {
        let g = GtkBox::new(Orientation::Horizontal, 2);
        g.add_css_class("photon-bar-group");
        g.set_valign(Align::Center);
        g
    };
    let icon_button = |icon: &str, tooltip: &str| {
        let b = Button::from_icon_name(icon);
        b.add_css_class("flat");
        b.set_tooltip_text(Some(tooltip));
        b
    };

    let start = group();
    start.set_margin_start(8);
    let back = icon_button("view-grid-symbolic", "Back to library (Esc)");
    let back_cb = ctx.on_back.clone();
    back.connect_clicked(move |_| back_cb());
    start.append(&back);
    let title = Label::new(Some("Compare"));
    title.add_css_class("heading");
    start.append(&title);
    start.append(&counter);
    bar.pack_start(&start);

    let center = GtkBox::new(Orientation::Horizontal, 8);
    let candidates = group();
    let prev = icon_button("go-previous-symbolic", "Previous candidate (←)");
    let next = icon_button("go-next-symbolic", "Next candidate (→)");
    candidates.append(&prev);
    candidates.append(&Label::new(Some("Candidate")));
    candidates.append(&next);
    let keep_group = group();
    let keep = Button::with_label("Make Select");
    keep.add_css_class("flat");
    keep.set_tooltip_text(Some("Keep the candidate: it becomes the select, the next photo the candidate (↑)"));
    keep_group.append(&keep);
    let zoom = group();
    let fit = icon_button("zoom-fit-best-symbolic", "Fit both (Ctrl+0)");
    let one = icon_button("zoom-original-symbolic", "Both at 100% (Z / Ctrl+1)");
    zoom.append(&fit);
    zoom.append(&one);
    center.append(&candidates);
    center.append(&keep_group);
    center.append(&zoom);
    bar.set_center_widget(Some(&center));
    root.append(&bar);

    let w = Rc::downgrade(&this);
    prev.connect_clicked(move |_| with(&w, |c| c.step_candidate(-1)));
    let w = Rc::downgrade(&this);
    next.connect_clicked(move |_| with(&w, |c| c.step_candidate(1)));
    let w = Rc::downgrade(&this);
    keep.connect_clicked(move |_| with(&w, |c| c.keep_candidate()));
    let w = Rc::downgrade(&this);
    fit.connect_clicked(move |_| with(&w, |c| c.zoom_active(|v| v.set_zoom(Zoom::Fit, None))));
    let w = Rc::downgrade(&this);
    one.connect_clicked(move |_| with(&w, |c| c.zoom_active(|v| v.set_zoom(Zoom::Scale(1.0), None))));

    // ── Keys ────────────────────────────────────────────
    // The key handler holds the view's state strongly: it lives exactly as
    // long as the view's root (the widgets inside hold it only weakly).
    let keys = EventControllerKey::new();
    let c = this.clone();
    keys.connect_key_pressed(move |_, key, _, state| {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::Escape => (c.ctx.on_back)(),
            gdk::Key::Left => c.step_candidate(-1),
            gdk::Key::Right => c.step_candidate(1),
            gdk::Key::Up | gdk::Key::Return | gdk::Key::KP_Enter => c.keep_candidate(),
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => c.set_active(1 - c.active.get()),
            gdk::Key::_0 | gdk::Key::KP_0 if ctrl => c.zoom_active(|v| v.set_zoom(Zoom::Fit, None)),
            gdk::Key::_1 | gdk::Key::KP_1 if ctrl => c.zoom_active(|v| v.set_zoom(Zoom::Scale(1.0), None)),
            gdk::Key::z | gdk::Key::Z => c.zoom_active(|v| v.toggle(None)),
            gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => c.zoom_active(|v| v.zoom_by(ZOOM_STEP, None)),
            gdk::Key::minus | gdk::Key::KP_Subtract => c.zoom_active(|v| v.zoom_by(1.0 / ZOOM_STEP, None)),
            _ => match mark::mark_for_key(key) {
                Some((r, f, col)) => c.save_marks(c.active.get(), r, f, col),
                None => return glib::Propagation::Proceed,
            },
        }
        glib::Propagation::Stop
    });
    root.add_controller(keys);

    this.load(0);
    this.load(1);
    this.set_active(1);
    root.grab_focus();
    root
}

fn with(weak: &Weak<Compare>, f: impl FnOnce(&Rc<Compare>)) {
    if let Some(c) = weak.upgrade() {
        f(&c);
    }
}

impl Compare {
    fn index_of(&self, side: usize) -> usize {
        if side == 0 { self.select.get() } else { self.candidate.get() }
    }

    /// Show the photo for `side` in its pane, at the other pane's zoom and position.
    fn load(self: &Rc<Self>, side: usize) {
        let pane = &self.panes[side];
        let Some(item) = self.photos.get(self.index_of(side)) else { return };
        pane.item_id.set(item.id);
        while let Some(child) = pane.holder.first_child() {
            pane.holder.remove(&child);
        }
        *pane.view.borrow_mut() = None;
        let image = self.ctx.db.conn().ok().and_then(|c| queries::get_image(&c, item.id).ok().flatten());
        let Some(image) = image else {
            pane.name.set_text("Photo no longer in library");
            return;
        };
        pane.name.set_text(&image.filename);
        pane.name.set_tooltip_text(Some(&image.filename));
        pane.mark.set_state(Some(mark::state_of(&image)));
        self.load_quality(side, &image);

        let other = self.panes[1 - side].view.borrow().as_ref().map(|v| v.state());
        let view = PhotoView::new(&image, &self.ctx.cache_dir, other, Rc::new(|_| {}));
        let weak = Rc::downgrade(self);
        view.connect_moved(move |moved| {
            let Some(c) = weak.upgrade() else { return };
            if c.syncing.replace(true) {
                return;
            }
            let follower = c.panes[1 - side].view.borrow().clone();
            if let Some(follower) = follower {
                follower.follow(moved.state());
            }
            c.syncing.set(false);
        });
        pane.holder.append(view.widget());
        *pane.view.borrow_mut() = Some(view);
        self.counter.set_text(&format!("{} / {}", self.index_of(1) + 1, self.photos.len()));
    }

    fn load_quality(&self, side: usize, image: &photon_core::models::Image) {
        let pane = &self.panes[side];
        pane.quality.set(None, "Photo quality: …");
        let (db, img, quality, id) = (self.ctx.db.clone(), image.clone(), pane.quality.clone(), image.id);
        let current = Rc::downgrade(&self.panes[side]);
        glib::spawn_future_local(async move {
            let reading = gtk4::gio::spawn_blocking(move || crate::ui::detail::quality_reading(&db, &img)).await.ok().flatten();
            // The pane shows another photo by now.
            if current.upgrade().map(|p| p.item_id.get()) != id {
                return;
            }
            match reading {
                Some((score, text)) => quality.set(score, &text),
                None => quality.set(None, "Photo quality: not analysed yet (Analyse Photo Quality…)"),
            }
        });
    }

    fn set_active(&self, side: usize) {
        self.active.set(side);
        for (i, pane) in self.panes.iter().enumerate() {
            if i == side {
                pane.root.add_css_class("active");
            } else {
                pane.root.remove_css_class("active");
            }
        }
    }

    fn zoom_active(&self, f: impl Fn(&Rc<PhotoView>)) {
        let view = self.panes[self.active.get()].view.borrow().clone();
        if let Some(view) = view {
            f(&view);
        }
    }

    /// The next photo in `delta`'s direction that isn't the select.
    fn neighbour(&self, from: usize, delta: isize) -> Option<usize> {
        let mut i = from as isize + delta;
        while (0..self.photos.len() as isize).contains(&i) {
            if i as usize != self.select.get() {
                return Some(i as usize);
            }
            i += delta;
        }
        None
    }

    fn step_candidate(self: &Rc<Self>, delta: isize) {
        if let Some(i) = self.neighbour(self.candidate.get(), delta) {
            self.candidate.set(i);
            self.load(1);
        }
    }

    /// The candidate wins: it becomes the select, and the next photo (or
    /// the previous, at the end) the candidate.
    fn keep_candidate(self: &Rc<Self>) {
        let winner = self.candidate.get();
        self.select.set(winner);
        let next = self.neighbour(winner, 1).or_else(|| self.neighbour(winner, -1));
        let Some(next) = next else { return };
        self.candidate.set(next);
        self.load(0);
        self.load(1);
    }

    fn save_marks(&self, side: usize, rating: Option<i32>, flag: Option<i32>, color: Option<photon_core::models::ColorLabel>) {
        let pane = &self.panes[side];
        let id = pane.item_id.get();
        if mark::save_marks(&self.ctx.db, self.ctx.undo.as_ref(), &self.ctx.notify, id, rating, flag, color) {
            if let Some(img) = self.ctx.db.conn().ok().and_then(|c| queries::get_image(&c, id).ok().flatten()) {
                pane.mark.set_state(Some(mark::state_of(&img)));
            }
        }
    }
}
