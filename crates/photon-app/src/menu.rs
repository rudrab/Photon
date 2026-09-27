//! Application menu models and keyboard shortcut registration.
//! Follows GNOME HIG with Primary Menu (hamburger) and Import Menu models.

use gtk4::gio;
use gtk4::prelude::*;
use gtk4::Application;

/// Build the primary application menu (hamburger menu in header bar).
pub fn build_primary_menu() -> gio::MenuModel {
    let menu = gio::Menu::new();

    // ── View Section ────────────────────────────────────
    let view_section = gio::Menu::new();
    view_section.append(Some("Refresh Library"), Some("win.refresh"));
    menu.append_section(None, &view_section);

    // ── Preferences & Shortcuts ─────────────────────────
    let prefs_section = gio::Menu::new();
    prefs_section.append(Some("Preferences"), Some("app.preferences"));
    prefs_section.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append_section(None, &prefs_section);

    // ── About & Quit ────────────────────────────────────
    let app_section = gio::Menu::new();
    app_section.append(Some("About Photon"), Some("win.about"));
    app_section.append(Some("Quit"), Some("app.quit"));
    menu.append_section(None, &app_section);

    menu.into()
}

/// Build the import menu (attached to header bar Import button).
pub fn build_import_menu() -> gio::MenuModel {
    let menu = gio::Menu::new();
    menu.append(Some("From Folder…"), Some("win.import_folder"));
    menu.append(Some("From Camera / Device…"), Some("win.import_camera"));
    menu.append(Some("From Shotwell Database…"), Some("win.import_shotwell"));
    menu.append(Some("From digiKam Database…"), Some("win.import_digikam"));
    menu.into()
}

/// Register keyboard accelerators for menu actions.
pub fn setup_shortcuts(app: &impl IsA<Application>) {
    let app = app.as_ref();
    app.set_accels_for_action("win.import_folder", &["<Ctrl>o"]);
    app.set_accels_for_action("win.refresh", &["F5", "<Ctrl>r"]);
    app.set_accels_for_action("win.search", &["<Ctrl>f"]);
    app.set_accels_for_action("app.quit", &["<Ctrl>q"]);
    app.set_accels_for_action("app.preferences", &["<Ctrl>comma"]);
    app.set_accels_for_action("win.shortcuts", &["<Ctrl>question"]);
}
