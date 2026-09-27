//! Floating action bar for the timeline selection (Google Photos / Files
//! style): appears at the bottom while photos are selected.
//!
//!   N photos selected   [Open] [Show in Files] [Copy] [Remove] [Trash]  [×]
//!
//! Actions work on photo *ids*, so they stay correct even if a live refresh
//! (e.g. an import) reorders the timeline between selecting and acting.
//! The two destructive actions ask first; Delete is a shortcut for Trash.

use crate::ui::timeline::{Timeline, WeakTimeline};
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use gtk4::{Align, Box as GtkBox, Button, EventControllerKey, Label, Orientation, Revealer};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::{queries, Database};
use photon_core::models::{Image, Preferences};
use std::cell::RefCell;
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
}

/// Build the bar and overlay it on `timeline`.
pub fn attach(timeline: &Timeline, ctx: Context) {
    let count = Label::new(None);
    count.add_css_class("heading");
    count.set_margin_start(10);
    count.set_margin_end(8);

    let bar = GtkBox::new(Orientation::Horizontal, 2);
    bar.add_css_class("photon-selection-bar");
    bar.append(&count);

    let weak = timeline.downgrade();
    let button = |icon: &str, tooltip: &str| {
        let b = Button::from_icon_name(icon);
        b.set_tooltip_text(Some(tooltip));
        b.add_css_class("flat");
        bar.append(&b);
        b
    };

    let pick = button("emblem-ok-symbolic", "Pick (P)");
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

    let sep = gtk4::Separator::new(Orientation::Vertical);
    sep.set_margin_start(4);
    sep.set_margin_end(4);
    bar.append(&sep);

    let open = button("document-edit-symbolic", "Open in Editor");
    let (w, c) = (weak.clone(), ctx.clone());
    open.connect_clicked(move |_| open_in_editors(&selected_images(&w, &c.db), &c));

    let show = button("folder-open-symbolic", "Show in Files");
    let (w, c) = (weak.clone(), ctx.clone());
    show.connect_clicked(move |_| show_in_files(&selected_images(&w, &c.db)));

    let copy = button("edit-copy-symbolic", "Copy");
    let (w, c) = (weak.clone(), ctx.clone());
    copy.connect_clicked(move |_| copy_to_clipboard(&selected_images(&w, &c.db), &c.window));

    let remove = button("list-remove-symbolic", "Remove from Library");
    let (w, c) = (weak.clone(), ctx.clone());
    remove.connect_clicked(move |_| confirm_remove(&w, &c));

    let trash = button("user-trash-symbolic", "Move to Trash (Delete)");
    trash.add_css_class("destructive-action");
    let (w, c) = (weak.clone(), ctx.clone());
    trash.connect_clicked(move |_| confirm_trash(&w, &c));

    let clear = button("window-close-symbolic", "Clear Selection (Esc)");
    let w = weak.clone();
    clear.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            t.clear_selection();
        }
    });

    let revealer = Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideUp)
        .halign(Align::Center)
        .valign(Align::End)
        .margin_bottom(20)
        .child(&bar)
        .build();
    timeline.widget().add_overlay(&revealer);

    timeline.connect_selection_changed(move |selected| {
        let n = selected.len();
        revealer.set_reveal_child(n > 0);
        count.set_text(&match n {
            1 => "1 photo selected".to_string(),
            n => format!("{n} photos selected"),
        });
    });

    // Delete → Move to Trash (the timeline handles its own navigation keys).
    let keys = EventControllerKey::new();
    let (w, c) = (weak, ctx);
    keys.connect_key_pressed(move |_, key, _, _| {
        let has_selection = w.upgrade().is_some_and(|t| !t.selected_ids().is_empty());
        if key == gdk::Key::Delete && has_selection {
            confirm_trash(&w, &c);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    timeline.widget().add_controller(keys);
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
                            let _ = queries::record_edit(&conn, id, editor, None);
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

/// Put the files on the clipboard, so they can be pasted in Files or dropped
/// into a chat or mail.
fn copy_to_clipboard(images: &[Image], window: &gtk4::Window) {
    let files: Vec<gio::File> = images.iter().map(|img| gio::File::for_path(&img.path)).collect();
    if files.is_empty() {
        return;
    }
    let list = gdk::FileList::from_array(&files);
    let provider = gdk::ContentProvider::for_value(&list.to_value());
    if let Err(e) = window.clipboard().set_content(Some(&provider)) {
        log::warn!("Copy to clipboard failed: {e}");
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

fn confirm_trash(timeline: &WeakTimeline, ctx: &Context) {
    let images = selected_images(timeline, &ctx.db);
    if images.is_empty() {
        return;
    }
    let (w, c) = (timeline.clone(), ctx.clone());
    confirm(
        ctx,
        &format!("Move {} to the Trash?", plural(images.len())),
        "The files (and their XMP sidecars) are moved to the Trash and removed \
         from the library. You can restore them from the Trash.",
        "Move to Trash",
        move || trash(&images, &w, &c),
    );
}

/// Trash files off the main thread, then drop the trashed ones from the
/// library. Files that could not be trashed stay in the library.
fn trash(images: &[Image], timeline: &WeakTimeline, ctx: &Context) {
    let jobs: Vec<(i64, PathBuf, Option<PathBuf>)> = images
        .iter()
        .filter_map(|img| Some((img.id?, img.path.clone(), xmp_sidecar(img))))
        .collect();
    let (tx, rx) = async_channel::bounded::<(Vec<i64>, Vec<String>)>(1);
    thread::spawn(move || {
        let mut trashed = Vec::new();
        let mut failed = Vec::new();
        for (id, path, xmp) in jobs {
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
    glib::spawn_future_local(async move {
        let Ok((trashed, failed)) = rx.recv().await else { return };
        if let Ok(mut conn) = c.db.conn() {
            if let Err(e) = queries::delete_images(&mut conn, &trashed) {
                log::error!("Trashed files could not be removed from the library: {e}");
            }
        }
        if let Some(t) = w.upgrade() {
            t.clear_selection();
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

fn xmp_sidecar(img: &Image) -> Option<PathBuf> {
    let json: serde_json::Value = serde_json::from_str(img.metadata_json.as_deref()?).ok()?;
    json.get("xmp_sidecar")?.as_str().map(PathBuf::from)
}
