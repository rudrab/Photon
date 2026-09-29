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
//! It is built lazily: one aggregated query, year rows up front, months and
//! days only when opened, so a multi-decade library costs a few dozen widgets.

use async_channel::Sender;
use chrono::{DateTime, Datelike, NaiveDate};
use gtk4::prelude::*;
use gtk4::{
    gdk, Align, Box, Button, Entry, Expander, GestureClick, Image, Label,
    Orientation, Popover, ScrolledWindow, Separator,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::queries;
use photon_core::db::Database;
use photon_core::models::{Album, Event, UIAction};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::thread;

/// Sidebar handle — holds references needed for refresh.
#[derive(Clone)]
pub struct Sidebar {
    pub widget: ScrolledWindow,
    pub missing_container: Box,
    pub events_container: Box,
    pub albums_container: Box,
    pub tags_container: Box,
    pub db: Database,
    pub sender: Sender<UIAction>,
    /// Bumped per refresh; only the newest refresh may fill the tree, so
    /// overlapping refreshes can't both append (which duplicated every year).
    generation: Rc<Cell<u64>>,
    /// The tree currently shown; a refresh with identical data is a no-op.
    tree: Rc<RefCell<Rc<Vec<YearNode>>>>,
    /// Open years `(year, None)` and months `(year, Some(month))`. Survives
    /// refreshes, so live updates during an import don't snap the tree shut.
    expanded: Rc<RefCell<HashSet<NodeKey>>>,
    /// Day events cache: (year, month, day) -> Event name
    pub event_names: Rc<RefCell<HashMap<(i32, u32, u32), String>>>,
}

type NodeKey = (i32, Option<u32>);

#[derive(Debug, Clone, PartialEq)]
struct YearNode {
    year: i32,
    count: u32,
    months: Vec<MonthNode>,
}

#[derive(Debug, Clone, PartialEq)]
struct MonthNode {
    month: u32,
    count: u32,
    /// `(day, count)`, newest first.
    days: Vec<(u32, u32)>,
}

/// Fold per-day counts (sorted newest first) into a year → month → day tree.
fn build_tree(rows: &[(i32, u32, u32, u32)]) -> Vec<YearNode> {
    let mut years: Vec<YearNode> = Vec::new();
    for &(year, month, day, count) in rows {
        if years.last().map(|y| y.year) != Some(year) {
            years.push(YearNode {
                year,
                count: 0,
                months: Vec::new(),
            });
        }
        let y = years.last_mut().expect("just pushed");
        y.count += count;
        if y.months.last().map(|m| m.month) != Some(month) {
            y.months.push(MonthNode {
                month,
                count: 0,
                days: Vec::new(),
            });
        }
        let m = y.months.last_mut().expect("just pushed");
        m.count += count;
        m.days.push((day, count));
    }
    years
}

impl Sidebar {
    /// Reload the year/month/day events tree from DB.
    pub fn refresh_events(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        let db = self.db.clone();
        let (tx_db, rx_db) = async_channel::bounded::<(Vec<(i32, u32, u32, u32)>, Vec<Event>)>(1);
        thread::spawn(move || {
            if let Ok(conn) = db.conn() {
                let rows = queries::date_tree(&conn).unwrap_or_default();
                let events = queries::get_all_events_with_counts(&conn)
                    .map(|evs| evs.into_iter().map(|(e, _)| e).collect())
                    .unwrap_or_default();
                let _ = tx_db.send_blocking((rows, events));
            }
        });

        let this = self.clone();
        gtk4::glib::MainContext::default().spawn_local(async move {
            let Ok((rows, events)) = rx_db.recv().await else { return };
            if this.generation.get() != generation {
                return; // a newer refresh will fill the tree
            }

            // Update day events map
            let mut ev_map = HashMap::new();
            for ev in &events {
                let start_dt = DateTime::from_timestamp(ev.start_date, 0);
                let end_dt = DateTime::from_timestamp(ev.end_date, 0);
                if let (Some(s), Some(e)) = (start_dt, end_dt) {
                    let mut curr = s.date_naive();
                    let end = e.date_naive();
                    while curr <= end {
                        ev_map.insert((curr.year(), curr.month(), curr.day()), ev.name.clone());
                        curr = curr.succ_opt().unwrap_or(curr);
                        if curr == end {
                            ev_map.insert((curr.year(), curr.month(), curr.day()), ev.name.clone());
                            break;
                        }
                    }
                }
            }
            *this.event_names.borrow_mut() = ev_map;

            let tree = build_tree(&rows);
            let tree = Rc::new(tree);
            *this.tree.borrow_mut() = tree.clone();
            this.rebuild(&tree);
        });
        self.refresh_albums();
        self.refresh_tags();
    }

    /// Reload the albums list with photo counts.
    pub fn refresh_albums(&self) {
        let db = self.db.clone();
        let (tx_albums, rx_albums) = async_channel::bounded::<Vec<(Album, u32)>>(1);
        thread::spawn(move || {
            if let Ok(conn) = db.conn() {
                if let Ok(albums) = queries::get_all_albums_with_counts(&conn) {
                    let _ = tx_albums.send_blocking(albums);
                }
            }
        });

        let this = self.clone();
        gtk4::glib::MainContext::default().spawn_local(async move {
            let Ok(albums) = rx_albums.recv().await else { return };
            while let Some(child) = this.albums_container.first_child() {
                this.albums_container.remove(&child);
            }

            if albums.is_empty() {
                let empty_lbl = Label::new(Some("No albums yet"));
                empty_lbl.set_css_classes(&["caption", "dim-label"]);
                empty_lbl.set_halign(Align::Start);
                empty_lbl.set_margin_start(16);
                empty_lbl.set_margin_top(4);
                empty_lbl.set_margin_bottom(4);
                this.albums_container.append(&empty_lbl);
            } else {
                for (album, count) in albums {
                    let btn = make_row_with_count(&album.name, "folder-pictures-symbolic", Some(count));
                    let tx = this.sender.clone();
                    let album_id = album.id;
                    btn.connect_clicked(move |_| {
                        let _ = tx.send_blocking(UIAction::FilterByAlbum(album_id));
                    });

                    // Context menu on secondary click
                    let click = GestureClick::new();
                    click.set_button(gdk::BUTTON_SECONDARY);
                    let alb_id = album.id;
                    let alb_name = album.name.clone();
                    let this_c = this.clone();
                    let btn_c = btn.clone();
                    click.connect_released(move |_, _, x, y| {
                        show_album_context_menu(&this_c, &btn_c, alb_id, &alb_name, x, y);
                    });
                    btn.add_controller(click);

                    this.albums_container.append(&btn);
                }
            }
        });
    }

    /// Reload the tags list with photo counts.
    pub fn refresh_tags(&self) {
        let db = self.db.clone();
        let (tx_tags, rx_tags) = async_channel::bounded::<Vec<(photon_core::models::Tag, u32)>>(1);
        thread::spawn(move || {
            let tags = db.conn().map_err(|e| e.to_string()).and_then(|conn| {
                queries::get_tags_with_counts(&conn).map_err(|e| e.to_string())
            });
            if let Ok(tags) = tags {
                let _ = tx_tags.send_blocking(tags);
            }
        });

        let this = self.clone();
        gtk4::glib::MainContext::default().spawn_local(async move {
            let Ok(tags) = rx_tags.recv().await else { return };
            while let Some(child) = this.tags_container.first_child() {
                this.tags_container.remove(&child);
            }

            if tags.is_empty() {
                let empty_lbl = Label::new(Some("No tags yet"));
                empty_lbl.set_css_classes(&["caption", "dim-label"]);
                empty_lbl.set_halign(Align::Start);
                empty_lbl.set_margin_start(16);
                empty_lbl.set_margin_top(4);
                empty_lbl.set_margin_bottom(4);
                this.tags_container.append(&empty_lbl);
            } else {
                for (tag, count) in tags {
                    let label = format!("#{}", tag.name);
                    let btn = make_row_with_count(&label, "tag-symbolic", Some(count));
                    let tx = this.sender.clone();
                    let tag_name = tag.name.clone();
                    btn.connect_clicked(move |_| {
                        let _ = tx.send_blocking(UIAction::FilterByTag(tag_name.clone()));
                    });
                    this.tags_container.append(&btn);
                }
            }
        });
    }

    /// Reload missing photos indicator and count.
    pub fn refresh_missing(&self) {
        let db = self.db.clone();
        let (tx_m, rx_m) = async_channel::bounded::<usize>(1);
        thread::spawn(move || {
            if let Ok(conn) = db.conn() {
                let missing_count = queries::get_missing_images(&conn).map(|imgs| imgs.len()).unwrap_or(0);
                let _ = tx_m.send_blocking(missing_count);
            }
        });

        let this = self.clone();
        gtk4::glib::MainContext::default().spawn_local(async move {
            let Ok(count) = rx_m.recv().await else { return };
            while let Some(child) = this.missing_container.first_child() {
                this.missing_container.remove(&child);
            }

            if count > 0 {
                let btn = make_row_with_count("Missing Photos", "dialog-warning-symbolic", Some(count as u32));
                btn.add_css_class("warning");
                let tx = this.sender.clone();
                btn.connect_clicked(move |_| {
                    let _ = tx.send_blocking(UIAction::FilterMissing);
                });
                this.missing_container.append(&btn);
            }
        });
    }

    fn rebuild(&self, tree: &Rc<Vec<YearNode>>) {
        let scroll = self.widget.vadjustment();
        let position = scroll.value();

        while let Some(child) = self.events_container.first_child() {
            self.events_container.remove(&child);
        }
        for index in 0..tree.len() {
            let expander = self.year_expander(tree.clone(), index);
            self.events_container.append(&expander);
        }

        // Rebuilt rows re-measure on the next frame; restore the scroll then.
        scroll.set_value(position);
        gtk4::glib::idle_add_local_once(move || scroll.set_value(position));
    }

    fn year_expander(&self, tree: Rc<Vec<YearNode>>, index: usize) -> Expander {
        let node = &tree[index];
        let key: NodeKey = (node.year, None);
        let expander = Expander::builder()
            .label(format!("{}  ·  {}", node.year, node.count))
            .build();
        let body = Box::new(Orientation::Vertical, 2);
        body.set_margin_start(10);
        body.set_margin_top(2);
        body.set_margin_bottom(2);
        expander.set_child(Some(&body));

        let this = self.clone();
        self.lazy(&expander, key, move || {
            let year = &tree[index];
            for m in 0..year.months.len() {
                body.append(&this.month_expander(tree.clone(), index, m));
            }
        });
        expander
    }

    fn month_expander(&self, tree: Rc<Vec<YearNode>>, year_index: usize, index: usize) -> Expander {
        let year = tree[year_index].year;
        let node = &tree[year_index].months[index];
        let (month, count) = (node.month, node.count);
        let expander = Expander::builder()
            .label(format!("{}  ·  {}", month_name(month), count))
            .build();
        let body = Box::new(Orientation::Vertical, 1);
        body.set_margin_start(12);
        body.set_margin_top(2);
        body.set_margin_bottom(2);
        expander.set_child(Some(&body));

        let sender = self.sender.clone();
        let this = self.clone();
        self.lazy(&expander, (year, Some(month)), move || {
            // "All of <Month>" row → shows cover cards for dates
            let all = make_row_with_count(
                &format!("All of {}", month_name(month)),
                "folder-symbolic",
                Some(count),
            );
            let tx = sender.clone();
            all.connect_clicked(move |_| {
                let _ = tx.send_blocking(UIAction::FilterByDate(year, month));
            });
            body.append(&all);

            let this_m = this.clone();
            for &(day, day_count) in &tree[year_index].months[index].days {
                let ev_name = this_m.event_names.borrow().get(&(year, month, day)).cloned();
                let label_text = if let Some(ref name) = ev_name {
                    format!("{} ({})", name, short_date_label(year, month, day))
                } else {
                    short_date_label(year, month, day)
                };
                let btn = make_row_with_count(
                    &label_text,
                    if ev_name.is_some() { "emblem-favorite-symbolic" } else { "x-office-calendar-symbolic" },
                    Some(day_count),
                );
                let tx = sender.clone();
                btn.connect_clicked(move |_| {
                    let _ = tx.send_blocking(UIAction::FilterByDay(year, month, day));
                });

                // Secondary click to Name Event
                let click = GestureClick::new();
                click.set_button(gdk::BUTTON_SECONDARY);
                let this_c = this_m.clone();
                let initial_name = ev_name.unwrap_or_default();
                click.connect_released(move |_, _, _, _| {
                    let sb = this_c.clone();
                    let date_str = short_date_label(year, month, day);
                    prompt_text_dialog(
                        "Name Event",
                        &format!("Name event for {date_str}:"),
                        &initial_name,
                        "Save",
                        move |name| {
                            if let Ok(conn) = sb.db.conn() {
                                if let Err(e) = queries::name_day_event(&conn, &name, year, month, day) {
                                    log::error!("name_day_event failed: {e}");
                                }
                            }
                            sb.refresh_events();
                        },
                    );
                });
                btn.add_controller(click);

                body.append(&btn);
            }
        });
        expander
    }

    /// Run `populate` the first time `expander` opens, track its open state
    /// under `key`, and reopen it now if it was open before a refresh.
    fn lazy(&self, expander: &Expander, key: NodeKey, populate: impl Fn() + 'static) {
        let populated = Cell::new(false);
        let expanded = self.expanded.clone();
        expander.connect_expanded_notify(move |e| {
            if e.is_expanded() {
                expanded.borrow_mut().insert(key);
                if !populated.replace(true) {
                    populate();
                }
            } else {
                expanded.borrow_mut().remove(&key);
            }
        });
        if self.expanded.borrow().contains(&key) {
            expander.set_expanded(true);
        }
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

    let missing_container = Box::new(Orientation::Vertical, 2);
    lib_box.append(&missing_container);

    root.append(&lib_box);
    root.append(&make_separator());

    // ── Events header ───────────────────────────────────
    let events_label = make_caption("Events");
    root.append(&events_label);

    let events_container = Box::new(Orientation::Vertical, 2);
    root.append(&events_container);

    root.append(&make_separator());

    // ── Albums header ───────────────────────────────────
    let albums_header_box = Box::new(Orientation::Horizontal, 4);
    albums_header_box.set_hexpand(true);
    let albums_label = make_caption("Albums");
    albums_label.set_hexpand(true);
    albums_header_box.append(&albums_label);

    let new_album_btn = Button::from_icon_name("list-add-symbolic");
    new_album_btn.add_css_class("flat");
    new_album_btn.add_css_class("circular");
    new_album_btn.set_tooltip_text(Some("New Album"));
    albums_header_box.append(&new_album_btn);
    root.append(&albums_header_box);

    let albums_container = Box::new(Orientation::Vertical, 2);
    root.append(&albums_container);

    root.append(&make_separator());

    // ── Tags header ─────────────────────────────────────
    let tags_label = make_caption("Tags");
    root.append(&tags_label);

    let tags_container = Box::new(Orientation::Vertical, 2);
    root.append(&tags_container);

    scrolled.set_child(Some(&root));

    let sidebar = Sidebar {
        widget: scrolled,
        missing_container,
        events_container,
        albums_container,
        tags_container,
        db: db.clone(),
        sender: sender.clone(),
        generation: Rc::new(Cell::new(0)),
        tree: Rc::new(RefCell::new(Rc::new(Vec::new()))),
        expanded: Rc::new(RefCell::new(HashSet::new())),
        event_names: Rc::new(RefCell::new(HashMap::new())),
    };

    let sb_clone = sidebar.clone();
    new_album_btn.connect_clicked(move |_| {
        let sb = sb_clone.clone();
        prompt_text_dialog(
            "New Album",
            "Enter a name for the new album:",
            "",
            "Create",
            move |name| {
                if let Ok(conn) = sb.db.conn() {
                    if let Ok(album_id) = queries::create_album(&conn, &name) {
                        sb.refresh_albums();
                        let _ = sb.sender.send_blocking(UIAction::FilterByAlbum(album_id));
                    }
                }
            },
        );
    });

    // Initial load
    sidebar.refresh_events();
    sidebar.refresh_albums();
    sidebar.refresh_tags();

    sidebar
}

fn show_album_context_menu(
    sidebar: &Sidebar,
    target_btn: &Button,
    album_id: i64,
    album_name: &str,
    x: f64,
    y: f64,
) {
    let popover = Popover::new();
    popover.set_parent(target_btn);
    let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));

    let menu_box = Box::new(Orientation::Vertical, 4);
    menu_box.set_margin_start(6);
    menu_box.set_margin_end(6);
    menu_box.set_margin_top(6);
    menu_box.set_margin_bottom(6);

    let rename_btn = Button::with_label("Rename Album…");
    rename_btn.add_css_class("flat");
    rename_btn.set_halign(Align::Fill);
    let pop_c = popover.clone();
    let sb_c = sidebar.clone();
    let name_c = album_name.to_string();
    rename_btn.connect_clicked(move |_| {
        pop_c.popdown();
        let sb = sb_c.clone();
        let old_name = name_c.clone();
        prompt_text_dialog(
            "Rename Album",
            &format!("Enter a new name for '{old_name}':"),
            &old_name,
            "Rename",
            move |new_name| {
                if let Ok(conn) = sb.db.conn() {
                    if let Err(e) = queries::rename_album(&conn, album_id, &new_name) {
                        log::error!("rename_album failed: {e}");
                    }
                }
                sb.refresh_albums();
            },
        );
    });

    let delete_btn = Button::with_label("Delete Album");
    delete_btn.add_css_class("flat");
    delete_btn.add_css_class("destructive-action");
    delete_btn.set_halign(Align::Fill);
    let pop_c2 = popover.clone();
    let sb_c2 = sidebar.clone();
    let name_c2 = album_name.to_string();
    delete_btn.connect_clicked(move |_| {
        pop_c2.popdown();
        let sb = sb_c2.clone();
        let name = name_c2.clone();
        let dialog = adw::MessageDialog::new(
            None::<&gtk4::Window>,
            Some(&format!("Delete album '{}'?", name)),
            Some("Photos in this album will remain in your library."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, resp| {
            if resp == "delete" {
                if let Ok(conn) = sb.db.conn() {
                    if let Err(e) = queries::delete_album(&conn, album_id) {
                        log::error!("delete_album failed: {e}");
                    }
                }
                sb.refresh_albums();
                let _ = sb.sender.send_blocking(UIAction::ShowAll);
            }
        });
        dialog.present();
    });

    menu_box.append(&rename_btn);
    menu_box.append(&delete_btn);
    popover.set_child(Some(&menu_box));
    popover.popup();
}

pub fn prompt_text_dialog(
    title: &str,
    heading: &str,
    initial: &str,
    confirm_label: &str,
    on_confirm: impl Fn(String) + 'static,
) {
    let dialog = adw::MessageDialog::new(
        None::<&gtk4::Window>,
        Some(title),
        Some(heading),
    );
    let entry = Entry::new();
    entry.set_text(initial);
    entry.set_hexpand(true);
    entry.set_activates_default(true);
    entry.set_margin_top(8);
    entry.set_margin_bottom(8);
    dialog.set_extra_child(Some(&entry));

    dialog.add_responses(&[("cancel", "Cancel"), ("confirm", confirm_label)]);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    dialog.connect_response(None, move |_, resp| {
        if resp == "confirm" {
            let text = entry.text().trim().to_string();
            if !text.is_empty() {
                on_confirm(text);
            }
        }
    });
    dialog.present();
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

    let b = Box::new(Orientation::Horizontal, 8);
    b.set_margin_top(1);
    b.set_margin_bottom(1);
    b.set_margin_start(4);
    b.set_margin_end(4);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_year_month_day_tree_with_totals() {
        let rows = [
            (2024, 6, 16, 3),
            (2024, 6, 15, 2),
            (2024, 1, 1, 1),
            (2023, 12, 31, 4),
        ];
        let tree = build_tree(&rows);

        assert_eq!(tree.len(), 2);
        assert_eq!((tree[0].year, tree[0].count), (2024, 6));
        assert_eq!(tree[0].months.len(), 2);
        assert_eq!((tree[0].months[0].month, tree[0].months[0].count), (6, 5));
        assert_eq!(tree[0].months[0].days, vec![(16, 3), (15, 2)]);
        assert_eq!((tree[1].year, tree[1].count), (2023, 4));
        assert!(build_tree(&[]).is_empty());
    }
}
