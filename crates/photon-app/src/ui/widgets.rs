//! Reusable UI widgets:
//!   - `EventCard`: cover-image card for month/day drill-down following GNOME HIG card styling
//!
//! Cards use `gtk4::Fixed` as outer container so they never stretch
//! beyond the requested thumbnail_size, regardless of FlowBox allocation.

use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use gtk4::{
    Box as GtkBox, Fixed, GestureClick, Image as GtkImage, Label, Orientation, Picture, Widget,
};
use photon_core::models::{Image, Preferences, UIAction};
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::path::Path;

// ---------------------------------------------------------------------------
// EventCard
// ---------------------------------------------------------------------------

pub struct EventCard;

impl EventCard {
    pub fn create(
        title: &str,
        subtitle: &str,
        cover: &Image,
        cache_dir: &Path,
        prefs: &Preferences,
        action: UIAction,
        sender: &Sender<UIAction>,
    ) -> Widget {
        let sz = prefs.thumbnail_size as i32;
        let pic_h = (sz as f64 * 0.72) as i32;
        let card_h = pic_h + 56;

        let fixed = Fixed::new();
        fixed.set_size_request(sz, card_h);

        let card = GtkBox::new(Orientation::Vertical, 0);
        card.set_size_request(sz, card_h);
        card.set_overflow(gtk4::Overflow::Hidden);
        card.add_css_class("card");
        card.add_css_class("photon-card");
        card.set_cursor_from_name(Some("pointer"));

        // ── Cover thumbnail ─────────────────────────────
        let thumb = thumb_path(cache_dir, ThumbSize::Grid, &cover.hash);

        let display: Widget = if thumb.exists() {
            make_picture(&thumb, sz, pic_h)
        } else {
            let ph_box = GtkBox::new(Orientation::Vertical, 0);
            ph_box.set_size_request(sz, pic_h);
            ph_box.set_valign(gtk4::Align::Center);
            ph_box.set_halign(gtk4::Align::Center);
            let ph = GtkImage::from_icon_name("folder-symbolic");
            ph.set_pixel_size(48);
            ph.set_opacity(0.4);
            ph_box.append(&ph);
            ph_box.into()
        };

        // ── Labels ──────────────────────────────────────
        let label_box = GtkBox::new(Orientation::Vertical, 2);
        label_box.set_margin_top(8);
        label_box.set_margin_start(10);
        label_box.set_margin_end(10);
        label_box.set_margin_bottom(8);

        let title_lbl = Label::new(Some(title));
        title_lbl.set_css_classes(&["heading"]);
        title_lbl.set_halign(gtk4::Align::Start);
        title_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_lbl.set_max_width_chars((sz / 8).max(10));

        let sub_lbl = Label::new(Some(subtitle));
        sub_lbl.set_css_classes(&["caption", "dim-label"]);
        sub_lbl.set_halign(gtk4::Align::Start);

        label_box.append(&title_lbl);
        label_box.append(&sub_lbl);

        card.append(&display);
        card.append(&label_box);

        fixed.put(&card, 0.0, 0.0);

        // ── Click → drill down ──────────────────────────
        let gesture = GestureClick::new();
        let tx = sender.clone();
        gesture.connect_released(move |_, _, _, _| {
            let _ = tx.send_blocking(action.clone());
        });
        fixed.add_controller(gesture);

        fixed.into()
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn make_picture(path: &Path, width: i32, height: i32) -> Widget {
    let frame = GtkBox::new(Orientation::Vertical, 0);
    frame.set_size_request(width, height);
    frame.set_overflow(gtk4::Overflow::Hidden);

    let p = Picture::new();
    load_texture_async(&p, path);
    p.set_size_request(width, height);
    p.set_content_fit(gtk4::ContentFit::Cover);
    p.set_can_shrink(true);
    p.set_hexpand(false);
    p.set_vexpand(false);

    frame.append(&p);
    frame.into()
}

/// Decode the image file on a worker thread, then show it. Keeps the main
/// loop responsive when a view has hundreds of cards.
pub fn load_texture_async(picture: &Picture, path: &Path) {
    let path = path.to_path_buf();
    let weak = picture.downgrade();
    glib::spawn_future_local(async move {
        let loaded = gio::spawn_blocking(move || gdk::Texture::from_filename(&path)).await;
        match (weak.upgrade(), loaded) {
            (Some(picture), Ok(Ok(texture))) => picture.set_paintable(Some(&texture)),
            (_, Ok(Err(e))) => log::warn!("Thumbnail load failed: {e}"),
            _ => {}
        }
    });
}
