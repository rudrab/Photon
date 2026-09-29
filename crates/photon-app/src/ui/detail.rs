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

#![allow(deprecated)]

use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{
    gio, glib, ActionBar, Align, Box as GtkBox, Button, Entry, EventControllerKey,
    FileChooserAction, FileChooserNative, FlowBox, Label, MediaControls,
    Orientation, Picture, ResponseType, Revealer, Scale, ScrolledWindow,
    ToggleButton, Video, Window,
};
use photon_core::db::queries;
use photon_core::db::Database;
use photon_core::models::{Image, Preferences, TimelineItem, UIAction};
use photon_import::thumbnails::{thumb_path, ThumbSize, ThumbnailGenerator};
use crate::ui::histogram::HistogramWidget;
use libadwaita as adw;
use libadwaita::prelude::MessageDialogExt;
use crate::ui::share;
use crate::ui::widgets::load_texture_async;
use crate::ui::photo_view::{PhotoView, ViewState, Zoom, MAX_ZOOM, ZOOM_STEP};
pub use crate::ui::photo_view::invalidate_full_res;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
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
    share_ctx: share::Context,
    undo_manager: Option<Rc<RefCell<crate::ui::undo::UndoManager>>>,
    on_start_slideshow: Option<Rc<dyn Fn()>>,
    parent_window: Option<gtk4::Window>,
) -> GtkBox {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.set_vexpand(true);
    root.set_hexpand(true);
    root.set_focusable(true);

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
    info_revealer.set_reveal_child(false);
    // Its entries expand, which would otherwise make the (hidden) panel take
    // half the spare width from the photo.
    info_revealer.set_hexpand(false);

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

    // ── Docked Bottom Action Bar (Adwaita / Material 3) ──
    let action_bar = ActionBar::new();
    action_bar.add_css_class("photon-bottom-bar");

    // ── Start area: Navigation & Title ──────────────────
    let start_box = GtkBox::new(Orientation::Horizontal, 6);
    start_box.set_valign(Align::Center);

    let back_btn = Button::from_icon_name("view-grid-symbolic");
    back_btn.add_css_class("flat");
    back_btn.set_tooltip_text(Some("Back to library (Esc)"));
    let tx_b = nav_tx.clone();
    let ba_b = back_action.clone();
    back_btn.connect_clicked(move |_| {
        let _ = tx_b.send_blocking(ba_b.clone());
    });
    start_box.append(&back_btn);

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
    start_box.append(&nav_group);

    let title_box = GtkBox::new(Orientation::Horizontal, 6);
    title_box.add_css_class("photon-viewer-title-box");
    title_box.set_valign(Align::Center);

    let filename_label = Label::new(None);
    filename_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    filename_label.set_max_width_chars(24);
    filename_label.set_css_classes(&["heading"]);

    let counter_label = Label::new(None);
    counter_label.set_css_classes(&["photon-badge"]);

    title_box.append(&filename_label);
    title_box.append(&counter_label);
    start_box.append(&title_box);

    action_bar.pack_start(&start_box);

    // ── Center area: Zoom & View Tools ──────────────────
    let center_box = GtkBox::new(Orientation::Horizontal, 8);
    center_box.set_valign(Align::Center);

    // Zoom: Fit, 1:1 and a continuous slider. The slider is log2 of the
    // zoom, so every step along it is the same ratio.
    let zoom_group = GtkBox::new(Orientation::Horizontal, 2);
    zoom_group.add_css_class("zoom-slider-group");
    zoom_group.set_valign(Align::Center);

    let fit_btn = Button::from_icon_name("zoom-fit-best-symbolic");
    fit_btn.add_css_class("flat");
    fit_btn.set_tooltip_text(Some("Fit to window (Ctrl+0)"));

    let one_btn = Button::from_icon_name("zoom-original-symbolic");
    one_btn.add_css_class("flat");
    one_btn.set_tooltip_text(Some("Actual pixels, 100% (Z / Ctrl+1)"));

    let zoom_scale = Scale::with_range(Orientation::Horizontal, -3.0, MAX_ZOOM.log2(), 0.01);
    zoom_scale.set_width_request(120);
    zoom_scale.set_draw_value(false);
    // Arrow keys step through photos; a focused slider would take them.
    zoom_scale.set_focusable(false);
    zoom_scale.set_tooltip_text(Some("Zoom (Ctrl+scroll, + / −)"));

    let zoom_label = Label::new(Some("Fit"));
    zoom_label.set_width_chars(5);
    zoom_label.add_css_class("caption");
    zoom_label.add_css_class("numeric");

    zoom_group.append(&fit_btn);
    zoom_group.append(&zoom_scale);
    zoom_group.append(&one_btn);
    zoom_group.append(&zoom_label);
    center_box.append(&zoom_group);

    let sep_tools = gtk4::Separator::new(Orientation::Vertical);
    center_box.append(&sep_tools);

    // Rotate buttons
    let rotate_group = GtkBox::new(Orientation::Horizontal, 0);
    rotate_group.add_css_class("linked");

    let rotate_left_btn = Button::from_icon_name("object-rotate-left-symbolic");
    rotate_left_btn.add_css_class("flat");
    rotate_left_btn.set_tooltip_text(Some("Rotate counter-clockwise ([)"));

    let rotate_right_btn = Button::from_icon_name("object-rotate-right-symbolic");
    rotate_right_btn.add_css_class("flat");
    rotate_right_btn.set_tooltip_text(Some("Rotate clockwise (] / Ctrl+R)"));

    rotate_group.append(&rotate_left_btn);
    rotate_group.append(&rotate_right_btn);
    center_box.append(&rotate_group);

    // Star rating
    let rating_btn = Button::with_label("★ 0");
    rating_btn.add_css_class("flat");
    rating_btn.set_tooltip_text(Some("Rating (1-5, 0 to clear)"));
    center_box.append(&rating_btn);

    // Compare button
    let compare_btn = ToggleButton::builder()
        .icon_name("view-dual-symbolic")
        .tooltip_text("Side-by-side Compare (C)")
        .build();
    compare_btn.add_css_class("flat");
    center_box.append(&compare_btn);

    action_bar.set_center_widget(Some(&center_box));

    // ── End area: Actions, Slideshow & Info ──────────────
    let end_box = GtkBox::new(Orientation::Horizontal, 6);
    end_box.set_valign(Align::Center);

    let action_group = GtkBox::new(Orientation::Horizontal, 0);
    action_group.add_css_class("linked");

    let open_btn = Button::from_icon_name("document-edit-symbolic");
    open_btn.add_css_class("flat");
    open_btn.set_tooltip_text(Some("Open in editor"));
    action_group.append(&open_btn);

    let current_image: Rc<RefCell<Option<Image>>> = Rc::new(RefCell::new(None));
    let image_share = current_image.clone();
    let photo: share::Photos = Rc::new(move || image_share.borrow().iter().cloned().collect());
    let share_btn = share::menu_button(&share_ctx, photo.clone());
    action_group.append(&share_btn);

    let export_btn = Button::from_icon_name("document-save-symbolic");
    export_btn.add_css_class("flat");
    export_btn.set_tooltip_text(Some("Export Photo (Ctrl+E)"));
    action_group.append(&export_btn);

    let trash_btn = Button::from_icon_name("user-trash-symbolic");
    trash_btn.add_css_class("flat");
    trash_btn.add_css_class("destructive-action");
    trash_btn.set_tooltip_text(Some("Move to Trash (Delete)"));
    action_group.append(&trash_btn);

    end_box.append(&action_group);

    let sep_end = gtk4::Separator::new(Orientation::Vertical);
    end_box.append(&sep_end);

    let slideshow_btn = Button::from_icon_name("media-playback-start-symbolic");
    slideshow_btn.add_css_class("flat");
    slideshow_btn.set_tooltip_text(Some("Start Slideshow (F5)"));
    if let Some(ref ss) = on_start_slideshow {
        let ss_cb = ss.clone();
        slideshow_btn.connect_clicked(move |_| ss_cb());
    } else {
        slideshow_btn.set_visible(false);
    }
    end_box.append(&slideshow_btn);

    let info_btn = ToggleButton::builder()
        .icon_name("dialog-information-symbolic")
        .tooltip_text("Show details (I)")
        .active(false)
        .build();
    info_btn.add_css_class("flat");
    end_box.append(&info_btn);

    action_bar.pack_end(&end_box);

    root.append(&action_bar);

    // ── Showing a photo ─────────────────────────────────
    let current_idx = Rc::new(Cell::new(index));
    let is_comparing = Rc::new(Cell::new(false));
    let cache = cache_dir.to_path_buf();
    let prefs = Rc::new(prefs.clone());

    // The photo being shown, when it is a still photo on its own (not a
    // video or a comparison): what the zoom controls act on.
    let current_view: Rc<RefCell<Option<Rc<PhotoView>>>> = Rc::default();
    // Set while the controls follow the view, so the slider doesn't zoom back.
    let syncing_zoom = Rc::new(Cell::new(false));

    let sync_zoom_ui: Rc<dyn Fn(&PhotoView)> = Rc::new(glib::clone!(
        #[weak] zoom_scale,
        #[weak] zoom_label,
        #[strong] syncing_zoom,
        move |view: &PhotoView| {
            let scale = view.scale();
            syncing_zoom.set(true);
            let adj = zoom_scale.adjustment();
            let lower = view.fit_scale().min(1.0).log2();
            adj.set_lower(lower);
            adj.set_upper(MAX_ZOOM.max(view.fit_scale()).log2());
            adj.set_value(scale.log2().max(lower));
            syncing_zoom.set(false);
            let pct = format!("{:.0}%", scale * 100.0);
            zoom_label.set_text(if view.zoom() == Zoom::Fit { "Fit" } else { &pct });
            zoom_label.set_tooltip_text(Some(&pct));
        }
    ));

    let with_view = {
        let current_view = current_view.clone();
        move |f: &dyn Fn(&Rc<PhotoView>)| {
            let view = current_view.borrow().clone();
            if let Some(view) = view {
                f(&view);
            }
        }
    };
    let toggle_zoom: Rc<dyn Fn()> = {
        let with_view = with_view.clone();
        Rc::new(move || with_view(&|v| v.toggle(None)))
    };
    let zoom_by: Rc<dyn Fn(f64)> = {
        let with_view = with_view.clone();
        Rc::new(move |factor| with_view(&|v| v.zoom_by(factor, None)))
    };
    let zoom_to: Rc<dyn Fn(Zoom)> = {
        let with_view = with_view.clone();
        Rc::new(move |zoom| with_view(&|v| v.set_zoom(zoom, None)))
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
        #[weak] zoom_group,
        #[weak] zoom_label,
        #[weak] compare_btn,
        #[weak] rotate_left_btn,
        #[weak] rotate_right_btn,
        #[strong] photos,
        #[strong] current_idx,
        #[strong] current_image,
        #[strong] current_view,
        #[strong] sync_zoom_ui,
        #[strong] is_comparing,
        #[strong] prefs,
        #[strong] db,
        #[strong] cache,
        #[strong] undo_manager,
        move |i: usize| {
            let Some(item) = photos.get(i) else { return };
            // Shown again (after a rating change): keep the zoom and position.
            // Another photo starts at Fit.
            let restore = if i == current_idx.get() {
                current_view.borrow_mut().take().map(|v| v.state())
            } else {
                current_view.borrow_mut().take();
                None
            };
            current_idx.set(i);
            counter_label.set_text(&format!("{}/{}", i + 1, photos.len()));
            prev_btn.set_sensitive(i > 0);
            next_btn.set_sensitive(i + 1 < photos.len());

            let mut image = db
                .conn()
                .ok()
                .and_then(|c| queries::get_image(&c, item.id).ok().flatten());

            if let Some(ref mut img) = image {
                let exists = img.path.exists();
                if exists == img.missing {
                    img.missing = !exists;
                    let marked = db.conn().map_err(|e| e.to_string()).and_then(|conn| {
                        queries::mark_missing(&conn, &[item.id], !exists).map_err(|e| e.to_string())
                    });
                    if let Err(e) = marked {
                        log::warn!("Marking {} missing={}: {e}", img.path.display(), !exists);
                    }
                }

                if exists {
                    let applied = db.conn().map_err(anyhow::Error::from).and_then(|mut conn| {
                        let applied = photon_import::read_image_xmp(&mut conn, item.id, &img.path, img.xmp_mtime)?;
                        if applied {
                            if let Some(refreshed) = queries::get_image(&conn, item.id)? {
                                *img = refreshed;
                            }
                        }
                        Ok(applied)
                    });
                    if let Err(e) = applied {
                        log::warn!("Reading XMP for {}: {e}", img.path.display());
                    }
                }
            }

            match &image {
                Some(img) => {
                    let is_vid = img.format.as_ref().map_or(false, |f| f.is_video());
                    compare_btn.set_sensitive(!is_vid);
                    rotate_left_btn.set_sensitive(!is_vid);
                    rotate_right_btn.set_sensitive(!is_vid);

                    let summary = format_metadata_summary(img);
                    if !summary.is_empty() {
                        filename_label.set_tooltip_text(Some(&format!("{}\n{}", img.filename, summary)));
                    } else {
                        filename_label.set_tooltip_text(Some(&img.filename));
                    }

                    let has_darktable_xmp = {
                        let p = &img.path;
                        let appended = {
                            let mut a = p.as_os_str().to_owned();
                            a.push(".xmp");
                            PathBuf::from(a)
                        };
                        let darktable_dot_xmp = p.with_extension("darktable.xmp");
                        darktable_dot_xmp.exists()
                            || (appended.exists()
                                && std::fs::read_to_string(&appended)
                                    .map(|c| c.contains("darktable:"))
                                    .unwrap_or(false))
                    };
                    if has_darktable_xmp {
                        rotate_left_btn.set_tooltip_text(Some(
                            "Rotate counter-clockwise ([)\nDarktable edit history exists; orientation change written to XMP sidecar but darktable may override on next export.",
                        ));
                        rotate_right_btn.set_tooltip_text(Some(
                            "Rotate clockwise (] / Ctrl+R)\nDarktable edit history exists; orientation change written to XMP sidecar but darktable may override on next export.",
                        ));
                    } else {
                        rotate_left_btn.set_tooltip_text(Some("Rotate counter-clockwise ([)"));
                        rotate_right_btn.set_tooltip_text(Some("Rotate clockwise (] / Ctrl+R)"));
                    }

                    rating_btn.set_label(&format!("★ {}", img.rating));

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
                            *current_view.borrow_mut() = render_photo(
                                &image_box,
                                &info_box,
                                img,
                                &cache,
                                &prefs,
                                &db,
                                restore,
                                sync_zoom_ui.clone(),
                                undo_manager.clone(),
                            );
                        }
                    } else {
                        filename_label.set_text(&img.filename);
                        *current_view.borrow_mut() = render_photo(
                            &image_box,
                            &info_box,
                            img,
                            &cache,
                            &prefs,
                            &db,
                            restore,
                            sync_zoom_ui.clone(),
                            undo_manager.clone(),
                        );
                    }
                }
                None => {
                    filename_label.set_text("Photo no longer in library");
                    filename_label.set_tooltip_text(None);
                    clear(&image_box);
                    clear(&info_box);
                }
            }
            *current_image.borrow_mut() = image;

            let view = current_view.borrow().clone();
            zoom_group.set_sensitive(view.is_some());
            match view {
                Some(view) => sync_zoom_ui(&view),
                None => zoom_label.set_text("—"),
            }
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
        let um_c = undo_manager.clone();
        Rc::new(move |new_rating: Option<i32>, new_flag: Option<i32>| {
            let idx = current_idx.get();
            let Some(item) = photos.get(idx) else { return };
            let img_id = item.id;

            let (old_rating, old_flag) = current_image
                .borrow()
                .as_ref()
                .map(|i| (i.rating, i.flagged))
                .unwrap_or((item.rating, item.flagged));

            if let Some(ref um) = um_c {
                if let Some(r) = new_rating {
                    um.borrow_mut().push(crate::ui::undo::UndoAction::Rating {
                        previous: vec![(img_id, old_rating)],
                        new_rating: r,
                    });
                }
                if let Some(f) = new_flag {
                    um.borrow_mut().push(crate::ui::undo::UndoAction::Flag {
                        previous: vec![(img_id, old_flag)],
                        new_flag: f,
                    });
                }
            }

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
                        sync_image_xmp(&conn, img_id, &path, XmpChange::Cull { rating: r, flagged: f });
                    }
                }
            });

            show(idx);
        })
    };

    let rotate = {
        let db = db.clone();
        let photos = photos.clone();
        let current_idx = current_idx.clone();
        let current_image = current_image.clone();
        let show = show.clone();
        let cache_dir = cache.clone();
        let um_rot = undo_manager.clone();
        let current_view = current_view.clone();
        Rc::new(move |cw: bool| {
            let idx = current_idx.get();
            let Some(item) = photos.get(idx) else { return };
            let img_id = item.id;

            let (path, hash, old_orient, next_orient) = {
                let img_borrow = current_image.borrow();
                let Some(img) = img_borrow.as_ref() else { return };
                if img.format.as_ref().map_or(false, |f| f.is_video()) {
                    return;
                }
                let old_orient = img.orientation;
                let next = photon_core::models::rotate_orientation(img.orientation, cw);
                (img.path.clone(), img.hash.clone(), old_orient, next)
            };

            let save_res = (|| -> Result<(), photon_core::PhotonError> {
                let mut conn = db.conn()?;
                queries::set_orientation(&mut conn, img_id, next_orient)?;
                Ok(())
            })();

            if let Err(e) = save_res {
                log::error!("Failed to save orientation for photo {img_id}: {e}");
                return;
            }

            if let Some(img) = current_image.borrow_mut().as_mut() {
                img.orientation = Some(next_orient);
            }

            if let Some(ref um) = um_rot {
                um.borrow_mut().push(crate::ui::undo::UndoAction::Orientation {
                    previous: vec![(img_id, old_orient, hash.clone())],
                    cw,
                });
            }

            // Invalidate disk thumbnail cache and memory full-res cache
            photon_import::thumbnails::invalidate_cache(&cache_dir, &hash);
            invalidate_full_res(&hash);
            // The old position means nothing on the rotated photo.
            current_view.borrow_mut().take();

            if let Ok(conn) = db.conn() {
                let p = path.clone();
                thread::spawn(move || {
                    sync_image_xmp(&conn, img_id, &p, XmpChange::Orientation(next_orient));
                });
            }

            show(idx);
        })
    };

    // ── Buttons ─────────────────────────────────────────
    let rot = rotate.clone();
    rotate_left_btn.connect_clicked(move |_| rot(false));
    let rot = rotate.clone();
    rotate_right_btn.connect_clicked(move |_| rot(true));

    info_btn.connect_toggled(glib::clone!(
        #[weak] info_revealer,
        move |btn| info_revealer.set_reveal_child(btn.is_active())
    ));


    let uc = update_cull.clone();
    let img_c = current_image.clone();
    rating_btn.connect_clicked(move |_| {
        let cur = img_c.borrow().as_ref().map(|i| i.rating).unwrap_or(0);
        uc(Some((cur + 1) % 6), None);
    });

    let zt = zoom_to.clone();
    fit_btn.connect_clicked(move |_| zt(Zoom::Fit));
    let zt = zoom_to.clone();
    one_btn.connect_clicked(move |_| zt(Zoom::Scale(1.0)));
    let zt = zoom_to.clone();
    zoom_scale.connect_value_changed(move |scale| {
        if !syncing_zoom.get() {
            zt(Zoom::Scale(scale.value().exp2()));
        }
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

    let confirm_trash = {
        let current_image = current_image.clone();
        let db = db.clone();
        let cache_dir = cache.clone();
        let parent = parent_window.clone();
        let undo_mgr = undo_manager.clone();
        let nav_tx = nav_tx.clone();
        let back_action = back_action.clone();
        Rc::new(move || {
            let img_opt = current_image.borrow().clone();
            let Some(img) = img_opt else { return };
            let Some(img_id) = img.id else { return };

            let dialog = adw::MessageDialog::new(
                parent.as_ref(),
                Some("Move Photo to the Trash?"),
                Some(&format!(
                    "\"{}\" and its sidecar will be moved to the Trash and removed from the library.",
                    img.filename
                )),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("trash", "Move to Trash");
            dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");

            let db = db.clone();
            let cache_dir = cache_dir.clone();
            let undo_mgr = undo_mgr.clone();
            let nav_tx = nav_tx.clone();
            let back_action = back_action.clone();

            dialog.connect_response(None, move |_, response| {
                if response != "trash" {
                    return;
                }
                let path = img.path.clone();
                let xmp = xmp_sidecar(&img);

                let p_clone = path.clone();
                let x_clone = xmp.clone();
                let (tx, rx) = async_channel::bounded::<Result<(), String>>(1);
                thread::spawn(move || {
                    let mut err = None;
                    if let Err(e) = gio::File::for_path(&p_clone).trash(gio::Cancellable::NONE) {
                        err = Some(format!("{}: {e}", p_clone.display()));
                    } else if let Some(ref x) = x_clone {
                        if x.exists() {
                            let _ = gio::File::for_path(x).trash(gio::Cancellable::NONE);
                        }
                    }
                    let res = match err {
                        Some(e) => Err(e),
                        None => Ok(()),
                    };
                    let _ = tx.send_blocking(res);
                });

                let db = db.clone();
                let cache_dir = cache_dir.clone();
                let undo_mgr = undo_mgr.clone();
                let nav_tx = nav_tx.clone();
                let back_action = back_action.clone();
                let img_clone = img.clone();
                let path_clone = path.clone();
                let xmp_clone = xmp.clone();

                glib::spawn_future_local(async move {
                    let Ok(res) = rx.recv().await else { return };
                    match res {
                        Ok(()) => {
                            if let Ok(mut conn) = db.conn() {
                                let _ = queries::delete_images(&mut conn, &[img_id]);
                            }
                            if let Some(ref um) = undo_mgr {
                                um.borrow_mut().push(crate::ui::undo::UndoAction::Trash {
                                    images: vec![img_clone.clone()],
                                    tags: vec![],
                                    trashed_paths: vec![(path_clone, xmp_clone)],
                                });
                            }
                            photon_import::thumbnails::invalidate_cache(&cache_dir, &img_clone.hash);
                            invalidate_full_res(&img_clone.hash);
                            let _ = nav_tx.send_blocking(back_action);
                        }
                        Err(e) => {
                            log::error!("Failed to trash photo: {e}");
                        }
                    }
                });
            });
            dialog.present();
        })
    };

    let ct = confirm_trash.clone();
    trash_btn.connect_clicked(move |_| ct());

    let copy_photo = {
        let ci = current_image.clone();
        let sc = share_ctx.clone();
        Rc::new(move || share::copy_selection(&sc, ci.borrow().iter().cloned().collect()))
    };

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
        #[strong] zoom_by,
        #[strong] zoom_to,
        #[strong] toggle_compare,
        #[strong] update_cull,
        #[strong] rotate,
        #[strong] step,
        #[strong] trigger_export,
        #[strong] copy_photo,
        #[strong] confirm_trash,
        #[strong] on_start_slideshow,
        #[upgrade_or] glib::Propagation::Proceed,
        move |_, key, _, state| {
            let is_shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
            let is_ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);

            if is_ctrl && (key == gtk4::gdk::Key::e || key == gtk4::gdk::Key::E) {
                trigger_export();
                return glib::Propagation::Stop;
            }
            if is_ctrl && (key == gtk4::gdk::Key::c || key == gtk4::gdk::Key::C) {
                copy_photo();
                return glib::Propagation::Stop;
            }
            if is_ctrl && (key == gtk4::gdk::Key::r || key == gtk4::gdk::Key::R) {
                rotate(true);
                return glib::Propagation::Stop;
            }
            if is_ctrl && (key == gtk4::gdk::Key::_1 || key == gtk4::gdk::Key::KP_1) {
                zoom_to(Zoom::Scale(1.0));
                return glib::Propagation::Stop;
            }
            if is_ctrl && (key == gtk4::gdk::Key::_0 || key == gtk4::gdk::Key::KP_0) {
                zoom_to(Zoom::Fit);
                return glib::Propagation::Stop;
            }

            match key {
                gtk4::gdk::Key::bracketleft => rotate(false),
                gtk4::gdk::Key::bracketright => rotate(true),
                gtk4::gdk::Key::Escape => {
                    let _ = tx_esc.send_blocking(back_action.clone());
                }
                gtk4::gdk::Key::Left => step(-1),
                gtk4::gdk::Key::Right => step(1),
                gtk4::gdk::Key::F5 => {
                    if let Some(ref ss) = on_start_slideshow {
                        ss();
                    }
                }
                gtk4::gdk::Key::Delete => {
                    confirm_trash();
                }
                gtk4::gdk::Key::i | gtk4::gdk::Key::I => {
                    info_btn.set_active(!info_btn.is_active());
                }
                gtk4::gdk::Key::z | gtk4::gdk::Key::Z => {
                    toggle_zoom();
                }
                gtk4::gdk::Key::plus | gtk4::gdk::Key::equal | gtk4::gdk::Key::KP_Add => {
                    zoom_by(ZOOM_STEP);
                }
                gtk4::gdk::Key::minus | gtk4::gdk::Key::KP_Subtract => {
                    zoom_by(1.0 / ZOOM_STEP);
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
    restore: Option<ViewState>,
    on_zoom_change: Rc<dyn Fn(&PhotoView)>,
    undo_manager: Option<Rc<RefCell<crate::ui::undo::UndoManager>>>,
) -> Option<Rc<PhotoView>> {
    clear(image_box);
    clear(info_box);

    if image.format.as_ref().map_or(false, |f| f.is_video()) {
        let video_container = GtkBox::new(Orientation::Vertical, 8);
        video_container.set_vexpand(true);
        video_container.set_hexpand(true);
        video_container.set_halign(Align::Fill);
        video_container.set_valign(Align::Fill);

        let gfile = gio::File::for_path(&image.path);
        let video = Video::for_file(Some(&gfile));
        video.set_autoplay(false);
        video.set_hexpand(true);
        video.set_vexpand(true);

        let bottom_bar = GtkBox::new(Orientation::Horizontal, 12);
        bottom_bar.set_margin_start(16);
        bottom_bar.set_margin_end(16);
        bottom_bar.set_margin_bottom(12);

        let stream = video.media_stream();
        let controls = MediaControls::new(stream.as_ref());
        controls.set_hexpand(true);

        let ext_btn = Button::with_label("Open in external player");
        ext_btn.set_icon_name("video-x-generic-symbolic");
        ext_btn.add_css_class("flat");
        ext_btn.set_tooltip_text(Some("Open in default system video player"));
        let vid_path = image.path.clone();
        ext_btn.connect_clicked(move |_| {
            let uri = format!("file://{}", vid_path.to_string_lossy());
            if gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>).is_err() {
                let _ = Command::new("xdg-open").arg(&vid_path).spawn();
            }
        });

        bottom_bar.append(&controls);
        bottom_bar.append(&ext_btn);

        video_container.append(&video);
        video_container.append(&bottom_bar);
        image_box.append(&video_container);
        populate_info_panel(info_box, image, cache_dir, prefs, db, undo_manager);
        return None;
    }

    let view = PhotoView::new(image, cache_dir, restore, on_zoom_change);
    image_box.append(view.widget());
    populate_info_panel(info_box, image, cache_dir, prefs, db, undo_manager);
    Some(view)
}

fn format_metadata_summary(img: &Image) -> String {
    let mut parts = Vec::new();
    if let Some(ref ss) = img.shutter_speed {
        if ss.ends_with('s') {
            parts.push(ss.clone());
        } else {
            parts.push(format!("{ss}s"));
        }
    }
    if let Some(ap) = img.aperture {
        parts.push(format!("f/{ap:.1}"));
    }
    if let Some(iso) = img.iso {
        parts.push(format!("ISO {iso}"));
    }
    if let Some(fl) = img.focal_length {
        parts.push(format!("{fl:.0}mm"));
    }
    if parts.is_empty() {
        if let (Some(w), Some(h)) = (img.width, img.height) {
            parts.push(format!("{w} × {h}"));
        }
        if let Some(ref cam) = img.camera_model {
            parts.push(cam.clone());
        }
    }
    parts.join(" • ")
}

fn xmp_sidecar(img: &Image) -> Option<PathBuf> {
    let json: serde_json::Value = serde_json::from_str(img.metadata_json.as_deref()?).ok()?;
    json.get("xmp_sidecar")?.as_str().map(PathBuf::from)
}

/// What changed about a photo, to be merged into its XMP sidecar.
enum XmpChange<'a> {
    Cull { rating: i32, flagged: i32 },
    Title(&'a str),
    Description(&'a str),
    Tags,
    Orientation(u16),
}

fn sync_image_xmp(conn: &rusqlite::Connection, image_id: i64, image_path: &Path, change: XmpChange) {
    let names = |tags: Vec<photon_core::models::Tag>| tags.into_iter().map(|t| t.name).collect::<Vec<_>>();
    let (tags, known) = match change {
        XmpChange::Tags => match (queries::get_tags_for_image(conn, image_id), queries::get_all_tags(conn)) {
            (Ok(tags), Ok(known)) => (names(tags), names(known)),
            (Err(e), _) | (_, Err(e)) => {
                log::warn!("Not syncing tags to XMP for {}: {e}", image_path.display());
                return;
            }
        },
        _ => Default::default(),
    };

    let update = match change {
        XmpChange::Cull { rating, flagged } => photon_import::XmpUpdate {
            rating: Some(rating),
            rejected: Some(flagged == -1),
            ..Default::default()
        },
        XmpChange::Title(title) => photon_import::XmpUpdate { title: Some(title), ..Default::default() },
        XmpChange::Description(text) => {
            photon_import::XmpUpdate { description: Some(text), ..Default::default() }
        }
        XmpChange::Tags => photon_import::XmpUpdate {
            keywords: Some(photon_import::Keywords { tags: &tags, known: &known }),
            ..Default::default()
        },
        XmpChange::Orientation(orient) => photon_import::XmpUpdate {
            orientation: Some(orient),
            ..Default::default()
        },
    };
    if let Err(e) = photon_import::write_image_xmp(conn, image_id, image_path, &update) {
        log::warn!("Writing XMP for {}: {e}", image_path.display());
    }
}

/// Point image `id` at `new_path`, if that file is the same photo: the same
/// size and content hash. Returns why not, for the user.
fn relink(db: &Database, id: i64, new_path: &Path, expected_size: i64, expected_hash: &str) -> Result<(), String> {
    let size = std::fs::metadata(new_path).map_err(|e| e.to_string())?.len();
    if size as i64 != expected_size {
        return Err(format!("it is {size} bytes, the missing photo was {expected_size}"));
    }
    let hash = photon_import::dedup::blake3_hash_file(new_path).map_err(|e| e.to_string())?;
    if hash != expected_hash {
        return Err("it is a different photo (the contents don't match)".into());
    }
    let conn = db.conn().map_err(|e| e.to_string())?;
    queries::relink_image(&conn, id, &new_path.to_string_lossy()).map_err(|e| e.to_string())
}

fn populate_info_panel(
    info_box: &GtkBox,
    image: &Image,
    cache_dir: &Path,
    prefs: &Preferences,
    db: &Database,
    undo_manager: Option<Rc<RefCell<crate::ui::undo::UndoManager>>>,
) {
    // ── Info panel content: sections stacked vertically ─
    let columns = GtkBox::new(Orientation::Vertical, 16);

    // Offline / Missing alert banner
    if image.missing || !image.path.exists() {
        let alert_box = GtkBox::new(Orientation::Vertical, 6);
        alert_box.set_margin_bottom(8);
        alert_box.add_css_class("card");

        let is_offline = photon_core::is_path_offline(&image.path);
        let status_title = if is_offline {
            "⚠ Offline Storage Disconnected"
        } else {
            "⚠ Photo File Missing"
        };

        let title_lbl = Label::new(Some(status_title));
        title_lbl.set_css_classes(&["heading", "destructive-action"]);
        title_lbl.set_halign(Align::Start);
        alert_box.append(&title_lbl);

        let desc_text = if is_offline {
            format!("The volume containing this photo is currently not mounted.\nExpected: {}", image.path.display())
        } else {
            format!("The photo could not be located at:\n{}", image.path.display())
        };
        let desc_lbl = Label::new(Some(&desc_text));
        desc_lbl.set_wrap(true);
        desc_lbl.set_halign(Align::Start);
        desc_lbl.add_css_class("dim-label");
        alert_box.append(&desc_lbl);

        let locate_btn = Button::with_label("Locate File…");
        locate_btn.set_halign(Align::Start);
        locate_btn.add_css_class("suggested-action");

        let db_loc = db.clone();
        let img_id_val = image.id;
        let expected_hash = image.hash.clone();
        let expected_size = image.size_bytes;

        let alert_weak = alert_box.downgrade();
        locate_btn.connect_clicked(move |btn| {
            let Some(win) = btn.root().and_then(|r| r.downcast::<Window>().ok()) else { return };
            let chooser = FileChooserNative::new(
                Some("Locate Photo File"),
                Some(&win),
                FileChooserAction::Open,
                Some("Select"),
                Some("Cancel"),
            );
            let db_c = db_loc.clone();
            let hash_exp = expected_hash.clone();
            let alert = alert_weak.clone();
            chooser.connect_response(move |dialog, resp| {
                if resp != ResponseType::Accept {
                    return;
                }
                let (Some(new_path), Some(id)) = (dialog.file().and_then(|f| f.path()), img_id_val) else { return };
                let (db, hash, win, alert) = (db_c.clone(), hash_exp.clone(), win.clone(), alert.clone());
                // Hashing a large RAW takes a moment: keep the UI responsive.
                glib::spawn_future_local(async move {
                    let path = new_path.clone();
                    let result = gio::spawn_blocking(move || relink(&db, id, &path, expected_size, &hash))
                        .await
                        .unwrap_or_else(|_| Err("the check crashed".into()));
                    let (heading, body) = match result {
                        Ok(()) => {
                            if let Some(alert) = alert.upgrade() {
                                alert.set_visible(false);
                            }
                            ("Photo Relinked".to_string(), format!("Photon now uses {}.", new_path.display()))
                        }
                        Err(why) => ("Could Not Relink".to_string(), format!("{}: {why}", new_path.display())),
                    };
                    let msg = adw::MessageDialog::new(Some(&win), Some(&heading), Some(&body));
                    msg.add_response("ok", "OK");
                    msg.present();
                });
            });
            chooser.show();
        });
        alert_box.append(&locate_btn);

        columns.append(&alert_box);
    }

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
            sync_image_xmp(&conn, id, &p_t, XmpChange::Title(&text));
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
            sync_image_xmp(&conn, id, &p_d, XmpChange::Description(&text));
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
        let undo_manager_rt = undo_manager.clone();
        let rt_action = {
            let db = db.clone();
            let chips_box = chips_box.clone();
            let img_path = img_path.clone();
            let rt_recurse = rt_self.clone();
            let undo_manager_rt = undo_manager_rt.clone();
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
                    let t_name = tag.name.clone();
                    let rt_call = rt_recurse.clone();
                    let um_rm = undo_manager_rt.clone();
                    rm_btn.connect_clicked(move |_| {
                        if let (Some(tag_id), Ok(conn)) = (tid, db_rm.conn()) {
                            if let Err(e) = queries::untag_image(&conn, id, tag_id) {
                                log::warn!("Removing tag from {}: {e}", p_rm.display());
                                return;
                            }
                            sync_image_xmp(&conn, id, &p_rm, XmpChange::Tags);
                            if let Some(ref um) = um_rm {
                                um.borrow_mut().push(crate::ui::undo::UndoAction::TagRemove {
                                    image_ids: vec![id],
                                    tag_id,
                                    tag_name: t_name.clone(),
                                });
                            }
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
        let um_at = undo_manager.clone();
        add_tag_entry.connect_activate(move |entry| {
            let text = entry.text().trim().to_string();
            if !text.is_empty() {
                if let Ok(conn) = db_at.conn() {
                    if let Ok(tag_id) = queries::ensure_tag(&conn, &text) {
                        if let Err(e) = queries::tag_image(&conn, id, tag_id) {
                            log::warn!("Tagging {}: {e}", p_at.display());
                            return;
                        }
                        sync_image_xmp(&conn, id, &p_at, XmpChange::Tags);
                        if let Some(ref um) = um_at {
                            um.borrow_mut().push(crate::ui::undo::UndoAction::TagAdd {
                                image_ids: vec![id],
                                tag_id,
                                tag_name: text.clone(),
                            });
                        }
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
    populate_info_panel(info_box, img_a, cache_dir, prefs, db, None);
}
