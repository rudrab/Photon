//! Photon — a fast photo manager for GNOME Linux following GNOME HIG and Material 3 design.

use anyhow::Result;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::Database;
use photon_import::thumbnails::ThumbnailGenerator;
use photon_import::ImportEngine;
use std::sync::Arc;

mod handlers;
mod menu;
mod ui;

const APP_ID: &str = "org.mavensgroup.photon";
const APP_CSS: &str = include_str!("../resources/style.css");

fn main() -> Result<()> {
    env_logger::init();

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        adw::init().expect("Failed to initialize libadwaita");
        install_app_css();
    });

    app.connect_activate(build_ui);

    // Quit action (Ctrl+Q)
    let quit_action = gtk4::gio::SimpleAction::new("quit", None);
    let app_weak = app.downgrade();
    quit_action.connect_activate(move |_, _| {
        if let Some(app) = app_weak.upgrade() {
            app.quit();
        }
    });
    app.add_action(&quit_action);

    app.run();
    Ok(())
}

fn install_app_css() {
    if let Some(display) = gdk::Display::default() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(APP_CSS);
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_ui(app: &adw::Application) {
    // --- Data directory ---
    let data_dir = glib::user_data_dir().join("photon");
    std::fs::create_dir_all(&data_dir).expect("Failed to create data directory");

    // --- Database ---
    let db_path = data_dir.join("photon.db");
    let db = Database::open(&db_path).expect("Failed to initialize database");
    let db_backup = db.clone();

    // --- Thumbnail cache ---
    let cache_dir = glib::user_cache_dir().join("photon").join("thumbnails");
    let thumbnail_gen = ThumbnailGenerator::new(cache_dir.clone());

    // --- Import engine ---
    let engine = Arc::new(ImportEngine::new(db.clone(), Some(thumbnail_gen)));

    // --- Keyboard shortcuts ---
    menu::setup_shortcuts(app);

    // --- Main window ---
    let window = ui::window::MainWindow::new(app, db, engine, cache_dir);
    window.present();

    // Development aid: `PHOTON_SCREENSHOT=/tmp/shot.png photon` renders the
    // window to a PNG after it settles, then quits.
    if let Some(path) = std::env::var_os("PHOTON_SCREENSHOT") {
        schedule_screenshot(&window.window, path.into());
    }

    handlers::import_handler::generate_missing_thumbnails(&window);
    back_up_catalog(&window.window, db_backup, data_dir.join("backups"));
}

/// Daily, in the background: check the library database and keep a
/// rotating set of copies of it. A failed check is shown to the user right
/// away — the sooner they know, the more recent their last good copy is.
fn back_up_catalog(window: &adw::ApplicationWindow, db: Database, dir: std::path::PathBuf) {
    const KEEP: usize = 14;
    const EVERY: std::time::Duration = std::time::Duration::from_secs(20 * 3600);

    let (tx, rx) = async_channel::bounded(1);
    let backups = dir.clone();
    std::thread::spawn(move || {
        let _ = tx.send_blocking(db.back_up(&backups, KEEP, EVERY));
    });
    let window = window.downgrade();
    glib::spawn_future_local(async move {
        let Ok(result) = rx.recv().await else { return };
        match result {
            Ok(photon_core::db::BackupOutcome::Saved(path)) => log::info!("Library backed up to {}", path.display()),
            Ok(photon_core::db::BackupOutcome::Recent) => {}
            Err(e) => {
                log::error!("Library backup: {e}");
                let Some(window) = window.upgrade() else { return };
                let dialog = adw::MessageDialog::new(
                    Some(&window),
                    Some("Library Database Problem"),
                    Some(&format!(
                        "{e}\n\nRatings, tags and albums may be affected. Your photo files are not. \
                         Earlier copies of the database are kept in {}.",
                        dir.display()
                    )),
                );
                dialog.add_response("ok", "OK");
                dialog.present();
            }
        }
    });
}

fn schedule_screenshot(window: &adw::ApplicationWindow, path: std::path::PathBuf) {
    let window = window.clone();
    glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
        let (width, height) = (window.width(), window.height());
        let paintable = gtk4::WidgetPaintable::new(window.content().as_ref());
        let snapshot = gtk4::Snapshot::new();
        paintable.snapshot(&snapshot, width as f64, height as f64);
        let renderer = gtk4::gsk::CairoRenderer::new();
        let result = match snapshot.to_node() {
            Some(node) if renderer.realize(None).is_ok() => {
                let viewport = gtk4::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
                let saved = renderer.render_texture(node, Some(&viewport)).save_to_png(&path);
                renderer.unrealize();
                saved.map_err(|e| e.to_string())
            }
            _ => Err(format!("nothing rendered ({width}x{height})")),
        };
        if let Err(e) = result {
            log::error!("Screenshot failed: {e}");
        }
        if let Some(app) = window.application() {
            app.quit();
        }
    });
}
