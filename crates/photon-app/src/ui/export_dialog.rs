//! The export dialog: format, size, colour space, sharpening, metadata,
//! watermark, naming and destination. Presets are saved in the library, and
//! the last export's settings come back next time.

// FileChooserNative and MessageDialog: the app targets libadwaita 1.4.
#![allow(deprecated)]

use gtk4::prelude::*;
use gtk4::{glib, Align, Box as GtkBox, Button, Entry, FileChooserAction, FileChooserNative, Orientation, ResponseType, StringList, Window};
use libadwaita as adw;
use libadwaita::prelude::*;
use photon_core::db::{queries, Database};
use photon_core::models::Image;
use photon_import::export::{
    batch_export, exiftool_installed, ExportConfig, ExportFormat, ExportItem, ExportReport, ExportResize, RawRenderer,
    Sharpening, WatermarkConfig, WatermarkPosition,
};
use photon_import::icc::ColorSpace;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Where the last export's settings are kept (`library_meta`).
const LAST_CONFIG_KEY: &str = "last_export_config";

const FORMATS: [&str; 5] = ["JPEG", "PNG", "WebP", "TIFF, 8-bit", "TIFF, 16-bit"];
/// The size choices: `None` is "Original"; the last is "Custom".
const SIZES: [(&str, Option<u32>); 5] = [
    ("Original", None),
    ("1080 px long edge", Some(1080)),
    ("2048 px long edge (web)", Some(2048)),
    ("3840 px long edge (4K)", Some(3840)),
    ("Custom", None),
];
const CUSTOM_SIZE: u32 = SIZES.len() as u32 - 1;
const SHARPENING: [(&str, Sharpening); 3] =
    [("None", Sharpening::None), ("For screen", Sharpening::Screen), ("For print", Sharpening::Print)];

type Presets = Rc<RefCell<Vec<(i64, String, ExportConfig)>>>;

/// The dialog's controls, read into and set from an [`ExportConfig`].
struct Form {
    preset: adw::ComboRow,
    delete_preset: Button,
    format: adw::ComboRow,
    quality: adw::SpinRow,
    color_space: adw::ComboRow,
    size: adw::ComboRow,
    long_edge: adw::SpinRow,
    sharpening: adw::ComboRow,
    darktable: adw::SwitchRow,
    metadata: adw::SwitchRow,
    strip_gps: adw::SwitchRow,
    watermark: adw::ExpanderRow,
    wm_text: adw::EntryRow,
    wm_image: RefCell<Option<PathBuf>>,
    wm_image_row: adw::ActionRow,
    wm_position: adw::ComboRow,
    wm_opacity: adw::SpinRow,
    wm_scale: adw::SpinRow,
    prefix: adw::EntryRow,
    suffix: adw::EntryRow,
    template: adw::EntryRow,
    destination: RefCell<PathBuf>,
    destination_row: adw::ActionRow,
}

impl Form {
    fn read(&self) -> ExportConfig {
        let quality = self.quality.value() as u8;
        let format = match self.format.selected() {
            1 => ExportFormat::Png,
            2 => ExportFormat::Webp { quality },
            3 => ExportFormat::Tiff { bit_depth: 8 },
            4 => ExportFormat::Tiff { bit_depth: 16 },
            _ => ExportFormat::Jpeg { quality },
        };
        let resize = match SIZES.get(self.size.selected() as usize) {
            Some((_, Some(edge))) => ExportResize::FitLongEdge(*edge),
            _ if self.size.selected() == CUSTOM_SIZE => ExportResize::FitLongEdge(self.long_edge.value() as u32),
            _ => ExportResize::Original,
        };
        let text = |row: &adw::EntryRow| Some(row.text().trim().to_string()).filter(|t| !t.is_empty());
        let watermark = self.watermark.enables_expansion().then(|| WatermarkConfig {
            text: text(&self.wm_text),
            image_path: self.wm_image.borrow().clone(),
            opacity: self.wm_opacity.value() as f32 / 100.0,
            scale: self.wm_scale.value() as f32 / 100.0,
            position: WatermarkPosition::ALL[self.wm_position.selected() as usize % WatermarkPosition::ALL.len()],
        });
        ExportConfig {
            destination_dir: self.destination.borrow().clone(),
            format,
            resize,
            prefix: self.prefix.text().to_string(),
            suffix: self.suffix.text().to_string(),
            preserve_metadata: self.metadata.is_active(),
            strip_gps: self.strip_gps.is_active(),
            filename_template: text(&self.template),
            raw_renderer: if self.darktable.is_active() { RawRenderer::Darktable } else { RawRenderer::Builtin },
            sharpening: SHARPENING[self.sharpening.selected() as usize % SHARPENING.len()].1,
            watermark,
            color_space: ColorSpace::ALL[self.color_space.selected() as usize % ColorSpace::ALL.len()],
        }
    }

    /// Show `config`, except its destination: presets don't move exports.
    fn apply(&self, config: &ExportConfig) {
        let (format, quality) = match config.format {
            ExportFormat::Jpeg { quality } => (0, Some(quality)),
            ExportFormat::Png => (1, None),
            ExportFormat::Webp { quality } => (2, Some(quality)),
            ExportFormat::Tiff { bit_depth: 16 } => (4, None),
            ExportFormat::Tiff { .. } => (3, None),
        };
        self.format.set_selected(format);
        if let Some(q) = quality {
            self.quality.set_value(q as f64);
        }
        let edge = match config.resize {
            ExportResize::Original => None,
            ExportResize::FitLongEdge(edge) => Some(edge),
            // Not offered here: the nearest long-edge limit.
            ExportResize::FitBoundingBox(w, h) => Some(w.max(h)),
        };
        match edge {
            None => self.size.set_selected(0),
            Some(edge) => match SIZES.iter().position(|(_, e)| *e == Some(edge)) {
                Some(i) => self.size.set_selected(i as u32),
                None => {
                    self.size.set_selected(CUSTOM_SIZE);
                    self.long_edge.set_value(edge as f64);
                }
            },
        }
        self.color_space.set_selected(ColorSpace::ALL.iter().position(|s| *s == config.color_space).unwrap_or(0) as u32);
        self.sharpening
            .set_selected(SHARPENING.iter().position(|(_, s)| *s == config.sharpening).unwrap_or(0) as u32);
        if self.darktable.is_sensitive() {
            self.darktable.set_active(config.raw_renderer == RawRenderer::Darktable);
        }
        self.metadata.set_active(config.preserve_metadata);
        self.strip_gps.set_active(config.strip_gps);
        self.prefix.set_text(&config.prefix);
        self.suffix.set_text(&config.suffix);
        self.template.set_text(config.filename_template.as_deref().unwrap_or(""));

        let wm = config.watermark.clone();
        self.watermark.set_enable_expansion(wm.is_some());
        let wm = wm.unwrap_or_default();
        self.wm_text.set_text(wm.text.as_deref().unwrap_or(""));
        self.set_watermark_image(wm.image_path);
        self.wm_position.set_selected(WatermarkPosition::ALL.iter().position(|p| *p == wm.position).unwrap_or(0) as u32);
        self.wm_opacity.set_value((wm.opacity * 100.0).round() as f64);
        self.wm_scale.set_value((wm.scale * 100.0).round() as f64);
        self.update_visibility();
    }

    fn set_watermark_image(&self, path: Option<PathBuf>) {
        let subtitle = match &path {
            Some(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            None => "None: the text above is used".to_string(),
        };
        self.wm_image_row.set_subtitle(&subtitle);
        *self.wm_image.borrow_mut() = path;
    }

    fn set_destination(&self, path: PathBuf) {
        self.destination_row.set_subtitle(&path.to_string_lossy());
        *self.destination.borrow_mut() = path;
    }

    fn update_visibility(&self) {
        self.quality.set_visible(matches!(self.format.selected(), 0 | 2));
        self.long_edge.set_visible(self.size.selected() == CUSTOM_SIZE);
        self.strip_gps.set_sensitive(self.metadata.is_active());
    }
}

fn combo(title: &str, items: &[&str]) -> adw::ComboRow {
    adw::ComboRow::builder().title(title).model(&StringList::new(items)).build()
}

fn spin(title: &str, min: f64, max: f64, step: f64, value: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    row.set_value(value);
    row
}

fn group(title: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).build()
}

/// Presets from the library; ones that no longer parse are skipped.
fn load_presets(db: &Database) -> Vec<(i64, String, ExportConfig)> {
    let rows = match db.conn().map_err(|e| e.to_string()).and_then(|c| queries::get_export_presets(&c).map_err(|e| e.to_string())) {
        Ok(rows) => rows,
        Err(e) => {
            log::warn!("Loading export presets: {e}");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|(id, name, json)| match serde_json::from_str(&json) {
            Ok(config) => Some((id, name, config)),
            Err(e) => {
                log::warn!("Skipping export preset {name:?}: {e}");
                None
            }
        })
        .collect()
}

/// Refill the preset list from the library, and select `select` by name
/// ("Custom" when `None`).
fn reload_presets(form: &Form, db: &Database, presets: &Presets, select: Option<&str>) {
    *presets.borrow_mut() = load_presets(db);
    let mut names = vec!["Custom".to_string()];
    names.extend(presets.borrow().iter().map(|(_, name, _)| name.clone()));
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let index = select.and_then(|s| names.iter().position(|n| *n == s)).unwrap_or(0);
    form.preset.set_model(Some(&StringList::new(&names)));
    form.preset.set_selected(index as u32);
}

pub fn show(
    parent: &impl IsA<Window>,
    db: Database,
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
        .default_width(520)
        .default_height(760)
        .title("Export Photos")
        .build();
    let root = GtkBox::new(Orientation::Vertical, 0);
    dialog.set_content(Some(&root));

    // ── Header bar ──────────────────────────────────────
    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    let cancel_btn = Button::with_label("Cancel");
    header.pack_start(&cancel_btn);
    let export_btn = Button::with_label("Export");
    export_btn.add_css_class("suggested-action");
    header.pack_end(&export_btn);
    let count = if images.len() == 1 { "1 photo".to_string() } else { format!("{} photos", images.len()) };
    header.set_title_widget(Some(&adw::WindowTitle::new("Export", &count)));
    root.append(&header);

    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    root.append(&page);

    // ── Preset ──────────────────────────────────────────
    let presets_group = group("Preset");
    let preset = combo("Preset", &["Custom"]);
    let save_preset = Button::from_icon_name("document-save-symbolic");
    save_preset.set_tooltip_text(Some("Save these settings as a preset"));
    let delete_preset = Button::from_icon_name("user-trash-symbolic");
    delete_preset.set_tooltip_text(Some("Delete this preset"));
    for b in [&save_preset, &delete_preset] {
        b.add_css_class("flat");
        b.set_valign(Align::Center);
        preset.add_suffix(b);
    }
    presets_group.add(&preset);
    page.add(&presets_group);

    // ── Image ───────────────────────────────────────────
    let image_group = group("Image");
    let format = combo("Format", &FORMATS);
    let quality = spin("Quality", 50.0, 100.0, 1.0, 90.0);
    let space_labels: Vec<&str> = ColorSpace::ALL.iter().map(|s| s.label()).collect();
    let color_space = combo("Colour space", &space_labels);
    color_space.set_subtitle("Adobe RGB and Display P3 keep wider colours, for print and modern screens");
    let size_labels: Vec<&str> = SIZES.iter().map(|(l, _)| *l).collect();
    let size = combo("Size", &size_labels);
    let long_edge = spin("Long edge (px)", 100.0, 20000.0, 100.0, 1600.0);
    let sharp_labels: Vec<&str> = SHARPENING.iter().map(|(l, _)| *l).collect();
    let sharpening = combo("Output sharpening", &sharp_labels);
    for row in [format.upcast_ref::<gtk4::Widget>(), quality.upcast_ref(), color_space.upcast_ref(), size.upcast_ref(), long_edge.upcast_ref(), sharpening.upcast_ref()] {
        image_group.add(row);
    }
    let darktable = adw::SwitchRow::builder()
        .title("Render RAW files with darktable")
        .subtitle("Includes your darktable edits")
        .build();
    if photon_import::raw::darktable_cli().is_some() {
        darktable.set_active(true);
    } else {
        darktable.set_sensitive(false);
        darktable.set_subtitle("darktable-cli not found: RAW files use Photon's built-in renderer");
    }
    image_group.add(&darktable);
    page.add(&image_group);

    // ── Metadata ────────────────────────────────────────
    let meta_group = group("Metadata");
    let metadata = adw::SwitchRow::builder()
        .title("Include metadata")
        .subtitle(if exiftool_installed() {
            "Camera, lens and date, plus title, description, keywords and rating"
        } else {
            "Title, description, keywords and rating. Install exiftool to also copy camera data from RAW files"
        })
        .active(true)
        .build();
    let strip_gps = adw::SwitchRow::builder()
        .title("Remove location")
        .subtitle("Leave out GPS coordinates")
        .build();
    meta_group.add(&metadata);
    meta_group.add(&strip_gps);
    page.add(&meta_group);

    // ── Watermark ───────────────────────────────────────
    let wm_group = group("Watermark");
    let watermark = adw::ExpanderRow::builder()
        .title("Watermark")
        .subtitle("Text or an image, such as a logo")
        .show_enable_switch(true)
        .enable_expansion(false)
        .build();
    let wm_text = adw::EntryRow::builder().title("Text").build();
    let wm_image_row = adw::ActionRow::builder().title("Image").build();
    let choose_wm = Button::with_label("Choose…");
    let clear_wm = Button::from_icon_name("edit-clear-symbolic");
    clear_wm.set_tooltip_text(Some("Use the text instead"));
    for b in [&choose_wm, &clear_wm] {
        b.add_css_class("flat");
        b.set_valign(Align::Center);
        wm_image_row.add_suffix(b);
    }
    let pos_labels: Vec<&str> = WatermarkPosition::ALL.iter().map(|p| p.label()).collect();
    let wm_position = combo("Position", &pos_labels);
    let wm_opacity = spin("Opacity (%)", 5.0, 100.0, 5.0, 70.0);
    let wm_scale = spin("Width (% of photo)", 5.0, 100.0, 1.0, 20.0);
    for row in [wm_text.upcast_ref::<gtk4::Widget>(), wm_image_row.upcast_ref(), wm_position.upcast_ref(), wm_opacity.upcast_ref(), wm_scale.upcast_ref()] {
        watermark.add_row(row);
    }
    wm_group.add(&watermark);
    page.add(&wm_group);

    // ── File names and destination ──────────────────────
    let names_group = adw::PreferencesGroup::builder()
        .title("File Names")
        .description("A template replaces prefix and suffix: {name}, {date} or {date:%Y-%m-%d}")
        .build();
    let prefix = adw::EntryRow::builder().title("Prefix").build();
    let suffix = adw::EntryRow::builder().title("Suffix").build();
    let template = adw::EntryRow::builder().title("Template").build();
    names_group.add(&prefix);
    names_group.add(&suffix);
    names_group.add(&template);
    page.add(&names_group);

    let dest_group = group("Destination");
    let destination_row = adw::ActionRow::builder().title("Folder").build();
    destination_row.add_css_class("property");
    let choose_dest = Button::with_label("Choose…");
    choose_dest.add_css_class("flat");
    choose_dest.set_valign(Align::Center);
    destination_row.add_suffix(&choose_dest);
    dest_group.add(&destination_row);
    page.add(&dest_group);

    let form = Rc::new(Form {
        preset,
        delete_preset,
        format,
        quality,
        color_space,
        size,
        long_edge,
        sharpening,
        darktable,
        metadata,
        strip_gps,
        watermark,
        wm_text,
        wm_image: RefCell::new(None),
        wm_image_row,
        wm_position,
        wm_opacity,
        wm_scale,
        prefix,
        suffix,
        template,
        destination: RefCell::new(PathBuf::new()),
        destination_row,
    });

    // The last export's settings, or the defaults.
    let last: ExportConfig = db
        .conn()
        .ok()
        .and_then(|c| queries::get_meta(&c, LAST_CONFIG_KEY).ok().flatten())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    form.apply(&last);
    form.set_destination(last.destination_dir.clone());

    let presets: Presets = Rc::new(RefCell::new(Vec::new()));
    reload_presets(&form, &db, &presets, None);
    form.delete_preset.set_sensitive(false);

    // ── Behaviour ───────────────────────────────────────
    for row in [&form.format, &form.size] {
        let f = form.clone();
        row.connect_selected_notify(move |_| f.update_visibility());
    }
    {
        let f = form.clone();
        form.metadata.connect_active_notify(move |_| f.update_visibility());
    }
    {
        let (f, p) = (form.clone(), presets.clone());
        form.preset.connect_selected_notify(move |row| {
            let index = row.selected() as usize;
            f.delete_preset.set_sensitive(index > 0);
            // Cloned out first: applying fires other handlers.
            let config = index.checked_sub(1).and_then(|i| p.borrow().get(i).map(|(_, _, c)| c.clone()));
            if let Some(config) = config {
                f.apply(&config);
            }
        });
    }
    {
        let (f, p, db, win) = (form.clone(), presets.clone(), db.clone(), dialog.clone());
        save_preset.connect_clicked(move |_| {
            let entry = Entry::builder().placeholder_text("Preset name").activates_default(true).build();
            let current = (f.preset.selected() as usize).checked_sub(1).and_then(|i| p.borrow().get(i).map(|x| x.1.clone()));
            entry.set_text(current.as_deref().unwrap_or(""));
            let ask = adw::MessageDialog::new(
                Some(&win),
                Some("Save Export Preset"),
                Some("Saves the current settings, except the destination folder. A preset with the same name is replaced."),
            );
            ask.set_extra_child(Some(&entry));
            ask.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
            ask.set_response_appearance("save", adw::ResponseAppearance::Suggested);
            ask.set_default_response(Some("save"));
            ask.set_close_response("cancel");
            let (f, p, db) = (f.clone(), p.clone(), db.clone());
            ask.connect_response(None, move |_, response| {
                let name = entry.text().trim().to_string();
                if response != "save" || name.is_empty() {
                    return;
                }
                let saved = serde_json::to_string(&f.read())
                    .map_err(|e| e.to_string())
                    .and_then(|json| {
                        let conn = db.conn().map_err(|e| e.to_string())?;
                        queries::save_export_preset(&conn, &name, &json).map_err(|e| e.to_string())
                    });
                match saved {
                    Ok(_) => reload_presets(&f, &db, &p, Some(&name)),
                    Err(e) => log::warn!("Saving export preset {name:?}: {e}"),
                }
            });
            ask.present();
        });
    }
    {
        let (f, p, db, win) = (form.clone(), presets.clone(), db.clone(), dialog.clone());
        form.delete_preset.connect_clicked(move |_| {
            let Some((id, name)) = (f.preset.selected() as usize)
                .checked_sub(1)
                .and_then(|i| p.borrow().get(i).map(|x| (x.0, x.1.clone())))
            else {
                return;
            };
            let ask = adw::MessageDialog::new(Some(&win), Some(&format!("Delete preset “{name}”?")), None);
            ask.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            ask.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            ask.set_close_response("cancel");
            let (f, p, db) = (f.clone(), p.clone(), db.clone());
            ask.connect_response(None, move |_, response| {
                if response != "delete" {
                    return;
                }
                let deleted = db
                    .conn()
                    .map_err(|e| e.to_string())
                    .and_then(|c| queries::delete_export_preset(&c, id).map_err(|e| e.to_string()));
                if let Err(e) = deleted {
                    log::warn!("Deleting export preset {name:?}: {e}");
                }
                reload_presets(&f, &db, &p, None);
            });
            ask.present();
        });
    }
    {
        let (f, win) = (form.clone(), dialog.clone());
        choose_wm.connect_clicked(move |_| {
            let chooser = FileChooserNative::new(
                Some("Choose Watermark Image"),
                Some(&win),
                FileChooserAction::Open,
                Some("Choose"),
                Some("Cancel"),
            );
            let filter = gtk4::FileFilter::new();
            filter.set_name(Some("Images"));
            filter.add_mime_type("image/png");
            filter.add_mime_type("image/jpeg");
            filter.add_mime_type("image/webp");
            chooser.add_filter(&filter);
            let f = f.clone();
            chooser.connect_response(move |chooser, response| {
                if response == ResponseType::Accept {
                    if let Some(path) = chooser.file().and_then(|file| file.path()) {
                        f.set_watermark_image(Some(path));
                    }
                }
            });
            chooser.show();
        });
    }
    {
        let f = form.clone();
        clear_wm.connect_clicked(move |_| f.set_watermark_image(None));
    }
    {
        let (f, win) = (form.clone(), dialog.clone());
        choose_dest.connect_clicked(move |_| {
            let chooser = FileChooserNative::new(
                Some("Select Export Folder"),
                Some(&win),
                FileChooserAction::SelectFolder,
                Some("Select"),
                Some("Cancel"),
            );
            let f = f.clone();
            chooser.connect_response(move |chooser, response| {
                if response == ResponseType::Accept {
                    if let Some(path) = chooser.file().and_then(|file| file.path()) {
                        f.set_destination(path);
                    }
                }
            });
            chooser.show();
        });
    }

    let d = dialog.clone();
    cancel_btn.connect_clicked(move |_| d.close());

    let progress_cb = Rc::new(on_progress);
    let done_cb = Rc::new(on_done);
    let d = dialog.clone();
    export_btn.connect_clicked(move |_| {
        let config = form.read();
        if let Some(wm) = &config.watermark {
            if wm.text.is_none() && wm.image_path.is_none() {
                form.wm_text.grab_focus();
                return;
            }
        }
        let remembered = serde_json::to_string(&config)
            .map_err(|e| e.to_string())
            .and_then(|json| {
                let conn = db.conn().map_err(|e| e.to_string())?;
                queries::set_meta(&conn, LAST_CONFIG_KEY, &json).map_err(|e| e.to_string())
            });
        if let Err(e) = remembered {
            log::warn!("Remembering export settings: {e}");
        }

        d.close();
        on_start(images.len());

        let imgs = images.clone();
        let db = db.clone();
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
                let items = with_keywords(&db, imgs);
                batch_export(&items, config, cancel, move |done, total| {
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

/// `images` with their keywords from the library, for embedding in exports.
fn with_keywords(db: &Database, images: Vec<Image>) -> Vec<ExportItem> {
    let conn = match db.conn() {
        Ok(conn) => Some(conn),
        Err(e) => {
            log::warn!("Exporting without keywords: {e}");
            None
        }
    };
    images
        .into_iter()
        .map(|image| {
            let keywords = match (&conn, image.id) {
                (Some(conn), Some(id)) => match queries::get_tags_for_image(conn, id) {
                    Ok(tags) => tags.into_iter().map(|t| t.name).collect(),
                    Err(e) => {
                        log::warn!("Keywords of {}: {e}", image.path.display());
                        Vec::new()
                    }
                },
                _ => Vec::new(),
            };
            ExportItem { image, keywords }
        })
        .collect()
}
