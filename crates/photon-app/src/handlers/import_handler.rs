//! Import dialog flows:
//!   - Folder import   (native FileDialog portal picker)
//!   - Shotwell DB     (native FileDialog with .db filter)
//!   - Camera / device (auto-detect via GIO VolumeMonitor, pick DCIM)
//!
//! Progress is polled on the GTK main loop at 50 ms intervals.

use crate::ui::window::MainWindow;
use crossbeam_channel::unbounded;
use gtk4::prelude::*;
use gtk4::{
    gio, glib, Align, Box as GtkBox, Button, CheckButton, FileDialog, FileFilter, Label,
    Orientation, ScrolledWindow, Window,
};
use photon_core::models::{FolderImportMode, ImportProgress};
use photon_import::engine::ImportConfig;
use photon_import::sources::disk::DiskSource;
use photon_import::sources::shotwell::ShotwellSource;
use photon_import::sources::ImportSource;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

// ═══════════════════════════════════════════════════════════
// Folder import
// ═══════════════════════════════════════════════════════════

/// Show the native folder picker, then the import options dialog.
pub fn show_folder_import_dialog(mw: &MainWindow) {
    let dialog = FileDialog::builder()
        .title("Select Folder to Import")
        .modal(true)
        .build();

    let mw = mw.clone();
    let parent_win = mw.window.clone();
    dialog.select_folder(Some(&parent_win), gio::Cancellable::NONE, move |result| {
        if let Ok(file) = result {
            if let Some(path) = file.path() {
                show_import_options_dialog(&mw, path, false);
            }
        }
    });
}

/// Pre-scan files and show the Pre-Import Review dialog ("N new / N already in library").
fn show_import_options_dialog(mw: &MainWindow, source_path: PathBuf, from_device: bool) {
    let dialog = Window::builder()
        .transient_for(&mw.window)
        .modal(true)
        .title("Review Import")
        .default_width(580)
        .default_height(460)
        .build();

    let vbox = GtkBox::new(Orientation::Vertical, 12);
    vbox.set_margin_top(18);
    vbox.set_margin_bottom(18);
    vbox.set_margin_start(20);
    vbox.set_margin_end(20);

    // Initial Scanning View
    let spinner = gtk4::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_size_request(32, 32);
    spinner.set_halign(Align::Center);

    let scanning_lbl = Label::new(Some("Analyzing photos in folder…"));
    scanning_lbl.set_css_classes(&["heading"]);
    scanning_lbl.set_halign(Align::Center);

    let loading_box = GtkBox::new(Orientation::Vertical, 12);
    loading_box.set_valign(Align::Center);
    loading_box.set_vexpand(true);
    loading_box.append(&spinner);
    loading_box.append(&scanning_lbl);
    vbox.append(&loading_box);

    dialog.set_child(Some(&vbox));
    dialog.present();

    let mw_clone = mw.clone();
    let src = source_path.clone();
    let (tx_scan, rx_scan) = crossbeam_channel::bounded::<(Vec<PathBuf>, photon_import::engine::PreImportReport)>(1);

    thread::spawn(move || {
        let disk_source = DiskSource::new(src, true);
        if let Ok(files) = disk_source.scan() {
            let mode = if from_device {
                FolderImportMode::Copy
            } else {
                FolderImportMode::InPlace
            };
            if let Ok(report) = mw_clone.engine.pre_scan(&files, mode) {
                let _ = tx_scan.send((files, report));
            }
        }
    });

    let d_window = dialog.clone();
    let mw_for_ui = mw.clone();
    let src_path = source_path.clone();

    glib::timeout_add_local(Duration::from_millis(50), move || {
        let Ok((_files, report)) = rx_scan.try_recv() else {
            return glib::ControlFlow::Continue;
        };

        // Clear loading state
        while let Some(c) = vbox.first_child() {
            vbox.remove(&c);
        }

        // ── Summary Header ──────────────────────────────
        let header_box = GtkBox::new(Orientation::Vertical, 4);

        let summary_text = format!(
            "{} New Photos  ·  {} Already in Library",
            report.new_files.len(),
            report.duplicates.len()
        );
        let title_lbl = Label::new(Some(&summary_text));
        title_lbl.set_css_classes(&["title-3"]);
        title_lbl.set_halign(Align::Start);

        let path_lbl = Label::new(Some(&format!("Source: {}", src_path.display())));
        path_lbl.set_css_classes(&["caption", "dim-label"]);
        path_lbl.set_halign(Align::Start);
        path_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);

        header_box.append(&title_lbl);
        header_box.append(&path_lbl);
        vbox.append(&header_box);

        // ── Mode Selection ──────────────────────────────
        let mode_card = GtkBox::new(Orientation::Vertical, 6);
        mode_card.add_css_class("card");
        mode_card.set_margin_top(4);
        mode_card.set_margin_bottom(4);

        let mode_box = GtkBox::new(Orientation::Horizontal, 16);
        mode_box.set_margin_top(8);
        mode_box.set_margin_bottom(8);
        mode_box.set_margin_start(12);
        mode_box.set_margin_end(12);

        let rb_link = CheckButton::with_label("Link (In Place)");
        rb_link.set_active(true);
        let rb_copy = CheckButton::with_label("Copy to Library");
        rb_copy.set_group(Some(&rb_link));
        let rb_move = CheckButton::with_label("Move to Library");
        rb_move.set_group(Some(&rb_link));

        if from_device {
            rb_copy.set_active(true);
            rb_link.set_sensitive(false);
            rb_link.set_tooltip_text(Some("Not available for cameras and memory cards"));
        }

        mode_box.append(&rb_link);
        mode_box.append(&rb_copy);
        mode_box.append(&rb_move);
        mode_card.append(&mode_box);
        vbox.append(&mode_card);

        // ── Previews / Item lists ───────────────────────
        let scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .min_content_height(180)
            .build();

        let list_box = GtkBox::new(Orientation::Vertical, 8);

        // New photos preview
        if !report.new_files.is_empty() {
            let new_expander = gtk4::Expander::builder()
                .label(format!("New Photos to Import ({})", report.new_files.len()))
                .expanded(true)
                .build();
            let new_box = GtkBox::new(Orientation::Vertical, 4);
            new_box.set_margin_start(8);
            new_box.set_margin_top(4);

            for item in report.new_files.iter().take(50) {
                let row = GtkBox::new(Orientation::Horizontal, 8);
                let icon = gtk4::Image::from_icon_name("camera-photo-symbolic");
                let name = Label::new(Some(&item.filename));
                name.set_halign(Align::Start);
                name.set_hexpand(true);
                name.set_ellipsize(gtk4::pango::EllipsizeMode::End);

                let size_str = format_bytes(item.size_bytes);
                let size_lbl = Label::new(Some(&size_str));
                size_lbl.set_css_classes(&["caption", "dim-label"]);

                row.append(&icon);
                row.append(&name);
                row.append(&size_lbl);
                new_box.append(&row);
            }
            if report.new_files.len() > 50 {
                let more_lbl = Label::new(Some(&format!("… and {} more files", report.new_files.len() - 50)));
                more_lbl.set_css_classes(&["caption", "dim-label"]);
                more_lbl.set_halign(Align::Start);
                new_box.append(&more_lbl);
            }
            new_expander.set_child(Some(&new_box));
            list_box.append(&new_expander);
        }

        // Duplicates preview (collapsed)
        if !report.duplicates.is_empty() {
            let dup_expander = gtk4::Expander::builder()
                .label(format!("Already in Library — Will be Skipped ({})", report.duplicates.len()))
                .expanded(false)
                .build();
            let dup_box = GtkBox::new(Orientation::Vertical, 4);
            dup_box.set_margin_start(8);
            dup_box.set_margin_top(4);

            for item in report.duplicates.iter().take(50) {
                let row = GtkBox::new(Orientation::Horizontal, 8);
                let icon = gtk4::Image::from_icon_name("emblem-ok-symbolic");
                let name = Label::new(Some(&item.filename));
                name.set_css_classes(&["dim-label"]);
                name.set_halign(Align::Start);
                name.set_hexpand(true);
                name.set_ellipsize(gtk4::pango::EllipsizeMode::End);

                row.append(&icon);
                row.append(&name);
                dup_box.append(&row);
            }
            dup_expander.set_child(Some(&dup_box));
            list_box.append(&dup_expander);
        }

        scroll.set_child(Some(&list_box));
        vbox.append(&scroll);

        // ── Action Buttons ──────────────────────────────
        let btn_box = GtkBox::new(Orientation::Horizontal, 12);
        btn_box.set_halign(Align::End);
        btn_box.set_margin_top(8);

        let cancel_btn = Button::with_label("Cancel");
        cancel_btn.add_css_class("flat");

        let import_label = if report.new_files.is_empty() {
            "No New Photos".to_string()
        } else {
            format!("Import {} Photos", report.new_files.len())
        };

        let import_btn = Button::with_label(&import_label);
        import_btn.add_css_class("suggested-action");
        if report.new_files.is_empty() {
            import_btn.set_sensitive(false);
        }

        btn_box.append(&cancel_btn);
        btn_box.append(&import_btn);
        vbox.append(&btn_box);

        let d_cancel = d_window.clone();
        cancel_btn.connect_clicked(move |_| d_cancel.close());

        let d_close = d_window.clone();
        let mw = mw_for_ui.clone();
        let src_for_import = src_path.clone();
        import_btn.connect_clicked(move |_| {
            d_close.close();

            let mode = if rb_copy.is_active() {
                FolderImportMode::Copy
            } else if rb_move.is_active() {
                FolderImportMode::Move
            } else {
                FolderImportMode::InPlace
            };

            let config = ImportConfig {
                mode,
                ..ImportConfig::default()
            };

            let source = DiskSource::new(src_for_import.clone(), true);
            run_import(&mw, Box::new(source), config);
        });

        glib::ControlFlow::Break
    });
}

fn format_bytes(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// ═══════════════════════════════════════════════════════════
// Shotwell DB import
// ═══════════════════════════════════════════════════════════

/// Show the native file picker filtered to .db files.
pub fn show_shotwell_import_dialog(mw: &MainWindow) {
    let filter = FileFilter::new();
    filter.set_name(Some("Shotwell Database"));
    filter.add_pattern("*.db");

    let filters = gio::ListStore::new::<FileFilter>();
    filters.append(&filter);

    let dialog = FileDialog::builder()
        .title("Select Shotwell Database")
        .modal(true)
        .filters(&filters)
        .build();

    let mw = mw.clone();
    let parent_win = mw.window.clone();
    dialog.open(Some(&parent_win), gio::Cancellable::NONE, move |result| {
        if let Ok(file) = result {
            if let Some(path) = file.path() {
                let source = ShotwellSource::new(path);
                let config = ImportConfig::default();
                run_import(&mw, Box::new(source), config);
            }
        }
    });
}

// ═══════════════════════════════════════════════════════════
// Camera / device import
// ═══════════════════════════════════════════════════════════

/// Detect mounted cameras/devices and show a picker dialog.
///
/// Looks for volumes/mounts that contain a DCIM folder (the standard for
/// cameras and phones). Also checks gvfs gphoto2 mounts.
pub fn show_camera_import_dialog(mw: &MainWindow) {
    let devices = detect_camera_mounts();

    if devices.is_empty() {
        // No camera found — show a message with option to browse manually
        let dialog = Window::builder()
            .transient_for(&mw.window)
            .modal(true)
            .title("Import from Camera")
            .default_width(400)
            .default_height(200)
            .build();

        let vbox = GtkBox::new(Orientation::Vertical, 12);
        vbox.set_margin_top(24);
        vbox.set_margin_bottom(24);
        vbox.set_margin_start(24);
        vbox.set_margin_end(24);

        let msg = Label::new(Some(
            "No camera or device detected.\n\n\
             Make sure your camera is connected and mounted.\n\
             You can also use File → Import → From Folder to browse manually.",
        ));
        msg.set_wrap(true);
        msg.set_halign(Align::Start);
        vbox.append(&msg);

        let btn_box = GtkBox::new(Orientation::Horizontal, 12);
        btn_box.set_halign(Align::End);
        btn_box.set_margin_top(12);

        let close_btn = Button::with_label("Close");
        let browse_btn = Button::with_label("Browse Manually…");
        browse_btn.add_css_class("suggested-action");

        btn_box.append(&close_btn);
        btn_box.append(&browse_btn);
        vbox.append(&btn_box);

        dialog.set_child(Some(&vbox));

        let d1 = dialog.clone();
        close_btn.connect_clicked(move |_| d1.close());

        let d2 = dialog.clone();
        let mw2 = mw.clone();
        browse_btn.connect_clicked(move |_| {
            d2.close();
            show_folder_import_dialog(&mw2);
        });

        dialog.present();
        return;
    }

    // Found one or more devices — let user pick
    if devices.len() == 1 {
        // Single device: skip picker, go straight to import options
        show_import_options_dialog(mw, devices[0].1.clone(), true);
        return;
    }

    // Multiple devices: show picker
    let dialog = Window::builder()
        .transient_for(&mw.window)
        .modal(true)
        .title("Select Camera / Device")
        .default_width(450)
        .default_height(300)
        .build();

    let vbox = GtkBox::new(Orientation::Vertical, 8);
    vbox.set_margin_top(16);
    vbox.set_margin_bottom(16);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);

    let header = Label::new(Some("Found devices:"));
    header.set_css_classes(&["title-4"]);
    header.set_halign(Align::Start);
    vbox.append(&header);

    for (name, path) in &devices {
        let btn = Button::builder().has_frame(false).build();
        let row = GtkBox::new(Orientation::Horizontal, 12);
        row.set_margin_top(4);
        row.set_margin_bottom(4);
        row.append(&gtk4::Image::from_icon_name("camera-photo-symbolic"));

        let labels = GtkBox::new(Orientation::Vertical, 2);
        let name_lbl = Label::new(Some(name));
        name_lbl.set_css_classes(&["title-4"]);
        name_lbl.set_halign(Align::Start);
        let path_lbl = Label::new(Some(&path.to_string_lossy()));
        path_lbl.set_css_classes(&["caption", "dim-label"]);
        path_lbl.set_halign(Align::Start);
        labels.append(&name_lbl);
        labels.append(&path_lbl);

        row.append(&labels);
        btn.set_child(Some(&row));

        let d = dialog.clone();
        let mw_c = mw.clone();
        let p = path.clone();
        btn.connect_clicked(move |_| {
            d.close();
            show_import_options_dialog(&mw_c, p.clone(), true);
        });

        vbox.append(&btn);
    }

    dialog.set_child(Some(&vbox));
    dialog.present();
}

/// Detect mounted volumes/paths that look like cameras (contain DCIM).
fn detect_camera_mounts() -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();

    let monitor = gio::VolumeMonitor::get();

    // Check all mounts
    for mount in monitor.mounts() {
        let root = mount.root();
        if let Some(path) = root.path() {
            let dcim = path.join("DCIM");
            if dcim.exists() && dcim.is_dir() {
                let name = mount.name().to_string();
                found.push((name, dcim));
            }
        }
    }

    // Also check common gvfs gphoto2 mount points
    if let Some(uid) = get_uid() {
        let gvfs_root = PathBuf::from(format!("/run/user/{}/gvfs", uid));
        if gvfs_root.exists() {
            if let Ok(entries) = std::fs::read_dir(&gvfs_root) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.to_string_lossy().contains("gphoto2") {
                        let dcim = p.join("DCIM");
                        if dcim.exists() {
                            let name = p
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            found.push((format!("Camera ({})", name), dcim));
                        } else {
                            // Some cameras don't have DCIM, import the root
                            let name = p
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            found.push((format!("Camera ({})", name), p));
                        }
                    }
                }
            }
        }
    }

    // USB-mounted SD cards: udisks uses /run/media/$USER (Fedora, Arch),
    // Debian/Ubuntu use /media/$USER.
    let user = std::env::var("USER").unwrap_or_default();
    for media in [format!("/run/media/{user}"), format!("/media/{user}")] {
        let media = PathBuf::from(media);
        if !user.is_empty() && media.exists() {
            if let Ok(entries) = std::fs::read_dir(&media) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    let dcim = p.join("DCIM");
                    if dcim.exists() && dcim.is_dir() {
                        let name = p
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        if !found.iter().any(|(_, fp)| fp == &dcim) {
                            found.push((name, dcim));
                        }
                    }
                }
            }
        }
    }

    found
}

fn get_uid() -> Option<u32> {
    // Simple: parse from /proc/self/status or use libc
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
}

// ═══════════════════════════════════════════════════════════
// Import execution (shared by all sources)
// ═══════════════════════════════════════════════════════════

fn run_import(
    mw: &MainWindow,
    source: Box<dyn photon_import::sources::ImportSource + 'static>,
    config: ImportConfig,
) {
    /// Newly committed photos are shown at most this often, so a fast import
    /// doesn't spend its time re-laying out the timeline.
    const LIVE_REFRESH: Duration = Duration::from_millis(1500);

    mw.progress_revealer.set_reveal_child(true);
    mw.status_label.set_text("Scanning…");
    mw.progress_bar.set_fraction(0.0);
    mw.progress_bar.set_text(None);

    let cancel = config.cancel.clone();
    mw.cancel_button.set_visible(true);
    mw.cancel_button.set_sensitive(true);
    let cancel_handler = mw.cancel_button.connect_clicked(move |btn| {
        cancel.cancel();
        btn.set_sensitive(false);
        btn.set_label("Stopping…");
    });

    let (progress_tx, progress_rx) = unbounded::<ImportProgress>();
    let (done_tx, done_rx) = unbounded::<anyhow::Result<photon_core::models::ImportBatch>>();

    let engine = mw.engine.clone();
    thread::spawn(move || {
        let result = engine.import(source.as_ref(), &config, Some(&progress_tx));
        let _ = done_tx.send(result);
    });

    let mw = mw.clone();
    let mut cancel_handler = Some(cancel_handler);
    let mut last_refresh = Instant::now();
    let mut pending_refresh = false;
    let mut imported = 0usize;

    glib::timeout_add_local(Duration::from_millis(50), move || {
        for progress in progress_rx.try_iter() {
            match progress {
                ImportProgress::Started { total } => {
                    mw.status_label.set_text(&format!("Importing {total} files…"));
                }
                ImportProgress::Processing {
                    processed,
                    total,
                    current_file,
                } => {
                    mw.progress_bar.set_fraction(processed as f64 / total.max(1) as f64);
                    mw.progress_bar.set_text(Some(&format!("{processed} / {total}")));
                    mw.status_label.set_text(&format!(
                        "Importing — {imported} new so far · {current_file}"
                    ));
                }
                ImportProgress::Committed { imported: n } => {
                    imported = n;
                    pending_refresh = true;
                }
                ImportProgress::Completed { .. } => {}
            }
        }

        // Photos appear in the library while the import runs.
        if pending_refresh && last_refresh.elapsed() >= LIVE_REFRESH {
            mw.refresh();
            pending_refresh = false;
            last_refresh = Instant::now();
        }

        let Ok(result) = done_rx.try_recv() else {
            return glib::ControlFlow::Continue;
        };

        let summary = match result {
            Ok(batch) => {
                let what = if batch.status == "cancelled" { "Import stopped" } else { "Import complete" };
                let mut text = format!(
                    "{what}: {} new, {} already in library",
                    batch.imported_count, batch.duplicate_count
                );
                if batch.error_count > 0 {
                    text.push_str(&format!(", {} failed (see log)", batch.error_count));
                }
                text
            }
            Err(e) => format!("Import failed: {e:#}"),
        };
        mw.refresh();
        mw.status_label.set_text(&summary);
        mw.progress_bar.set_fraction(1.0);

        if let Some(id) = cancel_handler.take() {
            mw.cancel_button.disconnect(id);
        }
        mw.cancel_button.set_visible(false);
        mw.cancel_button.set_label("Stop Import");

        let revealer = mw.progress_revealer.clone();
        glib::timeout_add_local_once(Duration::from_secs(6), move || {
            revealer.set_reveal_child(false);
        });
        glib::ControlFlow::Break
    });
}

// ═══════════════════════════════════════════════════════════
// Thumbnail backfill (startup)
// ═══════════════════════════════════════════════════════════

/// Drop the legacy id-keyed cache and generate thumbnails for every library
/// photo that has none, in the background. Reloads the current view when done.
pub fn generate_missing_thumbnails(mw: &MainWindow) {
    let engine = mw.engine.clone();
    let (progress_tx, progress_rx) = unbounded::<(usize, usize)>();
    let (done_tx, done_rx) = unbounded::<anyhow::Result<usize>>();

    thread::spawn(move || {
        if let Some(thumbs) = engine.thumbnails() {
            thumbs.remove_legacy_cache();
        }
        let result = engine.generate_missing_thumbnails(|done, total| {
            let _ = progress_tx.send((done, total));
        });
        let _ = done_tx.send(result);
    });

    let mw = mw.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if let Some((done, total)) = progress_rx.try_iter().last() {
            mw.progress_revealer.set_reveal_child(true);
            mw.progress_bar.set_fraction(done as f64 / total.max(1) as f64);
            mw.status_label
                .set_text(&format!("Generating thumbnails: {}/{}", done, total));
        }

        let Ok(result) = done_rx.try_recv() else {
            return glib::ControlFlow::Continue;
        };
        match result {
            Ok(0) => {}
            Ok(n) => {
                log::info!("Generated {n} missing thumbnails");
                mw.refresh();
            }
            Err(e) => log::warn!("Thumbnail backfill failed: {e:#}"),
        }
        mw.progress_revealer.set_reveal_child(false);
        glib::ControlFlow::Break
    });
}
