//! Main window following GNOME HIG and Material 3 design standards:
//!
//!   - AdwHeaderBar with WindowTitle, Import MenuButton, Refresh Button, and Primary MenuButton.
//!   - Modern navigation sidebar with rounded rows and count badges.
//!   - Virtualized justified timeline with rounded 8px photo tiles and section headers.
//!   - Cover cards with 12px rounded corners and smooth hover transitions.
//!   - Inline viewer with linked prev/next controls and bottom info card sheet.

#![allow(deprecated)]

use crate::handlers::import_handler;
use crate::menu;
use crate::ui::detail;
use crate::ui::preferences;
use crate::ui::selection_bar;
use crate::ui::share;
use crate::ui::shortcuts;
use crate::ui::sidebar;
use crate::ui::slideshow;
use crate::ui::timeline::Timeline;
use crate::ui::widgets::EventCard;
use async_channel;
use chrono::Datelike;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use gtk4::{
    Align, Box as GtkBox, Button, FileChooserAction, FileChooserNative, Label, MenuButton,
    Orientation, Paned, ProgressBar, ResponseType, Revealer, ScrolledWindow, Stack, ToggleButton,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::queries::{self, RatingFilter, TimelineFilter};
use photon_core::db::Database;
use photon_core::models::{ColorLabel, Image, Preferences, TimelineItem, UIAction};
use photon_import::ImportEngine;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone)]
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub window_title: adw::WindowTitle,
    pub timeline_container: GtkBox,
    pub viewer_container: GtkBox,
    pub db: Database,
    pub engine: Arc<ImportEngine>,
    pub status_label: Label,
    pub progress_bar: ProgressBar,
    pub progress_revealer: Revealer,
    /// Stops the running import; shown only while one runs.
    pub cancel_button: Button,
    pub cache_dir: PathBuf,
    pub prefs: Rc<RefCell<Preferences>>,
    pub nav_tx: async_channel::Sender<UIAction>,
    /// Photos of the current timeline, for the viewer's prev/next.
    pub current_photos: Rc<RefCell<Rc<Vec<TimelineItem>>>>,
    pub last_grid_action: Rc<RefCell<UIAction>>,
    stack: Stack,
    timeline: Timeline,
    /// What the timeline currently shows, and whether the library changed since.
    timeline_filter: Rc<RefCell<Option<TimelineFilter>>>,
    timeline_stale: Rc<Cell<bool>>,
    pub rating_filter: Rc<Cell<RatingFilter>>,
    pub cull_flag: Rc<Cell<Option<i32>>>,
    pub color_filter: Rc<Cell<Option<ColorLabel>>>,
    pub blur_filter: Rc<Cell<bool>>,
    /// Names the active rating/flag filter on the header bar's filter button.
    filter_label: Label,
    pub sidebar: sidebar::Sidebar,
    toasts: adw::ToastOverlay,
    pub undo_manager: Rc<RefCell<crate::ui::undo::UndoManager>>,
    pub current_album_id: Rc<RefCell<Option<i64>>>,
    pub bottom_bar: Rc<RefCell<Option<selection_bar::BottomBarHandle>>>,
    /// When the last library check (missing files, changed sidecars) started,
    /// and whether one is running.
    library_check: Rc<Cell<(Option<std::time::Instant>, bool)>>,
    /// The user said "Not Now" to downloading the face model, this session.
    face_model_declined: Rc<Cell<bool>>,
}

/// Coming back to Photon from darktable re-reads changed sidecars, at most
/// this often.
const LIBRARY_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

impl MainWindow {
    pub fn new(
        app: &impl IsA<gtk4::Application>,
        db: Database,
        engine: Arc<ImportEngine>,
        cache_dir: PathBuf,
    ) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Photon")
            .default_width(1200)
            .default_height(800)
            .build();

        let root_box = GtkBox::new(Orientation::Vertical, 0);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&root_box));
        window.set_content(Some(&toasts));

        // ── Header Bar (GNOME HIG) ──────────────────────────
        let header_bar = adw::HeaderBar::new();

        // Start: Import MenuButton + Refresh Button
        let import_menu = menu::build_import_menu();
        let import_btn = MenuButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Import Photos (Ctrl+O)")
            .menu_model(&import_menu)
            .build();
        import_btn.add_css_class("flat");
        header_bar.pack_start(&import_btn);

        let refresh_btn = Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh Library (F5)")
            .action_name("win.refresh")
            .build();
        refresh_btn.add_css_class("flat");
        header_bar.pack_start(&refresh_btn);

        // Center: WindowTitle with dynamic title and subtitle
        let window_title = adw::WindowTitle::new("Photon", "All Photos");
        header_bar.set_title_widget(Some(&window_title));

        // End: Filter Button + Primary MenuButton (Hamburger)
        let primary_menu = menu::build_primary_menu();
        let menu_btn = MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&primary_menu)
            .primary(true)
            .build();
        menu_btn.add_css_class("flat");
        header_bar.pack_end(&menu_btn);

        // Star icon + a label naming the active filter, so a filtered
        // timeline is never mistaken for the whole library.
        let filter_label = Label::new(Some("All"));
        let filter_content = GtkBox::new(Orientation::Horizontal, 6);
        filter_content.append(&gtk4::Image::from_icon_name("starred-symbolic"));
        filter_content.append(&filter_label);
        let filter_btn = MenuButton::builder()
            .child(&filter_content)
            .tooltip_text("Filter by Rating & Flag, Grid Density")
            .build();
        filter_btn.add_css_class("flat");
        header_bar.pack_end(&filter_btn);

        let select_btn = ToggleButton::builder()
            .icon_name("selection-mode-symbolic")
            .tooltip_text("Select Photos")
            .build();
        select_btn.add_css_class("flat");
        header_bar.pack_end(&select_btn);

        let search_btn = ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search Library (Ctrl+F)")
            .build();
        search_btn.add_css_class("flat");
        header_bar.pack_end(&search_btn);

        root_box.append(&header_bar);

        // ── Main Paned ──────────────────────────────────────
        let paned = Paned::new(Orientation::Horizontal);
        paned.set_position(260);
        paned.set_vexpand(true);
        paned.add_css_class("photon-main-paned");
        // Neither side may be squeezed below its minimum width (the viewer's
        // bottom bar has a wide one): the divider would move on while that
        // side overflowed under it. The sidebar keeps its width when the
        // window is resized.
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_resize_start_child(false);
        root_box.append(&paned);

        let (nav_tx, nav_rx) = async_channel::unbounded::<UIAction>();

        // ── Sidebar ─────────────────────────────────────
        let sidebar_handle = sidebar::create(db.clone(), nav_tx.clone());
        paned.set_start_child(Some(&sidebar_handle.widget));

        // ── Content area ────────────────────────────────
        let content_box = GtkBox::new(Orientation::Vertical, 0);

        let scrolled = ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_hscrollbar_policy(gtk4::PolicyType::Never);

        let timeline_container = GtkBox::new(Orientation::Vertical, 16);
        timeline_container.set_margin_top(16);
        timeline_container.set_margin_bottom(16);
        timeline_container.set_margin_start(20);
        timeline_container.set_margin_end(20);

        scrolled.set_child(Some(&timeline_container));

        let prefs = {
            let conn = db.conn().expect("DB for prefs");
            queries::load_preferences(&conn)
        };

        let timeline = Timeline::new(cache_dir.clone(), prefs.thumbnail_size as i32);
        timeline.set_db(db.clone());
        let tx = nav_tx.clone();
        timeline.connect_activate(move |index| {
            let _ = tx.send_blocking(UIAction::ViewPhoto(index));
        });

        // ── Filter Popover ──────────────────────────────
        let rating_filter = Rc::new(Cell::new(RatingFilter::Any));
        let cull_flag = Rc::new(Cell::new(None));
        let color_filter = Rc::new(Cell::new(None));
        let blur_filter = Rc::new(Cell::new(false));

        let popover = gtk4::Popover::new();
        let pop_box = GtkBox::new(Orientation::Vertical, 8);
        pop_box.set_margin_top(12);
        pop_box.set_margin_bottom(12);
        pop_box.set_margin_start(12);
        pop_box.set_margin_end(12);

        let rating_lbl = Label::new(Some("Rating Filter"));
        rating_lbl.add_css_class("caption-heading");
        rating_lbl.set_halign(Align::Start);
        pop_box.append(&rating_lbl);

        let rating_box = GtkBox::new(Orientation::Horizontal, 0);
        rating_box.add_css_class("linked");
        let ratings = [
            ("Any", RatingFilter::Any),
            ("Unrated", RatingFilter::Unrated),
            ("★ 1+", RatingFilter::AtLeast(1)),
            ("★ 2+", RatingFilter::AtLeast(2)),
            ("★ 3+", RatingFilter::AtLeast(3)),
            ("★ 4+", RatingFilter::AtLeast(4)),
            ("★ 5", RatingFilter::AtLeast(5)),
        ];
        let mut rating_btns: Vec<(ToggleButton, RatingFilter)> = Vec::new();
        for (label, val) in ratings {
            let b = ToggleButton::with_label(label);
            b.set_active(val == RatingFilter::Any);
            if let Some((first, _)) = rating_btns.first() {
                b.set_group(Some(first));
            }
            rating_box.append(&b);
            rating_btns.push((b, val));
        }
        pop_box.append(&rating_box);

        let status_lbl = Label::new(Some("Status Filter"));
        status_lbl.add_css_class("caption-heading");
        status_lbl.set_halign(Align::Start);
        pop_box.append(&status_lbl);

        let status_box = GtkBox::new(Orientation::Horizontal, 0);
        status_box.add_css_class("linked");
        let flags: [(&str, Option<i32>); 4] = [
            ("All", None),
            ("Picks", Some(1)),
            ("Unflagged", Some(0)),
            ("Rejects", Some(-1)),
        ];
        let mut flag_btns: Vec<(ToggleButton, Option<i32>)> = Vec::new();
        for (label, val) in flags {
            let b = ToggleButton::with_label(label);
            b.set_active(val.is_none());
            if let Some((first, _)) = flag_btns.first() {
                b.set_group(Some(first));
            }
            status_box.append(&b);
            flag_btns.push((b, val));
        }
        pop_box.append(&status_box);

        let color_lbl = Label::new(Some("Colour Label Filter"));
        color_lbl.add_css_class("caption-heading");
        color_lbl.set_halign(Align::Start);
        pop_box.append(&color_lbl);

        let color_box = GtkBox::new(Orientation::Horizontal, 0);
        color_box.add_css_class("linked");
        let colors: [(&str, Option<ColorLabel>); 7] = [
            ("All", None),
            ("", Some(ColorLabel::Red)),
            ("", Some(ColorLabel::Yellow)),
            ("", Some(ColorLabel::Green)),
            ("", Some(ColorLabel::Blue)),
            ("", Some(ColorLabel::Purple)),
            ("None", Some(ColorLabel::None)),
        ];
        let mut color_btns: Vec<(ToggleButton, Option<ColorLabel>)> = Vec::new();
        for (label, val) in colors {
            // Colours as dots (symbolic), "All" and "None" as words.
            let b = if label.is_empty() {
                let b = ToggleButton::new();
                b.set_child(Some(&selection_bar::label_dot(val.unwrap_or_default())));
                b
            } else {
                ToggleButton::with_label(label)
            };
            b.set_active(val.is_none());
            if let Some((first, _)) = color_btns.first() {
                b.set_group(Some(first));
            }
            if let Some(c) = val {
                b.set_tooltip_text(Some(c.display_name()));
            } else {
                b.set_tooltip_text(Some("All colour labels"));
            }
            color_box.append(&b);
            color_btns.push((b, val));
        }
        pop_box.append(&color_box);

        let quality_lbl = Label::new(Some("Quality Filter"));
        quality_lbl.add_css_class("caption-heading");
        quality_lbl.set_halign(Align::Start);
        pop_box.append(&quality_lbl);

        let blur_btn = ToggleButton::with_label("Possibly blurred");
        blur_btn.add_css_class("flat");
        pop_box.append(&blur_btn);

        let zoom_lbl = Label::new(Some("Grid Density"));
        zoom_lbl.add_css_class("caption-heading");
        zoom_lbl.set_halign(Align::Start);
        pop_box.append(&zoom_lbl);

        let zoom_box = GtkBox::new(Orientation::Horizontal, 0);
        zoom_box.add_css_class("linked");
        let zooms = [
            ("Small", 140),
            ("Medium", 220),
            ("Large", 320),
            ("Huge", 440),
        ];
        let mut zoom_btns = Vec::new();
        for (label, val) in zooms {
            let b = Button::with_label(label);
            b.add_css_class("flat");
            if val == prefs.thumbnail_size as i32 {
                b.add_css_class("suggested-action");
            }
            zoom_box.append(&b);
            zoom_btns.push((b, val));
        }
        pop_box.append(&zoom_box);

        popover.set_child(Some(&pop_box));
        filter_btn.set_popover(Some(&popover));

        let viewer_container = GtkBox::new(Orientation::Vertical, 0);
        viewer_container.set_vexpand(true);
        viewer_container.set_hexpand(true);

        // ── Search Bar (GNOME HIG) ─────────────────────────
        let search_bar = gtk4::SearchBar::new();
        let search_entry = gtk4::SearchEntry::new();
        search_entry.set_hexpand(true);
        search_entry.set_placeholder_text(Some("Search by filename, tag:name, camera:make, lens:model, is:pick..."));
        search_bar.set_child(Some(&search_entry));
        search_bar.connect_entry(&search_entry);
        search_bar.set_key_capture_widget(Some(&window));
        search_bar
            .bind_property("search-mode-enabled", &search_btn, "active")
            .bidirectional()
            .build();

        let tx_search = nav_tx.clone();
        search_entry.connect_search_changed(move |entry| {
            let text = entry.text().to_string();
            if text.trim().is_empty() {
                let _ = tx_search.send_blocking(UIAction::ShowAll);
            } else {
                let _ = tx_search.send_blocking(UIAction::Search(text));
            }
        });
        content_box.append(&search_bar);

        let stack = Stack::new();
        stack.set_vexpand(true);
        stack.add_named(&scrolled, Some("cards"));
        stack.add_named(timeline.widget(), Some("timeline"));
        stack.add_named(&viewer_container, Some("viewer"));
        content_box.append(&stack);

        // ── Status bar / In-App Notification ─────────────
        let progress_revealer = Revealer::new();
        progress_revealer.set_transition_type(gtk4::RevealerTransitionType::SlideUp);

        // The theme's stock card (its radius, colours and shadow), padded inside.
        let status_card = GtkBox::new(Orientation::Vertical, 0);
        status_card.add_css_class("card");
        status_card.set_margin_top(8);
        status_card.set_margin_bottom(12);
        status_card.set_margin_start(16);
        status_card.set_margin_end(16);
        let status_box = GtkBox::new(Orientation::Vertical, 6);
        status_box.set_margin_top(10);
        status_box.set_margin_bottom(10);
        status_box.set_margin_start(12);
        status_box.set_margin_end(12);
        status_card.append(&status_box);

        let status_label = Label::new(Some("Ready"));
        status_label.set_halign(Align::Start);
        status_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        // The label says how far along; the bar only shows it.
        let progress_bar = ProgressBar::new();

        // Stopping keeps what's done: not a destructive action (HIG).
        let cancel_button = Button::with_label("Stop Import");
        cancel_button.set_visible(false);

        let progress_row = GtkBox::new(Orientation::Horizontal, 12);
        progress_bar.set_hexpand(true);
        progress_bar.set_valign(Align::Center);
        progress_row.append(&progress_bar);
        progress_row.append(&cancel_button);

        status_box.append(&status_label);
        status_box.append(&progress_row);
        progress_revealer.set_child(Some(&status_card));
        content_box.append(&progress_revealer);

        paned.set_end_child(Some(&content_box));

        let mw = Self {
            window,
            window_title,
            timeline_container,
            viewer_container,
            db,
            engine,
            status_label,
            progress_bar,
            progress_revealer,
            cancel_button,
            cache_dir,
            prefs: Rc::new(RefCell::new(prefs)),
            nav_tx: nav_tx.clone(),
            current_photos: Rc::new(RefCell::new(Rc::new(Vec::new()))),
            last_grid_action: Rc::new(RefCell::new(UIAction::ShowAll)),
            stack,
            timeline,
            timeline_filter: Rc::new(RefCell::new(None)),
            timeline_stale: Rc::new(Cell::new(true)),
            library_check: Rc::new(Cell::new((None, false))),
            face_model_declined: Rc::new(Cell::new(false)),
            rating_filter,
            cull_flag,
            color_filter,
            blur_filter,
            filter_label,
            sidebar: sidebar_handle,
            toasts,
            undo_manager: Rc::new(RefCell::new(crate::ui::undo::UndoManager::new())),
            current_album_id: Rc::new(RefCell::new(None)),
            bottom_bar: Rc::new(RefCell::new(None)),
        };

        let mw_t_ref = mw.clone();
        mw.timeline.connect_refresh(move || mw_t_ref.refresh());
        let mw_t_ss = mw.clone();
        mw.timeline.connect_start_slideshow(move || mw_t_ss.start_slideshow());
        mw.timeline.set_undo_manager(mw.undo_manager.clone());
        let toasts = mw.toasts.clone();
        mw.timeline.set_notify(move |text| toasts.add_toast(adw::Toast::new(text)));

        let mw_b = mw.clone();
        blur_btn.connect_toggled(move |b| {
            mw_b.blur_filter.set(b.is_active());
            mw_b.filters_changed();
        });

        for (b, val) in rating_btns {
            let mw_f = mw.clone();
            b.connect_toggled(move |b| {
                if b.is_active() {
                    mw_f.rating_filter.set(val);
                    mw_f.filters_changed();
                }
            });
        }

        for (b, val) in flag_btns {
            let mw_f = mw.clone();
            b.connect_toggled(move |b| {
                if b.is_active() {
                    mw_f.cull_flag.set(val);
                    mw_f.filters_changed();
                }
            });
        }

        for (b, val) in color_btns {
            let mw_f = mw.clone();
            b.connect_toggled(move |b| {
                if b.is_active() {
                    mw_f.color_filter.set(val);
                    mw_f.filters_changed();
                }
            });
        }

        let mw_f = mw.clone();
        for (b, val) in &zoom_btns {
            let b_clone = b.clone();
            let all_b: Vec<_> = zoom_btns.iter().map(|(btn, _)| btn.clone()).collect();
            let mw_f = mw_f.clone();
            let val = *val;
            b.connect_clicked(move |_| {
                for other in &all_b {
                    other.remove_css_class("suggested-action");
                }
                b_clone.add_css_class("suggested-action");
                mw_f.timeline.set_row_height(val);
                mw_f.set_bottom_bar_zoom(val);
            });
        }

        // ── Navigation handler ──────────────────────────
        let mw_nav = mw.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(action) = nav_rx.recv().await {
                mw_nav.navigate(&action);
            }
        });

        // ── Window Actions ──────────────────────────────
        let mw_r = mw.clone();
        let act_refresh = gio::SimpleAction::new("refresh", None);
        act_refresh.connect_activate(move |_, _| mw_r.refresh());
        mw.window.add_action(&act_refresh);

        let mw_u = mw.clone();
        let act_undo = gio::SimpleAction::new("undo", None);
        act_undo.connect_activate(move |_, _| mw_u.undo());
        mw.window.add_action(&act_undo);

        let mw_redo = mw.clone();
        let act_redo = gio::SimpleAction::new("redo", None);
        act_redo.connect_activate(move |_, _| mw_redo.redo());
        mw.window.add_action(&act_redo);

        let mw_ss = mw.clone();
        let act_slideshow = gio::SimpleAction::new("slideshow", None);
        act_slideshow.connect_activate(move |_, _| mw_ss.start_slideshow());
        mw.window.add_action(&act_slideshow);

        let mw_i = mw.clone();
        let act_import = gio::SimpleAction::new("import_folder", None);
        act_import.connect_activate(move |_, _| import_handler::show_folder_import_dialog(&mw_i));
        mw.window.add_action(&act_import);

        let mw_s = mw.clone();
        let act_shotwell = gio::SimpleAction::new("import_shotwell", None);
        act_shotwell
            .connect_activate(move |_, _| import_handler::show_shotwell_import_dialog(&mw_s));
        mw.window.add_action(&act_shotwell);

        let mw_cam = mw.clone();
        let act_camera = gio::SimpleAction::new("import_camera", None);
        act_camera
            .connect_activate(move |_, _| import_handler::show_camera_import_dialog(&mw_cam));
        mw.window.add_action(&act_camera);

        let mw_dk = mw.clone();
        let act_digikam = gio::SimpleAction::new("import_digikam", None);
        act_digikam.connect_activate(move |_, _| import_handler::show_digikam_import_dialog(&mw_dk));
        mw.window.add_action(&act_digikam);

        let win_keys = mw.window.clone();
        let act_shortcuts = gio::SimpleAction::new("shortcuts", None);
        act_shortcuts.connect_activate(move |_, _| shortcuts::show(&win_keys));
        mw.window.add_action(&act_shortcuts);

        let mw_loc = mw.clone();
        let act_locate = gio::SimpleAction::new("locate_folder", None);
        act_locate.connect_activate(move |_, _| mw_loc.locate_missing_folder());
        mw.window.add_action(&act_locate);


        let mw_aq = mw.clone();
        let act_analyse_quality = gio::SimpleAction::new("analyse_quality", None);
        act_analyse_quality.connect_activate(move |_, _| mw_aq.analyse_quality());
        mw.window.add_action(&act_analyse_quality);

        // ── Preferences ─────────────────────────────────
        let mw_p = mw.clone();
        let act_prefs = gio::SimpleAction::new("preferences", None);
        act_prefs.connect_activate(move |_, _| {
            let mw2 = mw_p.clone();
            preferences::show(&mw_p.window, &mw_p.db, move |p| {
                mw2.timeline.set_row_height(p.thumbnail_size as i32);
                *mw2.prefs.borrow_mut() = p;
                mw2.refresh();
            });
        });
        mw.window.add_action(&act_prefs);

        // ── About Dialog ────────────────────────────────
        let win_about = mw.window.clone();
        let act_about = gio::SimpleAction::new("about", None);
        act_about.connect_activate(move |_, _| {
            let about = adw::AboutWindow::builder()
                .transient_for(&win_about)
                .modal(true)
                .application_name("Photon")
                .application_icon("org.mavensgroup.photon")
                .developer_name("Photon Team")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Fast photo manager for Linux following GNOME HIG and Material 3 design")
                .license_type(gtk4::License::Gpl30)
                .website("https://github.com/photon-app/photon")
                .issue_url("https://github.com/photon-app/photon/issues")
                .build();
            about.present();
        });
        mw.window.add_action(&act_about);
        // ── Search Action ──────────────────────────────
        let sbtn_act = search_btn.clone();
        let act_search = gio::SimpleAction::new("search", None);
        act_search.connect_activate(move |_, _| {
            sbtn_act.set_active(!sbtn_act.is_active());
        });
        mw.window.add_action(&act_search);

        // ── Selection mode ─────────────────────────────
        let tl = mw.timeline.clone();
        select_btn.connect_toggled(move |b| tl.set_selection_mode(b.is_active()));
        let sb = select_btn.clone();
        mw.timeline.connect_selection_mode_changed(move |on| sb.set_active(on));
        // Selecting only makes sense on the timeline.
        let sb = select_btn.clone();
        let mw_vis = mw.clone();
        mw.stack.connect_visible_child_name_notify(move |stack| {
            let on_timeline = stack.visible_child_name().as_deref() == Some("timeline");
            let is_viewer = stack.visible_child_name().as_deref() == Some("viewer");
            if !on_timeline {
                sb.set_active(false);
            }
            sb.set_sensitive(on_timeline);
            mw_vis.set_bottom_bar_revealed(!is_viewer);
            // Slideshows (F5) start from the library, not from the 1-up view.
            if let Some(action) = mw_vis.window.lookup_action("slideshow").and_downcast::<gio::SimpleAction>() {
                action.set_enabled(!is_viewer);
            }
        });

        // Dragged-out photos follow the "Share Sends" preference too.
        let share_ctx = mw.share_context();
        mw.timeline.set_drag_filter(move |images| share::versions_to_share(&share_ctx, images));

        let mw_changed = mw.clone();
        let mw_export = mw.clone();
        let mw_ss_sel = mw.clone();
        let bottom_bar_handle = selection_bar::attach(
            &mw.timeline,
            selection_bar::Context {
                window: mw.window.clone().upcast(),
                db: mw.db.clone(),
                prefs: mw.prefs.clone(),
                on_library_changed: Rc::new(move || mw_changed.refresh()),
                on_export: Rc::new(move |images| mw_export.start_export(images)),
                share: mw.share_context(),
                undo_manager: mw.undo_manager.clone(),
                current_album_id: mw.current_album_id.clone(),
                on_start_slideshow: Some(Rc::new(move || mw_ss_sel.start_slideshow())),
                on_compare: {
                    let mw = mw.clone();
                    Rc::new(move |ids| mw.show_compare(ids))
                },
            },
        );
        content_box.append(&bottom_bar_handle.widget);
        bottom_bar_handle.widget.set_revealed(true);
        *mw.bottom_bar.borrow_mut() = Some(bottom_bar_handle);

        // Missing files and sidecars changed in other tools: at startup, and
        // whenever the window is focused again (e.g. after rating in darktable).
        mw.check_library();
        let mw_focus = mw.clone();
        mw.window.connect_is_active_notify(move |w| {
            if w.is_active() {
                mw_focus.check_library();
            }
        });

        mw.navigate(&UIAction::ShowAll);
        mw
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// In the background: mark photos whose files are missing (or back), and
    /// read sidecars changed since Photon last read or wrote them. Refreshes
    /// the view if anything changed. Throttled; never two at once.
    fn check_library(&self) {
        let (last, running) = self.library_check.get();
        if running || last.is_some_and(|t| t.elapsed() < LIBRARY_CHECK_INTERVAL) {
            return;
        }
        self.library_check.set((Some(std::time::Instant::now()), true));

        let db_bg = self.db.clone();
        let (tx_bg, rx_bg) = async_channel::bounded::<bool>(1);
        std::thread::spawn(move || {
            let changed = (|| {
                let mut conn = db_bg.conn().ok()?;
                let images_to_check = queries::get_all_images_for_integrity_check(&conn).ok()?;

                let mut to_mark_missing = Vec::new();
                let mut to_mark_found = Vec::new();
                let mut changed = false;

                for (id, path, xmp_mtime, was_missing) in images_to_check {
                    let exists = path.exists();
                    if !exists && !was_missing {
                        to_mark_missing.push(id);
                    } else if exists && was_missing {
                        to_mark_found.push(id);
                    }

                    if exists {
                        match photon_import::read_image_xmp(&mut conn, id, &path, xmp_mtime) {
                            Ok(applied) => changed |= applied,
                            Err(e) => log::warn!("Reading XMP for {}: {e}", path.display()),
                        }
                    }
                }

                // One rating per shot: repair shots whose files disagree.
                match photon_import::reconcile_shots(&mut conn) {
                    Ok(0) => {}
                    Ok(n) => {
                        log::info!("Made the files of {n} shots agree on rating and pick/reject");
                        changed = true;
                    }
                    Err(e) => log::warn!("Reconciling shots: {e:#}"),
                }

                for (ids, missing) in [(&to_mark_missing, true), (&to_mark_found, false)] {
                    if ids.is_empty() {
                        continue;
                    }
                    match queries::mark_missing(&conn, ids, missing) {
                        Ok(()) => changed = true,
                        Err(e) => log::warn!("Marking {} photos missing={missing}: {e}", ids.len()),
                    }
                }
                Some(changed)
            })();
            let _ = tx_bg.send_blocking(changed.unwrap_or(false));
        });

        let mw = self.clone();
        gtk4::glib::MainContext::default().spawn_local(async move {
            let changed = rx_bg.recv().await.unwrap_or(false);
            let (started, _) = mw.library_check.get();
            mw.library_check.set((started, false));
            if changed {
                mw.refresh();
            }
        });
    }

    // ── Navigation ──────────────────────────────────────

    pub fn navigate(&self, action: &UIAction) {
        if !matches!(action, UIAction::ViewPhoto(_)) {
            *self.last_grid_action.borrow_mut() = action.clone();
            if !matches!(action, UIAction::FilterByAlbum(_)) {
                *self.current_album_id.borrow_mut() = None;
            }
        }
        match action {
            UIAction::ShowAll => {
                self.show_timeline(TimelineFilter::All, "All Photos".to_string());
            }
            UIAction::FilterByDay(y, m, d) => {
                let title = format!("{}, {} {}, {}", day_of_week(*y, *m, *d), month_name(*m), d, y);
                self.show_timeline(TimelineFilter::Day(*y, *m, *d), title);
            }
            UIAction::Search(text) => {
                self.show_timeline(TimelineFilter::Search(text.clone()), format!("\"{}\"", text));
            }
            UIAction::FilterByYear(y) => {
                self.show_cards();
                self.show_year(*y);
            }
            UIAction::FilterByDate(y, m) => {
                self.show_cards();
                self.show_month(*y, *m);
            }
            UIAction::ViewPhoto(idx) => {
                self.show_viewer(*idx);
            }
            UIAction::FilterByTag(tag) => {
                self.show_timeline(TimelineFilter::Tag(tag.clone()), format!("Tag: #{tag}"));
            }
            UIAction::FilterByAlbum(album_id) => {
                *self.current_album_id.borrow_mut() = Some(*album_id);
                let album_name = if let Ok(conn) = self.db.conn() {
                    queries::get_album(&conn, *album_id)
                        .ok()
                        .flatten()
                        .map(|a| a.name)
                        .unwrap_or_else(|| "Album".to_string())
                } else {
                    "Album".to_string()
                };
                self.show_timeline(TimelineFilter::Album(*album_id), format!("Album: {album_name}"));
            }
            UIAction::FilterByEvent(event_id) => {
                let event_name = if let Ok(conn) = self.db.conn() {
                    queries::get_event(&conn, *event_id)
                        .ok()
                        .flatten()
                        .map(|e| e.name)
                        .unwrap_or_else(|| "Event".to_string())
                } else {
                    "Event".to_string()
                };
                self.show_timeline(TimelineFilter::Event(*event_id), format!("Event: {event_name}"));
            }
            UIAction::FilterMissing => {
                self.show_timeline(TimelineFilter::Missing, "Missing Photos".to_string());
            }
            UIAction::FilterBySmartCollection(coll_id) => {
                let name = if let Ok(conn) = self.db.conn() {
                    queries::get_smart_collection(&conn, *coll_id)
                        .ok()
                        .flatten()
                        .map(|c| c.name)
                        .unwrap_or_else(|| "Smart Collection".to_string())
                } else {
                    "Smart Collection".to_string()
                };
                self.show_timeline(TimelineFilter::SmartCollection(*coll_id), format!("Smart Collection: {name}"));
            }
        }
    }

    /// The library changed (import, thumbnails, preferences). Update what is
    /// on screen without losing the scroll position; a viewer that is open
    /// stays open, and the timeline catches up when the user returns.
    pub fn refresh(&self) {
        self.timeline_stale.set(true);
        self.sidebar.refresh_events();
        self.sidebar.refresh_tags();
        self.sidebar.refresh_albums();
        self.sidebar.refresh_smart_collections();
        self.sidebar.refresh_missing();
        let action = self.last_grid_action.borrow().clone();
        let showing_viewer = self.stack.visible_child_name().as_deref() == Some("viewer");
        if !showing_viewer {
            self.navigate(&action);
        }
    }

    pub fn locate_missing_folder(&self) {
        let parent_win = self.window.clone();
        let db = self.db.clone();
        let mw = self.clone();

        let chooser = FileChooserNative::new(
            Some("Locate Folder for Missing Photos"),
            Some(&parent_win),
            FileChooserAction::SelectFolder,
            Some("Select Folder"),
            Some("Cancel"),
        );

        chooser.connect_response(move |dialog, response| {
            if response == ResponseType::Accept {
                if let Some(file) = dialog.file() {
                    if let Some(path) = file.path() {
                        let db_c = db.clone();
                        let mw_c = mw.clone();
                        glib::spawn_future_local(async move {
                            let relinked = gio::spawn_blocking(move || {
                                if let Ok(conn) = db_c.conn() {
                                    if let Ok(missing) = queries::get_missing_images(&conn) {
                                        return photon_import::library::relink_missing_folder(&conn, &missing, &path).unwrap_or(0);
                                    }
                                }
                                0
                            }).await.unwrap_or(0);

                            mw_c.refresh();
                            let toast = adw::Toast::new(&format!("Successfully relinked {relinked} photos"));
                            mw_c.toasts.add_toast(toast);
                        });
                    }
                }
            }
        });

        chooser.show();
    }

    /// For Share menus: the window, and in-app notifications.
    pub fn share_context(&self) -> share::Context {
        let toasts = self.toasts.clone();
        share::Context {
            window: self.window.clone().upcast(),
            notify: Rc::new(move |text| toasts.add_toast(adw::Toast::new(text))),
            db: self.db.clone(),
            prefs: self.prefs.clone(),
        }
    }

    pub fn update_bottom_bar_status(&self, text: &str) {
        if let Some(ref handle) = *self.bottom_bar.borrow() {
            (handle.set_status_text)(text);
        }
    }

    pub fn set_bottom_bar_revealed(&self, revealed: bool) {
        if let Some(ref handle) = *self.bottom_bar.borrow() {
            handle.widget.set_revealed(revealed);
        }
    }

    pub fn set_bottom_bar_zoom(&self, val: i32) {
        if let Some(ref handle) = *self.bottom_bar.borrow() {
            (handle.set_zoom_value)(val);
        }
    }

    /// The rating, flag, color or blur filter changed: relabel the filter button and
    /// reload the timeline.
    fn filters_changed(&self) {
        let rating = match self.rating_filter.get() {
            RatingFilter::Any => None,
            RatingFilter::Unrated => Some("Unrated".to_string()),
            RatingFilter::AtLeast(5) => Some("5".to_string()),
            RatingFilter::AtLeast(n) => Some(format!("{n}+")),
        };
        let flag = match self.cull_flag.get() {
            None => None,
            Some(1) => Some("Picks".to_string()),
            Some(-1) => Some("Rejects".to_string()),
            Some(_) => Some("Unflagged".to_string()),
        };
        let color = match self.color_filter.get() {
            None => None,
            Some(c) => Some(c.display_name().to_string()),
        };
        let blur = if self.blur_filter.get() { Some("Blurred".to_string()) } else { None };

        let mut parts = Vec::new();
        if let Some(r) = rating { parts.push(r); }
        if let Some(f) = flag { parts.push(f); }
        if let Some(c) = color { parts.push(c); }
        if let Some(b) = blur { parts.push(b); }

        let text = if parts.is_empty() { "All".to_string() } else { parts.join(" · ") };
        self.filter_label.set_text(&text);
        if text == "All" {
            self.filter_label.remove_css_class("accent");
        } else {
            self.filter_label.add_css_class("accent");
        }
        self.timeline_stale.set(true);
        self.refresh();
    }

    fn show_cards(&self) {
        self.clear_viewer();
        self.clear_timeline();
        self.stack.set_visible_child_name("cards");
        self.set_bottom_bar_revealed(true);
    }

    /// Show `filter` in the virtualized timeline. Reuses the loaded timeline
    /// (and its scroll position) when nothing changed.
    fn show_timeline(&self, filter: TimelineFilter, title: String) {
        let effective_filter = if self.blur_filter.get() {
            // The same threshold as Analyse Photo Quality uses outside bursts.
            let median = self
                .db
                .conn()
                .ok()
                .and_then(|c| queries::library_sharpness_median(&c, photon_import::QUALITY_VERSION).ok().flatten());
            TimelineFilter::Blurred(median.map_or(photon_import::quality::BLUR_SHARPNESS_FLOOR, |m| {
                m * photon_import::quality::BLUR_RATIO_OF_LIBRARY_MEDIAN
            }))
        } else {
            filter.clone()
        };

        let same = self.timeline_filter.borrow().as_ref() == Some(&effective_filter);
        if !same || self.timeline_stale.get() {
            let started = std::time::Instant::now();
            let items = match self.db.conn() {
                Ok(conn) => {
                    queries::timeline_items_with_filters(
                        &conn,
                        &effective_filter,
                        self.rating_filter.get(),
                        self.cull_flag.get(),
                        self.color_filter.get(),
                    ).unwrap_or_else(|e| {
                        log::error!("Timeline query failed: {e}");
                        Vec::new()
                    })
                }
                Err(e) => return self.show_error(&e.to_string()),
            };
            let items = Rc::new(items);
            *self.current_photos.borrow_mut() = items.clone();
            let count = items.len();
            self.timeline.set_items(items, true, same);
            log::debug!("Timeline: {count} photos loaded and laid out in {:?}", started.elapsed());
            *self.timeline_filter.borrow_mut() = Some(effective_filter);
            self.timeline_stale.set(false);
        }

        let count = self.current_photos.borrow().len();
        let count_str = if count == 1 { "1 photo" } else { &format!("{count} photos") };
        let is_filtered = self.rating_filter.get() != RatingFilter::Any
            || self.cull_flag.get().is_some()
            || self.color_filter.get().is_some()
            || self.blur_filter.get();
        let filter_tag = if is_filtered { " · Filtered" } else { "" };
        let subtitle = format!("{title} · {count_str}{filter_tag}");
        self.window_title.set_subtitle(&subtitle);
        self.status_label.set_text(&subtitle);
        self.update_bottom_bar_status(&subtitle);
        self.set_bottom_bar_revealed(true);

        if count == 0 {
            self.show_cards();
            let msg = match self.last_grid_action.borrow().clone() {
                UIAction::ShowAll => "No photos yet. Import some!".to_string(),
                UIAction::Search(t) => format!("No results for \"{t}\""),
                _ => format!("No photos on {title}"),
            };
            return self.show_empty(&msg);
        }
        self.clear_viewer();
        self.clear_timeline(); // drop a viewer we are returning from
        self.stack.set_visible_child_name("timeline");
    }

    // ── Views ───────────────────────────────────────────

    /// Year view: cover cards per month (Shotwell-style).
    /// Clicking a cover card drills into that month.
    fn show_year(&self, year: i32) {
        let conn = match self.db.conn() {
            Ok(c) => c,
            Err(e) => return self.show_error(&e.to_string()),
        };
        let covers = queries::get_month_covers(&conn, year).unwrap_or_default();
        let subtitle = format!("{} · {} months", year, covers.len());
        self.window_title.set_subtitle(&subtitle);
        self.status_label.set_text(&subtitle);

        if covers.is_empty() {
            return self.show_empty(&format!("No photos in {}", year));
        }

        self.append_header(&year.to_string());
        let fb = self.make_grid();
        self.timeline_container.append(&fb);

        let prefs = self.prefs.borrow();
        for (month, count, cover) in &covers {
            let card = EventCard::create(
                &format!("{} {}", month_name(*month), year),
                &format!("{} Photos", count),
                cover,
                &self.cache_dir,
                &prefs,
                UIAction::FilterByDate(year, *month),
                &self.nav_tx,
            );
            Self::append_card(&fb, card, prefs.thumbnail_size);
        }
    }

    /// Month view: cover cards per day (Shotwell-style).
    /// Clicking a cover card drills into that specific day.
    fn show_month(&self, year: i32, month: u32) {
        let conn = match self.db.conn() {
            Ok(c) => c,
            Err(e) => return self.show_error(&e.to_string()),
        };
        let covers = queries::get_day_covers(&conn, year, month).unwrap_or_default();
        let subtitle = format!("{} {} · {} days", month_name(month), year, covers.len());
        self.window_title.set_subtitle(&subtitle);
        self.status_label.set_text(&subtitle);

        if covers.is_empty() {
            return self.show_empty(&format!("No photos in {} {}", month_name(month), year));
        }

        self.append_header(&format!("{} {}", month_name(month), year));
        let fb = self.make_grid();
        self.timeline_container.append(&fb);

        let prefs = self.prefs.borrow();
        for (day, count, cover) in &covers {
            let day_label = format!(
                "{}, {} {}",
                day_of_week(year, month, *day),
                month_name(month),
                day
            );
            let card = EventCard::create(
                &day_label,
                &format!("{} Photos", count),
                cover,
                &self.cache_dir,
                &prefs,
                UIAction::FilterByDay(year, month, *day),
                &self.nav_tx,
            );
            Self::append_card(&fb, card, prefs.thumbnail_size);
        }
    }

    // ── Inline Viewer ───────────────────────────────────

    fn show_viewer(&self, index: usize) {
        let photos = self.current_photos.borrow().clone();
        self.open_viewer(photos, index);
    }

    /// The selected photos compared: 2 side by side (Compare), 3–9 in a
    /// grid (Survey).
    pub fn show_compare(&self, ids: Vec<i64>) {
        let all = self.current_photos.borrow().clone();
        let mut positions: Vec<usize> = ids.iter().filter_map(|id| all.iter().position(|p| p.id == *id)).collect();
        positions.sort_unstable();
        let back_tx = self.nav_tx.clone();
        let back_action = self.last_grid_action.borrow().clone();
        let ctx = crate::ui::compare::ViewContext {
            db: self.db.clone(),
            cache_dir: self.cache_dir.clone(),
            notify: self.share_context().notify,
            undo: Some(self.undo_manager.clone()),
            on_back: Rc::new(move || {
                let _ = back_tx.send_blocking(back_action.clone());
            }),
        };
        let view = match positions.as_slice() {
            &[a, b] => crate::ui::compare::build_compare(ctx, all, a, b),
            p if (3..=9).contains(&p.len()) => {
                let items: Vec<TimelineItem> = p.iter().map(|&i| all[i].clone()).collect();
                let mw = self.clone();
                let on_open = Rc::new(move |id: i64| {
                    let index = mw.current_photos.borrow().iter().position(|p| p.id == id);
                    if let Some(index) = index {
                        mw.show_viewer(index);
                    }
                });
                crate::ui::survey::build_survey(ctx, items, on_open)
            }
            _ => return self.toast("Select 2 photos to compare, or 3–9 to survey"),
        };
        self.clear_viewer();
        self.viewer_container.append(&view);
        self.stack.set_visible_child_name("viewer");
        self.set_bottom_bar_revealed(false);
    }

    fn open_viewer(&self, photos: Rc<Vec<TimelineItem>>, index: usize) {
        if index >= photos.len() {
            return;
        }

        let back_action = self.last_grid_action.borrow().clone();
        let prefs = self.prefs.borrow();

        self.clear_viewer();
        let this = self.clone();
        let on_export = Some(Rc::new(move |images| this.start_export(images)) as Rc<dyn Fn(Vec<Image>)>);
        let viewer = detail::build_viewer(
            photos,
            index,
            &self.cache_dir,
            &prefs,
            &self.nav_tx,
            back_action,
            &self.db,
            on_export,
            self.share_context(),
            Some(self.undo_manager.clone()),
            Some(self.window.clone().upcast()),
        );
        self.viewer_container.append(&viewer);
        self.stack.set_visible_child_name("viewer");
        self.set_bottom_bar_revealed(false);
    }

    /// Batch export dialog and progress reporting for photos.
    pub fn start_export(&self, images: Vec<Image>) {
        if images.is_empty() {
            return;
        }
        let status = self.status_label.clone();
        let pbar = self.progress_bar.clone();
        let revealer = self.progress_revealer.clone();

        let s1 = status.clone();
        let p1 = pbar.clone();
        let r1 = revealer.clone();
        let on_start = move |total: usize| {
            s1.set_text(&format!("Exporting {total} photos..."));
            p1.set_fraction(0.0);
            p1.set_text(Some(&format!("0/{total}")));
            r1.set_reveal_child(true);
        };

        let s2 = status.clone();
        let p2 = pbar.clone();
        let on_progress = move |done: usize, total: usize| {
            let s = s2.clone();
            let p = p2.clone();
            glib::idle_add_local_once(move || {
                s.set_text(&format!("Exporting photo {done} of {total}..."));
                p.set_fraction(done as f64 / total.max(1) as f64);
                p.set_text(Some(&format!("{done}/{total}")));
            });
        };

        let s3 = status.clone();
        let p3 = pbar.clone();
        let r3 = revealer.clone();
        let win_alert = self.window.clone();
        let on_done = move |report: photon_import::export::ExportReport| {
            s3.set_text(&format!(
                "Export complete: {} of {} exported ({} failed)",
                report.exported,
                report.total,
                report.failed
            ));
            p3.set_fraction(1.0);
            p3.set_text(Some("Complete"));
            let r = r3.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                r.set_reveal_child(false);
            });

            if report.failed > 0 || !report.errors.is_empty() {
                let mut body = String::new();
                for (file, err) in report.errors.iter().take(12) {
                    body.push_str(&format!("• {file}: {err}\n"));
                }
                if report.errors.len() > 12 {
                    body.push_str(&format!("... and {} more notices\n", report.errors.len() - 12));
                }
                let dialog = adw::MessageDialog::new(
                    Some(&win_alert),
                    Some("Export Notices / Errors"),
                    Some(&body),
                );
                dialog.add_response("ok", "OK");
                dialog.present();
            }
        };

        crate::ui::export_dialog::show(
            &self.window,
            self.db.clone(),
            images,
            on_start,
            on_progress,
            on_done,
        );
    }

    // ── Helpers ─────────────────────────────────────────

    fn clear_viewer(&self) {
        while let Some(c) = self.viewer_container.first_child() {
            self.viewer_container.remove(&c);
        }
    }

    fn clear_timeline(&self) {
        while let Some(c) = self.timeline_container.first_child() {
            self.timeline_container.remove(&c);
        }
    }

    fn show_empty(&self, msg: &str) {
        let lbl = Label::new(Some(msg));
        lbl.set_css_classes(&["dim-label"]);
        lbl.set_margin_top(40);
        self.timeline_container.append(&lbl);
    }

    fn show_error(&self, msg: &str) {
        self.status_label.set_text(&format!("Error: {}", msg));
    }

    fn append_header(&self, text: &str) {
        let label = Label::new(None);
        label.set_halign(Align::Start);
        label.set_markup(&format!(
            "<span size='large' weight='bold'>{}</span>",
            glib::markup_escape_text(text)
        ));
        label.set_margin_bottom(6);
        label.set_margin_top(12);
        self.timeline_container.append(&label);
    }

    /// Create a wrapping grid container.
    fn make_grid(&self) -> GtkBox {
        let grid = GtkBox::new(Orientation::Vertical, 12);
        grid.set_halign(Align::Fill);
        grid.set_hexpand(true);
        grid
    }

    /// Append a card to the grid. Manages row wrapping based on available width.
    fn append_card(grid: &GtkBox, card: gtk4::Widget, thumb_size: u32) {
        let card_w = thumb_size as i32;
        let spacing = 12i32;

        let last_row = grid.last_child().and_then(|w| w.downcast::<GtkBox>().ok());

        let (row, child_count) = if let Some(ref row) = last_row {
            let mut count = 0i32;
            let mut c = row.first_child();
            while let Some(child) = c {
                count += 1;
                c = child.next_sibling();
            }
            (row.clone(), count)
        } else {
            let row = GtkBox::new(Orientation::Horizontal, spacing);
            row.set_halign(Align::Start);
            grid.append(&row);
            (row, 0)
        };

        let grid_width = {
            let w = grid.allocated_width();
            if w > 100 {
                w
            } else {
                900
            }
        };
        let max_per_row = ((grid_width + spacing) / (card_w + spacing)).max(1);

        if child_count >= max_per_row {
            let new_row = GtkBox::new(Orientation::Horizontal, spacing);
            new_row.set_halign(Align::Start);
            grid.append(&new_row);
            new_row.append(&card);
        } else {
            row.append(&card);
        }
    }

    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    pub fn undo(&self) {
        match self.undo_manager.borrow_mut().undo(&self.db, &self.cache_dir) {
            Ok(Some(desc)) => {
                self.toast(&desc);
                self.refresh();
            }
            Ok(None) => {
                self.toast("Nothing to undo");
            }
            Err(e) => {
                log::error!("Undo failed: {e}");
                self.toast(&format!("Undo failed: {e}"));
                // A partial trash restore may already have changed the library.
                self.refresh();
            }
        }
    }

    pub fn redo(&self) {
        match self.undo_manager.borrow_mut().redo(&self.db, &self.cache_dir) {
            Ok(Some(desc)) => {
                self.toast(&desc);
                self.refresh();
            }
            Ok(None) => {
                self.toast("Nothing to redo");
            }
            Err(e) => {
                log::error!("Redo failed: {e}");
                self.toast(&format!("Redo failed: {e}"));
                self.refresh();
            }
        }
    }

    pub fn start_slideshow(&self) {
        let selected = self.timeline.selected_items();
        let items = if !selected.is_empty() {
            selected
        } else {
            (**self.current_photos.borrow()).clone()
        };

        if items.is_empty() {
            self.toast("No photos to display in slideshow");
            return;
        }

        slideshow::start(
            &self.window,
            items,
            self.cache_dir.clone(),
        );
    }

    /// The photos a quality tool works on: the selection, else the photos
    /// of the current view (a day, event, album, tag, search). The year and
    /// month overviews list no photos: then `None`, the whole library.
    /// Also a phrase naming it, for messages.
    fn quality_scope(&self) -> (Option<Vec<i64>>, &'static str) {
        let selected = self.timeline.selected_items();
        if !selected.is_empty() {
            return (Some(selected.iter().map(|p| p.id).collect()), "the selected photos");
        }
        if self.stack.visible_child_name().as_deref() == Some("cards") {
            return (None, "the library");
        }
        (Some(self.current_photos.borrow().iter().map(|p| p.id).collect()), "this view")
    }

    /// Score what isn't scored yet in the selection or view, then show the
    /// photos that look blurred or badly exposed.
    fn run_quality_check(&self) {
        let (scope, what) = self.quality_scope();
        let ids = match scope {
            Some(ids) => ids,
            None => match self.db.conn().map_err(|e| e.to_string()).and_then(|c| {
                queries::get_all_images(&c, i32::MAX, 0).map_err(|e| e.to_string())
            }) {
                Ok(all) => all.iter().filter_map(|i| i.id).collect(),
                Err(e) => return self.show_error(&e),
            },
        };
        if ids.is_empty() {
            self.toast("No photos in this view");
            return;
        }
        let mw = self.clone();
        let ids_after = ids.clone();
        self.analyse_quality_in(Some(ids), what, false, Some(Box::new(move || mw.open_quality_results(ids_after, what))));
    }

    /// Score `ids` again from scratch, then show the results again.
    fn reanalyse_quality(&self, ids: Vec<i64>, what: &'static str) {
        let mw = self.clone();
        let ids_after = ids.clone();
        self.analyse_quality_in(Some(ids), what, true, Some(Box::new(move || mw.open_quality_results(ids_after, what))));
    }

    fn open_quality_results(&self, target_ids: Vec<i64>, what: &'static str) {
        let conn = match self.db.conn() {
            Ok(c) => c,
            Err(e) => return self.show_error(&e.to_string()),
        };
        let full_images: Vec<Image> =
            target_ids.iter().filter_map(|&id| queries::get_image(&conn, id).ok().flatten()).collect();
        let scores = queries::get_image_quality_batch(&conn, &target_ids).unwrap_or_default();
        let library = photon_import::quality::library_reference(&conn);
        drop(conn);

        let mw = self.clone();
        let (mw_again, ids_again) = (self.clone(), target_ids.clone());
        crate::ui::rejects_dialog::show(
            &self.window,
            full_images,
            scores,
            library,
            self.cache_dir.clone(),
            move |reject_ids| mw.apply_rejects(reject_ids),
            move || mw_again.reanalyse_quality(ids_again.clone(), what),
        );
    }

    /// Reject the photos the user confirmed in the Photo Quality results: the same DB
    /// write and XMP sync as culling by hand, off the UI thread. Undoable once
    /// saved; sidecars that couldn't be written are reported.
    pub fn apply_rejects(&self, reject_ids: Vec<i64>) {
        if reject_ids.is_empty() {
            return;
        }
        let db = self.db.clone();
        let mw = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || -> Result<(Vec<(i64, i32)>, usize), String> {
                let mut conn = db.conn().map_err(|e| e.to_string())?;
                let mut previous = Vec::with_capacity(reject_ids.len());
                for &id in &reject_ids {
                    if let Some(img) = queries::get_image(&conn, id).map_err(|e| e.to_string())? {
                        previous.push((id, img.flagged));
                    }
                }
                let ids: Vec<i64> = previous.iter().map(|&(id, _)| id).collect();
                queries::batch_set_flag(&mut conn, &ids, -1).map_err(|e| e.to_string())?;
                let xmp_failed = crate::ui::timeline::sync_cull_to_xmp(&conn, &ids, crate::ui::timeline::XmpFields::CULL);
                Ok((previous, xmp_failed))
            })
            .await
            .unwrap_or_else(|_| Err("the worker thread panicked".into()));

            match result {
                Ok((previous, xmp_failed)) => {
                    let count = previous.len();
                    mw.undo_manager.borrow_mut().push(crate::ui::undo::UndoAction::Flag { previous, new_flag: -1 });
                    mw.timeline_stale.set(true);
                    mw.refresh();
                    if xmp_failed > 0 {
                        mw.toast(&format!(
                            "Rejected {count} photos, but {xmp_failed} XMP sidecars couldn't be updated (see the log)"
                        ));
                    } else {
                        mw.toast(&format!("Rejected {count} photos (Ctrl+Z to undo)"));
                    }
                }
                Err(e) => {
                    log::error!("Rejecting suggested photos: {e}");
                    mw.show_error(&format!("The photos couldn't be rejected: {e}"));
                }
            }
        });
    }

    /// Score the photos without a current quality score (AI-7), in the
    /// background with progress and Stop. Scores come only from the Large
    /// preview (made if missing), so every score is comparable; a photo
    /// without one is skipped.
    /// Run `then`, first offering to download the face model if it isn't
    /// there yet and could run: without it, a blurred face in front of a
    /// sharp background passes (the whole frame looks sharp).
    fn with_face_model(&self, then: Rc<dyn Fn()>) {
        let store = photon_ai::store::ModelStore::default_store();
        let spec = photon_ai::faces::face_spec();
        let offer = !self.face_model_declined.get()
            && !photon_ai::faces::model_ready(&store)
            && photon_ai::openvino::OpenVinoBackend::is_available();
        let Some(spec) = spec.filter(|_| offer) else { return then() };

        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some("Check Faces for Blur?"),
            Some(&format!(
                "A blurred face in front of a sharp background looks sharp to the whole-photo check. \
                 A small face-detection model finds faces so their eyes can be checked.\n\n\
                 {} · {} KB · {} · {} licence\nIt runs on this computer; no photo leaves it.",
                spec.id,
                spec.size_bytes / 1024,
                spec.source,
                spec.licence
            )),
        );
        dialog.add_response("later", "Not Now");
        dialog.add_response("download", "Download");
        dialog.set_response_appearance("download", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("download"));
        dialog.set_close_response("later");
        let mw = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "download" {
                mw.face_model_declined.set(true);
                return then();
            }
            mw.toast("Downloading the face model…");
            let (mw, then, spec, store) = (mw.clone(), then.clone(), spec.clone(), store.clone());
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || store.download(&spec, None, None).map(drop))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("the download worker crashed")));
                if let Err(e) = result {
                    log::warn!("Downloading the face model: {e:#}");
                    mw.toast(&format!("The face model couldn't be downloaded: {e}. Continuing without it."));
                    mw.face_model_declined.set(true);
                }
                then();
            });
        });
        dialog.present();
    }

    /// "Analyse Photo Quality…": offer the face model if missing, score the
    /// selection or view, and show what looks blurred or badly exposed.
    pub fn analyse_quality(&self) {
        let mw = self.clone();
        self.with_face_model(Rc::new(move || mw.run_quality_check()));
    }

    /// Score the photos of `scope` (all photos if `None`; `what` names it)
    /// that have no current score — all of them with `force` — then run
    /// `then` (also when there was nothing to score; not when stopped).
    fn analyse_quality_in(
        &self,
        scope: Option<Vec<i64>>,
        what: &'static str,
        force: bool,
        then: Option<Box<dyn FnOnce()>>,
    ) {
        if self.progress_revealer.reveals_child() {
            self.toast("Wait for the current task (import or thumbnails) to finish");
            return;
        }
        // "Unscored below version MAX" = every photo that can be scored.
        let below = if force { i32::MAX } else { photon_import::QUALITY_VERSION };
        // With the face model downloaded, photos scored before it get their faces checked.
        let faces_on = photon_ai::faces::model_ready(&photon_ai::store::ModelStore::default_store())
            && photon_ai::openvino::OpenVinoBackend::is_available();
        let mut unscored = match self
            .db
            .conn()
            .map_err(|e| e.to_string())
            .and_then(|c| queries::get_unscored_images(&c, below, faces_on).map_err(|e| e.to_string()))
        {
            Ok(list) => list,
            Err(e) => return self.show_error(&e),
        };
        if let Some(ids) = &scope {
            let ids: std::collections::HashSet<i64> = ids.iter().copied().collect();
            unscored.retain(|(id, _, _)| ids.contains(id));
        }
        if unscored.is_empty() {
            match then {
                Some(then) => then(),
                None => self.toast(&format!("All photos in {what} have current quality scores")),
            }
            return;
        }

        let total = unscored.len();
        self.status_label.set_text(&format!("Analysing the quality of {what}: 0 of {total}"));
        self.progress_bar.set_fraction(0.0);
        self.cancel_button.set_label("Stop");
        self.cancel_button.set_visible(true);
        self.progress_revealer.set_reveal_child(true);

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_c = cancel.clone();
        let cancel_handler = self
            .cancel_button
            .connect_clicked(move |_| cancel_c.store(true, std::sync::atomic::Ordering::Relaxed));

        enum Progress {
            Step(usize),
            Done { scored: usize, skipped: usize, failed: usize, stopped: bool },
        }
        let (tx, rx) = async_channel::bounded::<Progress>(64);

        let mw = self.clone();
        let mut then = then;
        glib::spawn_future_local(async move {
            let mut cancel_handler = Some(cancel_handler);
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Progress::Step(done) => {
                        mw.progress_bar.set_fraction(done as f64 / total as f64);
                        mw.status_label.set_text(&format!("Analysing the quality of {what}: {done} of {total}"));
                    }
                    Progress::Done { scored, skipped, failed, stopped } => {
                        mw.progress_revealer.set_reveal_child(false);
                        mw.cancel_button.set_visible(false);
                        if let Some(h) = cancel_handler.take() {
                            mw.cancel_button.disconnect(h);
                        }
                        mw.refresh();
                        let mut text = if stopped {
                            format!("Quality analysis stopped: {scored} of {total} photos scored")
                        } else {
                            format!("Quality analysis done: {scored} photos scored")
                        };
                        if skipped > 0 {
                            text.push_str(&format!(", {skipped} without a preview skipped"));
                        }
                        if failed > 0 {
                            text.push_str(&format!(", {failed} not saved (see the log)"));
                        }
                        match then.take() {
                            Some(then) if !stopped => then(),
                            _ => mw.toast(&text),
                        }
                        break;
                    }
                }
            }
        });

        let db = self.db.clone();
        let thumbs = photon_import::thumbnails::ThumbnailGenerator::new(self.cache_dir.clone());
        std::thread::spawn(move || {
            use rayon::prelude::*;
            use std::sync::atomic::{AtomicUsize, Ordering};
            let (done, scored, skipped, failed) =
                (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
            // One detector for all workers: inference takes ~5 ms, decoding
            // the preview longer, so they rarely wait for it.
            let detector = if faces_on {
                let backend = photon_ai::openvino::OpenVinoBackend::new();
                match photon_ai::faces::load_detector(&photon_ai::store::ModelStore::default_store(), &backend) {
                    Ok(Some((detector, device))) => {
                        log::info!("Face-aware quality analysis on {device}");
                        Some(std::sync::Mutex::new(detector))
                    }
                    Ok(None) => None,
                    Err(e) => {
                        log::warn!("Face model unavailable; scoring without faces: {e:#}");
                        None
                    }
                }
            } else {
                None
            };

            unscored.par_iter().for_each(|(id, _hash, _path)| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let preview = db
                    .conn()
                    .map_err(|e| e.to_string())
                    .and_then(|conn| queries::get_image(&conn, *id).map_err(|e| e.to_string()))
                    .and_then(|img| img.ok_or_else(|| "no longer in the library".to_string()))
                    .and_then(|img| {
                        thumbs.ensure(&img, photon_import::thumbnails::ThumbSize::Large).map_err(|e| format!("{e:#}"))
                    })
                    .and_then(|path| image::open(&path).map(|d| d.to_rgb8()).map_err(|e| e.to_string()));
                match preview {
                    Ok(rgb) => {
                        let score = photon_import::compute_quality(&rgb);
                        let mut model = score.to_model(*id, chrono::Utc::now().timestamp());
                        if let Some(detector) = &detector {
                            let found = detector
                                .lock()
                                .map_err(|_| anyhow::anyhow!("face detector poisoned"))
                                .and_then(|mut d| photon_ai::faces::largest_face_eyes(&mut d, &rgb));
                            match found {
                                Ok((faces, eyes)) => {
                                    model.faces = Some(faces as i32);
                                    model.eye_sharpness = eyes;
                                }
                                Err(e) => log::warn!("Face detection for photo {id}: {e:#}"),
                            }
                        }
                        let saved = db
                            .conn()
                            .map_err(|e| e.to_string())
                            .and_then(|conn| queries::save_image_quality(&conn, &model).map_err(|e| e.to_string()));
                        match saved {
                            Ok(()) => scored.fetch_add(1, Ordering::Relaxed),
                            Err(e) => {
                                log::warn!("Saving the quality score of photo {id}: {e}");
                                failed.fetch_add(1, Ordering::Relaxed)
                            }
                        };
                    }
                    Err(e) => {
                        log::info!("No quality score for photo {id}: {e}");
                        skipped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n % 10 == 0 {
                    let _ = tx.send_blocking(Progress::Step(n));
                }
            });

            let _ = tx.send_blocking(Progress::Done {
                scored: scored.into_inner(),
                skipped: skipped.into_inner(),
                failed: failed.into_inner(),
                stopped: cancel.load(Ordering::Relaxed),
            });
        });
    }
}

// ── Free functions ──────────────────────────────────────

fn month_name(month: u32) -> &'static str {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Unknown",
    }
}

fn day_of_week(year: i32, month: u32, day: u32) -> &'static str {
    use chrono::NaiveDate;
    if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
        match date.weekday() {
            chrono::Weekday::Mon => "Monday",
            chrono::Weekday::Tue => "Tuesday",
            chrono::Weekday::Wed => "Wednesday",
            chrono::Weekday::Thu => "Thursday",
            chrono::Weekday::Fri => "Friday",
            chrono::Weekday::Sat => "Saturday",
            chrono::Weekday::Sun => "Sunday",
        }
    } else {
        ""
    }
}
