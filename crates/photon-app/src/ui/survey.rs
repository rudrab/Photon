//! Survey: 3–9 selected photos in a grid, to choose among near-duplicates
//! (Lightroom's Survey view).
//!
//! Each photo shows its marks and its quality ring. Drop the ones you don't
//! want from the survey (✕, or Backspace/Delete) and the rest grow to fill
//! the space; mark them as usual (P X U, 1–5 0, 6–9 on the active photo,
//! chosen by clicking or with the arrow keys). Enter opens the active photo
//! in the 1-up viewer; Esc goes back to the library. Dropping a photo from
//! the survey doesn't change it.

use crate::ui::compare::ViewContext;
use crate::ui::mark::{self, MarkButton, QualityGauge};
use gtk4::prelude::*;
use gtk4::{
    gdk, glib, ActionBar, Align, Box as GtkBox, Button, EventControllerKey, GestureClick, Grid, Label, Orientation,
    Overlay, Picture,
};
use photon_core::db::queries;
use photon_core::models::TimelineItem;
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

struct Photo {
    id: i64,
    root: GtkBox,
    mark: MarkButton,
}

struct Survey {
    ctx: ViewContext,
    grid: Grid,
    /// In display order; photos dropped from the survey are removed.
    cells: RefCell<Vec<Rc<Photo>>>,
    active: Cell<usize>,
    counter: Label,
    on_open: Rc<dyn Fn(i64)>,
}

/// Columns for `n` photos: one row up to 3, then two or three rows.
fn columns(n: usize) -> usize {
    match n {
        0..=3 => n.max(1),
        4 => 2,
        _ => 3,
    }
}

pub fn build_survey(ctx: ViewContext, items: Vec<TimelineItem>, on_open: Rc<dyn Fn(i64)>) -> GtkBox {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.set_vexpand(true);
    root.set_hexpand(true);
    root.set_focusable(true);

    let grid = Grid::builder()
        .row_homogeneous(true)
        .column_homogeneous(true)
        .row_spacing(8)
        .column_spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .vexpand(true)
        .hexpand(true)
        .build();
    root.append(&grid);

    let counter = Label::new(None);
    counter.add_css_class("photon-badge");
    let this = Rc::new(Survey {
        ctx: ctx.clone(),
        grid,
        cells: RefCell::new(Vec::new()),
        active: Cell::new(0),
        counter: counter.clone(),
        on_open,
    });
    let cells: Vec<Rc<Photo>> = items.iter().filter_map(|item| this.make_cell(item)).collect();
    *this.cells.borrow_mut() = cells;

    // ── Bottom bar ──────────────────────────────────────
    let bar = ActionBar::new();
    bar.add_css_class("photon-bottom-bar");
    let start = GtkBox::new(Orientation::Horizontal, 2);
    start.add_css_class("photon-bar-group");
    start.set_valign(Align::Center);
    start.set_margin_start(8);
    let back = Button::from_icon_name("view-grid-symbolic");
    back.add_css_class("flat");
    back.set_tooltip_text(Some("Back to library (Esc)"));
    let back_cb = ctx.on_back.clone();
    back.connect_clicked(move |_| back_cb());
    start.append(&back);
    let title = Label::new(Some("Survey"));
    title.add_css_class("heading");
    start.append(&title);
    start.append(&counter);
    bar.pack_start(&start);
    let hint = Label::new(Some("✕ drops a photo from the survey · Enter opens it · P X 1–5 mark it"));
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    bar.set_center_widget(Some(&hint));
    root.append(&bar);

    // The key handler holds the survey strongly: it lives as long as the root.
    let keys = EventControllerKey::new();
    let s = this.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        let n = s.cells.borrow().len();
        let cols = columns(n) as isize;
        let move_by = |d: isize| {
            let next = s.active.get() as isize + d;
            if (0..n as isize).contains(&next) {
                s.set_active(next as usize);
            }
        };
        match key {
            gdk::Key::Escape => (s.ctx.on_back)(),
            gdk::Key::Left => move_by(-1),
            gdk::Key::Right => move_by(1),
            gdk::Key::Up => move_by(-cols),
            gdk::Key::Down => move_by(cols),
            gdk::Key::Return | gdk::Key::KP_Enter => {
                let id = s.cells.borrow().get(s.active.get()).map(|c| c.id);
                if let Some(id) = id {
                    (s.on_open)(id);
                }
            }
            gdk::Key::BackSpace | gdk::Key::Delete => s.drop_photo(s.active.get()),
            _ => match mark::mark_for_key(key) {
                Some((r, f, c)) => s.save_marks(s.active.get(), r, f, c),
                None => return glib::Propagation::Proceed,
            },
        }
        glib::Propagation::Stop
    });
    root.add_controller(keys);

    this.relayout();
    this.set_active(0);
    root.grab_focus();
    root
}

impl Survey {
    fn make_cell(self: &Rc<Self>, item: &TimelineItem) -> Option<Rc<Photo>> {
        let image = self.ctx.db.conn().ok().and_then(|c| queries::get_image(&c, item.id).ok().flatten())?;
        let id = item.id;
        let weak = Rc::downgrade(self);

        let picture = Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_vexpand(true);
        picture.set_hexpand(true);
        let grid_thumb = thumb_path(&self.ctx.cache_dir, ThumbSize::Grid, &image.hash);
        if grid_thumb.exists() {
            picture.set_filename(Some(&grid_thumb));
        }
        let large = thumb_path(&self.ctx.cache_dir, ThumbSize::Large, &image.hash);
        if large.exists() {
            crate::ui::widgets::load_texture_async(&picture, &large);
        } else {
            crate::ui::detail::load_large_preview(&picture, &image, &self.ctx.cache_dir);
        }

        let drop_btn = Button::from_icon_name("window-close-symbolic");
        drop_btn.add_css_class("osd");
        drop_btn.add_css_class("circular");
        drop_btn.set_tooltip_text(Some("Drop from the survey (the photo isn't changed)"));
        drop_btn.set_halign(Align::End);
        drop_btn.set_valign(Align::Start);
        drop_btn.set_margin_top(6);
        drop_btn.set_margin_end(6);
        let w = weak.clone();
        drop_btn.connect_clicked(move |_| {
            with(&w, |s| {
                let pos = s.cells.borrow().iter().position(|c| c.id == id);
                if let Some(pos) = pos {
                    s.drop_photo(pos);
                }
            })
        });
        let overlay = Overlay::new();
        overlay.add_css_class("photon-photo-backdrop");
        overlay.set_child(Some(&picture));
        overlay.add_overlay(&drop_btn);

        let caption = GtkBox::new(Orientation::Horizontal, 8);
        caption.set_margin_start(6);
        caption.set_margin_end(6);
        let name = Label::new(Some(&image.filename));
        name.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        name.set_hexpand(true);
        name.set_xalign(0.0);
        let w = weak.clone();
        let mark_btn = mark::mark_button(move |m| {
            with(&w, |s| {
                let pos = s.cells.borrow().iter().position(|c| c.id == id);
                let (r, f, c) = match m {
                    mark::Mark::Rating(r) => (Some(r), None, None),
                    mark::Mark::Flag(f) => (None, Some(f), None),
                    mark::Mark::Color(c) => (None, None, Some(c)),
                };
                if let Some(pos) = pos {
                    s.save_marks(pos, r, f, c);
                }
            })
        });
        mark_btn.set_state(Some(mark::state_of(&image)));
        let quality = mark::quality_gauge();
        load_quality(&self.ctx, &image, &quality);
        caption.append(&name);
        caption.append(&mark_btn.button);
        caption.append(&quality.widget);

        let root = GtkBox::new(Orientation::Vertical, 4);
        root.add_css_class("photon-compare-pane");
        root.append(&overlay);
        root.append(&caption);

        let click = GestureClick::new();
        let w = weak.clone();
        click.connect_released(move |_, n_press, _, _| {
            with(&w, |s| {
                let pos = s.cells.borrow().iter().position(|c| c.id == id);
                if let Some(pos) = pos {
                    s.set_active(pos);
                    if n_press == 2 {
                        (s.on_open)(id);
                    }
                }
            })
        });
        root.add_controller(click);

        Some(Rc::new(Photo { id, root, mark: mark_btn }))
    }

    /// Place the cells in the grid for their number.
    fn relayout(&self) {
        while let Some(child) = self.grid.first_child() {
            self.grid.remove(&child);
        }
        let cells = self.cells.borrow();
        let cols = columns(cells.len());
        for (i, cell) in cells.iter().enumerate() {
            self.grid.attach(&cell.root, (i % cols) as i32, (i / cols) as i32, 1, 1);
        }
        self.counter.set_text(&format!("{} photos", cells.len()));
    }

    fn set_active(&self, pos: usize) {
        self.active.set(pos);
        for (i, cell) in self.cells.borrow().iter().enumerate() {
            if i == pos {
                cell.root.add_css_class("active");
            } else {
                cell.root.remove_css_class("active");
            }
        }
    }

    /// Take the photo at `pos` out of the survey (not out of the library).
    fn drop_photo(&self, pos: usize) {
        {
            let mut cells = self.cells.borrow_mut();
            if pos >= cells.len() || cells.len() <= 1 {
                return;
            }
            cells.remove(pos);
        }
        self.relayout();
        let n = self.cells.borrow().len();
        self.set_active(pos.min(n - 1));
    }

    fn save_marks(&self, pos: usize, rating: Option<i32>, flag: Option<i32>, color: Option<photon_core::models::ColorLabel>) {
        let Some(cell) = self.cells.borrow().get(pos).cloned() else { return };
        if mark::save_marks(&self.ctx.db, self.ctx.undo.as_ref(), &self.ctx.notify, cell.id, rating, flag, color) {
            if let Some(img) = self.ctx.db.conn().ok().and_then(|c| queries::get_image(&c, cell.id).ok().flatten()) {
                cell.mark.set_state(Some(mark::state_of(&img)));
            }
        }
    }
}

fn with(weak: &Weak<Survey>, f: impl FnOnce(&Rc<Survey>)) {
    if let Some(s) = weak.upgrade() {
        f(&s);
    }
}

fn load_quality(ctx: &ViewContext, image: &photon_core::models::Image, quality: &QualityGauge) {
    quality.set(None, "Photo quality: …");
    let (db, img, quality) = (ctx.db.clone(), image.clone(), quality.clone());
    glib::spawn_future_local(async move {
        let reading = gtk4::gio::spawn_blocking(move || crate::ui::detail::quality_reading(&db, &img)).await.ok().flatten();
        match reading {
            Some((score, text)) => quality.set(score, &text),
            None => quality.set(None, "Photo quality: not analysed yet (Analyse Photo Quality…)"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::columns;

    #[test]
    fn grid_shapes() {
        let shapes: Vec<(usize, usize)> = (3..=9).map(|n| (columns(n), n.div_ceil(columns(n)))).collect();
        assert_eq!(shapes, [(3, 1), (2, 2), (3, 2), (3, 2), (3, 3), (3, 3), (3, 3)]);
    }
}
