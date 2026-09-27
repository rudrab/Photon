//! Sidebar: Shotwell-style hierarchical browser following GNOME HIG navigation sidebar standards.
//!
//!   Year  (expander)
//!     Month  (expander)
//!       Date  (clickable row, e.g. "Sun Aug 24")
//!       Date  ...
//!     Month  ...
//!   Year  ...
//!
//! The events tree can be refreshed after import via `refresh_events()`.

use async_channel::Sender;
use chrono::NaiveDate;
use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Expander, Image, Label, Orientation, ScrolledWindow, Separator};
use photon_core::db::queries;
use photon_core::db::Database;
use photon_core::models::UIAction;
use std::cell::Cell;
use std::rc::Rc;
use std::thread;

/// Sidebar handle — holds references needed for refresh.
#[derive(Clone)]
pub struct Sidebar {
    pub widget: ScrolledWindow,
    pub events_container: Box,
    pub db: Database,
    pub sender: Sender<UIAction>,
    /// Bumped per refresh; only the newest refresh may fill the tree, so
    /// overlapping refreshes can't both append (which duplicated every year).
    generation: Rc<Cell<u64>>,
}

/// Data fetched from DB on background thread for the full tree.
type TreeData = Vec<(i32, u32, Vec<(u32, u32, Vec<(u32, u32)>)>)>;

impl Sidebar {
    /// Reload the year/month/day events tree from DB.
    pub fn refresh_events(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        let db = self.db.clone();
        let (tx_db, rx_db) = async_channel::unbounded::<TreeData>();

        thread::spawn(move || {
            if let Ok(conn) = db.conn() {
                if let Ok(years) = queries::get_years(&conn) {
                    let mut tree_data: TreeData = Vec::new();
                    for (year, year_count) in years {
                        if let Ok(months) = queries::get_months_in_year(&conn, year) {
                            let mut month_data = Vec::new();
                            for (month, m_count) in months {
                                let days = queries::get_days_in_month(&conn, year, month)
                                    .unwrap_or_default();
                                month_data.push((month, m_count, days));
                            }
                            tree_data.push((year, year_count, month_data));
                        }
                    }
                    let _ = tx_db.send_blocking(tree_data);
                }
            }
        });

        let events_container = self.events_container.clone();
        let sender = self.sender.clone();
        let latest = self.generation.clone();

        gtk4::glib::MainContext::default().spawn_local(async move {
            if let Ok(tree_data) = rx_db.recv().await {
                if latest.get() != generation {
                    return; // a newer refresh will fill the tree
                }
                while let Some(child) = events_container.first_child() {
                    events_container.remove(&child);
                }
                for (year, year_count, months) in tree_data {
                    // ── Year expander ────────────────────────
                    let year_expander = Expander::builder()
                        .label(format!("{}  ·  {}", year, year_count))
                        .expanded(false)
                        .build();

                    let year_box = Box::new(Orientation::Vertical, 2);
                    year_box.set_margin_start(12);

                    for (month, m_count, days) in &months {
                        // ── Month expander (nested) ─────────
                        let month_expander = Expander::builder()
                            .label(format!("{}  ·  {}", month_name(*month), m_count))
                            .expanded(false)
                            .build();

                        let month_box = Box::new(Orientation::Vertical, 2);
                        month_box.set_margin_start(12);

                        // "All of <Month>" row → shows cover cards for dates
                        let btn_month_all = make_row_with_count(
                            &format!("All of {}", month_name(*month)),
                            "folder-symbolic",
                            Some(*m_count),
                        );
                        let tx = sender.clone();
                        let y = year;
                        let m = *month;
                        btn_month_all.connect_clicked(move |_| {
                            let _ = tx.send_blocking(UIAction::FilterByDate(y, m));
                        });
                        month_box.append(&btn_month_all);

                        for (day, d_count) in days {
                            let day_label = short_date_label(year, *month, *day);
                            let btn = make_row_with_count(
                                &day_label,
                                "x-office-calendar-symbolic",
                                Some(*d_count),
                            );
                            let tx = sender.clone();
                            let y = year;
                            let m = *month;
                            let d = *day;
                            btn.connect_clicked(move |_| {
                                let _ = tx.send_blocking(UIAction::FilterByDay(y, m, d));
                            });
                            month_box.append(&btn);
                        }

                        month_expander.set_child(Some(&month_box));
                        year_box.append(&month_expander);
                    }

                    year_expander.set_child(Some(&year_box));
                    events_container.append(&year_expander);
                }
            }
        });
    }
}

/// Create the sidebar widget and return a handle for refreshing.
pub fn create(db: Database, sender: Sender<UIAction>) -> Sidebar {
    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_width(260)
        .build();

    let root = Box::new(Orientation::Vertical, 0);
    root.add_css_class("navigation-sidebar");
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    // ── Static library items ────────────────────────────
    let lib_header = make_caption("Library");
    root.append(&lib_header);

    let lib_box = Box::new(Orientation::Vertical, 2);

    let btn_all = make_row_with_count("All Photos", "camera-photo-symbolic", None);
    let tx = sender.clone();
    btn_all.connect_clicked(move |_| {
        let _ = tx.send_blocking(UIAction::ShowAll);
    });
    lib_box.append(&btn_all);

    root.append(&lib_box);
    root.append(&make_separator());

    // ── Events header ───────────────────────────────────
    let events_label = make_caption("Events");
    root.append(&events_label);

    let events_container = Box::new(Orientation::Vertical, 2);
    root.append(&events_container);

    scrolled.set_child(Some(&root));

    let sidebar = Sidebar {
        widget: scrolled,
        events_container,
        db,
        sender,
        generation: Rc::new(Cell::new(0)),
    };

    // Initial load
    sidebar.refresh_events();

    sidebar
}

// ── Helpers ──────────────────────────────────────────────

/// Format a date for the sidebar: "Sun Aug 24" style.
fn short_date_label(year: i32, month: u32, day: u32) -> String {
    if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
        let dow = match date.format("%a").to_string().as_str() {
            "Mon" => "Mon",
            "Tue" => "Tue",
            "Wed" => "Wed",
            "Thu" => "Thu",
            "Fri" => "Fri",
            "Sat" => "Sat",
            "Sun" => "Sun",
            _ => "",
        };
        let mon = short_month(month);
        format!("{} {} {}", dow, mon, day)
    } else {
        format!("Day {}", day)
    }
}

fn short_month(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

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

fn make_row_with_count(text: &str, icon: &str, count: Option<u32>) -> Button {
    let btn = Button::builder()
        .has_frame(false)
        .halign(Align::Fill)
        .build();
    btn.add_css_class("flat");

    let b = Box::new(Orientation::Horizontal, 10);
    b.set_margin_top(4);
    b.set_margin_bottom(4);
    b.set_margin_start(6);
    b.set_margin_end(6);

    let icon_widget = Image::from_icon_name(icon);
    b.append(&icon_widget);

    let label = Label::new(Some(text));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    b.append(&label);

    if let Some(c) = count {
        let badge = Label::new(Some(&c.to_string()));
        badge.add_css_class("photon-badge");
        badge.set_halign(Align::End);
        b.append(&badge);
    }

    btn.set_child(Some(&b));
    btn
}

fn make_caption(text: &str) -> Label {
    let lbl = Label::builder()
        .label(text)
        .halign(Align::Start)
        .margin_start(12)
        .margin_top(6)
        .margin_bottom(4)
        .build();
    lbl.add_css_class("dim-label");
    lbl.add_css_class("caption-heading");
    lbl
}

fn make_separator() -> Separator {
    let s = Separator::new(Orientation::Horizontal);
    s.set_opacity(0.3);
    s.set_margin_top(8);
    s.set_margin_bottom(8);
    s
}
