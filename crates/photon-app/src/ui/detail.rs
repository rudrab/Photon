//! Inline photo viewer: image-dominant with compact controls.
//!
//! Layout:
//!   ┌──────────────────────────────────────────────────────┐
//!   │ [<][>]      filename.jpg          3/42   [ℹ] [✎]    │  ← compact toolbar
//!   ├───────────────────────────────────────┬──────────────┤
//!   │                                       │ Details      │
//!   │            (large image)              │ Open With    │  ← info side panel
//!   │                                       │ Sidecars     │    (toggled by ℹ / I)
//!   │                                       │ Edit History │
//!   └───────────────────────────────────────┴──────────────┘
//!
//! Keyboard: Left=previous, Right=next, Escape=back to the grid, I=toggle info

use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{
    gio, glib, Align, Box as GtkBox, Button, Entry, EventControllerKey, FlowBox, GestureClick, Label, Orientation,
    Picture, Revealer, ScrolledWindow, ToggleButton,
};
use photon_core::db::queries;
use photon_core::db::Database;
use photon_core::models::{Image, Preferences, TimelineItem, UIAction};
use photon_import::thumbnails::{thumb_path, ThumbSize, ThumbnailGenerator};
use crate::ui::histogram::HistogramWidget;
use crate::ui::widgets::load_texture_async;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::thread;

/// Width of the info side panel.
const INFO_PANEL_WIDTH: i32 = 300;

/// Build the inline viewer for `photos[index]`. Full image rows are loaded
/// one at a time as the user steps through, so the list can be huge.
pub fn build_viewer(
    photos: Rc<Vec<TimelineItem>>,
    index: usize,
    cache_dir: &Path,
    prefs: &Preferences,
    nav_tx: &Sender<UIAction>,
    back_action: UIAction,
    db: &Database,
    on_export: Option<Rc<dyn Fn(Vec<Image>)>>,
) -> GtkBox {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.set_vexpand(true);
    root.set_hexpand(true);
    root.set_focusable(true);

    // ── Compact Toolbar ─────────────────────────────────
    let toolbar = GtkBox::new(Orientation::Horizontal, 8);
    toolbar.add_css_class("viewer-toolbar");
    toolbar.set_margin_start(12);
    toolbar.set_margin_end(12);
    toolbar.set_margin_top(8);
    toolbar.set_margin_bottom(8);

    let back_btn = Button::from_icon_name("view-grid-symbolic");
    back_btn.add_css_class("flat");
    back_btn.set_tooltip_text(Some("Back to library (Esc)"));
    let tx_b = nav_tx.clone();
    let ba_b = back_action.clone();
    back_btn.connect_clicked(move |_| {
        let _ = tx_b.send_blocking(ba_b.clone());
    });

    let nav_group = GtkBox::new(Orientation::Horizontal, 0);
    nav_group.add_css_class("linked");

    let prev_btn = Button::from_icon_name("go-previous-symbolic");
    prev_btn.add_css_class("flat");
    prev_btn.set_tooltip_text(Some("Previous (←)"));

    let next_btn = Button::from_icon_name("go-next-symbolic");
    next_btn.add_css_class("flat");
    next_btn.set_tooltip_text(Some("Next (→)"));

    nav_group.append(&prev_btn);
    nav_group.append(&next_btn);

    // Culling button group
    let cull_group = GtkBox::new(Orientation::Horizontal, 0);
    cull_group.add_css_class("linked");

    let pick_btn = Button::from_icon_name("emblem-ok-symbolic");
    pick_btn.add_css_class("flat");
    pick_btn.set_tooltip_text(Some("Pick (P)"));

    let reject_btn = Button::from_icon_name("process-stop-symbolic");
    reject_btn.add_css_class("flat");
    reject_btn.set_tooltip_text(Some("Reject (X)"));

    let unflag_btn = Button::from_icon_name("view-refresh-symbolic");
    unflag_btn.add_css_class("flat");
    unflag_btn.set_tooltip_text(Some("Unflag (U)"));

    cull_group.append(&pick_btn);
    cull_group.append(&reject_btn);
    cull_group.append(&unflag_btn);

    // Star rating pill
    let rating_btn = Button::with_label("★ 0");
    rating_btn.add_css_class("flat");
    rating_btn.set_tooltip_text(Some("Rating (1-5, 0 to clear)"));

    // Zoom 1:1 button
    let zoom_btn = ToggleButton::builder()
        .icon_name("zoom-original-symbolic")
        .tooltip_text("1:1 Pixel Zoom (Z)")
        .build();
    zoom_btn.add_css_class("flat");

    // Compare button
    let compare_btn = ToggleButton::builder()
        .icon_name("view-dual-symbolic")
        .tooltip_text("Side-by-side Compare (C)")
        .build();
    compare_btn.add_css_class("flat");

    let filename_label = Label::new(None);
    filename_label.set_hexpand(true);
    filename_label.set_halign(Align::Center);
    filename_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    filename_label.set_css_classes(&["heading"]);

    let counter_label = Label::new(None);
    counter_label.set_css_classes(&["photon-badge"]);
    counter_label.set_margin_end(8);

    let info_btn = ToggleButton::builder()
        .icon_name("dialog-information-symbolic")
        .tooltip_text("Show details (I)")
        .active(true)
        .build();
    info_btn.add_css_class("flat");

    let open_btn = Button::from_icon_name("document-edit-symbolic");
    open_btn.add_css_class("flat");
    open_btn.set_tooltip_text(Some("Open in editor"));

    let export_btn = Button::from_icon_name("document-save-symbolic");
    export_btn.add_css_class("flat");
    export_btn.set_tooltip_text(Some("Export Photo (Ctrl+E)"));

    toolbar.append(&back_btn);
    toolbar.append(&nav_group);
    toolbar.append(&cull_group);
    toolbar.append(&rating_btn);
    toolbar.append(&zoom_btn);
    toolbar.append(&compare_btn);
    toolbar.append(&filename_label);
    toolbar.append(&counter_label);
    toolbar.append(&info_btn);
    toolbar.append(&open_btn);
    toolbar.append(&export_btn);
    root.append(&toolbar);

    // ── Body: image | info side panel ───────────────────
    let body = GtkBox::new(Orientation::Horizontal, 0);
    body.set_vexpand(true);
    body.set_hexpand(true);
    root.append(&body);

    let image_box = GtkBox::new(Orientation::Vertical, 0);
    image_box.set_vexpand(true);
    image_box.set_hexpand(true);
    image_box.set_halign(Align::Fill);
    image_box.set_valign(Align::Fill);
    body.append(&image_box);

    let info_revealer = Revealer::new();
    info_revealer.set_transition_type(gtk4::RevealerTransitionType::SlideLeft);
    info_revealer.set_reveal_child(true);

    let info_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_width(INFO_PANEL_WIDTH)
        .vexpand(true)
        .build();
    info_scroll.add_css_class("info-panel");
    let info_box = GtkBox::new(Orientation::Vertical, 6);
    info_box.set_margin_top(16);
    info_box.set_margin_bottom(16);
    info_box.set_margin_start(16);
    info_box.set_margin_end(16);
    info_scroll.set_child(Some(&info_box));
    info_revealer.set_child(Some(&info_scroll));
    body.append(&info_revealer);

    // ── Showing a photo ─────────────────────────────────
    let current_idx = Rc::new(Cell::new(index));
    let current_image: Rc<RefCell<Option<Image>>> = Rc::new(RefCell::new(None));
    let is_zoomed = Rc::new(Cell::new(false));
    let is_comparing = Rc::new(Cell::new(false));
    let cache = cache_dir.to_path_buf();
    let prefs = Rc::new(prefs.clone());

    let toggle_zoom: Rc<dyn Fn()> = {
        let zoom_btn = zoom_btn.clone();
        Rc::new(move || {
            zoom_btn.set_active(!zoom_btn.is_active());
        })
    };

    let toggle_compare: Rc<dyn Fn()> = {
        let compare_btn = compare_btn.clone();
        Rc::new(move || {
            compare_btn.set_active(!compare_btn.is_active());
        })
    };

    let show: Rc<dyn Fn(usize)> = Rc::new(glib::clone!(
        #[weak] image_box,
        #[weak] info_box,
        #[weak] filename_label,
        #[weak] counter_label,
        #[weak] prev_btn,
        #[weak] next_btn,
        #[weak] rating_btn,
        #[weak] pick_btn,
        #[weak] reject_btn,
        #[strong] photos,
        #[strong] current_idx,
        #[strong] current_image,
        #[strong] is_zoomed,
        #[strong] is_comparing,
        #[strong] toggle_zoom,
        #[strong] prefs,
        #[strong] db,
        move |i: usize| {
            let Some(item) = photos.get(i) else { return };
            current_idx.set(i);
            counter_label.set_text(&format!("{}/{}", i + 1, photos.len()));
            prev_btn.set_sensitive(i > 0);
            next_btn.set_sensitive(i + 1 < photos.len());

            let image = db
                .conn()
                .ok()
                .and_then(|c| queries::get_image(&c, item.id).ok().flatten());
            match &image {
                Some(img) => {
                    rating_btn.set_label(&format!("★ {}", img.rating));
                    if img.flagged == 1 {
                        pick_btn.add_css_class("suggested-action");
                        reject_btn.remove_css_class("destructive-action");
                    } else if img.flagged == -1 {
                        pick_btn.remove_css_class("suggested-action");
                        reject_btn.add_css_class("destructive-action");
                    } else {
                        pick_btn.remove_css_class("suggested-action");
                        reject_btn.remove_css_class("destructive-action");
                    }

                    if is_comparing.get() {
                        let next_idx = if i + 1 < photos.len() { i + 1 } else { i.saturating_sub(1) };
                        let image_b = if next_idx != i {
                            db.conn()
                                .ok()
                                .and_then(|c| queries::get_image(&c, photos[next_idx].id).ok().flatten())
                        } else {
                            None
                        };

                        if let Some(img_b) = image_b {
                            filename_label.set_text(&format!("{}  vs  {}", img.filename, img_b.filename));
                            render_compare(&image_box, &info_box, img, &img_b, &cache, &prefs, &db);
                        } else {
                            filename_label.set_text(&img.filename);
                            render_photo(&image_box, &info_box, img, &cache, &prefs, &db, is_zoomed.get(), toggle_zoom.clone());
                        }
                    } else {
                        filename_label.set_text(&img.filename);
                        render_photo(&image_box, &info_box, img, &cache, &prefs, &db, is_zoomed.get(), toggle_zoom.clone());
                    }
                }
                None => {
                    filename_label.set_text("Photo no longer in library");
                    clear(&image_box);
                    clear(&info_box);
                }
            }
            *current_image.borrow_mut() = image;
        }
    ));
    show(index);

    let step = {
        let show = show.clone();
        let current_idx = current_idx.clone();
        let len = photos.len();
        Rc::new(move |delta: isize| {
            let target = current_idx.get() as isize + delta;
            if (0..len as isize).contains(&target) {
                show(target as usize);
            }
        })
    };

    let update_cull = {
        let db = db.clone();
        let photos = photos.clone();
        let current_idx = current_idx.clone();
        let current_image = current_image.clone();
        let show = show.clone();
        Rc::new(move |new_rating: Option<i32>, new_flag: Option<i32>| {
            let idx = current_idx.get();
            let Some(item) = photos.get(idx) else { return };
            let img_id = item.id;

            let mut img_info = None;
            if let Some(img) = current_image.borrow_mut().as_mut() {
                if let Some(r) = new_rating {
                    img.rating = r;
                }
                if let Some(f) = new_flag {
                    img.flagged = f;
                }
                img_info = Some((img.path.clone(), img.rating, img.flagged));
            }

            let db = db.clone();
            thread::spawn(move || {
                if let Ok(mut conn) = db.conn() {
                    if let Some(r) = new_rating {
                        let _ = queries::set_rating(&mut conn, img_id, r);
                    }
                    if let Some(f) = new_flag {
                        let _ = queries::set_flag(&mut conn, img_id, f);
                    }
                    if let Some((path, r, f)) = img_info {
                        sync_image_xmp(&conn, img_id, &path, r, f);
                    }
                }
            });

            show(idx);
        })
    };

    // ── Buttons ─────────────────────────────────────────
    info_btn.connect_toggled(glib::clone!(
        #[weak] info_revealer,
        move |btn| info_revealer.set_reveal_child(btn.is_active())
    ));

    let uc = update_cull.clone();
    pick_btn.connect_clicked(move |_| uc(None, Some(1)));
    let uc = update_cull.clone();
    reject_btn.connect_clicked(move |_| uc(None, Some(-1)));
    let uc = update_cull.clone();
    unflag_btn.connect_clicked(move |_| uc(None, Some(0)));

    let uc = update_cull.clone();
    let img_c = current_image.clone();
    rating_btn.connect_clicked(move |_| {
        let cur = img_c.borrow().as_ref().map(|i| i.rating).unwrap_or(0);
        uc(Some((cur + 1) % 6), None);
    });

    let s = show.clone();
    let ci = current_idx.clone();
    let iz = is_zoomed.clone();
    zoom_btn.connect_toggled(move |btn| {
        iz.set(btn.is_active());
        s(ci.get());
    });

    let s = show.clone();
    let ci = current_idx.clone();
    let ic = is_comparing.clone();
    compare_btn.connect_toggled(move |btn| {
        ic.set(btn.is_active());
        s(ci.get());
    });

    let db_open = db.clone();
    let prefs_open = prefs.clone();
    let image_open = current_image.clone();
    open_btn.connect_clicked(move |_| {
        if let Some(image) = image_open.borrow().as_ref() {
            open_in_editor(image, &prefs_open, &db_open);
        }
    });

    let on_export_c = on_export.clone();
    let image_export = current_image.clone();
    let trigger_export: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(cb) = &on_export_c {
            if let Some(image) = image_export.borrow().as_ref() {
                cb(vec![image.clone()]);
            }
        }
    });

    let te = trigger_export.clone();
    export_btn.connect_clicked(move |_| te());

    let step_prev = step.clone();
    prev_btn.connect_clicked(move |_| step_prev(-1));
    let step_next = step.clone();
    next_btn.connect_clicked(move |_| step_next(1));

    // ── Keyboard navigation ─────────────────────────────
    let key_ctrl = EventControllerKey::new();
    let tx_esc = nav_tx.clone();
    key_ctrl.connect_key_pressed(glib::clone!(
        #[weak] info_btn,
        #[strong] toggle_zoom,
        #[strong] toggle_compare,
        #[strong] update_cull,
        #[strong] step,
        #[strong] trigger_export,
        #[upgrade_or] glib::Propagation::Proceed,
        move |_, key, _, state| {
            let is_shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
            let is_ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);

            if is_ctrl && (key == gtk4::gdk::Key::e || key == gtk4::gdk::Key::E) {
                trigger_export();
                return glib::Propagation::Stop;
            }

            match key {
                gtk4::gdk::Key::Escape => {
                    let _ = tx_esc.send_blocking(back_action.clone());
                }
                gtk4::gdk::Key::Left => step(-1),
                gtk4::gdk::Key::Right => step(1),
                gtk4::gdk::Key::i | gtk4::gdk::Key::I => {
                    info_btn.set_active(!info_btn.is_active());
                }
                gtk4::gdk::Key::z | gtk4::gdk::Key::Z => {
                    toggle_zoom();
                }
                gtk4::gdk::Key::c | gtk4::gdk::Key::C => {
                    toggle_compare();
                }
                gtk4::gdk::Key::_1 | gtk4::gdk::Key::KP_1 | gtk4::gdk::Key::exclam => {
                    update_cull(Some(1), None);
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::_2 | gtk4::gdk::Key::KP_2 | gtk4::gdk::Key::at => {
                    update_cull(Some(2), None);
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::_3 | gtk4::gdk::Key::KP_3 | gtk4::gdk::Key::numbersign => {
                    update_cull(Some(3), None);
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::_4 | gtk4::gdk::Key::KP_4 | gtk4::gdk::Key::dollar => {
                    update_cull(Some(4), None);
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::_5 | gtk4::gdk::Key::KP_5 | gtk4::gdk::Key::percent => {
                    update_cull(Some(5), None);
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::_0 | gtk4::gdk::Key::KP_0 | gtk4::gdk::Key::parenright | gtk4::gdk::Key::grave | gtk4::gdk::Key::asciitilde => {
                    update_cull(Some(0), None);
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::p | gtk4::gdk::Key::P => {
                    update_cull(None, Some(1));
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::x | gtk4::gdk::Key::X => {
                    update_cull(None, Some(-1));
                    if is_shift {
                        step(1);
                    }
                }
                gtk4::gdk::Key::u | gtk4::gdk::Key::U => {
                    update_cull(None, Some(0));
                    if is_shift {
                        step(1);
                    }
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }
    ));
    root.add_controller(key_ctrl);

    // Grab focus so keyboard works immediately
    root.grab_focus();

    root
}

fn clear(container: &GtkBox) {
    while let Some(c) = container.first_child() {
        container.remove(&c);
    }
}

// ── Render a single photo ───────────────────────────────

fn render_photo(
    image_box: &GtkBox,
    info_box: &GtkBox,
    image: &Image,
    cache_dir: &Path,
    prefs: &Preferences,
    db: &Database,
    is_zoomed: bool,
    on_toggle_zoom: Rc<dyn Fn()>,
) {
    clear(image_box);
    clear(info_box);

    // ── Large image ─────────────────────────────────────
    let large = thumb_path(cache_dir, ThumbSize::Large, &image.hash);
    let grid = thumb_path(cache_dir, ThumbSize::Grid, &image.hash);
    let picture = Picture::new();
    if grid.exists() {
        picture.set_filename(Some(&grid));
    }
    if large.exists() {
        load_texture_async(&picture, &large);
    } else {
        load_large_preview(&picture, image, cache_dir);
    }

    let click = GestureClick::new();
    let otz = on_toggle_zoom.clone();
    click.connect_released(move |_, n_press, _, _| {
        if n_press == 2 {
            otz();
        }
    });

    if is_zoomed {
        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .build();
        let w = image.width.unwrap_or(2000) as i32;
        let h = image.height.unwrap_or(1500) as i32;
        picture.set_size_request(w, h);
        picture.set_content_fit(gtk4::ContentFit::Fill);
        picture.set_can_shrink(false);
        picture.add_controller(click);
        scrolled.set_child(Some(&picture));
        image_box.append(&scrolled);
    } else {
        picture.set_size_request(-1, -1);
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_halign(Align::Fill);
        picture.set_valign(Align::Fill);
        picture.add_controller(click);
        image_box.append(&picture);
    }

    populate_info_panel(info_box, image, cache_dir, prefs, db);
}

fn sync_image_xmp(
    conn: &rusqlite::Connection,
    image_id: i64,
    image_path: &Path,
    rating: i32,
    flagged: i32,
) {
    let tags = queries::get_tags_for_image(conn, image_id)
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.name)
        .collect::<Vec<_>>();
    let (title, description): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT title, description FROM images WHERE id = ?1",
            rusqlite::params![image_id],
            |r: &rusqlite::Row| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((None, None));

    let _ = photon_import::sync_xmp_metadata(
        image_path,
        if rating > 0 { Some(rating) } else { None },
        if flagged != 0 { Some(flagged) } else { None },
        &tags,
        title.as_deref(),
        description.as_deref(),
    );
}

fn populate_info_panel(
    info_box: &GtkBox,
    image: &Image,
    cache_dir: &Path,
    prefs: &Preferences,
    db: &Database,
) {
    // ── Info panel content: sections stacked vertically ─
    let columns = GtkBox::new(Orientation::Vertical, 16);

    // Section 1: Live RGB / Luminance Histogram
    let hist_box = GtkBox::new(Orientation::Vertical, 4);
    let hist = HistogramWidget::new();
    let grid_path = thumb_path(cache_dir, ThumbSize::Grid, &image.hash);
    if grid_path.exists() {
        hist.load_for_path(&grid_path);
    } else {
        hist.load_for_path(&image.path);
    }
    hist_box.append(hist.widget());
    columns.append(&hist_box);

    // Section 2: Metadata & Tags Editor
    let meta_edit_box = GtkBox::new(Orientation::Vertical, 6);
    let meta_edit_header = Label::new(Some("Metadata & Tags"));
    meta_edit_header.set_css_classes(&["title-4"]);
    meta_edit_header.set_halign(Align::Start);
    meta_edit_box.append(&meta_edit_header);

    let img_id = image.id;
    let img_path = image.path.clone();
    let img_rating = image.rating;
    let img_flag = image.flagged;

    // Title
    let title_lbl = Label::new(Some("Title"));
    title_lbl.set_css_classes(&["caption-heading"]);
    title_lbl.set_halign(Align::Start);
    meta_edit_box.append(&title_lbl);

    let title_entry = Entry::new();
    title_entry.set_placeholder_text(Some("Add a title..."));
    if let Some(ref t) = image.title {
        title_entry.set_text(t);
    }
    let db_t = db.clone();
    let p_t = img_path.clone();
    title_entry.connect_activate(move |entry| {
        let text = entry.text().to_string();
        if let (Some(id), Ok(conn)) = (img_id, db_t.conn()) {
            let _ = queries::set_title(&conn, id, &text);
            sync_image_xmp(&conn, id, &p_t, img_rating, img_flag);
        }
    });
    meta_edit_box.append(&title_entry);

    // Caption / Description
    let desc_lbl = Label::new(Some("Caption / Description"));
    desc_lbl.set_css_classes(&["caption-heading"]);
    desc_lbl.set_halign(Align::Start);
    meta_edit_box.append(&desc_lbl);

    let desc_entry = Entry::new();
    desc_entry.set_placeholder_text(Some("Add a caption..."));
    if let Some(ref d) = image.description {
        desc_entry.set_text(d);
    }
    let db_d = db.clone();
    let p_d = img_path.clone();
    desc_entry.connect_activate(move |entry| {
        let text = entry.text().to_string();
        if let (Some(id), Ok(conn)) = (img_id, db_d.conn()) {
            let _ = queries::set_description(&conn, id, &text);
            sync_image_xmp(&conn, id, &p_d, img_rating, img_flag);
        }
    });
    meta_edit_box.append(&desc_entry);

    // Tags Section
    let tags_lbl = Label::new(Some("Tags"));
    tags_lbl.set_css_classes(&["caption-heading"]);
    tags_lbl.set_halign(Align::Start);
    meta_edit_box.append(&tags_lbl);

    let chips_box = GtkBox::new(Orientation::Vertical, 4);
    meta_edit_box.append(&chips_box);

    let add_tag_entry = Entry::new();
    add_tag_entry.set_placeholder_text(Some("+ Add tag and press Enter"));
    meta_edit_box.append(&add_tag_entry);

    if let Some(id) = img_id {
        let render_tags: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let rt_self = render_tags.clone();
        let rt_action = {
            let db = db.clone();
            let chips_box = chips_box.clone();
            let img_path = img_path.clone();
            let rt_recurse = rt_self.clone();
            Rc::new(move || {
                while let Some(c) = chips_box.first_child() {
                    chips_box.remove(&c);
                }
                let Ok(conn) = db.conn() else { return };
                let tags = queries::get_tags_for_image(&conn, id).unwrap_or_default();
                let flow = FlowBox::new();
                flow.set_selection_mode(gtk4::SelectionMode::None);
                flow.set_max_children_per_line(12);
                flow.set_row_spacing(4);
                flow.set_column_spacing(4);

                for tag in tags {
                    let chip = GtkBox::new(Orientation::Horizontal, 4);
                    chip.add_css_class("tag-chip");
                    let lbl = Label::new(Some(&format!("#{}", tag.name)));
                    chip.append(&lbl);

                    let rm_btn = Button::from_icon_name("window-close-symbolic");
                    rm_btn.add_css_class("flat");
                    rm_btn.add_css_class("tag-remove-btn");
                    let db_rm = db.clone();
                    let p_rm = img_path.clone();
                    let tid = tag.id;
                    let rt_call = rt_recurse.clone();
                    rm_btn.connect_clicked(move |_| {
                        if let (Some(tag_id), Ok(conn)) = (tid, db_rm.conn()) {
                            let _ = queries::untag_image(&conn, id, tag_id);
                            sync_image_xmp(&conn, id, &p_rm, img_rating, img_flag);
                            if let Some(r) = rt_call.borrow().as_ref() {
                                r();
                            }
                        }
                    });
                    chip.append(&rm_btn);
                    flow.append(&chip);
                }
                chips_box.append(&flow);
            })
        };
        *render_tags.borrow_mut() = Some(rt_action.clone());
        rt_action();

        let db_at = db.clone();
        let p_at = img_path.clone();
        let rt_at = rt_action.clone();
        add_tag_entry.connect_activate(move |entry| {
            let text = entry.text().trim().to_string();
            if !text.is_empty() {
                if let Ok(conn) = db_at.conn() {
                    if let Ok(tag_id) = queries::create_tag(&conn, &text, None) {
                        let _ = queries::tag_image(&conn, id, tag_id);
                        sync_image_xmp(&conn, id, &p_at, img_rating, img_flag);
                    }
                }
                entry.set_text("");
                rt_at();
            }
        });
    }

    columns.append(&meta_edit_box);

    // Section 3: Details (Metadata)
    let meta_col = GtkBox::new(Orientation::Vertical, 3);
    let meta_header = Label::new(Some("Details"));
    meta_header.set_css_classes(&["title-4"]);
    meta_header.set_halign(Align::Start);
    meta_col.append(&meta_header);

    add_row(&meta_col, "File", &image.filename);
    add_row(&meta_col, "Path", &image.path.to_string_lossy());
    if let Some(ts) = image.created_at {
        // Camera wall-clock time, stored as UTC: format without zone conversion.
        let dt = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default();
        add_row(&meta_col, "Date", &dt.format("%Y-%m-%d %H:%M").to_string());
    }
    if let (Some(w), Some(h)) = (image.width, image.height) {
        add_row(
            &meta_col,
            "Size",
            &format!("{} × {}  ({})", w, h, format_size(image.size_bytes)),
        );
    } else {
        add_row(&meta_col, "Size", &format_size(image.size_bytes));
    }
    if let Some(ref v) = image.camera_make {
        add_row(&meta_col, "Camera", v);
    }
    if let Some(ref v) = image.camera_model {
        add_row(&meta_col, "Model", v);
    }

    let mut exif_parts: Vec<String> = Vec::new();
    if let Some(f) = image.focal_length {
        exif_parts.push(format!("{:.0}mm", f));
    }
    if let Some(a) = image.aperture {
        exif_parts.push(format!("f/{:.1}", a));
    }
    if let Some(ref ss) = image.shutter_speed {
        exif_parts.push(ss.clone());
    }
    if let Some(iso) = image.iso {
        exif_parts.push(format!("ISO {}", iso));
    }
    if !exif_parts.is_empty() {
        add_row(&meta_col, "EXIF", &exif_parts.join("  "));
    }
    if let Some(ref fmt) = image.format {
        add_row(&meta_col, "Format", fmt.as_str());
    }

    columns.append(&meta_col);

    // Column 2: Open With (shows editors for THIS file)
    let open_col = GtkBox::new(Orientation::Vertical, 3);
    let open_header = Label::new(Some("Open With"));
    open_header.set_css_classes(&["title-4"]);
    open_header.set_halign(Align::Start);
    open_col.append(&open_header);

    let is_raw = image.format.as_ref().map(|f| f.is_raw()).unwrap_or(false);

    // Viewer
    let btn_view = make_btn(&format!("View ({})", prefs.viewer));
    let p1 = image.path.clone();
    let cmd1 = prefs.viewer.clone();
    btn_view.connect_clicked(move |_| {
        let _ = Command::new(&cmd1).arg(&p1).spawn();
    });
    open_col.append(&btn_view);

    // Primary editor based on format
    let (editor_cmd, editor_label) = if is_raw {
        (prefs.raw_editor.clone(), "RAW Editor")
    } else {
        (prefs.raster_editor.clone(), "Editor")
    };
    let btn_edit = make_btn(&format!("{} ({})", editor_label, editor_cmd));
    let p2 = image.path.clone();
    let cmd2 = editor_cmd.clone();
    let db2 = db.clone();
    let img_id = image.id;
    let tool = editor_cmd.clone();
    btn_edit.connect_clicked(move |_| {
        let _ = Command::new(&cmd2).arg(&p2).spawn();
        if let (Some(id), Ok(conn)) = (img_id, db2.conn()) {
            let _ = queries::record_edit(&conn, id, &tool, None);
        }
    });
    open_col.append(&btn_edit);

    // Secondary editor for RAW files (also offer raster editor)
    if is_raw {
        let btn_raster = make_btn(&format!("Raster ({})", prefs.raster_editor));
        let p3 = image.path.clone();
        let cmd3 = prefs.raster_editor.clone();
        let db3 = db.clone();
        let img_id2 = image.id;
        let tool2 = prefs.raster_editor.clone();
        btn_raster.connect_clicked(move |_| {
            let _ = Command::new(&cmd3).arg(&p3).spawn();
            if let (Some(id), Ok(conn)) = (img_id2, db3.conn()) {
                let _ = queries::record_edit(&conn, id, &tool2, None);
            }
        });
        open_col.append(&btn_raster);
    }

    columns.append(&open_col);

    // Column 3: Sidecar Group — show all related files
    if let Some(ref group_hash) = image.group_hash {
        if let Ok(conn) = db.conn() {
            if let Ok(group_images) = queries::get_images_in_group(&conn, group_hash) {
                if group_images.len() > 1 {
                    let group_col = GtkBox::new(Orientation::Vertical, 3);
                    let group_header =
                        Label::new(Some(&format!("Sidecar Group ({} files)", group_images.len())));
                    group_header.set_css_classes(&["title-4"]);
                    group_header.set_halign(Align::Start);
                    group_col.append(&group_header);

                    for gi in &group_images {
                        let gi_is_raw =
                            gi.format.as_ref().map(|f| f.is_raw()).unwrap_or(false);
                        let gi_is_edit = gi.is_edited_variant();

                        let label = if gi_is_raw {
                            format!("RAW: {}", gi.filename)
                        } else if gi_is_edit {
                            format!("Edit: {}", gi.filename)
                        } else {
                            format!("JPG: {}", gi.filename)
                        };

                        // Which editor to use for this variant
                        let cmd = if gi_is_raw {
                            prefs.raw_editor.clone()
                        } else {
                            prefs.raster_editor.clone()
                        };

                        let btn = make_btn(&format!(
                            "{} → {}",
                            label,
                            cmd
                        ));
                        let path = gi.path.clone();
                        let cmd_clone = cmd.clone();
                        let db_g = db.clone();
                        let gi_id = gi.id;
                        let tool_name = cmd.clone();
                        btn.connect_clicked(move |_| {
                            let _ = Command::new(&cmd_clone).arg(&path).spawn();
                            if let (Some(id), Ok(conn)) = (gi_id, db_g.conn()) {
                                let _ = queries::record_edit(&conn, id, &tool_name, None);
                            }
                        });
                        group_col.append(&btn);
                    }

                    // Show XMP info if present
                    if let Some(ref json) = image.metadata_json {
                        if json.contains("xmp_sidecar") {
                            let xmp_lbl = Label::new(Some("Has Darktable XMP sidecar"));
                            xmp_lbl.set_css_classes(&["caption", "dim-label"]);
                            xmp_lbl.set_halign(Align::Start);
                            xmp_lbl.set_margin_top(4);
                            group_col.append(&xmp_lbl);
                        }
                    }

                    columns.append(&group_col);
                }
            }
        }
    }

    // Column 4: Edit History (if no sidecar group, or in addition to it)
    if let Some(id) = image.id {
        if let Ok(conn) = db.conn() {
            if let Ok(edits) = queries::get_edit_history(&conn, id) {
                if !edits.is_empty() {
                    let hist_col = GtkBox::new(Orientation::Vertical, 3);
                    let hist_header = Label::new(Some("Edit History"));
                    hist_header.set_css_classes(&["title-4"]);
                    hist_header.set_halign(Align::Start);
                    hist_col.append(&hist_header);

                    for edit in edits.iter().take(5) {
                        let dt =
                            chrono::DateTime::from_timestamp(edit.edited_at, 0).unwrap_or_default();
                        let text = format!("{} — {}", edit.tool_name, dt.format("%Y-%m-%d %H:%M"));
                        let lbl = Label::new(Some(&text));
                        lbl.set_css_classes(&["caption"]);
                        lbl.set_halign(Align::Start);
                        hist_col.append(&lbl);
                    }

                    columns.append(&hist_col);
                }
            }
        }
    }

    info_box.append(&columns);
}

/// Generate the large preview on a worker thread and show it in `picture`,
/// unless the viewer has moved on to another photo by then.
fn load_large_preview(picture: &Picture, image: &Image, cache_dir: &Path) {
    let generator = ThumbnailGenerator::new(cache_dir.to_path_buf());
    let image = image.clone();
    let weak = picture.downgrade();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || generator.ensure(&image, ThumbSize::Large)).await;
        match result {
            Ok(Ok(path)) => {
                if let Some(picture) = weak.upgrade().filter(|p| p.parent().is_some()) {
                    load_texture_async(&picture, &path);
                }
            }
            Ok(Err(e)) => log::warn!("Large preview failed: {e:#}"),
            Err(_) => log::warn!("Large preview worker panicked"),
        }
    });
}

/// Open in the appropriate editor based on format.
fn open_in_editor(image: &Image, prefs: &Preferences, db: &Database) {
    let is_raw = image.format.as_ref().map(|f| f.is_raw()).unwrap_or(false);
    let cmd = if is_raw {
        &prefs.raw_editor
    } else {
        &prefs.raster_editor
    };
    let _ = Command::new(cmd).arg(&image.path).spawn();

    if let (Some(id), Ok(conn)) = (image.id, db.conn()) {
        let _ = queries::record_edit(&conn, id, cmd, None);
    }
}

// ── Helpers ──────────────────────────────────────────────

fn add_row(container: &GtkBox, label: &str, value: &str) {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    let lbl = Label::new(Some(label));
    lbl.set_css_classes(&["dim-label"]);
    lbl.set_valign(Align::Start);
    lbl.set_width_chars(7);
    lbl.set_xalign(0.0);
    // Narrow panel: wrap long values (paths) anywhere instead of cutting them.
    let val = Label::new(Some(value));
    val.set_hexpand(true);
    val.set_xalign(0.0);
    val.set_wrap(true);
    val.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    // A wrapping label's natural width is its unwrapped text; cap it so long
    // values wrap inside the panel instead of widening it.
    val.set_max_width_chars(24);
    val.set_selectable(true);
    val.set_can_focus(false);
    row.append(&lbl);
    row.append(&val);
    container.append(&row);
}

fn make_btn(label: &str) -> Button {
    let btn = Button::builder().has_frame(false).build();
    btn.add_css_class("flat");
    let b = GtkBox::new(Orientation::Horizontal, 8);
    b.set_margin_top(2);
    b.set_margin_bottom(2);
    b.set_margin_start(4);
    b.set_margin_end(4);
    b.append(&gtk4::Image::from_icon_name(
        "application-x-executable-symbolic",
    ));
    b.append(&Label::new(Some(label)));
    btn.set_child(Some(&b));
    btn
}

fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn render_compare(
    image_box: &GtkBox,
    info_box: &GtkBox,
    img_a: &Image,
    img_b: &Image,
    cache_dir: &Path,
    prefs: &Preferences,
    db: &Database,
) {
    clear(image_box);
    clear(info_box);

    let split_box = GtkBox::new(Orientation::Horizontal, 12);
    split_box.set_hexpand(true);
    split_box.set_vexpand(true);
    split_box.set_homogeneous(true);
    split_box.set_halign(Align::Fill);
    split_box.set_valign(Align::Fill);

    // Left pane (Image A)
    let left_box = GtkBox::new(Orientation::Vertical, 6);
    left_box.set_hexpand(true);
    left_box.set_vexpand(true);
    left_box.set_halign(Align::Fill);
    left_box.set_valign(Align::Fill);

    let pic_a = Picture::new();
    let large_a = thumb_path(cache_dir, ThumbSize::Large, &img_a.hash);
    let grid_a = thumb_path(cache_dir, ThumbSize::Grid, &img_a.hash);
    if grid_a.exists() {
        pic_a.set_filename(Some(&grid_a));
    }
    if large_a.exists() {
        load_texture_async(&pic_a, &large_a);
    } else {
        load_large_preview(&pic_a, img_a, cache_dir);
    }
    pic_a.set_content_fit(gtk4::ContentFit::Contain);
    pic_a.set_can_shrink(true);
    pic_a.set_hexpand(true);
    pic_a.set_vexpand(true);
    pic_a.set_halign(Align::Fill);
    pic_a.set_valign(Align::Fill);

    let flag_str_a = match img_a.flagged {
        1 => " [Pick ✓]",
        -1 => " [Reject ✕]",
        _ => "",
    };
    let lbl_a = Label::new(Some(&format!("A: {} (★ {}){}", img_a.filename, img_a.rating, flag_str_a)));
    lbl_a.add_css_class("heading");
    lbl_a.set_halign(Align::Center);
    left_box.append(&lbl_a);
    left_box.append(&pic_a);
    split_box.append(&left_box);

    // Right pane (Image B)
    let right_box = GtkBox::new(Orientation::Vertical, 6);
    right_box.set_hexpand(true);
    right_box.set_vexpand(true);
    right_box.set_halign(Align::Fill);
    right_box.set_valign(Align::Fill);

    let pic_b = Picture::new();
    let large_b = thumb_path(cache_dir, ThumbSize::Large, &img_b.hash);
    let grid_b = thumb_path(cache_dir, ThumbSize::Grid, &img_b.hash);
    if grid_b.exists() {
        pic_b.set_filename(Some(&grid_b));
    }
    if large_b.exists() {
        load_texture_async(&pic_b, &large_b);
    } else {
        load_large_preview(&pic_b, img_b, cache_dir);
    }
    pic_b.set_content_fit(gtk4::ContentFit::Contain);
    pic_b.set_can_shrink(true);
    pic_b.set_hexpand(true);
    pic_b.set_vexpand(true);
    pic_b.set_halign(Align::Fill);
    pic_b.set_valign(Align::Fill);

    let flag_str_b = match img_b.flagged {
        1 => " [Pick ✓]",
        -1 => " [Reject ✕]",
        _ => "",
    };
    let lbl_b = Label::new(Some(&format!("B: {} (★ {}){}", img_b.filename, img_b.rating, flag_str_b)));
    lbl_b.add_css_class("heading");
    lbl_b.set_halign(Align::Center);
    right_box.append(&lbl_b);
    right_box.append(&pic_b);
    split_box.append(&right_box);

    image_box.append(&split_box);
    populate_info_panel(info_box, img_a, cache_dir, prefs, db);
}
