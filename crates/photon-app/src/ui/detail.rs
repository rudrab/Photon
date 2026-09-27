//! Inline photo viewer: image-dominant with compact controls.
//!
//! Layout:
//!   ┌──────────────────────────────────────────────┐
//!   │ [←] [◂] [▸]  filename.jpg   3/42   [ℹ] [⚙] │  ← compact toolbar
//!   ├──────────────────────────────────────────────┤
//!   │                                              │
//!   │              (large image)                   │
//!   │                                              │
//!   ├──────────────────────────────────────────────┤
//!   │  Info panel (revealed on ℹ click)            │  ← slides up from bottom
//!   └──────────────────────────────────────────────┘
//!
//! Keyboard: Left=prev, Right=next, Escape=back, i=toggle info

use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, EventControllerKey, Label, Orientation, Picture, Revealer,
    ScrolledWindow,
};
use photon_core::db::queries;
use photon_core::db::Database;
use gtk4::{gio, glib};
use photon_core::models::{Image, Preferences, TimelineItem, UIAction};
use photon_import::thumbnails::{thumb_path, ThumbSize, ThumbnailGenerator};
use crate::ui::widgets::load_texture_async;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

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

    let back_btn = Button::from_icon_name("go-previous-symbolic");
    back_btn.add_css_class("flat");
    back_btn.set_tooltip_text(Some("Back (Esc)"));

    let nav_group = GtkBox::new(Orientation::Horizontal, 0);
    nav_group.add_css_class("linked");

    let prev_btn = Button::from_icon_name("go-up-symbolic");
    prev_btn.add_css_class("flat");
    prev_btn.set_tooltip_text(Some("Previous (←)"));

    let next_btn = Button::from_icon_name("go-down-symbolic");
    next_btn.add_css_class("flat");
    next_btn.set_tooltip_text(Some("Next (→)"));

    nav_group.append(&prev_btn);
    nav_group.append(&next_btn);

    let filename_label = Label::new(None);
    filename_label.set_hexpand(true);
    filename_label.set_halign(Align::Center);
    filename_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    filename_label.set_css_classes(&["heading"]);

    let counter_label = Label::new(None);
    counter_label.set_css_classes(&["photon-badge"]);
    counter_label.set_margin_end(8);

    let info_btn = Button::from_icon_name("dialog-information-symbolic");
    info_btn.add_css_class("flat");
    info_btn.set_tooltip_text(Some("Toggle info (I)"));

    let open_btn = Button::from_icon_name("document-edit-symbolic");
    open_btn.add_css_class("flat");
    open_btn.set_tooltip_text(Some("Open in editor"));

    toolbar.append(&back_btn);
    toolbar.append(&nav_group);
    toolbar.append(&filename_label);
    toolbar.append(&counter_label);
    toolbar.append(&info_btn);
    toolbar.append(&open_btn);
    root.append(&toolbar);

    // ── Image area (takes all remaining space) ──────────
    let image_box = GtkBox::new(Orientation::Vertical, 0);
    image_box.set_vexpand(true);
    image_box.set_hexpand(true);
    root.append(&image_box);

    // ── Info panel (revealed from bottom) ───────────────
    let info_revealer = Revealer::new();
    info_revealer.set_transition_type(gtk4::RevealerTransitionType::SlideUp);
    info_revealer.set_reveal_child(false);

    let info_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .max_content_height(260)
        .min_content_height(150)
        .build();
    let info_box = GtkBox::new(Orientation::Vertical, 6);
    info_box.set_margin_top(12);
    info_box.set_margin_bottom(12);
    info_box.set_margin_start(16);
    info_box.set_margin_end(16);
    info_box.add_css_class("card");
    info_box.add_css_class("info-sheet");
    info_scroll.set_child(Some(&info_box));
    info_revealer.set_child(Some(&info_scroll));
    root.append(&info_revealer);

    // ── Showing a photo ─────────────────────────────────
    // Closures hold widgets weakly: the buttons own these closures, so strong
    // references would form cycles and leak every viewer ever opened.
    let current_idx = Rc::new(Cell::new(index));
    let current_image: Rc<RefCell<Option<Image>>> = Rc::new(RefCell::new(None));
    let cache = cache_dir.to_path_buf();
    let prefs = Rc::new(prefs.clone());

    let show: Rc<dyn Fn(usize)> = Rc::new(glib::clone!(
        #[weak] image_box,
        #[weak] info_box,
        #[weak] filename_label,
        #[weak] counter_label,
        #[weak] prev_btn,
        #[weak] next_btn,
        #[strong] photos,
        #[strong] current_idx,
        #[strong] current_image,
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
                    filename_label.set_text(&img.filename);
                    render_photo(&image_box, &info_box, img, &cache, &prefs, &db);
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

    // ── Buttons ─────────────────────────────────────────
    let tx_back = nav_tx.clone();
    let back = back_action.clone();
    back_btn.connect_clicked(move |_| {
        let _ = tx_back.send_blocking(back.clone());
    });

    info_btn.connect_clicked(glib::clone!(
        #[weak] info_revealer,
        move |_| info_revealer.set_reveal_child(!info_revealer.reveals_child())
    ));

    let db_open = db.clone();
    let prefs_open = prefs.clone();
    let image_open = current_image.clone();
    open_btn.connect_clicked(move |_| {
        if let Some(image) = image_open.borrow().as_ref() {
            open_in_editor(image, &prefs_open, &db_open);
        }
    });

    let step_prev = step.clone();
    prev_btn.connect_clicked(move |_| step_prev(-1));
    let step_next = step.clone();
    next_btn.connect_clicked(move |_| step_next(1));

    // ── Keyboard navigation ─────────────────────────────
    let key_ctrl = EventControllerKey::new();
    let tx_esc = nav_tx.clone();
    key_ctrl.connect_key_pressed(glib::clone!(
        #[weak] info_revealer,
        #[upgrade_or] glib::Propagation::Proceed,
        move |_, key, _, _| {
            match key {
                gtk4::gdk::Key::Escape => {
                    let _ = tx_esc.send_blocking(back_action.clone());
                }
                gtk4::gdk::Key::Left => step(-1),
                gtk4::gdk::Key::Right => step(1),
                gtk4::gdk::Key::i | gtk4::gdk::Key::I => {
                    info_revealer.set_reveal_child(!info_revealer.reveals_child());
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
) {
    clear(image_box);
    clear(info_box);

    // ── Large image ─────────────────────────────────────
    // Show the cached large preview if there is one; otherwise show the grid
    // thumbnail now and swap in the large preview once it's generated.
    let large = thumb_path(cache_dir, ThumbSize::Large, &image.hash);
    let grid = thumb_path(cache_dir, ThumbSize::Grid, &image.hash);
    let picture = Picture::new();
    if large.exists() {
        load_texture_async(&picture, &large);
    } else {
        if grid.exists() {
            picture.set_filename(Some(&grid)); // small: fine to decode inline
        }
        load_large_preview(&picture, image, cache_dir);
    }
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    image_box.append(&picture);

    // ── Info panel content (horizontal: metadata | open-with | group/history) ─
    let columns = GtkBox::new(Orientation::Horizontal, 24);

    // Column 1: Metadata
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
    let row = GtkBox::new(Orientation::Horizontal, 6);
    let lbl = Label::new(Some(label));
    lbl.set_css_classes(&["dim-label"]);
    lbl.set_halign(Align::Start);
    lbl.set_width_chars(7);
    lbl.set_xalign(0.0);
    let val = Label::new(Some(value));
    val.set_halign(Align::Start);
    val.set_wrap(true);
    val.set_xalign(0.0);
    val.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
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
