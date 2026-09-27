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
use crate::ui::sidebar;
use crate::ui::timeline::Timeline;
use crate::ui::widgets::EventCard;
use async_channel;
use chrono::Datelike;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use gtk4::{
    Align, Box as GtkBox, Button, Label, MenuButton, Orientation, Paned,
    ProgressBar, Revealer, ScrolledWindow, Stack,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::queries::{self, TimelineFilter};
use photon_core::db::Database;
use photon_core::models::{Preferences, TimelineItem, UIAction};
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
    pub cull_min_rating: Rc<Cell<i32>>,
    pub cull_flag: Rc<Cell<Option<i32>>>,
    pub sidebar: sidebar::Sidebar,
}

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
        window.set_content(Some(&root_box));

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
        let filter_btn = MenuButton::builder()
            .icon_name("view-filter-symbolic")
            .tooltip_text("Filter & Grid Options")
            .build();
        filter_btn.add_css_class("flat");
        header_bar.pack_end(&filter_btn);

        let primary_menu = menu::build_primary_menu();
        let menu_btn = MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&primary_menu)
            .primary(true)
            .build();
        menu_btn.add_css_class("flat");
        header_bar.pack_end(&menu_btn);

        root_box.append(&header_bar);

        // ── Main Paned ──────────────────────────────────────
        let paned = Paned::new(Orientation::Horizontal);
        paned.set_position(260);
        paned.set_vexpand(true);
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
        let cull_min_rating = Rc::new(Cell::new(0));
        let cull_flag = Rc::new(Cell::new(None));

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
            ("Any", 0),
            ("★ 1+", 1),
            ("★ 2+", 2),
            ("★ 3+", 3),
            ("★ 4+", 4),
            ("★ 5", 5),
        ];
        let mut rating_btns = Vec::new();
        for (label, val) in ratings {
            let b = Button::with_label(label);
            b.add_css_class("flat");
            if val == 0 {
                b.add_css_class("suggested-action");
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
        let mut flag_btns = Vec::new();
        for (label, val) in flags {
            let b = Button::with_label(label);
            b.add_css_class("flat");
            if val.is_none() {
                b.add_css_class("suggested-action");
            }
            status_box.append(&b);
            flag_btns.push((b, val));
        }
        pop_box.append(&status_box);

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

        let stack = Stack::new();
        stack.set_vexpand(true);
        stack.add_named(&scrolled, Some("cards"));
        stack.add_named(timeline.widget(), Some("timeline"));
        content_box.append(&stack);

        // ── Status bar / In-App Notification ─────────────
        let progress_revealer = Revealer::new();
        progress_revealer.set_transition_type(gtk4::RevealerTransitionType::SlideUp);

        let status_box = GtkBox::new(Orientation::Vertical, 6);
        status_box.add_css_class("status-card");
        status_box.set_margin_top(8);
        status_box.set_margin_bottom(12);
        status_box.set_margin_start(16);
        status_box.set_margin_end(16);

        let status_label = Label::new(Some("Ready"));
        status_label.set_halign(Align::Start);
        let progress_bar = ProgressBar::new();
        progress_bar.set_show_text(true);

        let cancel_button = Button::with_label("Stop Import");
        cancel_button.add_css_class("destructive-action");
        cancel_button.set_visible(false);

        let progress_row = GtkBox::new(Orientation::Horizontal, 12);
        progress_bar.set_hexpand(true);
        progress_bar.set_valign(Align::Center);
        progress_row.append(&progress_bar);
        progress_row.append(&cancel_button);

        status_box.append(&status_label);
        status_box.append(&progress_row);
        progress_revealer.set_child(Some(&status_box));
        content_box.append(&progress_revealer);

        paned.set_end_child(Some(&content_box));

        let mw = Self {
            window,
            window_title,
            timeline_container,
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
            cull_min_rating,
            cull_flag,
            sidebar: sidebar_handle,
        };

        let mw_f = mw.clone();
        for (b, val) in &rating_btns {
            let b_clone = b.clone();
            let all_b: Vec<_> = rating_btns.iter().map(|(btn, _)| btn.clone()).collect();
            let mw_f = mw_f.clone();
            let val = *val;
            b.connect_clicked(move |_| {
                for other in &all_b {
                    other.remove_css_class("suggested-action");
                }
                b_clone.add_css_class("suggested-action");
                mw_f.cull_min_rating.set(val);
                mw_f.timeline_stale.set(true);
                mw_f.refresh();
            });
        }

        let mw_f = mw.clone();
        for (b, val) in &flag_btns {
            let b_clone = b.clone();
            let all_b: Vec<_> = flag_btns.iter().map(|(btn, _)| btn.clone()).collect();
            let mw_f = mw_f.clone();
            let val = *val;
            b.connect_clicked(move |_| {
                for other in &all_b {
                    other.remove_css_class("suggested-action");
                }
                b_clone.add_css_class("suggested-action");
                mw_f.cull_flag.set(val);
                mw_f.timeline_stale.set(true);
                mw_f.refresh();
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
                .application_icon("camera-photo-symbolic")
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

        let mw_changed = mw.clone();
        selection_bar::attach(
            &mw.timeline,
            selection_bar::Context {
                window: mw.window.clone().upcast(),
                db: mw.db.clone(),
                prefs: mw.prefs.clone(),
                on_library_changed: Rc::new(move || mw_changed.refresh()),
            },
        );

        mw.navigate(&UIAction::ShowAll);
        mw
    }

    pub fn present(&self) {
        self.window.present();
    }

    // ── Navigation ──────────────────────────────────────

    pub fn navigate(&self, action: &UIAction) {
        if !matches!(action, UIAction::ViewPhoto(_)) {
            *self.last_grid_action.borrow_mut() = action.clone();
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
                self.show_cards();
                self.show_viewer(*idx);
            }
            UIAction::FilterByTag(_) => {}
        }
    }

    /// The library changed (import, thumbnails, preferences). Update what is
    /// on screen without losing the scroll position; a viewer that is open
    /// stays open, and the timeline catches up when the user returns.
    pub fn refresh(&self) {
        self.timeline_stale.set(true);
        self.sidebar.refresh_events();
        let action = self.last_grid_action.borrow().clone();
        let showing_timeline = self.stack.visible_child_name().as_deref() == Some("timeline");
        let showing_viewer = !showing_timeline
            && matches!(
                action,
                UIAction::ShowAll | UIAction::FilterByDay(..) | UIAction::Search(_)
            )
            && self.timeline_container.first_child().is_some();
        if !showing_viewer {
            self.navigate(&action);
        }
    }

    fn show_cards(&self) {
        self.clear_timeline();
        self.stack.set_visible_child_name("cards");
    }

    /// Show `filter` in the virtualized timeline. Reuses the loaded timeline
    /// (and its scroll position) when nothing changed.
    fn show_timeline(&self, filter: TimelineFilter, title: String) {
        let same = self.timeline_filter.borrow().as_ref() == Some(&filter);
        if !same || self.timeline_stale.get() {
            let started = std::time::Instant::now();
            let items = match self.db.conn() {
                Ok(conn) => {
                    let min_r = self.cull_min_rating.get();
                    queries::timeline_items_with_cull(
                        &conn,
                        &filter,
                        if min_r > 0 { Some(min_r) } else { None },
                        self.cull_flag.get(),
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
            *self.timeline_filter.borrow_mut() = Some(filter);
            self.timeline_stale.set(false);
        }

        let count = self.current_photos.borrow().len();
        let count_str = if count == 1 { "1 photo" } else { &format!("{count} photos") };
        let is_filtered = self.cull_min_rating.get() > 0 || self.cull_flag.get().is_some();
        let filter_tag = if is_filtered { " · Filtered" } else { "" };
        let subtitle = format!("{title} · {count_str}{filter_tag}");
        self.window_title.set_subtitle(&subtitle);
        self.status_label.set_text(&subtitle);

        if count == 0 {
            self.show_cards();
            let msg = match self.last_grid_action.borrow().clone() {
                UIAction::ShowAll => "No photos yet. Import some!".to_string(),
                UIAction::Search(t) => format!("No results for \"{t}\""),
                _ => format!("No photos on {title}"),
            };
            return self.show_empty(&msg);
        }
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
        if index >= photos.len() {
            return;
        }

        let back_action = self.last_grid_action.borrow().clone();
        let prefs = self.prefs.borrow();

        let viewer = detail::build_viewer(
            photos,
            index,
            &self.cache_dir,
            &prefs,
            &self.nav_tx,
            back_action,
            &self.db,
        );
        self.timeline_container.append(&viewer);
    }

    // ── Helpers ─────────────────────────────────────────

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
