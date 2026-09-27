//! Export Dialog following GNOME HIG and Material 3 design standards.
//!
//! Provides batch export options: format (JPEG/PNG/WebP), quality,
//! downsampling dimensions, metadata preservation, and custom prefix/suffix.

#![allow(deprecated)]

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, Entry, FileChooserAction, FileChooserNative, Label,
    Orientation, ResponseType, Scale, Window,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::models::Image;
use photon_import::export::{batch_export, ExportConfig, ExportFormat, ExportReport, ExportResize};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub fn show(
    parent: &impl IsA<Window>,
    images: Vec<Image>,
    on_start: impl Fn(usize) + 'static,
    on_progress: impl Fn(usize, usize) + 'static,
    on_done: impl Fn(ExportReport) + 'static,
) {
    if images.is_empty() {
        return;
    }

    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(480)
        .default_height(520)
        .title("Export Photos")
        .build();

    let root = GtkBox::new(Orientation::Vertical, 0);
    dialog.set_content(Some(&root));

    // ── Header Bar ──────────────────────────────────────
    let header = adw::HeaderBar::new();
    let cancel_btn = Button::with_label("Cancel");
    cancel_btn.add_css_class("flat");
    header.pack_start(&cancel_btn);

    let export_btn = Button::with_label("Export");
    export_btn.add_css_class("suggested-action");
    header.pack_end(&export_btn);

    let title_count = if images.len() == 1 {
        "Export 1 Photo".to_string()
    } else {
        format!("Export {} Photos", images.len())
    };
    let title = adw::WindowTitle::new(&title_count, "Configure export format & size");
    header.set_title_widget(Some(&title));
    root.append(&header);

    // ── Dialog Content ──────────────────────────────────
    let content = GtkBox::new(Orientation::Vertical, 16);
    content.set_margin_top(16);
    content.set_margin_bottom(20);
    content.set_margin_start(24);
    content.set_margin_end(24);

    // 1. File Format Group
    let fmt_label = Label::new(Some("File Format"));
    fmt_label.add_css_class("caption-heading");
    fmt_label.set_halign(Align::Start);
    content.append(&fmt_label);

    let fmt_box = GtkBox::new(Orientation::Horizontal, 0);
    fmt_box.add_css_class("linked");
    let btn_jpg = Button::with_label("JPEG");
    let btn_png = Button::with_label("PNG");
    let btn_webp = Button::with_label("WebP");
    btn_jpg.add_css_class("suggested-action");
    fmt_box.append(&btn_jpg);
    fmt_box.append(&btn_png);
    fmt_box.append(&btn_webp);
    content.append(&fmt_box);

    // Quality Slider (shown for JPEG and WebP)
    let quality_row = GtkBox::new(Orientation::Horizontal, 12);
    let q_label = Label::new(Some("Quality: 90%"));
    q_label.set_width_chars(14);
    q_label.set_halign(Align::Start);
    let q_scale = Scale::with_range(Orientation::Horizontal, 50.0, 100.0, 1.0);
    q_scale.set_value(90.0);
    q_scale.set_hexpand(true);
    let ql = q_label.clone();
    q_scale.connect_value_changed(move |s| {
        ql.set_text(&format!("Quality: {}%", s.value() as i32));
    });
    quality_row.append(&q_label);
    quality_row.append(&q_scale);
    content.append(&quality_row);

    // 2. Resizing Dimensions Group
    let resize_label = Label::new(Some("Image Dimensions"));
    resize_label.add_css_class("caption-heading");
    resize_label.set_halign(Align::Start);
    content.append(&resize_label);

    let resize_box = GtkBox::new(Orientation::Horizontal, 0);
    resize_box.add_css_class("linked");
    let btn_orig = Button::with_label("Original");
    let btn_2048 = Button::with_label("2048px (Web)");
    let btn_1080 = Button::with_label("1080px (HD)");
    let btn_3840 = Button::with_label("3840px (4K)");
    btn_orig.add_css_class("suggested-action");
    resize_box.append(&btn_orig);
    resize_box.append(&btn_2048);
    resize_box.append(&btn_1080);
    resize_box.append(&btn_3840);
    content.append(&resize_box);

    // 3. Naming Pattern Group
    let name_label = Label::new(Some("File Naming"));
    name_label.add_css_class("caption-heading");
    name_label.set_halign(Align::Start);
    content.append(&name_label);

    let name_box = GtkBox::new(Orientation::Horizontal, 12);
    let prefix_entry = Entry::new();
    prefix_entry.set_placeholder_text(Some("Prefix (e.g. Export_)"));
    prefix_entry.set_hexpand(true);
    let suffix_entry = Entry::new();
    suffix_entry.set_placeholder_text(Some("Suffix (e.g. _web)"));
    suffix_entry.set_hexpand(true);
    name_box.append(&prefix_entry);
    name_box.append(&suffix_entry);
    content.append(&name_box);

    // 4. Metadata Preservation
    let meta_check = CheckButton::with_label("Preserve EXIF & camera metadata in sidecar");
    meta_check.set_active(true);
    content.append(&meta_check);

    // 5. Destination Folder
    let dest_label = Label::new(Some("Destination Folder"));
    dest_label.add_css_class("caption-heading");
    dest_label.set_halign(Align::Start);
    content.append(&dest_label);

    let default_dest = dirs::picture_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Photon_Export");
    let dest_path = Rc::new(RefCell::new(default_dest));

    let dest_row = GtkBox::new(Orientation::Horizontal, 8);
    let dest_lbl = Label::new(Some(&dest_path.borrow().to_string_lossy()));
    dest_lbl.set_hexpand(true);
    dest_lbl.set_halign(Align::Start);
    dest_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    dest_lbl.add_css_class("dim-label");

    let choose_btn = Button::with_label("Choose...");
    choose_btn.add_css_class("flat");

    let d_path = dest_path.clone();
    let d_lbl = dest_lbl.clone();
    let parent_win = dialog.clone();
    choose_btn.connect_clicked(move |_| {
        let chooser = FileChooserNative::new(
            Some("Select Export Folder"),
            Some(&parent_win),
            FileChooserAction::SelectFolder,
            Some("Select"),
            Some("Cancel"),
        );
        let dp = d_path.clone();
        let dl = d_lbl.clone();
        chooser.connect_response(move |dialog, response| {
            if response == ResponseType::Accept {
                if let Some(folder) = dialog.file() {
                    if let Some(p) = folder.path() {
                        dl.set_text(&p.to_string_lossy());
                        *dp.borrow_mut() = p;
                    }
                }
            }
        });
        chooser.show();
    });

    dest_row.append(&dest_lbl);
    dest_row.append(&choose_btn);
    content.append(&dest_row);

    root.append(&content);

    // ── State Tracking ──────────────────────────────────
    let selected_format = Rc::new(RefCell::new(0)); // 0: JPG, 1: PNG, 2: WEBP
    let selected_resize = Rc::new(RefCell::new(0)); // 0: Orig, 1: 2048, 2: 1080, 3: 3840

    // Format button clicks
    let fmt_btns = [btn_jpg.clone(), btn_png.clone(), btn_webp.clone()];
    for (i, btn) in fmt_btns.iter().enumerate() {
        let sf = selected_format.clone();
        let all = fmt_btns.clone();
        let b = btn.clone();
        let qr = quality_row.clone();
        btn.connect_clicked(move |_| {
            for other in &all {
                other.remove_css_class("suggested-action");
            }
            b.add_css_class("suggested-action");
            *sf.borrow_mut() = i;
            qr.set_visible(i != 1); // Hide quality row for lossless PNG
        });
    }

    // Resize button clicks
    let res_btns = [btn_orig.clone(), btn_2048.clone(), btn_1080.clone(), btn_3840.clone()];
    for (i, btn) in res_btns.iter().enumerate() {
        let sr = selected_resize.clone();
        let all = res_btns.clone();
        let b = btn.clone();
        btn.connect_clicked(move |_| {
            for other in &all {
                other.remove_css_class("suggested-action");
            }
            b.add_css_class("suggested-action");
            *sr.borrow_mut() = i;
        });
    }

    // Cancel click
    let d = dialog.clone();
    cancel_btn.connect_clicked(move |_| d.close());

    // Export click
    let d = dialog.clone();
    let sf = selected_format.clone();
    let sr = selected_resize.clone();
    let dp = dest_path.clone();
    let pe = prefix_entry.clone();
    let se = suffix_entry.clone();
    let qs = q_scale.clone();
    let mc = meta_check.clone();
    let progress_cb = Rc::new(on_progress);
    let done_cb = Rc::new(on_done);

    export_btn.connect_clicked(move |_| {
        let quality = qs.value() as u8;
        let format = match *sf.borrow() {
            1 => ExportFormat::Png,
            2 => ExportFormat::Webp { quality },
            _ => ExportFormat::Jpeg { quality },
        };

        let resize = match *sr.borrow() {
            1 => ExportResize::FitLongEdge(2048),
            2 => ExportResize::FitLongEdge(1080),
            3 => ExportResize::FitLongEdge(3840),
            _ => ExportResize::Original,
        };

        let config = ExportConfig {
            destination_dir: dp.borrow().clone(),
            format,
            resize,
            prefix: pe.text().to_string(),
            suffix: se.text().to_string(),
            preserve_metadata: mc.is_active(),
        };

        d.close();
        on_start(images.len());

        let imgs = images.clone();
        let p_cb = progress_cb.clone();
        let d_cb = done_cb.clone();
        let cancel = Arc::new(AtomicBool::new(false));

        let (tx_p, rx_p) = async_channel::unbounded::<(usize, usize)>();
        glib::spawn_future_local(async move {
            while let Ok((done, total)) = rx_p.recv().await {
                p_cb(done, total);
            }
        });

        glib::spawn_future_local(async move {
            let report = gtk4::gio::spawn_blocking(move || {
                batch_export(&imgs, config, cancel, move |done, total| {
                    let _ = tx_p.send_blocking((done, total));
                })
            })
            .await;

            if let Ok(rep) = report {
                d_cb(rep);
            }
        });
    });

    dialog.present();
}
