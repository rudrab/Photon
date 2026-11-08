//! "Analyse Photo Quality…" results (AI-7): the blurred, badly exposed and
//! weak-looking photos, and the photos already rejected.
//!
//! Groups photos by burst and standalone candidates, showing thumbnails,
//! reasons, and checkboxes for the user to confirm before setting rejects.
//! Technical faults are ticked; matters of taste (a low aesthetic score) are
//! listed unticked. The photos of the scope that are already rejected are
//! listed last, ticked: unticking one restores it, so a reject made here (or
//! by hand) can be undone one photo at a time, not only all at once with Ctrl+Z.
//! Move Rejects to Trash… saves the choices, then trashes every photo that is
//! rejected after them (asking first, as Move to Trash does).

#![allow(deprecated)]

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, Label, Orientation, Picture,
    ScrolledWindow, Window,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::models::{Image, ImageQuality};
use photon_import::quality::{burst_keeper, find_bursts, suggest_rejects, RejectSuggestion};
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What the user chose: photos to reject, and rejected photos to restore.
#[derive(Default)]
struct Choice {
    reject: HashSet<i64>,
    restore: HashSet<i64>,
}

/// The kind of row, for its checkbox.
enum Row<'a> {
    /// Suggested for rejection: ticked = reject.
    Suggested(&'a RejectSuggestion),
    /// Already rejected: ticked = stays rejected, unticked = restore.
    Rejected(Option<&'a RejectSuggestion>),
    /// Shown for context in a burst: no checkbox.
    Context { keeper: bool },
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    parent: &impl IsA<Window>,
    photos: Vec<Image>,
    scores: HashMap<i64, ImageQuality>,
    library: photon_import::quality::LibraryReference,
    cache_dir: PathBuf,
    on_apply: impl Fn(Vec<i64>, Vec<i64>) + 'static,
    on_trash: impl Fn(Vec<i64>, Vec<i64>, Vec<i64>) + 'static,
    on_reanalyse: impl Fn() + 'static,
) {
    if photos.is_empty() {
        return;
    }
    let on_reanalyse = Rc::new(on_reanalyse);

    let suggestions = suggest_rejects(&photos, &scores, library);
    let rejected: Vec<&Image> = photos.iter().filter(|p| p.flagged == -1).collect();
    if suggestions.is_empty() && rejected.is_empty() {
        let dialog = adw::MessageDialog::new(
            Some(parent),
            Some("No Problems Found"),
            Some(&format!("All {} photos look sharp and well exposed.", photos.len())),
        );
        dialog.add_response("close", "Close");
        dialog.add_response("again", "Analyse Again");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        let again = on_reanalyse.clone();
        dialog.connect_response(Some("again"), move |_, _| again());
        dialog.present();
        return;
    }
    // Why each rejected photo would have been suggested, had it not been
    // rejected already (`suggest_rejects` leaves rejected shots out).
    let unrejected: Vec<Image> = photos
        .iter()
        .map(|p| Image { flagged: if p.flagged == -1 { 0 } else { p.flagged }, ..p.clone() })
        .collect();
    let rejected_ids: HashSet<i64> = rejected.iter().filter_map(|p| p.id).collect();
    let rejected_reasons: HashMap<i64, RejectSuggestion> = suggest_rejects(&unrejected, &scores, library)
        .into_iter()
        .filter(|s| rejected_ids.contains(&s.image_id))
        .map(|s| (s.image_id, s))
        .collect();

    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(600)
        .default_height(700)
        .title("Photo Quality")
        .build();

    let root = GtkBox::new(Orientation::Vertical, 0);
    dialog.set_content(Some(&root));

    // Header bar
    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);

    let cancel_btn = Button::with_label("Close");
    let d_cancel = dialog.clone();
    cancel_btn.connect_clicked(move |_| {
        d_cancel.close();
    });
    header.pack_start(&cancel_btn);

    let again_btn = Button::from_icon_name("view-refresh-symbolic");
    again_btn.set_tooltip_text(Some("Analyse Again"));
    let (d_again, again) = (dialog.clone(), on_reanalyse.clone());
    again_btn.connect_clicked(move |_| {
        d_again.close();
        again();
    });
    header.pack_start(&again_btn);

    // Rejecting marks photos, and Ctrl+Z undoes it: not a destructive action.
    let apply_btn = Button::with_label("Apply");
    apply_btn.add_css_class("suggested-action");
    header.pack_end(&apply_btn);

    let flagged = suggestions.iter().map(|s| s.image_id).collect::<HashSet<_>>().len();
    let mut subtitle = format!("{flagged} of {} photos may be worth rejecting", photos.len());
    if !rejected.is_empty() {
        subtitle.push_str(&format!(" · {} rejected", rejected.len()));
    }
    header.set_title_widget(Some(&adw::WindowTitle::new("Photo Quality", &subtitle)));
    root.append(&header);

    // Scrolled content
    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();
    let content_box = GtkBox::new(Orientation::Vertical, 16);
    content_box.set_margin_top(16);
    content_box.set_margin_bottom(16);
    content_box.set_margin_start(16);
    content_box.set_margin_end(16);
    scroll.set_child(Some(&content_box));
    root.append(&scroll);

    // Destructive, so apart from Apply and at the bottom; it asks again first.
    let trash_btn = Button::with_label("Move Rejects to Trash…");
    trash_btn.add_css_class("destructive-action");
    trash_btn.set_tooltip_text(Some(
        "Apply the choices above, then move every photo that is rejected to the Trash \
         (asks first; restore from the Trash or with Ctrl+Z)",
    ));
    let bottom = gtk4::ActionBar::new();
    bottom.pack_end(&trash_btn);
    root.append(&bottom);

    let suggestion_map: HashMap<i64, RejectSuggestion> =
        suggestions.iter().map(|s| (s.image_id, s.clone())).collect();

    let choice = Rc::new(RefCell::new(Choice {
        reject: suggestions.iter().filter(|s| s.preselected).map(|s| s.image_id).collect(),
        restore: HashSet::new(),
    }));

    // The photos rejected once the choices are saved.
    let final_rejects = {
        let (choice, rejected_ids) = (choice.clone(), rejected_ids.clone());
        Rc::new(move || -> Vec<i64> {
            let c = choice.borrow();
            let mut ids: Vec<i64> = rejected_ids.difference(&c.restore).chain(c.reject.iter()).copied().collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        })
    };

    let update_button_label: Rc<dyn Fn()> = {
        let apply_btn = apply_btn.clone();
        let (trash_btn, final_rejects) = (trash_btn.clone(), final_rejects.clone());
        let choice = choice.clone();
        Rc::new(move || {
            let n = final_rejects().len();
            trash_btn.set_label(&if n == 0 { "Move Rejects to Trash…".to_string() } else { format!("Move {n} Rejects to Trash…") });
            trash_btn.set_sensitive(n > 0);
            let c = choice.borrow();
            let (r, u) = (c.reject.len(), c.restore.len());
            let label = match (r, u) {
                (0, 0) => "Apply".to_string(),
                (r, 0) => format!("Reject {r}"),
                (0, u) => format!("Restore {u}"),
                (r, u) => format!("Reject {r} · Restore {u}"),
            };
            apply_btn.set_label(&label);
            apply_btn.set_sensitive(r + u > 0);
        })
    };
    update_button_label();

    let add_row = |group: &GtkBox, photo: &Image, kind: Row| {
        group.append(&render_photo_row(photo, kind, scores.get(&photo.id.unwrap_or(0)), &cache_dir, choice.clone(), update_button_label.clone()));
    };

    // Bursts with something suggested, every frame shown for comparison
    // (the rejected ones are listed below, with the other rejects).
    let bursts = find_bursts(&photos);
    let mut burst_photo_ids: HashSet<i64> = HashSet::new();
    for burst in &bursts {
        burst_photo_ids.extend(burst.items.iter().filter_map(|i| i.id));
        if !burst.items.iter().any(|i| i.id.is_some_and(|id| suggestion_map.contains_key(&id))) {
            continue;
        }
        let keeper: HashSet<i64> = burst_keeper(&burst.items, &scores).into_iter().collect();
        let cam = burst.items.first().and_then(|i| i.camera_model.as_deref()).unwrap_or("Camera");
        let group = group_card(&format!("Burst: {cam} ({} photos)", burst.items.len()), None);
        for item in burst.items.iter().filter(|i| i.flagged != -1) {
            let Some(id) = item.id else { continue };
            let kind = match suggestion_map.get(&id) {
                Some(s) => Row::Suggested(s),
                None => Row::Context { keeper: keeper.contains(&id) },
            };
            add_row(&group, item, kind);
        }
        content_box.append(&group);
    }

    // Suggested photos outside bursts.
    let standalone: Vec<&Image> = photos
        .iter()
        .filter(|p| p.id.is_some_and(|id| !burst_photo_ids.contains(&id) && suggestion_map.contains_key(&id)))
        .collect();
    if !standalone.is_empty() {
        let group = group_card("Individual Photos", None);
        for item in standalone {
            if let Some(s) = item.id.and_then(|id| suggestion_map.get(&id)) {
                add_row(&group, item, Row::Suggested(s));
            }
        }
        content_box.append(&group);
    }

    if !rejected.is_empty() {
        let group = group_card(
            &format!("Already Rejected ({})", rejected.len()),
            Some("Untick a photo to restore it"),
        );
        for item in &rejected {
            let reason = item.id.and_then(|id| rejected_reasons.get(&id));
            add_row(&group, item, Row::Rejected(reason));
        }
        content_box.append(&group);
    }

    let (d_trash, choice_trash) = (dialog.clone(), choice.clone());
    trash_btn.connect_clicked(move |_| {
        let ids = final_rejects();
        let c = choice_trash.borrow();
        let (reject, restore) = (c.reject.iter().copied().collect(), c.restore.iter().copied().collect());
        drop(c);
        d_trash.close();
        on_trash(reject, restore, ids);
    });

    let d_confirm = dialog.clone();
    apply_btn.connect_clicked(move |_| {
        let c = choice.borrow();
        let reject: Vec<i64> = c.reject.iter().copied().collect();
        let restore: Vec<i64> = c.restore.iter().copied().collect();
        d_confirm.close();
        if !reject.is_empty() || !restore.is_empty() {
            on_apply(reject, restore);
        }
    });

    dialog.present();
}

/// A card with a heading (and an optional line under it) for a group of rows.
fn group_card(title: &str, subtitle: Option<&str>) -> GtkBox {
    let group = GtkBox::new(Orientation::Vertical, 8);
    group.add_css_class("card");
    group.set_margin_bottom(8);

    let header = GtkBox::new(Orientation::Vertical, 2);
    header.set_margin_top(8);
    header.set_margin_bottom(4);
    header.set_margin_start(12);
    header.set_margin_end(12);
    let heading = Label::new(Some(title));
    heading.add_css_class("heading");
    heading.set_halign(Align::Start);
    header.append(&heading);
    if let Some(text) = subtitle {
        let sub = Label::new(Some(text));
        sub.add_css_class("dim-label");
        sub.add_css_class("caption");
        sub.set_halign(Align::Start);
        header.append(&sub);
    }
    group.append(&header);
    group
}

fn render_photo_row(
    photo: &Image,
    kind: Row,
    quality: Option<&ImageQuality>,
    cache_dir: &Path,
    choice: Rc<RefCell<Choice>>,
    update_btn: Rc<dyn Fn()>,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 12);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.set_valign(Align::Center);

    let id = photo.id.unwrap_or(0);

    let check = CheckButton::new();
    check.set_valign(Align::Center);
    match kind {
        Row::Suggested(s) => {
            check.set_active(s.preselected);
            check.set_tooltip_text(Some("Ticked: reject this photo"));
            check.connect_toggled(move |btn| {
                if btn.is_active() {
                    choice.borrow_mut().reject.insert(id);
                } else {
                    choice.borrow_mut().reject.remove(&id);
                }
                update_btn();
            });
        }
        Row::Rejected(_) => {
            check.set_active(true);
            check.set_tooltip_text(Some("Untick to restore this photo"));
            check.connect_toggled(move |btn| {
                if btn.is_active() {
                    choice.borrow_mut().restore.remove(&id);
                } else {
                    choice.borrow_mut().restore.insert(id);
                }
                update_btn();
            });
        }
        Row::Context { .. } => {
            check.set_sensitive(false);
            check.set_opacity(0.0);
        }
    }
    row.append(&check);

    let pic = Picture::new();
    pic.set_size_request(64, 48);
    pic.set_content_fit(gtk4::ContentFit::Cover);
    let thumb_p = thumb_path(cache_dir, ThumbSize::Grid, &photo.hash);
    if thumb_p.exists() {
        pic.set_filename(Some(&thumb_p));
    }
    row.append(&pic);

    let info_box = GtkBox::new(Orientation::Vertical, 2);
    info_box.set_hexpand(true);
    info_box.set_valign(Align::Center);

    let name_lbl = Label::new(Some(&photo.filename));
    name_lbl.add_css_class("body");
    name_lbl.set_halign(Align::Start);
    info_box.append(&name_lbl);

    let aesthetic = quality.and_then(|q| {
        let a = q.aesthetic?;
        Some(match q.aesthetic_crop.filter(|_| photon_import::quality::crop_helps(q)) {
            Some(c) => format!("aesthetic {a:.1}, {c:.1} cropped"),
            None => format!("aesthetic {a:.1}"),
        })
    });
    let (reason, css) = match &kind {
        Row::Suggested(s) if s.preselected => (s.reason.clone(), "error"),
        Row::Suggested(s) => (s.reason.clone(), "warning"),
        Row::Rejected(Some(s)) => (s.reason.clone(), "dim-label"),
        Row::Rejected(None) => {
            let mut text = "no fault found".to_string();
            if let Some(a) = &aesthetic {
                text.push_str(&format!(" · {a}"));
            }
            (text, "dim-label")
        }
        Row::Context { .. } => {
            let mut parts = Vec::new();
            if let Some(q) = quality {
                parts.push(format!("sharpness {:.0}", q.sharpness));
            }
            parts.extend(aesthetic.clone());
            (if parts.is_empty() { "good quality".to_string() } else { parts.join(" · ") }, "dim-label")
        }
    };
    let reason_lbl = Label::new(Some(&reason));
    reason_lbl.add_css_class(css);
    reason_lbl.set_halign(Align::Start);
    reason_lbl.set_wrap(true);
    reason_lbl.set_xalign(0.0);
    info_box.append(&reason_lbl);

    // A fault's reason doesn't mention how the photo looks: say it too.
    if let (Row::Suggested(s), Some(a)) = (&kind, &aesthetic) {
        if s.preselected {
            let extra = Label::new(Some(a));
            extra.add_css_class("dim-label");
            extra.add_css_class("caption");
            extra.set_halign(Align::Start);
            info_box.append(&extra);
        }
    }

    row.append(&info_box);

    if let Row::Context { keeper: true } = kind {
        let best_badge = Label::new(Some("★ Best"));
        best_badge.add_css_class("photon-badge");
        best_badge.set_tooltip_text(Some("The frame to keep: the best-looking of the sharp ones (the sharpest without aesthetic scores)"));
        best_badge.set_valign(Align::Center);
        row.append(&best_badge);
    }

    row
}
