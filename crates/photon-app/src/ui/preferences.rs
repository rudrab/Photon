//! Preferences dialog following GNOME HIG using libadwaita PreferencesWindow.
//!
//! Scans /usr/share/applications/*.desktop and ~/.local/share/applications/*.desktop
//! for apps that handle image/* MIME types. Presents them in native Adwaita ComboRows.

use gtk4::prelude::*;
use gtk4::StringList;
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::queries;
use photon_core::db::Database;
use photon_core::models::{Versions, DesktopApp, Preferences};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub fn show(
    parent: &impl IsA<gtk4::Window>,
    db: &Database,
    on_save: impl Fn(Preferences) + 'static,
) {
    let prefs = Rc::new(RefCell::new({
        let conn = db.conn().expect("DB conn for prefs");
        queries::load_preferences(&conn)
    }));

    // Discover installed image apps
    let apps = discover_image_apps();

    let raster_apps: Vec<DesktopApp> = apps
        .iter()
        .filter(|a| a.handles_raster)
        .cloned()
        .collect();
    let raw_apps: Vec<DesktopApp> = apps.iter().filter(|a| a.handles_raw).cloned().collect();
    let viewer_apps: Vec<DesktopApp> = apps
        .iter()
        .filter(|a| a.handles_viewer)
        .cloned()
        .collect();

    let window = adw::PreferencesWindow::builder()
        .transient_for(parent)
        .modal(true)
        .title("Preferences")
        .default_width(580)
        .default_height(480)
        .build();

    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("preferences-other-symbolic")
        .build();

    // ── External Editors ────────────────────────────────
    let group_editors = adw::PreferencesGroup::builder()
        .title("External Editors")
        .description("Applications detected from installed desktop files for editing and viewing photos")
        .build();

    let current_prefs = prefs.borrow().clone();

    let (raster_row, raster_execs) = make_combo_row(
        "Raster Editor",
        "For JPG, PNG, WebP and other raster images",
        &raster_apps,
        &current_prefs.raster_editor,
    );
    group_editors.add(&raster_row);

    let (raw_row, raw_execs) = make_combo_row(
        "RAW Editor",
        "For camera digital negatives (ORF, CR2, NEF, DNG, etc.)",
        &raw_apps,
        &current_prefs.raw_editor,
    );
    group_editors.add(&raw_row);

    let (viewer_row, viewer_execs) = make_combo_row(
        "Image Viewer",
        "For quickly previewing files externally",
        &viewer_apps,
        &current_prefs.viewer,
    );
    group_editors.add(&viewer_row);

    page.add(&group_editors);

    // ── Display Group ───────────────────────────────────
    let group_display = adw::PreferencesGroup::builder()
        .title("Display")
        .description("Customize grid layout and thumbnail sizes")
        .build();

    let spin_adj = gtk4::Adjustment::new(
        current_prefs.thumbnail_size as f64,
        100.0,
        400.0,
        25.0,
        50.0,
        0.0,
    );

    let spin_row = adw::SpinRow::builder()
        .title("Thumbnail Size")
        .subtitle("Height for photos in the timeline grid (in pixels)")
        .adjustment(&spin_adj)
        .climb_rate(25.0)
        .digits(0)
        .build();
    group_display.add(&spin_row);

    page.add(&group_display);

    // ── RAW + JPG shots ─────────────────────────────────
    let group_delete = adw::PreferencesGroup::builder()
        .title("RAW + JPG Shots")
        .description(
            "A shot can have several versions: a RAW file, a JPG from the camera, and edits. \
             Photos without other versions are always used themselves.",
        )
        .build();

    let versions_row = |title: &str, subtitle: &str, current: Versions| {
        adw::ComboRow::builder()
            .title(title)
            .subtitle(subtitle)
            .model(&StringList::new(&Versions::ALL.map(Versions::label)))
            .selected(Versions::ALL.iter().position(|&m| m == current).unwrap_or(0) as u32)
            .build()
    };
    let share_row = versions_row(
        "Share Sends",
        "Copy, email, chats and other apps; a selected RAW is swapped for its JPG under Raster only",
        current_prefs.share_versions,
    );
    group_delete.add(&share_row);
    let delete_row = versions_row(
        "Move to Trash Removes",
        "Applies to every version of the selected shots",
        current_prefs.delete_mode,
    );
    group_delete.add(&delete_row);

    page.add(&group_delete);

    // ── Import safety ───────────────────────────────────
    let group_import = adw::PreferencesGroup::builder()
        .title("Import Safety")
        .description("For imports that copy or move photos into the library, e.g. from a memory card.")
        .build();

    let verify_row = adw::SwitchRow::builder()
        .title("Verify Copies")
        .subtitle("Read each copy back from the disk and compare it with the card before trusting it")
        .active(current_prefs.verify_imports)
        .build();
    group_import.add(&verify_row);

    let backup_dir = Rc::new(RefCell::new(current_prefs.import_backup_dir.clone()));
    let backup_row = adw::ActionRow::builder().title("Backup Copy").build();
    let describe_backup = {
        let backup_row = backup_row.clone();
        move |dir: Option<&Path>| match dir {
            None => backup_row.set_subtitle("Off: photos are only in the library after import"),
            Some(dir) if same_disk(dir, &photon_import::library::library_root(None)) => backup_row.set_subtitle(&format!(
                "{} — on the library's disk: guards against mistakes, not a disk failure",
                dir.display()
            )),
            Some(dir) => backup_row.set_subtitle(&format!(
                "{} — a move deletes a photo from the card only once both copies exist",
                dir.display()
            )),
        }
    };
    describe_backup(backup_dir.borrow().as_deref());
    let choose_btn = gtk4::Button::builder().label("Choose…").valign(gtk4::Align::Center).build();
    let clear_btn = gtk4::Button::builder()
        .icon_name("edit-clear-symbolic")
        .tooltip_text("Turn off")
        .valign(gtk4::Align::Center)
        .build();
    clear_btn.add_css_class("flat");
    clear_btn.set_sensitive(backup_dir.borrow().is_some());
    backup_row.add_suffix(&choose_btn);
    backup_row.add_suffix(&clear_btn);
    group_import.add(&backup_row);

    page.add(&group_import);
    window.add(&page);

    // ── Live Save on changes ────────────────────────────
    let on_save = Rc::new(on_save);
    let db_clone = db.clone();

    let save_changes = {
        let prefs = prefs.clone();
        let raster_execs = raster_execs.clone();
        let raw_execs = raw_execs.clone();
        let viewer_execs = viewer_execs.clone();
        let raster_row = raster_row.clone();
        let raw_row = raw_row.clone();
        let viewer_row = viewer_row.clone();
        let spin_row = spin_row.clone();
        let delete_row = delete_row.clone();
        let share_row = share_row.clone();
        let verify_row = verify_row.clone();
        let backup_dir = backup_dir.clone();
        let on_save = on_save.clone();
        let db = db_clone.clone();

        Rc::new(move || {
            let new_prefs = Preferences {
                raster_editor: get_combo_exec(&raster_row, &raster_execs),
                raw_editor: get_combo_exec(&raw_row, &raw_execs),
                viewer: get_combo_exec(&viewer_row, &viewer_execs),
                thumbnail_size: spin_row.value() as u32,
                delete_mode: Versions::ALL
                    .get(delete_row.selected() as usize)
                    .copied()
                    .unwrap_or_default(),
                share_versions: Versions::ALL
                    .get(share_row.selected() as usize)
                    .copied()
                    .unwrap_or(Versions::RasterOnly),
                verify_imports: verify_row.is_active(),
                import_backup_dir: backup_dir.borrow().clone(),
            };

            *prefs.borrow_mut() = new_prefs.clone();

            if let Ok(conn) = db.conn() {
                if let Err(e) = queries::save_preferences(&conn, &new_prefs) {
                    log::error!("Failed to save preferences: {}", e);
                }
            }

            on_save(new_prefs);
        })
    };

    let sc1 = save_changes.clone();
    raster_row.connect_selected_notify(move |_| sc1());

    let sc2 = save_changes.clone();
    raw_row.connect_selected_notify(move |_| sc2());

    let sc3 = save_changes.clone();
    viewer_row.connect_selected_notify(move |_| sc3());

    let sc4 = save_changes.clone();
    spin_row.connect_value_notify(move |_| sc4());

    let sc5 = save_changes.clone();
    delete_row.connect_selected_notify(move |_| sc5());

    let sc6 = save_changes.clone();
    share_row.connect_selected_notify(move |_| sc6());

    let sc7 = save_changes.clone();
    verify_row.connect_active_notify(move |_| sc7());

    let set_backup_dir = {
        let backup_dir = backup_dir.clone();
        let clear_btn = clear_btn.clone();
        let save = save_changes.clone();
        Rc::new(move |dir: Option<std::path::PathBuf>| {
            describe_backup(dir.as_deref());
            clear_btn.set_sensitive(dir.is_some());
            *backup_dir.borrow_mut() = dir;
            save();
        })
    };
    let set = set_backup_dir.clone();
    clear_btn.connect_clicked(move |_| set(None));
    let parent_win = window.clone();
    choose_btn.connect_clicked(move |_| {
        let dialog = gtk4::FileDialog::builder().title("Folder for Backup Copies").modal(true).build();
        let set = set_backup_dir.clone();
        dialog.select_folder(Some(&parent_win), gtk4::gio::Cancellable::NONE, move |result| {
            if let Some(path) = result.ok().and_then(|f| f.path()) {
                set(Some(path));
            }
        });
    });

    window.present();
}

// ═══════════════════════════════════════════════════════════
// .desktop file scanner
// ═══════════════════════════════════════════════════════════

/// Scan all .desktop files and return apps that handle image MIME types.
fn discover_image_apps() -> Vec<DesktopApp> {
    let mut apps_map: HashMap<String, DesktopApp> = HashMap::new();

    // Directories to scan
    let mut dirs: Vec<PathBuf> = vec![PathBuf::from("/usr/share/applications")];
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join("applications"));
    }
    // Also check flatpak exports
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
    }

    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                if let Some(app) = parse_desktop_file(&path) {
                    // Deduplicate by exec command
                    apps_map.entry(app.exec.clone()).or_insert(app);
                }
            }
        }
    }

    // Always include xdg-open as a viewer
    apps_map
        .entry("xdg-open".to_string())
        .or_insert(DesktopApp {
            name: "Default (xdg-open)".to_string(),
            exec: "xdg-open".to_string(),
            handles_raw: false,
            handles_raster: false,
            handles_viewer: true,
        });

    let mut apps: Vec<DesktopApp> = apps_map.into_values().collect();
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

/// Parse a single .desktop file for image handling capabilities.
fn parse_desktop_file(path: &Path) -> Option<DesktopApp> {
    let content = fs::read_to_string(path).ok()?;

    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut mime_types: Vec<String> = Vec::new();
    let mut is_desktop_entry = false;
    let mut no_display = false;

    for line in content.lines() {
        let line = line.trim();

        if line == "[Desktop Entry]" {
            is_desktop_entry = true;
            continue;
        }
        if line.starts_with('[') && line != "[Desktop Entry]" {
            if is_desktop_entry {
                break;
            }
            continue;
        }
        if !is_desktop_entry {
            continue;
        }

        if let Some(val) = line.strip_prefix("Name=") {
            if name.is_none() {
                name = Some(val.to_string());
            }
        } else if let Some(val) = line.strip_prefix("Exec=") {
            exec = Some(clean_exec(val));
        } else if let Some(val) = line.strip_prefix("MimeType=") {
            mime_types = val.split(';').map(|s| s.trim().to_string()).collect();
        } else if line.starts_with("NoDisplay=true") {
            no_display = true;
        }
    }

    if no_display {
        return None;
    }

    let name = name?;
    let exec = exec?;

    // Check if any MIME type matches image/*
    let handles_image = mime_types.iter().any(|m| m.starts_with("image/"));
    if !handles_image {
        return None;
    }

    // Categorize
    let raw_mimes = [
        "image/x-olympus-orf",
        "image/x-canon-cr2",
        "image/x-canon-cr3",
        "image/x-nikon-nef",
        "image/x-sony-arw",
        "image/x-adobe-dng",
        "image/x-fuji-raf",
        "image/x-panasonic-rw2",
        "image/x-dcraw",
        "image/x-raw",
    ];

    let raster_mimes = [
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/tiff",
        "image/gif",
        "image/bmp",
    ];

    let handles_raw = mime_types.iter().any(|m| {
        raw_mimes.contains(&m.as_str())
            || m == "image/x-dcraw"
            || (m.starts_with("image/x-") && m.contains("raw"))
    });

    let handles_raster = mime_types
        .iter()
        .any(|m| raster_mimes.contains(&m.as_str()));

    let handles_viewer = handles_image;

    Some(DesktopApp {
        name,
        exec,
        handles_raw,
        handles_raster,
        handles_viewer,
    })
}

/// Clean an Exec= line: remove %f, %F, %u, %U, %i, %c, %k and field codes.
fn clean_exec(exec: &str) -> String {
    let parts: Vec<&str> = exec.split_whitespace().collect();
    if let Some(cmd) = parts.first() {
        let binary = Path::new(cmd)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        binary
    } else {
        exec.to_string()
    }
}

// ═══════════════════════════════════════════════════════════
// UI helpers
// ═══════════════════════════════════════════════════════════

fn make_combo_row(
    title: &str,
    subtitle: &str,
    apps: &[DesktopApp],
    current_exec: &str,
) -> (adw::ComboRow, Vec<String>) {
    let labels: Vec<String> = apps
        .iter()
        .map(|a| format!("{} ({})", a.name, a.exec))
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

    let string_list = if label_refs.is_empty() {
        StringList::new(&["(None detected)"])
    } else {
        StringList::new(&label_refs)
    };

    let row = adw::ComboRow::builder()
        .title(title)
        .subtitle(subtitle)
        .model(&string_list)
        .build();

    let execs: Vec<String> = apps.iter().map(|a| a.exec.clone()).collect();
    let selected = apps
        .iter()
        .position(|a| a.exec == current_exec)
        .unwrap_or(0);
    row.set_selected(selected as u32);

    (row, execs)
}

fn get_combo_exec(row: &adw::ComboRow, execs: &[String]) -> String {
    let idx = row.selected() as usize;
    if idx < execs.len() {
        execs[idx].clone()
    } else if !execs.is_empty() {
        execs[0].clone()
    } else {
        "xdg-open".to_string()
    }
}

/// Whether `a` and `b` (or their nearest existing ancestors) are on one filesystem.
fn same_disk(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dev = |p: &Path| p.ancestors().find_map(|p| fs::metadata(p).ok()).map(|m| m.dev());
    matches!((dev(a), dev(b)), (Some(x), Some(y)) if x == y)
}
