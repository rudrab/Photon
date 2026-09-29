//! Share menu: hand photos to other apps and sites. Needs no API keys:
//!
//!   * **Copy**: files on the clipboard (paste into Files, a chat, a mail);
//!     a single photo also goes on as image data, which web chats accept.
//!   * **Email**: the desktop's mail client via the xdg-desktop-portal Email
//!     portal (works in Flatpak), falling back to `xdg-email`.
//!   * **WhatsApp Web / Google Photos**: opens the site; the photos are on
//!     the clipboard, and tiles can be dragged from Photon onto the page.
//!   * **Open With**: every installed app that handles all the photos' types
//!     (Telegram, Thunderbird, GIMP, …), launched with all of them at once.
//!
//! The menu asks for its photos when an item is chosen, so it always acts on
//! the current selection, narrowed to the versions the "Share Sends"
//! preference asks for (by default a RAW+JPG shot sends only its JPG).

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, MenuButton};
use photon_core::db::{queries, Database};
use photon_core::models::{Image, Preferences};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

/// What the menu needs from the window.
#[derive(Clone)]
pub struct Context {
    pub window: gtk4::Window,
    /// Shows a short in-app notification.
    pub notify: Rc<dyn Fn(&str)>,
    pub db: Database,
    pub prefs: Rc<RefCell<Preferences>>,
}

/// The versions of the `selected` photos' shots that Share sends.
pub fn versions_to_share(ctx: &Context, selected: Vec<Image>) -> Vec<Image> {
    let mode = ctx.prefs.borrow().share_versions;
    let shots = match ctx.db.conn().map_err(|e| e.to_string()).and_then(|conn| {
        queries::shots_of(&conn, selected.clone()).map_err(|e| e.to_string())
    }) {
        Ok(shots) => shots,
        Err(e) => {
            log::warn!("Could not look up versions, sharing the selection as is: {e}");
            return selected;
        }
    };
    let mut seen = HashSet::new();
    let mut images = Vec::new();
    for (versions, selected_ids) in &shots {
        let selected: Vec<&Image> =
            versions.iter().filter(|v| v.id.is_some_and(|id| selected_ids.contains(&id))).collect();
        for img in mode.pick_selected(versions, &selected) {
            if seen.insert(img.id) {
                images.push(img.clone());
            }
        }
    }
    images
}

/// Photos the menu acts on, resolved when an item is chosen.
pub type Photos = Rc<dyn Fn() -> Vec<Image>>;

/// A Share menu button acting on `photos`.
pub fn menu_button(ctx: &Context, photos: Photos) -> MenuButton {
    let photos: Photos = {
        let ctx = ctx.clone();
        Rc::new(move || versions_to_share(&ctx, photos()))
    };
    let button = MenuButton::builder()
        .icon_name("send-to-symbolic")
        .tooltip_text("Share")
        .build();
    button.add_css_class("flat");

    let group = gio::SimpleActionGroup::new();
    let add = |name: &str, f: fn(&Context, &[Image])| {
        let action = gio::SimpleAction::new(name, None);
        let (ctx, photos) = (ctx.clone(), photos.clone());
        action.connect_activate(move |_, _| {
            let images = photos();
            if !images.is_empty() {
                f(&ctx, &images);
            }
        });
        group.add_action(&action);
    };
    add("copy", copy);
    add("email", email);
    add("whatsapp", whatsapp_web);
    add("google-photos", google_photos);

    let open_with = gio::SimpleAction::new("open-with", Some(glib::VariantTy::STRING));
    let (c, p) = (ctx.clone(), photos.clone());
    open_with.connect_activate(move |_, id| {
        let Some(id) = id.and_then(|v| v.str().map(str::to_string)) else { return };
        let images = p();
        if let Some(app) = gio::DesktopAppInfo::new(&id) {
            launch_with(&c, &app.upcast(), &images);
        }
    });
    group.add_action(&open_with);
    button.insert_action_group("share", Some(&group));

    // Rebuilt on every open: the "Open With" apps depend on the photos' types.
    button.set_create_popup_func(move |button| {
        button.set_menu_model(Some(&build_menu(&photos())));
    });
    button
}

fn build_menu(images: &[Image]) -> gio::Menu {
    let menu = gio::Menu::new();

    let send = gio::Menu::new();
    send.append(Some("Copy"), Some("share.copy"));
    send.append(Some("Email…"), Some("share.email"));
    menu.append_section(None, &send);

    let web = gio::Menu::new();
    web.append(Some("WhatsApp Web"), Some("share.whatsapp"));
    web.append(Some("Google Photos"), Some("share.google-photos"));
    menu.append_section(None, &web);

    let apps = apps_for(images);
    if !apps.is_empty() {
        let open_with = gio::Menu::new();
        for app in apps {
            let Some(id) = app.id() else { continue };
            let item = gio::MenuItem::new(Some(&app.display_name()), None);
            item.set_action_and_target_value(Some("share.open-with"), Some(&id.to_variant()));
            open_with.append_item(&item);
        }
        menu.append_submenu(Some("Open With"), &open_with);
    }
    menu
}

/// Installed apps that can open every one of `images`, by name (not Photon).
fn apps_for(images: &[Image]) -> Vec<gio::AppInfo> {
    let mut types: Vec<String> = images
        .iter()
        .map(|img| gio::content_type_guess(Some(&img.path), &[]).0.to_string())
        .collect();
    types.sort();
    types.dedup();
    let Some((first, rest)) = types.split_first() else { return Vec::new() };

    let own_id = format!("{}.desktop", crate::APP_ID);
    let mut apps: BTreeMap<String, gio::AppInfo> = gio::AppInfo::all_for_type(first)
        .into_iter()
        .filter(|app| app.should_show() && app.id().is_some_and(|id| id != own_id))
        .filter_map(|app| Some((app.id()?.to_string(), app)))
        .collect();
    for content_type in rest {
        let ids: Vec<String> = gio::AppInfo::all_for_type(content_type)
            .iter()
            .filter_map(|app| app.id().map(|id| id.to_string()))
            .collect();
        apps.retain(|id, _| ids.contains(id));
    }
    let mut apps: Vec<gio::AppInfo> = apps.into_values().collect();
    apps.sort_by_key(|app| app.display_name().to_lowercase());
    apps
}

fn files(images: &[Image]) -> Vec<gio::File> {
    images.iter().map(|img| gio::File::for_path(&img.path)).collect()
}

fn plural(n: usize) -> String {
    if n == 1 {
        "1 photo".to_string()
    } else {
        format!("{n} photos")
    }
}

/// Copy the versions of `selected` that Share sends (Ctrl+C).
pub fn copy_selection(ctx: &Context, selected: Vec<Image>) {
    let images = versions_to_share(ctx, selected);
    if !images.is_empty() {
        copy(ctx, &images);
    }
}

/// Put the files on the clipboard. A single raster photo also goes on as
/// image data (decoded off the main thread): web apps such as WhatsApp Web
/// take pasted images, not pasted files.
fn copy(ctx: &Context, images: &[Image]) {
    let list = gdk::FileList::from_array(&files(images));
    let file_provider = gdk::ContentProvider::for_value(&list.to_value());
    let clipboard = ctx.window.clipboard();
    if let Err(e) = clipboard.set_content(Some(&file_provider)) {
        log::warn!("Copy to clipboard failed: {e}");
        return;
    }
    (ctx.notify)(&format!("Copied {}", plural(images.len())));

    let [image] = images else { return };
    if image.format.is_some_and(|f| f.is_raw()) {
        return;
    }
    let path = image.path.clone();
    glib::spawn_future_local(async move {
        let loaded = gio::spawn_blocking(move || gdk::Texture::from_filename(&path)).await;
        let Ok(Ok(texture)) = loaded else { return };
        // Skip if something else was copied meanwhile.
        if clipboard.content().as_ref() != Some(&file_provider) {
            return;
        }
        let both = gdk::ContentProvider::new_union(&[
            file_provider.clone(),
            gdk::ContentProvider::for_value(&texture.to_value()),
        ]);
        let _ = clipboard.set_content(Some(&both));
    });
}

/// Compose a mail with the photos attached.
fn email(ctx: &Context, images: &[Image]) {
    let paths: Vec<PathBuf> = images.iter().map(|img| img.path.clone()).collect();
    let fallback = {
        let (ctx, paths) = (ctx.clone(), paths.clone());
        move || {
            let mut cmd = Command::new("xdg-email");
            for path in &paths {
                cmd.arg("--attach").arg(path);
            }
            if let Err(e) = cmd.spawn() {
                log::warn!("xdg-email failed: {e}");
                (ctx.notify)("No mail app found");
            }
        }
    };

    let fds = gio::UnixFDList::new();
    let mut handles = Vec::new();
    for path in &paths {
        let appended = std::fs::File::open(path)
            .map_err(|e| e.to_string())
            .and_then(|f| fds.append(f.as_raw_fd()).map_err(|e| e.to_string()));
        match appended {
            Ok(index) => handles.push(glib::variant::Handle(index)),
            Err(e) => {
                log::warn!("Cannot attach {}: {e}", path.display());
                return fallback();
            }
        }
    }
    let options = glib::VariantDict::new(None);
    options.insert_value("attachment_fds", &handles.to_variant());
    let params = ("", options.end()).to_variant();

    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return fallback();
    };
    bus.call_with_unix_fd_list(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Email",
        "ComposeEmail",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        Some(&fds),
        gio::Cancellable::NONE,
        move |result| {
            if let Err(e) = result {
                log::info!("Email portal unavailable ({e}); trying xdg-email");
                fallback();
            }
        },
    );
}

fn open_uri(ctx: &Context, uri: &str) {
    gtk4::UriLauncher::new(uri).launch(Some(&ctx.window), gio::Cancellable::NONE, |result| {
        if let Err(e) = result {
            log::warn!("Could not open browser: {e}");
        }
    });
}

/// WhatsApp has no API for personal chats: copy the photos and open
/// WhatsApp Web, where they can be pasted or dropped into a chat.
fn whatsapp_web(ctx: &Context, images: &[Image]) {
    copy(ctx, images);
    open_uri(ctx, "https://web.whatsapp.com/");
    (ctx.notify)(&format!(
        "{} copied. In a chat, paste with Ctrl+V or drag photos from Photon",
        plural(images.len())
    ));
}

/// Uploading through the Google Photos API needs an OAuth app; the web
/// page takes photos dropped onto it.
fn google_photos(ctx: &Context, images: &[Image]) {
    copy(ctx, images);
    open_uri(ctx, "https://photos.google.com/");
    (ctx.notify)("Drag photos from Photon onto the Google Photos page to upload them");
}

fn launch_with(ctx: &Context, app: &gio::AppInfo, images: &[Image]) {
    let launch_ctx = WidgetExt::display(&ctx.window).app_launch_context();
    // GIO starts apps that take one file (%f) once per photo.
    if let Err(e) = app.launch(&files(images), Some(&launch_ctx)) {
        log::warn!("Could not start {}: {e}", app.display_name());
        (ctx.notify)(&format!("Could not start {}", app.display_name()));
    }
}
