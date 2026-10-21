//! Adjust Date and Time (R-22): move the capture time of the selected photos
//! when a camera's clock was wrong, so shots from two bodies (or a phone)
//! interleave correctly. Either by an amount, or by saying what time one photo
//! should have shown. Shot-wide, written to the sidecars, undoable.
//!
//! The library keeps the camera's wall-clock time without a zone, so a shift
//! is plain addition. Zone changes are done by shifting by the difference.

use crate::ui::selection_bar::Context;
use crate::ui::undo::{self, UndoAction};
use chrono::NaiveDateTime;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, DropDown, Entry, Label, Orientation, SpinButton, StringList};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::queries;
use std::rc::Rc;
use std::thread;

const FORMAT: &str = "%Y-%m-%d %H:%M:%S";

fn parse_wall_clock(text: &str) -> Option<i64> {
    let text = text.trim();
    [FORMAT, "%Y-%m-%d %H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(text, f).ok())
        .map(|t| t.and_utc().timestamp())
}

fn format_wall_clock(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.naive_utc().format(FORMAT).to_string())
        .unwrap_or_default()
}

/// Shift the capture time of `ids` (and the rest of their shots) by `delta`
/// seconds. Returns how many files changed; photos without a capture time are
/// left alone.
pub fn apply_shift(ctx: &Context, ids: &[i64], delta: i64) -> usize {
    if delta == 0 {
        return 0;
    }
    let Ok(mut conn) = ctx.db.conn() else { return 0 };
    let all = queries::expand_shot_image_ids(&conn, ids).unwrap_or_else(|_| ids.to_vec());
    let previous: Vec<(i64, i64)> = all
        .iter()
        .filter_map(|&id| {
            let created = queries::get_image(&conn, id).ok().flatten()?.created_at?;
            Some((id, created))
        })
        .collect();
    if previous.is_empty() {
        (ctx.share.notify)("These photos have no capture time to adjust");
        return 0;
    }
    let shifted: Vec<(i64, i64)> = previous.iter().map(|&(id, t)| (id, t + delta)).collect();
    if let Err(e) = queries::batch_set_created_at(&mut conn, &shifted) {
        log::error!("Adjusting capture times: {e}");
        (ctx.share.notify)("Could not change the date and time");
        return 0;
    }
    let touched: Vec<i64> = previous.iter().map(|p| p.0).collect();
    let shots = queries::count_distinct_shots(&conn, &touched).unwrap_or(touched.len());
    ctx.undo_manager.borrow_mut().push(UndoAction::TimeShift { previous, delta_secs: delta });

    let db = ctx.db.clone();
    let sync = touched.clone();
    thread::spawn(move || {
        if let Ok(conn) = db.conn() {
            for id in sync {
                undo::sync_capture_time_xmp(&conn, id);
            }
        }
    });
    (ctx.share.notify)(&format!(
        "Moved {shots} photo{} by {}",
        if shots == 1 { "" } else { "s" },
        describe(delta)
    ));
    (ctx.on_library_changed)();
    touched.len()
}

fn describe(delta: i64) -> String {
    let (sign, mut s) = (if delta < 0 { "-" } else { "+" }, delta.unsigned_abs());
    let (d, h, m) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60);
    s %= 60;
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d}d"));
    }
    parts.push(format!("{h:02}:{m:02}:{s:02}"));
    format!("{sign}{}", parts.join(" "))
}

pub fn show(ctx: &Context, ids: Vec<i64>) {
    if ids.is_empty() {
        return;
    }
    let Ok(conn) = ctx.db.conn() else { return };
    // The reference photo for "set its time": the earliest of those selected.
    let mut selected: Vec<_> = ids
        .iter()
        .filter_map(|&id| queries::get_image(&conn, id).ok().flatten())
        .filter(|i| i.created_at.is_some())
        .collect();
    selected.sort_by_key(|i| i.created_at);
    let Some(reference) = selected.first().cloned() else {
        (ctx.share.notify)("These photos have no capture time to adjust");
        return;
    };
    let ref_time = reference.created_at.unwrap_or(0);
    let shots = queries::count_distinct_shots(&conn, &ids).unwrap_or(ids.len());
    drop(conn);

    let body = format!("{shots} photo{} selected.", if shots == 1 { "" } else { "s" });
    let dialog = adw::MessageDialog::new(Some(&ctx.window), Some("Adjust Date and Time"), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("apply", "Adjust");
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");

    let root = GtkBox::new(Orientation::Vertical, 10);
    let mode = DropDown::new(Some(StringList::new(&["Shift by an amount", "Set the correct time of one photo"])), gtk4::Expression::NONE);
    root.append(&mode);

    // Mode 0: direction + d/h/m/s.
    let amount = GtkBox::new(Orientation::Horizontal, 6);
    let direction = DropDown::new(Some(StringList::new(&["Later (+)", "Earlier (−)"])), gtk4::Expression::NONE);
    amount.append(&direction);
    let spin = |max: f64, label: &str| {
        let s = SpinButton::with_range(0.0, max, 1.0);
        s.set_width_chars(3);
        amount.append(&s);
        let l = Label::new(Some(label));
        l.add_css_class("dim-label");
        amount.append(&l);
        s
    };
    let (days, hours, minutes, seconds) = (spin(3650.0, "d"), spin(23.0, "h"), spin(59.0, "m"), spin(59.0, "s"));
    root.append(&amount);

    // Mode 1: what the reference photo should have said.
    let by_photo = GtkBox::new(Orientation::Vertical, 6);
    let ref_label = Label::new(Some(&format!("{} was recorded as {}", reference.filename, format_wall_clock(ref_time))));
    ref_label.set_halign(Align::Start);
    ref_label.set_wrap(true);
    by_photo.append(&ref_label);
    let entry = Entry::new();
    entry.set_text(&format_wall_clock(ref_time));
    entry.set_placeholder_text(Some("YYYY-MM-DD HH:MM:SS"));
    by_photo.append(&entry);
    by_photo.set_visible(false);
    root.append(&by_photo);

    let preview = Label::new(None);
    preview.set_halign(Align::Start);
    preview.add_css_class("dim-label");
    preview.set_wrap(true);
    root.append(&preview);
    dialog.set_extra_child(Some(&root));

    let delta: Rc<dyn Fn() -> Option<i64>> = {
        let (mode, direction, entry) = (mode.clone(), direction.clone(), entry.clone());
        let (days, hours, minutes, seconds) = (days.clone(), hours.clone(), minutes.clone(), seconds.clone());
        Rc::new(move || {
            if mode.selected() == 0 {
                let secs = days.value() as i64 * 86_400
                    + hours.value() as i64 * 3600
                    + minutes.value() as i64 * 60
                    + seconds.value() as i64;
                Some(if direction.selected() == 1 { -secs } else { secs })
            } else {
                parse_wall_clock(&entry.text()).map(|t| t - ref_time)
            }
        })
    };
    let refresh: Rc<dyn Fn()> = {
        let (delta, preview, mode, by_photo, amount) =
            (delta.clone(), preview.clone(), mode.clone(), by_photo.clone(), amount.clone());
        let name = reference.filename.clone();
        Rc::new(move || {
            by_photo.set_visible(mode.selected() == 1);
            amount.set_visible(mode.selected() == 0);
            preview.set_text(&match delta() {
                None => "Enter the time as YYYY-MM-DD HH:MM:SS".to_string(),
                Some(0) => "No change".to_string(),
                Some(d) => format!("{name}: {} → {}  ({})", format_wall_clock(ref_time), format_wall_clock(ref_time + d), describe(d)),
            });
        })
    };
    refresh();
    for dd in [&mode, &direction] {
        let r = refresh.clone();
        dd.connect_selected_notify(move |_| r());
    }
    for s in [&days, &hours, &minutes, &seconds] {
        let r = refresh.clone();
        s.connect_value_changed(move |_| r());
    }
    let r = refresh.clone();
    entry.connect_changed(move |_| r());

    let ctx = ctx.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }
        match delta() {
            Some(d) if d != 0 => {
                apply_shift(&ctx, &ids, d);
            }
            Some(_) => {}
            None => (ctx.share.notify)("That time isn't valid; use YYYY-MM-DD HH:MM:SS"),
        }
    });
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_round_trip_and_shift_text() {
        let t = parse_wall_clock("2026-10-21 14:02:11").unwrap();
        assert_eq!(format_wall_clock(t), "2026-10-21 14:02:11");
        assert_eq!(format_wall_clock(t + 7200), "2026-10-21 16:02:11");
        assert_eq!(parse_wall_clock("2026-10-21 14:02"), Some(t - 11));
        assert_eq!(parse_wall_clock("tomorrow"), None);
        assert_eq!(describe(7200), "+02:00:00");
        assert_eq!(describe(-90_061), "-1d 01:01:01");
    }
}
