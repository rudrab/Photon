//! Floating action bar for the timeline selection (Google Photos / Files
//! style): appears at the bottom while photos are selected, or in selection
//! mode.
//!
//!   N selected  [Pick] [Reject] … [Open] [Show in Files] [Share ▾] [Export] [Remove] [Trash]  [×]
//!
//! Actions work on photo *ids*, so they stay correct even if a live refresh
//! (e.g. an import) reorders the timeline between selecting and acting.
//! The two destructive actions ask first; Delete is a shortcut for Trash and
//! follows the "Move to Trash Removes" preference for RAW+JPG shots.

use crate::ui::share;
use crate::ui::timeline::{Timeline, WeakTimeline};
use crate::ui::undo::{UndoAction, UndoManager};
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use gtk4::{
    Align, Box as GtkBox, Button, EventControllerKey, Label, Orientation, Popover, Scale,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::{queries, Database};
use photon_core::models::{Image, Preferences, Versions};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::thread;

/// What the bar needs from the window.
#[derive(Clone)]
pub struct Context {
    pub window: gtk4::Window,
    pub db: Database,
    pub prefs: Rc<RefCell<Preferences>>,
    /// Called after photos were removed from the library.
    pub on_library_changed: Rc<dyn Fn()>,
    /// Called to export selected photos.
    pub on_export: Rc<dyn Fn(Vec<Image>)>,
    pub share: share::Context,
    pub undo_manager: Rc<RefCell<UndoManager>>,
    pub current_album_id: Rc<RefCell<Option<i64>>>,
    pub on_start_slideshow: Option<Rc<dyn Fn()>>,
}

/// Handle to control the adaptable bottom bar.
#[derive(Clone)]
pub struct BottomBarHandle {
    pub widget: gtk4::ActionBar,
    pub set_status_text: Rc<dyn Fn(&str)>,
    pub set_zoom_value: Rc<dyn Fn(i32)>,
}

/// Build the adaptable docked bottom bar (Shotwell style) and connect it to `timeline`.
pub fn attach(timeline: &Timeline, ctx: Context) -> BottomBarHandle {
    let action_bar = gtk4::ActionBar::new();
    action_bar.add_css_class("photon-bottom-bar");

    // ── Start area: Idle status vs Selection count ─────────────
    let start_box = GtkBox::new(Orientation::Horizontal, 8);
    start_box.set_valign(Align::Center);

    let idle_label = Label::new(Some("Ready"));
    idle_label.add_css_class("dim-label");
    idle_label.set_halign(Align::Start);
    start_box.append(&idle_label);

    let selection_box = GtkBox::new(Orientation::Horizontal, 6);
    selection_box.set_valign(Align::Center);
    let count = Label::new(None);
    count.add_css_class("heading");
    count.set_margin_start(4);
    count.set_margin_end(4);
    selection_box.append(&count);

    let clear = Button::from_icon_name("window-close-symbolic");
    clear.set_tooltip_text(Some("Deselect all (Esc)"));
    clear.add_css_class("flat");
    let weak = timeline.downgrade();
    let w_clear = weak.clone();
    clear.connect_clicked(move |_| {
        if let Some(t) = w_clear.upgrade() {
            t.clear_selection();
            t.set_selection_mode(false);
        }
    });
    selection_box.append(&clear);
    selection_box.set_visible(false);
    start_box.append(&selection_box);

    action_bar.pack_start(&start_box);

    // ── Center area: Selection actions ─────────────────────────
    let center_box = GtkBox::new(Orientation::Horizontal, 2);
    center_box.set_valign(Align::Center);

    let actions: Rc<RefCell<Vec<gtk4::Widget>>> = Rc::default();
    let button = |icon: &str, tooltip: &str| {
        let b = Button::from_icon_name(icon);
        b.set_tooltip_text(Some(tooltip));
        b.add_css_class("flat");
        center_box.append(&b);
        actions.borrow_mut().push(b.clone().upcast());
        b
    };

    let pick = button("object-select-symbolic", "Pick (P)");
    let w = weak.clone();
    pick.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            t.cull_flag_selected(1);
        }
    });

    let reject = button("process-stop-symbolic", "Reject (X)");
    let w = weak.clone();
    reject.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            t.cull_flag_selected(-1);
        }
    });

    let unflag = button("view-refresh-symbolic", "Unflag (U)");
    let w = weak.clone();
    unflag.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            t.cull_flag_selected(0);
        }
    });

    let rate5 = button("starred-symbolic", "Rate 5 Stars (5)");
    let w = weak.clone();
    rate5.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            t.cull_rating_selected(5);
        }
    });

    let rotate_left = button(
        "object-rotate-left-symbolic",
        "Rotate Counter-Clockwise ([)\nNote: darktable ignores XMP rotation; exported RAWs via darktable use camera orientation.",
    );
    let w = weak.clone();
    rotate_left.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            t.rotate_selected(false);
        }
    });

    let rotate_right = button(
        "object-rotate-right-symbolic",
        "Rotate Clockwise (] / Ctrl+R)\nNote: darktable ignores XMP rotation; exported RAWs via darktable use camera orientation.",
    );
    let w = weak.clone();
    rotate_right.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            t.rotate_selected(true);
        }
    });

    let sep1 = gtk4::Separator::new(Orientation::Vertical);
    sep1.set_margin_start(4);
    sep1.set_margin_end(4);
    center_box.append(&sep1);

    let album_btn = button("folder-pictures-symbolic", "Add to Album…");
    let (w, c) = (weak.clone(), ctx.clone());
    let alb_b = album_btn.clone();
    album_btn.connect_clicked(move |_| {
        let imgs = selected_images(&w, &c.db);
        if !imgs.is_empty() {
            show_add_to_album_popover(&alb_b, imgs, &c);
        }
    });

    let remove_album_btn = button("edit-delete-symbolic", "Remove from Album");
    let (w, c) = (weak.clone(), ctx.clone());
    remove_album_btn.connect_clicked(move |_| {
        let Some(album_id) = *c.current_album_id.borrow() else { return };
        let ids: Vec<i64> = selected_images(&w, &c.db).iter().filter_map(|i| i.id).collect();
        if !ids.is_empty() {
            if let Ok(mut conn) = c.db.conn() {
                if let Err(e) = queries::remove_images_from_album(&mut conn, album_id, &ids) {
                    log::error!("remove_images_from_album failed: {e}");
                }
            }
            (c.share.notify)(&format!("Removed {} photo(s) from album", ids.len()));
            (c.on_library_changed)();
        }
    });
    remove_album_btn.set_visible(false);

    let open = button("document-edit-symbolic", "Open in Editor");
    let (w, c) = (weak.clone(), ctx.clone());
    open.connect_clicked(move |_| open_in_editors(&selected_images(&w, &c.db), &c));

    let show = button("folder-open-symbolic", "Show in Files");
    let (w, c) = (weak.clone(), ctx.clone());
    show.connect_clicked(move |_| show_in_files(&selected_images(&w, &c.db)));

    let (w, db) = (weak.clone(), ctx.db.clone());
    let share_btn = share::menu_button(&ctx.share, Rc::new(move || selected_images(&w, &db)));
    center_box.append(&share_btn);
    actions.borrow_mut().push(share_btn.upcast());

    let export = button("document-save-symbolic", "Export Selected (Ctrl+E)");
    let (w, c) = (weak.clone(), ctx.clone());
    export.connect_clicked(move |_| {
        let imgs = selected_images(&w, &c.db);
        if !imgs.is_empty() {
            (c.on_export)(imgs);
        }
    });

    let sep2 = gtk4::Separator::new(Orientation::Vertical);
    sep2.set_margin_start(4);
    sep2.set_margin_end(4);
    center_box.append(&sep2);

    let remove = button("list-remove-symbolic", "Remove from Library");
    let (w, c) = (weak.clone(), ctx.clone());
    remove.connect_clicked(move |_| confirm_remove(&w, &c));

    let trash = button("user-trash-symbolic", "Move to Trash (Delete)");
    trash.add_css_class("destructive-action");
    let (w, c) = (weak.clone(), ctx.clone());
    trash.connect_clicked(move |_| confirm_trash(&w, &c));

    center_box.set_visible(false);
    action_bar.set_center_widget(Some(&center_box));

    // ── End area: Zoom slider & Slideshow ─────────────────────
    let end_box = GtkBox::new(Orientation::Horizontal, 6);
    end_box.set_valign(Align::Center);

    if let Some(on_slideshow) = ctx.on_start_slideshow.clone() {
        let slideshow_btn = Button::from_icon_name("media-playback-start-symbolic");
        slideshow_btn.set_tooltip_text(Some("Start Slideshow (F5)"));
        slideshow_btn.add_css_class("flat");
        slideshow_btn.connect_clicked(move |_| on_slideshow());
        end_box.append(&slideshow_btn);
    }

    let zoom_group = GtkBox::new(Orientation::Horizontal, 2);
    zoom_group.add_css_class("zoom-slider-group");
    zoom_group.set_valign(Align::Center);

    let zoom_out = Button::from_icon_name("image-zoom-out-symbolic");
    zoom_out.set_tooltip_text(Some("Smaller thumbnails"));
    zoom_out.add_css_class("flat");

    let current_row_height = timeline.row_height();
    let zoom_scale = Scale::with_range(Orientation::Horizontal, 110.0, 480.0, 10.0);
    zoom_scale.set_width_request(110);
    zoom_scale.set_draw_value(false);
    zoom_scale.set_value(current_row_height as f64);
    zoom_scale.set_tooltip_text(Some("Thumbnail size"));

    let zoom_in = Button::from_icon_name("image-zoom-in-symbolic");
    zoom_in.set_tooltip_text(Some("Larger thumbnails"));
    zoom_in.add_css_class("flat");

    let w_zoom = weak.clone();
    let c_prefs = ctx.prefs.clone();
    let c_db = ctx.db.clone();
    zoom_scale.connect_value_changed(move |scale| {
        let val = scale.value() as i32;
        if let Some(t) = w_zoom.upgrade() {
            t.set_row_height(val);
        }
        c_prefs.borrow_mut().thumbnail_size = val as u32;
        if let Ok(conn) = c_db.conn() {
            let _ = queries::save_preferences(&conn, &c_prefs.borrow());
        }
    });

    let zs_out = zoom_scale.clone();
    zoom_out.connect_clicked(move |_| {
        let next = (zs_out.value() - 40.0).max(110.0);
        zs_out.set_value(next);
    });

    let zs_in = zoom_scale.clone();
    zoom_in.connect_clicked(move |_| {
        let next = (zs_in.value() + 40.0).min(480.0);
        zs_in.set_value(next);
    });

    zoom_group.append(&zoom_out);
    zoom_group.append(&zoom_scale);
    zoom_group.append(&zoom_in);
    end_box.append(&zoom_group);

    action_bar.pack_end(&end_box);

    // ── Adaptation on Selection Changes ───────────────────────
    let w = weak.clone();
    let c_alb = ctx.current_album_id.clone();
    let rab_c = remove_album_btn.clone();
    let sb_c = selection_box.clone();
    let ib_c = idle_label.clone();
    let cb_c = center_box.clone();
    timeline.connect_selection_changed(move |selected| {
        let n = selected.len();
        let selecting = w.upgrade().is_some_and(|t| t.selection_mode());
        let has_sel = n > 0 || selecting;

        ib_c.set_visible(!has_sel);
        sb_c.set_visible(has_sel);
        cb_c.set_visible(has_sel);

        count.set_text(&match n {
            0 => "Select photos".to_string(),
            1 => "1 photo selected".to_string(),
            n => format!("{n} photos selected"),
        });
        for action in actions.borrow().iter() {
            action.set_sensitive(n > 0);
        }
        rab_c.set_visible(c_alb.borrow().is_some() && n > 0);
    });

    // Keyboard shortcuts controller on timeline
    let keys = EventControllerKey::new();
    let (w, c) = (weak, ctx);
    keys.connect_key_pressed(move |_, key, _, state| {
        let has_selection = w.upgrade().is_some_and(|t| !t.selected_ids().is_empty());
        if key == gdk::Key::F5 {
            if let Some(on_slideshow) = &c.on_start_slideshow {
                on_slideshow();
                return glib::Propagation::Stop;
            }
        }
        if key == gdk::Key::Delete && has_selection {
            confirm_trash(&w, &c);
            return glib::Propagation::Stop;
        }
        if (key == gdk::Key::c || key == gdk::Key::C)
            && state.contains(gdk::ModifierType::CONTROL_MASK)
            && has_selection
        {
            share::copy_selection(&c.share, selected_images(&w, &c.db));
            return glib::Propagation::Stop;
        }
        if (key == gdk::Key::e || key == gdk::Key::E)
            && state.contains(gdk::ModifierType::CONTROL_MASK)
            && has_selection
        {
            let imgs = selected_images(&w, &c.db);
            if !imgs.is_empty() {
                (c.on_export)(imgs);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    timeline.widget().add_controller(keys);

    let lbl_status = idle_label.clone();
    let zs_setter = zoom_scale.clone();
    BottomBarHandle {
        widget: action_bar,
        set_status_text: Rc::new(move |text| lbl_status.set_text(text)),
        set_zoom_value: Rc::new(move |val| zs_setter.set_value(val as f64)),
    }
}

fn selected_images(timeline: &WeakTimeline, db: &Database) -> Vec<Image> {
    let Some(timeline) = timeline.upgrade() else { return Vec::new() };
    let Ok(conn) = db.conn() else { return Vec::new() };
    timeline
        .selected_ids()
        .into_iter()
        .filter_map(|id| queries::get_image(&conn, id).ok().flatten())
        .collect()
}

/// RAW files go to the RAW editor, everything else to the raster editor;
/// one process per editor with all its files.
fn open_in_editors(images: &[Image], ctx: &Context) {
    let prefs = ctx.prefs.borrow();
    let (raw, raster): (Vec<&Image>, Vec<&Image>) = images
        .iter()
        .partition(|img| img.format.is_some_and(|f| f.is_raw()));

    for (editor, group) in [(&prefs.raw_editor, raw), (&prefs.raster_editor, raster)] {
        if group.is_empty() {
            continue;
        }
        let paths: Vec<&PathBuf> = group.iter().map(|img| &img.path).collect();
        match Command::new(editor).args(&paths).spawn() {
            Ok(_) => {
                if let Ok(conn) = ctx.db.conn() {
                    for img in &group {
                        if let Some(id) = img.id {
                            if let Err(e) = queries::record_edit(&conn, id, editor, None) {
                                log::error!("record_edit failed for id={id}: {e}");
                            }
                        }
                    }
                }
            }
            Err(e) => log::warn!("Could not start {editor}: {e}"),
        }
    }
}

/// Ask the file manager to open the folders with the files selected
/// (org.freedesktop.FileManager1), falling back to opening the folder.
fn show_in_files(images: &[Image]) {
    let Some(first) = images.first() else { return };
    let uris: Vec<String> = images
        .iter()
        .map(|img| gio::File::for_path(&img.path).uri().to_string())
        .collect();
    let folder = first.path.parent().map(|p| gio::File::for_path(p).uri().to_string());

    let open_folder = move || {
        if let Some(folder) = &folder {
            if let Err(e) = gio::AppInfo::launch_default_for_uri(folder, gio::AppLaunchContext::NONE) {
                log::warn!("Could not open {folder}: {e}");
            }
        }
    };
    match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(bus) => bus.call(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1",
            "ShowItems",
            Some(&(uris, "").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            move |result| {
                if result.is_err() {
                    open_folder();
                }
            },
        ),
        Err(_) => open_folder(),
    }
}

fn plural(n: usize) -> String {
    if n == 1 {
        "1 photo".to_string()
    } else {
        format!("{n} photos")
    }
}

fn confirm(
    ctx: &Context,
    heading: &str,
    body: &str,
    action_label: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::MessageDialog::new(Some(&ctx.window), Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "Cancel"), ("confirm", action_label)]);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "confirm" {
            on_confirm();
        }
    });
    dialog.present();
}

fn confirm_remove(timeline: &WeakTimeline, ctx: &Context) {
    let Some(t) = timeline.upgrade() else { return };
    let ids = t.selected_ids();
    if ids.is_empty() {
        return;
    }
    let (w, c) = (timeline.clone(), ctx.clone());
    confirm(
        ctx,
        &format!("Remove {} from the library?", plural(ids.len())),
        "The files stay where they are on disk. Importing them again brings them back.",
        "Remove",
        move || {
            match c.db.conn().map_err(|e| e.to_string()).and_then(|mut conn| {
                queries::delete_images(&mut conn, &ids).map_err(|e| e.to_string())
            }) {
                Ok(_) => {
                    if let Some(t) = w.upgrade() {
                        t.clear_selection();
                    }
                    (c.on_library_changed)();
                }
                Err(e) => log::error!("Remove from library failed: {e}"),
            }
        },
    );
}

/// Files a Move to Trash removes: the selection, widened or narrowed to other
/// versions of the same shots by the delete-mode preference.
struct TrashPlan {
    images: Vec<Image>,
    /// XMP sidecars of versions that stay, which a trashed version may share
    /// ("IMG_1.xmp" next to both IMG_1.ORF and IMG_1.JPG).
    kept_xmps: HashSet<PathBuf>,
    kept: usize,
}

fn trash_plan(db: &Database, selected: Vec<Image>, mode: Versions) -> TrashPlan {
    let mut plan = TrashPlan { images: Vec::new(), kept_xmps: HashSet::new(), kept: 0 };
    let shots = match db.conn().map_err(|e| e.to_string()).and_then(|conn| {
        queries::shots_of(&conn, selected).map_err(|e| e.to_string())
    }) {
        Ok(shots) => shots,
        Err(e) => {
            log::error!("Could not look up versions: {e}");
            return plan;
        }
    };
    let mut seen = HashSet::new();
    for (versions, selected_ids) in shots {
        let selected: Vec<&Image> =
            versions.iter().filter(|v| v.id.is_some_and(|id| selected_ids.contains(&id))).collect();
        let picked: HashSet<Option<i64>> = mode.pick(&versions, &selected).iter().map(|v| v.id).collect();
        for version in versions {
            if picked.contains(&version.id) {
                if seen.insert(version.id) {
                    plan.images.push(version);
                }
            } else {
                plan.kept += 1;
                plan.kept_xmps.extend(xmp_sidecar(&version));
            }
        }
    }
    plan
}

fn confirm_trash(timeline: &WeakTimeline, ctx: &Context) {
    let selected = selected_images(timeline, &ctx.db);
    if selected.is_empty() {
        return;
    }
    let n_selected = selected.len();
    let mode = ctx.prefs.borrow().delete_mode;
    let plan = trash_plan(&ctx.db, selected, mode);

    let files = plan.images.len();
    let raw = plan.images.iter().filter(|i| i.format.is_some_and(|f| f.is_raw())).count();
    let mut body = format!(
        "{} ({raw} RAW, {} JPG or other) and their XMP sidecars are moved to the Trash and \
         removed from the library. You can restore them from the Trash.",
        if files == 1 { "1 file".to_string() } else { format!("{files} files") },
        files - raw,
    );
    if plan.kept > 0 {
        let noun = if plan.kept == 1 { "version stays" } else { "versions stay" };
        body.push_str(&format!("\n\n{} other {noun}.", plan.kept));
    }
    body.push_str(&format!(
        "\n\nDeleting removes: {}. Change this in Preferences.",
        mode.label().to_lowercase()
    ));

    let (w, c) = (timeline.clone(), ctx.clone());
    confirm(
        ctx,
        &format!("Move {} to the Trash?", plural(n_selected)),
        &body,
        "Move to Trash",
        move || trash(&plan.images, &plan.kept_xmps, &w, &c),
    );
}

/// Trash files off the main thread, then drop the trashed ones from the
/// library. Files that could not be trashed stay in the library.
fn trash(images: &[Image], kept_xmps: &HashSet<PathBuf>, timeline: &WeakTimeline, ctx: &Context) {
    let jobs: Vec<(i64, PathBuf, Option<PathBuf>)> = images
        .iter()
        .filter_map(|img| {
            let xmp = xmp_sidecar(img).filter(|x| !kept_xmps.contains(x));
            Some((img.id?, img.path.clone(), xmp))
        })
        .collect();

    let mut tags = Vec::new();
    if let Ok(conn) = ctx.db.conn() {
        for img in images {
            if let Some(id) = img.id {
                if let Ok(img_tags) = queries::get_tags_for_image(&conn, id) {
                    for t in img_tags {
                        if let Some(tid) = t.id {
                            tags.push((id, tid));
                        }
                    }
                }
            }
        }
    }

    let (tx, rx) = async_channel::bounded::<(Vec<i64>, Vec<String>)>(1);
    let jobs_clone = jobs.clone();
    thread::spawn(move || {
        let mut trashed = Vec::new();
        let mut failed = Vec::new();
        for (id, path, xmp) in jobs_clone {
            match gio::File::for_path(&path).trash(gio::Cancellable::NONE) {
                Ok(()) => {
                    trashed.push(id);
                    if let Some(xmp) = xmp.filter(|x| x.exists()) {
                        let _ = gio::File::for_path(xmp).trash(gio::Cancellable::NONE);
                    }
                }
                Err(e) => failed.push(format!("{}: {e}", path.display())),
            }
        }
        let _ = tx.send_blocking((trashed, failed));
    });

    let (w, c) = (timeline.clone(), ctx.clone());
    let images_vec = images.to_vec();
    glib::spawn_future_local(async move {
        let Ok((trashed, failed)) = rx.recv().await else { return };
        if let Ok(mut conn) = c.db.conn() {
            if let Err(e) = queries::delete_images(&mut conn, &trashed) {
                log::error!("Trashed files could not be removed from the library: {e}");
            }
        }
        if !trashed.is_empty() {
            let trashed_set: HashSet<i64> = trashed.iter().copied().collect();
            let trashed_images: Vec<Image> = images_vec
                .into_iter()
                .filter(|i| i.id.is_some_and(|id| trashed_set.contains(&id)))
                .collect();
            let trashed_tags: Vec<(i64, i64)> = tags
                .into_iter()
                .filter(|(id, _)| trashed_set.contains(id))
                .collect();
            let trashed_paths: Vec<(PathBuf, Option<PathBuf>)> = jobs
                .into_iter()
                .filter(|(id, _, _)| trashed_set.contains(id))
                .map(|(_, p, x)| (p, x))
                .collect();
            c.undo_manager.borrow_mut().push(UndoAction::Trash {
                images: trashed_images,
                tags: trashed_tags,
                trashed_paths,
            });
        }
        if let Some(t) = w.upgrade() {
            t.clear_selection();
            t.set_selection_mode(false);
        }
        (c.on_library_changed)();

        if !failed.is_empty() {
            for f in &failed {
                log::warn!("Could not trash {f}");
            }
            let dialog = adw::MessageDialog::new(
                Some(&c.window),
                Some(&format!("{} could not be moved to the Trash", plural(failed.len()))),
                Some(&failed.iter().take(5).cloned().collect::<Vec<_>>().join("\n")),
            );
            dialog.add_response("ok", "OK");
            dialog.present();
        }
    });
}

fn show_add_to_album_popover(target: &Button, images: Vec<Image>, ctx: &Context) {
    let popover = Popover::new();
    popover.set_parent(target);

    let pop_box = GtkBox::new(Orientation::Vertical, 4);
    pop_box.set_margin_start(8);
    pop_box.set_margin_end(8);
    pop_box.set_margin_top(8);
    pop_box.set_margin_bottom(8);

    let heading = Label::new(Some("Add to Album"));
    heading.add_css_class("caption-heading");
    heading.set_halign(Align::Start);
    pop_box.append(&heading);

    let new_btn = Button::with_label("+ New Album…");
    new_btn.add_css_class("flat");
    new_btn.set_halign(Align::Fill);
    let pop_c = popover.clone();
    let ctx_c = ctx.clone();
    let imgs_c = images.clone();
    new_btn.connect_clicked(move |_| {
        pop_c.popdown();
        let c = ctx_c.clone();
        let imgs = imgs_c.clone();
        crate::ui::sidebar::prompt_text_dialog(
            "New Album",
            "Enter album name:",
            "",
            "Create & Add",
            move |name| {
                if let Ok(mut conn) = c.db.conn() {
                    if let Ok(album_id) = queries::create_album(&conn, &name) {
                        let ids: Vec<i64> = imgs.iter().filter_map(|i| i.id).collect();
                        if let Err(e) = queries::add_images_to_album(&mut conn, album_id, &ids) {
                            log::error!("add_images_to_album failed: {e}");
                        }
                        (c.share.notify)(&format!("Added {} photo(s) to '{}'", ids.len(), name));
                        (c.on_library_changed)();
                    }
                }
            },
        );
    });
    pop_box.append(&new_btn);

    // List existing albums
    if let Ok(conn) = ctx.db.conn() {
        if let Ok(albums) = queries::get_all_albums_with_counts(&conn) {
            if !albums.is_empty() {
                let sep = gtk4::Separator::new(Orientation::Horizontal);
                pop_box.append(&sep);

                for (album, count) in albums {
                    let label = format!("{} ({})", album.name, count);
                    let b = Button::with_label(&label);
                    b.add_css_class("flat");
                    b.set_halign(Align::Fill);
                    let pop_c = popover.clone();
                    let ctx_c = ctx.clone();
                    let imgs_c = images.clone();
                    let alb_id = album.id;
                    let alb_name = album.name.clone();
                    b.connect_clicked(move |_| {
                        pop_c.popdown();
                        let ids: Vec<i64> = imgs_c.iter().filter_map(|i| i.id).collect();
                        if let Ok(mut conn) = ctx_c.db.conn() {
                            if let Err(e) = queries::add_images_to_album(&mut conn, alb_id, &ids) {
                                log::error!("add_images_to_album failed: {e}");
                            }
                        }
                        (ctx_c.share.notify)(&format!("Added {} photo(s) to '{}'", ids.len(), alb_name));
                        (ctx_c.on_library_changed)();
                    });
                    pop_box.append(&b);
                }
            }
        }
    }

    popover.set_child(Some(&pop_box));
    popover.popup();
}

fn xmp_sidecar(img: &Image) -> Option<PathBuf> {
    let json: serde_json::Value = serde_json::from_str(img.metadata_json.as_deref()?).ok()?;
    json.get("xmp_sidecar")?.as_str().map(PathBuf::from)
}
