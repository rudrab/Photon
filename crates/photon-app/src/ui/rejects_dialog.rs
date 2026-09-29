//! "Analyse Photo Quality…" results (AI-7): the blurred and badly exposed photos.
//!
//! Groups photos by burst and standalone candidates, showing thumbnails,
//! reasons, and checkboxes for the user to confirm before setting rejects.

#![allow(deprecated)]

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, Label, Orientation, Picture,
    ScrolledWindow, Window,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::models::Image;
use photon_import::quality::{find_bursts, suggest_rejects, RejectSuggestion};
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub fn show(
    parent: &impl IsA<Window>,
    photos: Vec<Image>,
    scores: HashMap<i64, photon_core::models::ImageQuality>,
    library: photon_import::quality::LibraryReference,
    cache_dir: PathBuf,
    on_reject: impl Fn(Vec<i64>) + 'static,
    on_reanalyse: impl Fn() + 'static,
) {
    if photos.is_empty() {
        return;
    }
    let on_reanalyse = Rc::new(on_reanalyse);

    let suggestions = suggest_rejects(&photos, &scores, library);
    if suggestions.is_empty() {
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

    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(560)
        .default_height(680)
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
    let reject_btn = Button::with_label(&format!("Reject Selected ({})", suggestions.len()));
    reject_btn.add_css_class("suggested-action");
    header.pack_end(&reject_btn);

    let subtitle = format!("{} of {} photos look blurred or badly exposed", suggestions.len(), photos.len());
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

    // Map suggestion lookup by id
    let suggestion_map: HashMap<i64, RejectSuggestion> = suggestions
        .iter()
        .map(|s| (s.image_id, s.clone()))
        .collect();

    // Group photos by burst
    let bursts = find_bursts(&photos);
    let mut burst_photo_ids: HashSet<i64> = HashSet::new();

    let selected_ids = Rc::new(RefCell::new(
        suggestions.iter().map(|s| s.image_id).collect::<HashSet<i64>>(),
    ));

    let update_button_label = {
        let reject_btn = reject_btn.clone();
        let selected_ids = selected_ids.clone();
        Rc::new(move || {
            let count = selected_ids.borrow().len();
            reject_btn.set_label(&format!("Reject Selected ({count})"));
            reject_btn.set_sensitive(count > 0);
        })
    };

    // Render bursts first
    for burst in &bursts {
        let mut burst_has_suggestion = false;
        for item in &burst.items {
            if let Some(id) = item.id {
                burst_photo_ids.insert(id);
                if suggestion_map.contains_key(&id) {
                    burst_has_suggestion = true;
                }
            }
        }

        if !burst_has_suggestion {
            continue;
        }

        let group_box = GtkBox::new(Orientation::Vertical, 8);
        group_box.add_css_class("card");
        group_box.set_margin_bottom(8);

        let header_row = GtkBox::new(Orientation::Horizontal, 8);
        header_row.set_margin_top(8);
        header_row.set_margin_bottom(4);
        header_row.set_margin_start(12);
        header_row.set_margin_end(12);

        let cam = burst.items.first().and_then(|i| i.camera_model.as_deref()).unwrap_or("Camera");
        let group_title = Label::new(Some(&format!("Burst: {cam} ({} photos)", burst.items.len())));
        group_title.add_css_class("heading");
        group_title.set_halign(Align::Start);
        header_row.append(&group_title);
        group_box.append(&header_row);

        for item in &burst.items {
            let Some(id) = item.id else { continue };
            let suggestion = suggestion_map.get(&id);
            let row = render_photo_row(
                item,
                suggestion,
                scores.get(&id),
                &cache_dir,
                selected_ids.clone(),
                update_button_label.clone(),
            );
            group_box.append(&row);
        }

        content_box.append(&group_box);
    }

    // Render standalone candidates
    let standalone: Vec<&Image> = photos
        .iter()
        .filter(|p| p.id.map(|id| !burst_photo_ids.contains(&id) && suggestion_map.contains_key(&id)).unwrap_or(false))
        .collect();

    if !standalone.is_empty() {
        let group_box = GtkBox::new(Orientation::Vertical, 8);
        group_box.add_css_class("card");
        group_box.set_margin_bottom(8);

        let header_row = GtkBox::new(Orientation::Horizontal, 8);
        header_row.set_margin_top(8);
        header_row.set_margin_bottom(4);
        header_row.set_margin_start(12);
        header_row.set_margin_end(12);

        let group_title = Label::new(Some("Individual Photos"));
        group_title.add_css_class("heading");
        group_title.set_halign(Align::Start);
        header_row.append(&group_title);
        group_box.append(&header_row);

        for item in standalone {
            let Some(id) = item.id else { continue };
            let suggestion = suggestion_map.get(&id);
            let row = render_photo_row(
                item,
                suggestion,
                scores.get(&id),
                &cache_dir,
                selected_ids.clone(),
                update_button_label.clone(),
            );
            group_box.append(&row);
        }

        content_box.append(&group_box);
    }

    // Confirm button handler
    let d_confirm = dialog.clone();
    let selected_ids_confirm = selected_ids.clone();
    reject_btn.connect_clicked(move |_| {
        let ids: Vec<i64> = selected_ids_confirm.borrow().iter().copied().collect();
        d_confirm.close();
        if !ids.is_empty() {
            on_reject(ids);
        }
    });

    dialog.present();
}

fn render_photo_row(
    photo: &Image,
    suggestion: Option<&RejectSuggestion>,
    quality: Option<&photon_core::models::ImageQuality>,
    cache_dir: &Path,
    selected_ids: Rc<RefCell<HashSet<i64>>>,
    update_btn: Rc<dyn Fn()>,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 12);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.set_valign(Align::Center);

    let id = photo.id.unwrap_or(0);
    let is_suggested = suggestion.is_some();

    // Checkbox
    let check = CheckButton::new();
    check.set_active(is_suggested);
    check.set_valign(Align::Center);

    if !is_suggested {
        check.set_sensitive(false);
        check.set_opacity(0.0);
    } else {
        let sel_rc = selected_ids.clone();
        let update_rc = update_btn.clone();
        check.connect_toggled(move |btn| {
            if btn.is_active() {
                sel_rc.borrow_mut().insert(id);
            } else {
                sel_rc.borrow_mut().remove(&id);
            }
            update_rc();
        });
    }
    row.append(&check);

    // Thumbnail picture
    let pic = Picture::new();
    pic.set_size_request(64, 48);
    pic.set_content_fit(gtk4::ContentFit::Cover);
    let thumb_p = thumb_path(cache_dir, ThumbSize::Grid, &photo.hash);
    if thumb_p.exists() {
        pic.set_filename(Some(&thumb_p));
    }
    row.append(&pic);

    // Info details
    let info_box = GtkBox::new(Orientation::Vertical, 2);
    info_box.set_hexpand(true);
    info_box.set_valign(Align::Center);

    let name_lbl = Label::new(Some(&photo.filename));
    name_lbl.add_css_class("body");
    name_lbl.set_halign(Align::Start);
    info_box.append(&name_lbl);

    let reason_text = if let Some(s) = suggestion {
        s.reason.clone()
    } else if let Some(q) = quality {
        format!("sharpness {:.1} (best in burst)", q.sharpness)
    } else {
        "good quality".to_string()
    };

    let reason_lbl = Label::new(Some(&reason_text));
    if is_suggested {
        reason_lbl.add_css_class("destructive-action");
    } else {
        reason_lbl.add_css_class("dim-label");
    }
    reason_lbl.set_halign(Align::Start);
    info_box.append(&reason_lbl);

    row.append(&info_box);

    // Badge on right if best in burst
    if !is_suggested {
        let best_badge = Label::new(Some("★ Sharpest"));
        best_badge.add_css_class("photon-badge");
        best_badge.set_valign(Align::Center);
        row.append(&best_badge);
    }

    row
}
