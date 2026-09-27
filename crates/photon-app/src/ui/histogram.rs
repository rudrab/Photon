//! Live RGB and Luminance Histogram Widget.
//!
//! Renders real-time, anti-aliased Red, Green, Blue, and Luminance
//! curves using Cairo with EV stops and exposure clipping markers.

use gtk4::cairo::Context;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, DrawingArea, Label, Orientation};
use photon_import::{compute_histogram, HistogramData};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

#[derive(Clone)]
pub struct HistogramWidget {
    pub container: GtkBox,
    data: Rc<RefCell<Option<HistogramData>>>,
    drawing_area: DrawingArea,
}

impl HistogramWidget {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 4);
        container.add_css_class("histogram-container");

        let header_row = GtkBox::new(Orientation::Horizontal, 8);
        let title = Label::new(Some("Histogram"));
        title.set_css_classes(&["caption-heading"]);
        title.set_halign(gtk4::Align::Start);
        header_row.append(&title);
        container.append(&header_row);

        let drawing_area = DrawingArea::new();
        drawing_area.set_content_width(260);
        drawing_area.set_content_height(96);
        drawing_area.set_hexpand(true);

        let data: Rc<RefCell<Option<HistogramData>>> = Rc::new(RefCell::new(None));
        let data_draw = data.clone();

        drawing_area.set_draw_func(move |_, cr, width, height| {
            draw_histogram(cr, width as f64, height as f64, data_draw.borrow().as_ref());
        });

        container.append(&drawing_area);

        Self {
            container,
            data,
            drawing_area,
        }
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Load and compute the histogram asynchronously from an image path.
    pub fn load_for_path(&self, path: &Path) {
        let path = path.to_path_buf();
        let weak_draw = self.drawing_area.downgrade();
        let data = self.data.clone();

        glib::spawn_future_local(async move {
            let hist = gtk4::gio::spawn_blocking(move || compute_histogram(&path)).await;
            if let Ok(Some(h)) = hist {
                *data.borrow_mut() = Some(h);
                if let Some(da) = weak_draw.upgrade() {
                    da.queue_draw();
                }
            }
        });
    }

    #[allow(dead_code)]
    pub fn clear(&self) {
        *self.data.borrow_mut() = None;
        self.drawing_area.queue_draw();
    }
}

fn draw_histogram(cr: &Context, w: f64, h: f64, data: Option<&HistogramData>) {
    // 1. Background rounded rectangle
    cr.save().ok();
    let r = 8.0;
    cr.new_sub_path();
    cr.arc(w - r, r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(w - r, h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(r, h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(r, r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();

    cr.set_source_rgba(0.10, 0.10, 0.10, 0.90);
    cr.fill_preserve().ok();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
    cr.set_line_width(1.0);
    cr.stroke().ok();
    cr.restore().ok();

    // 2. EV stop grid lines (25%, 50%, 75%)
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.06);
    cr.set_line_width(1.0);
    for fraction in [0.25, 0.50, 0.75] {
        let x = (w * fraction).round();
        cr.move_to(x, 2.0);
        cr.line_to(x, h - 2.0);
        cr.stroke().ok();
    }

    let Some(hist) = data else {
        return;
    };

    if hist.max_val == 0 {
        return;
    }

    let draw_channel = |color: (f64, f64, f64, f64), channel: &[u32; 256]| {
        cr.set_source_rgba(color.0, color.1, color.2, color.3);
        cr.move_to(0.0, h);
        for i in 0..256 {
            let x = (i as f64 / 255.0) * w;
            let val = (channel[i] as f64 / hist.max_val as f64).min(1.0);
            let y = h - (val * (h - 8.0));
            cr.line_to(x, y);
        }
        cr.line_to(w, h);
        cr.close_path();
        cr.fill().ok();
    };

    // Draw channels: Red, Green, Blue, and Luminance
    draw_channel((0.92, 0.22, 0.22, 0.35), &hist.r);
    draw_channel((0.22, 0.82, 0.32, 0.35), &hist.g);
    draw_channel((0.22, 0.52, 0.95, 0.35), &hist.b);
    draw_channel((0.90, 0.90, 0.90, 0.25), &hist.lum);

    // Exposure clipping indicators
    if hist.shadow_clip {
        cr.set_source_rgba(0.2, 0.6, 1.0, 0.9);
        cr.rectangle(4.0, 4.0, 6.0, 6.0);
        cr.fill().ok();
    }
    if hist.highlight_clip {
        cr.set_source_rgba(1.0, 0.2, 0.2, 0.9);
        cr.rectangle(w - 10.0, 4.0, 6.0, 6.0);
        cr.fill().ok();
    }
}
