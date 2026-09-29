//! Floating action bar for the timeline selection (Google Photos / Files
//! style): appears at the bottom while photos are selected, or in selection
//! mode.
//!
//!   N selected [×] [Select all] | Pick Reject Unflag ★▾ | ⟲ ⟳ | Compare |
//!   Album▾ | Open  Show in Files  Share▾  Export | Trash ⋯
//!
//! Compare needs exactly two photos. On narrow windows Album, Open and Show
//! in Files move into the ⋯ menu (which always holds Remove from Library).
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
    /// Compare the two photos (ids) side by side.
    pub on_compare: Rc<dyn Fn(Vec<i64>)>,
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
    // The same 8px from the bar's edges as between groups.
    start_box.set_margin_start(8);

    // Every cluster is a `photon-bar-group` (see style.css): one height and
    // corner radius, no dividers, spacing between groups.
    let idle_group = GtkBox::new(Orientation::Horizontal, 2);
    idle_group.add_css_class("photon-bar-group");
    let idle_label = Label::new(Some("Ready"));
    idle_label.add_css_class("dim-label");
    idle_group.append(&idle_label);
    start_box.append(&idle_group);

    let selection_box = GtkBox::new(Orientation::Horizontal, 2);
    selection_box.add_css_class("photon-bar-group");
    selection_box.set_valign(Align::Center);
    let count = Label::new(None);
    count.add_css_class("heading");
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

    let select_all = Button::from_icon_name("edit-select-all-symbolic");
    select_all.set_tooltip_text(Some("Select All (Ctrl+A)"));
    select_all.add_css_class("flat");
    let w_all = weak.clone();
    select_all.connect_clicked(move |_| {
        if let Some(t) = w_all.upgrade() {
            t.set_selection_mode(true);
            t.select_all();
        }
    });
    selection_box.append(&select_all);
    selection_box.set_visible(false);
    start_box.append(&selection_box);

    action_bar.pack_start(&start_box);

    // ── Center area: Selection actions, in groups ──────────────
    let center_box = GtkBox::new(Orientation::Horizontal, 8);
    center_box.set_valign(Align::Center);

    // Actions that need at least one photo selected.
    let actions: Rc<RefCell<Vec<gtk4::Widget>>> = Rc::default();
    let group = || {
        let g = GtkBox::new(Orientation::Horizontal, 2);
        g.add_css_class("photon-bar-group");
        g.set_valign(Align::Center);
        center_box.append(&g);
        g
    };
    let button = |into: &GtkBox, icon: &str, tooltip: &str| {
        let b = Button::from_icon_name(icon);
        b.set_tooltip_text(Some(tooltip));
        b.add_css_class("flat");
        into.append(&b);
        actions.borrow_mut().push(b.clone().upcast());
        b
    };
    let on_timeline = |f: fn(&Timeline)| {
        let w = weak.clone();
        move |_: &Button| {
            if let Some(t) = w.upgrade() {
                f(&t);
            }
        }
    };

    // Mark: pick/reject, stars and colour label, in one button.
    let cull = group();
    let w = weak.clone();
    let mark = crate::ui::mark::mark_button(move |m| {
        let Some(t) = w.upgrade() else { return };
        match m {
            crate::ui::mark::Mark::Flag(f) => t.cull_flag_selected(f),
            crate::ui::mark::Mark::Rating(r) => t.cull_rating_selected(r),
            crate::ui::mark::Mark::Color(c) => t.cull_color_selected(c),
        }
    });
    cull.append(&mark.button);
    actions.borrow_mut().push(mark.button.clone().upcast());

    // Rotate
    let rotate = group();
    button(
        &rotate,
        "object-rotate-left-symbolic",
        "Rotate Counter-Clockwise ([)\nNote: darktable ignores XMP rotation; exported RAWs via darktable use camera orientation.",
    )
    .connect_clicked(on_timeline(|t| t.rotate_selected(false)));
    button(
        &rotate,
        "object-rotate-right-symbolic",
        "Rotate Clockwise (] / Ctrl+R)\nNote: darktable ignores XMP rotation; exported RAWs via darktable use camera orientation.",
    )
    .connect_clicked(on_timeline(|t| t.rotate_selected(true)));

    // Compare: exactly two photos (its sensitivity is set below, not with `actions`).
    let compare_group = group();
    let compare = Button::from_icon_name("view-dual-symbolic");
    compare.set_tooltip_text(Some("Compare 2 photos side by side, or survey 3–9 in a grid"));
    compare.add_css_class("flat");
    let (w, c) = (weak.clone(), ctx.clone());
    compare.connect_clicked(move |_| {
        if let Some(t) = w.upgrade() {
            let ids = t.selected_ids();
            if (2..=9).contains(&ids.len()) {
                (c.on_compare)(ids);
            }
        }
    });
    compare_group.append(&compare);

    // Organise (folds into ⋯ on narrow windows)
    let organise = group();
    let album_btn = button(&organise, "folder-pictures-symbolic", "Add to Album…");
    let (w, c) = (weak.clone(), ctx.clone());
    let alb_b = album_btn.clone();
    album_btn.connect_clicked(move |_| {
        let imgs = selected_images(&w, &c.db);
        if !imgs.is_empty() {
            show_add_to_album_popover(&alb_b, imgs, &c);
        }
    });
    let remove_album_btn = button(&organise, "list-remove-symbolic", "Remove from Album");
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

    // Output: Open and Show in Files fold into ⋯ on narrow windows.
    let output = group();
    let output_extra = GtkBox::new(Orientation::Horizontal, 2);
    output.append(&output_extra);
    let open = button(&output_extra, "document-edit-symbolic", "Open in Editor");
    let (w, c) = (weak.clone(), ctx.clone());
    open.connect_clicked(move |_| open_in_editors(&selected_images(&w, &c.db), &c));
    let show = button(&output_extra, "folder-open-symbolic", "Show in Files");
    let (w, c) = (weak.clone(), ctx.clone());
    show.connect_clicked(move |_| show_in_files(&selected_images(&w, &c.db)));
    let (w, db) = (weak.clone(), ctx.db.clone());
    let share_btn = share::menu_button(&ctx.share, Rc::new(move || selected_images(&w, &db)));
    output.append(&share_btn);
    actions.borrow_mut().push(share_btn.upcast());
    let export = button(&output, "document-save-symbolic", "Export Selected (Ctrl+E)");
    let (w, c) = (weak.clone(), ctx.clone());
    export.connect_clicked(move |_| {
        let imgs = selected_images(&w, &c.db);
        if !imgs.is_empty() {
            (c.on_export)(imgs);
        }
    });

    // Trash, and ⋯ for the rest
    let end_group = group();
    let trash = button(&end_group, "user-trash-symbolic", "Move to Trash (Delete)");
    trash.add_css_class("destructive-action");
    let (w, c) = (weak.clone(), ctx.clone());
    trash.connect_clicked(move |_| confirm_trash(&w, &c));

    let overflow = gtk4::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text("More").build();
    overflow.add_css_class("flat");
    let overflow_pop = Popover::new();
    let overflow_box = GtkBox::new(Orientation::Vertical, 2);
    for m in [&overflow_box] {
        m.set_margin_top(6);
        m.set_margin_bottom(6);
        m.set_margin_start(6);
        m.set_margin_end(6);
    }
    let menu_item = |into: &GtkBox, label: &str, target: &Button| {
        let item = Button::with_label(label);
        item.add_css_class("flat");
        if let Some(l) = item.child().and_downcast::<Label>() {
            l.set_xalign(0.0);
        }
        let (target, pop) = (target.clone(), overflow_pop.clone());
        item.connect_clicked(move |_| {
            pop.popdown();
            target.emit_clicked();
        });
        into.append(&item);
    };
    // Shown only when the window is narrow (see the breakpoint below).
    let overflow_extra = GtkBox::new(Orientation::Vertical, 2);
    overflow_extra.set_visible(false);
    let album_item = Button::with_label("Add to Album…");
    album_item.add_css_class("flat");
    if let Some(l) = album_item.child().and_downcast::<Label>() {
        l.set_xalign(0.0);
    }
    let (w, c, anchor, pop) = (weak.clone(), ctx.clone(), overflow.clone(), overflow_pop.clone());
    album_item.connect_clicked(move |_| {
        pop.popdown();
        let imgs = selected_images(&w, &c.db);
        if !imgs.is_empty() {
            show_add_to_album_popover(&anchor, imgs, &c);
        }
    });
    overflow_extra.append(&album_item);
    menu_item(&overflow_extra, "Open in Editor", &open);
    menu_item(&overflow_extra, "Show in Files", &show);
    overflow_extra.append(&gtk4::Separator::new(Orientation::Horizontal));
    overflow_box.append(&overflow_extra);
    let remove = Button::new();
    remove.connect_clicked({
        let (w, c) = (weak.clone(), ctx.clone());
        move |_| confirm_remove(&w, &c)
    });
    menu_item(&overflow_box, "Remove from Library…", &remove);
    overflow_pop.set_child(Some(&overflow_box));
    overflow.set_popover(Some(&overflow_pop));
    end_group.append(&overflow);
    actions.borrow_mut().push(overflow.clone().upcast());

    if let Some(window) = ctx.window.downcast_ref::<adw::ApplicationWindow>() {
        // Too narrow for every group: Album, Open and Show in Files go to ⋯.
        match adw::BreakpointCondition::parse("max-width: 1250sp") {
            Ok(condition) => {
                let narrow = adw::Breakpoint::new(condition);
                narrow.add_setter(&organise, "visible", Some(&false.to_value()));
                narrow.add_setter(&output_extra, "visible", Some(&false.to_value()));
                narrow.add_setter(&overflow_extra, "visible", Some(&true.to_value()));
                window.add_breakpoint(narrow);
            }
            Err(e) => log::warn!("Bottom bar breakpoint: {e}"),
        }
    }

    center_box.set_visible(false);
    action_bar.set_center_widget(Some(&center_box));

    // ── End area: Zoom slider & Slideshow ─────────────────────
    let end_box = GtkBox::new(Orientation::Horizontal, 8);
    end_box.set_margin_end(8);
    end_box.set_valign(Align::Center);

    if let Some(on_slideshow) = ctx.on_start_slideshow.clone() {
        let slideshow_btn = Button::from_icon_name("media-playback-start-symbolic");
        slideshow_btn.set_tooltip_text(Some("Start Slideshow (F5)"));
        slideshow_btn.add_css_class("flat");
        slideshow_btn.connect_clicked(move |_| on_slideshow());
        let slideshow_group = GtkBox::new(Orientation::Horizontal, 2);
        slideshow_group.add_css_class("photon-bar-group");
        slideshow_group.append(&slideshow_btn);
        end_box.append(&slideshow_group);
    }

    let zoom_group = GtkBox::new(Orientation::Horizontal, 2);
    zoom_group.add_css_class("photon-bar-group");
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
    let compare_c = compare.clone();
    let sb_c = selection_box.clone();
    let ib_c = idle_group.clone();
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
        compare_c.set_sensitive((2..=9).contains(&n));
        rab_c.set_visible(c_alb.borrow().is_some() && n > 0);

        // The Mark button shows the selection's marks when they all agree.
        let states: Vec<crate::ui::mark::MarkState> = w
            .upgrade()
            .map(|t| t.selected_items())
            .unwrap_or_default()
            .iter()
            .map(|i| crate::ui::mark::MarkState { flag: i.flagged, rating: i.rating, color: i.color_label })
            .collect();
        let common = states.first().copied().filter(|first| states.iter().all(|s| s == first));
        mark.set_state(common);
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
        if (key == gdk::Key::a || key == gdk::Key::A) && state.contains(gdk::ModifierType::CONTROL_MASK) {
            if let Some(t) = w.upgrade() {
                t.set_selection_mode(true);
                t.select_all();
            }
            return glib::Propagation::Stop;
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

/// A small dot in a colour label's colour (the theme's palette), hollow for
/// no label. Symbolic, unlike emoji, which render in their own colours.
pub(crate) fn label_dot(color: photon_core::models::ColorLabel) -> GtkBox {
    let dot = GtkBox::new(Orientation::Horizontal, 0);
    dot.add_css_class("photon-label-dot");
    dot.set_valign(Align::Center);
    dot.set_halign(Align::Center);
    set_label_dot(&dot, color);
    dot
}

pub(crate) fn set_label_dot(dot: &GtkBox, color: photon_core::models::ColorLabel) {
    use photon_core::models::ColorLabel;
    for class in ["red", "yellow", "green", "blue", "purple", "none"] {
        dot.remove_css_class(class);
    }
    dot.add_css_class(match color {
        ColorLabel::Red => "red",
        ColorLabel::Yellow => "yellow",
        ColorLabel::Green => "green",
        ColorLabel::Blue => "blue",
        ColorLabel::Purple => "purple",
        ColorLabel::None => "none",
    });
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
pub(crate) fn show_in_files(images: &[Image]) {
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

fn show_add_to_album_popover(target: &impl IsA<gtk4::Widget>, images: Vec<Image>, ctx: &Context) {
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
