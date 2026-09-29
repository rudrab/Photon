//! The Mark button: a photo's verdict — pick/reject, stars and colour label —
//! in one button whose face shows the current state (e.g. "✓ ★★★ ●"), with a
//! popover to change any of them. Used in the 1-up viewer and in the library
//! selection bar.
//!
//! Pick/reject stays its own three-way control, not folded into the stars:
//! "keep or discard?" and "how good?" are different questions. The popover
//! stays open while choosing, so a flag, stars and a label can be set in one
//! go. The keys (P X U, 1–5 0, 6–9) are unchanged and shown in it.

use crate::ui::selection_bar::{label_dot, set_label_dot};
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, DrawingArea, Image, Label, MenuButton, Orientation, Popover, ToggleButton};

/// Side of the die face, in pixels.
const DIE: i32 = 16;

/// A die face showing `pips` (1–5), drawn in the widget's text colour so it
/// reads like a symbolic icon. (The Unicode faces ⚀–⚄ depend on the font.)
fn die_face(pips: Rc<Cell<i32>>) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_width(DIE);
    area.set_content_height(DIE);
    area.set_valign(Align::Center);
    area.set_draw_func(move |area, cr, w, h| {
        let c = area.color();
        let (w, h) = (w as f64, h as f64);
        // Rounded outline.
        let (r, inset) = (3.5, 0.75);
        let (x0, y0, x1, y1) = (inset, inset, w - inset, h - inset);
        cr.new_sub_path();
        cr.arc(x1 - r, y0 + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
        cr.arc(x1 - r, y1 - r, r, 0.0, std::f64::consts::FRAC_PI_2);
        cr.arc(x0 + r, y1 - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
        cr.arc(x0 + r, y0 + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
        cr.close_path();
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64 * 0.7);
        cr.set_line_width(1.5);
        let _ = cr.stroke();
        // Pips, as on a die: corners, then the centre for odd numbers.
        let (lo, mid, hi) = (w * 0.3, w * 0.5, w * 0.7);
        let (tlo, thi) = (h * 0.3, h * 0.7);
        let spots: &[(f64, f64)] = match pips.get() {
            1 => &[(mid, h * 0.5)],
            2 => &[(lo, tlo), (hi, thi)],
            3 => &[(lo, tlo), (mid, h * 0.5), (hi, thi)],
            4 => &[(lo, tlo), (hi, tlo), (lo, thi), (hi, thi)],
            5 => &[(lo, tlo), (hi, tlo), (mid, h * 0.5), (lo, thi), (hi, thi)],
            _ => &[],
        };
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64);
        for &(x, y) in spots {
            cr.arc(x, y, w * 0.085, 0.0, 2.0 * std::f64::consts::PI);
            let _ = cr.fill();
        }
    });
    area
}
use photon_core::db::{queries, Database};
use photon_core::models::ColorLabel;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Give photo `item_id`'s whole shot (RAW + JPG, edits) the marks that are
/// `Some`, with undo and the sidecars (only the changed fields). The library
/// write is a few rows, done here so a view refreshed right after shows it;
/// the sidecars are written in the background. Errors are told through
/// `notify`. Returns whether the library was changed.
pub fn save_marks(
    db: &Database,
    undo: Option<&Rc<RefCell<crate::ui::undo::UndoManager>>>,
    notify: &Rc<dyn Fn(&str)>,
    item_id: i64,
    rating: Option<i32>,
    flag: Option<i32>,
    color: Option<ColorLabel>,
) -> bool {
    type Before = (Vec<(i64, i32)>, Vec<(i64, i32)>, Vec<(i64, ColorLabel)>, Vec<i64>);
    let saved = (|| -> Result<Before, String> {
        let mut conn = db.conn().map_err(|e| e.to_string())?;
        let members = queries::shot_member_ids(&conn, &[item_id]).map_err(|e| e.to_string())?;
        let (mut ratings, mut flags, mut colors) = (Vec::new(), Vec::new(), Vec::new());
        for &id in &members {
            if let Some(img) = queries::get_image(&conn, id).map_err(|e| e.to_string())? {
                ratings.push((id, img.rating));
                flags.push((id, img.flagged));
                colors.push((id, img.color_label));
            }
        }
        if let Some(r) = rating {
            queries::batch_set_rating(&mut conn, &members, r).map_err(|e| e.to_string())?;
        }
        if let Some(f) = flag {
            queries::batch_set_flag(&mut conn, &members, f).map_err(|e| e.to_string())?;
        }
        if let Some(c) = color {
            queries::batch_set_color_label(&mut conn, &members, c).map_err(|e| e.to_string())?;
        }
        Ok((ratings, flags, colors, members))
    })();
    let (ratings, flags, colors, members) = match saved {
        Ok(saved) => saved,
        Err(e) => {
            log::error!("Saving the marks of photo {item_id}: {e}");
            notify(&format!("Couldn't save the change: {e}"));
            return false;
        }
    };

    if let Some(um) = undo {
        use crate::ui::undo::UndoAction;
        if let Some(r) = rating {
            um.borrow_mut().push(UndoAction::Rating { previous: ratings, new_rating: r });
        }
        if let Some(f) = flag {
            um.borrow_mut().push(UndoAction::Flag { previous: flags, new_flag: f });
        }
        if let Some(c) = color {
            um.borrow_mut().push(UndoAction::ColorLabel { previous: colors, new_color: c });
        }
    }

    let fields = crate::ui::timeline::XmpFields { cull: rating.is_some() || flag.is_some(), color: color.is_some() };
    let (db, notify) = (db.clone(), notify.clone());
    gtk4::glib::spawn_future_local(async move {
        let failed = gtk4::gio::spawn_blocking(move || {
            db.conn().map(|conn| crate::ui::timeline::sync_cull_to_xmp(&conn, &members, fields)).unwrap_or(1)
        })
        .await
        .unwrap_or(1);
        if failed > 0 {
            notify("The change is saved, but an XMP sidecar couldn't be updated (see the log)");
        }
    });
    true
}

/// A photo's marks as the Mark button shows them.
pub fn state_of(img: &photon_core::models::Image) -> MarkState {
    MarkState { flag: img.flagged, rating: img.rating, color: img.color_label }
}

/// Apply a key press to marks: P X U, 1–5 0, 6–9. `None` if the key isn't one.
pub fn mark_for_key(key: gtk4::gdk::Key) -> Option<(Option<i32>, Option<i32>, Option<ColorLabel>)> {
    use gtk4::gdk::Key;
    Some(match key {
        Key::p | Key::P => (None, Some(1), None),
        Key::x | Key::X => (None, Some(-1), None),
        Key::u | Key::U => (None, Some(0), None),
        Key::_1 | Key::KP_1 => (Some(1), None, None),
        Key::_2 | Key::KP_2 => (Some(2), None, None),
        Key::_3 | Key::KP_3 => (Some(3), None, None),
        Key::_4 | Key::KP_4 => (Some(4), None, None),
        Key::_5 | Key::KP_5 => (Some(5), None, None),
        Key::_0 | Key::KP_0 => (Some(0), None, None),
        Key::_6 | Key::KP_6 => (None, None, Some(ColorLabel::Red)),
        Key::_7 | Key::KP_7 => (None, None, Some(ColorLabel::Yellow)),
        Key::_8 | Key::KP_8 => (None, None, Some(ColorLabel::Green)),
        Key::_9 | Key::KP_9 => (None, None, Some(ColorLabel::Blue)),
        _ => return None,
    })
}

/// A ring that fills with a photo's sharpness relative to what is sharp for
/// it, coloured by grade (green → yellow → orange → red, from the theme's
/// palette through CSS classes); an empty dim ring when not analysed.
#[derive(Clone)]
pub struct QualityGauge {
    pub widget: DrawingArea,
    /// 0–1, or `None` when there is no score.
    fill: Rc<Cell<Option<f64>>>,
}

pub fn quality_gauge() -> QualityGauge {
    let fill: Rc<Cell<Option<f64>>> = Rc::default();
    let widget = DrawingArea::new();
    widget.set_content_width(DIE);
    widget.set_content_height(DIE);
    widget.set_valign(Align::Center);
    widget.add_css_class("photon-quality");
    let f = fill.clone();
    widget.set_draw_func(move |area, cr, w, h| {
        let c = area.color();
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let r = cx.min(cy) - 2.0;
        let rgba = |a: f64| cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64 * a);
        cr.set_line_width(2.5);
        let full = 2.0 * std::f64::consts::PI;
        match f.get() {
            None => {
                rgba(0.35);
                cr.arc(cx, cy, r, 0.0, full);
                let _ = cr.stroke();
            }
            Some(fraction) => {
                rgba(0.25);
                cr.arc(cx, cy, r, 0.0, full);
                let _ = cr.stroke();
                rgba(1.0);
                let start = -std::f64::consts::FRAC_PI_2;
                cr.arc(cx, cy, r, start, start + full * fraction.clamp(0.04, 1.0));
                let _ = cr.stroke();
            }
        }
    });
    QualityGauge { widget, fill }
}

impl QualityGauge {
    /// Show `percent` (of "sharp") in `grade`'s colour, or no score.
    pub fn set(&self, score: Option<(f64, photon_import::quality::Grade)>, tooltip: &str) {
        use photon_import::quality::Grade;
        for class in ["good", "fair", "poor", "bad"] {
            self.widget.remove_css_class(class);
        }
        if let Some((_, grade)) = score {
            self.widget.add_css_class(match grade {
                Grade::Good => "good",
                Grade::Fair => "fair",
                Grade::Poor => "poor",
                Grade::Bad => "bad",
            });
        }
        self.fill.set(score.map(|(percent, _)| percent / 100.0));
        self.widget.set_tooltip_text(Some(tooltip));
        self.widget.queue_draw();
    }
}

/// A photo's marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkState {
    /// 1 pick, 0 none, -1 reject.
    pub flag: i32,
    pub rating: i32,
    pub color: ColorLabel,
}

/// What the user chose in the popover; the caller applies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Flag(i32),
    Rating(i32),
    Color(ColorLabel),
}

#[derive(Clone)]
pub struct MarkButton {
    pub button: MenuButton,
    flag_icon: Image,
    /// ☆ when nothing is marked at all.
    empty: Label,
    /// The rating as a die face (pips), compact where five stars aren't.
    die: DrawingArea,
    die_pips: Rc<Cell<i32>>,
    dot: GtkBox,
    /// Pick, none, reject.
    flags: Vec<(ToggleButton, i32)>,
    star_labels: Vec<Label>,
    colors: Vec<(ToggleButton, ColorLabel)>,
    state: Rc<Cell<Option<MarkState>>>,
    /// Set while the controls follow the state, so they don't mark.
    syncing: Rc<Cell<bool>>,
}

fn row(content: &impl IsA<gtk4::Widget>, keys: &str) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 12);
    content.set_hexpand(true);
    row.append(content);
    let hint = Label::new(Some(keys));
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    row.append(&hint);
    row
}

pub fn mark_button(on_mark: impl Fn(Mark) + 'static) -> MarkButton {
    let on_mark = Rc::new(on_mark);
    let state: Rc<Cell<Option<MarkState>>> = Rc::default();
    let syncing = Rc::new(Cell::new(false));

    // ── The face ────────────────────────────────────────
    let face = GtkBox::new(Orientation::Horizontal, 5);
    let flag_icon = Image::new();
    let empty = Label::new(Some("☆"));
    let die_pips = Rc::new(Cell::new(0));
    let die = die_face(die_pips.clone());
    let dot = label_dot(ColorLabel::None);
    face.append(&flag_icon);
    face.append(&empty);
    face.append(&die);
    face.append(&dot);

    // ── The popover ─────────────────────────────────────
    let list = GtkBox::new(Orientation::Vertical, 10);
    list.set_halign(Align::Start);
    list.set_margin_top(10);
    list.set_margin_bottom(10);
    list.set_margin_start(10);
    list.set_margin_end(10);

    // Flag: one of pick / none / reject.
    let flag_box = GtkBox::new(Orientation::Horizontal, 0);
    flag_box.add_css_class("linked");
    let mut flags: Vec<(ToggleButton, i32)> = Vec::new();
    for (icon, text, value) in [
        ("object-select-symbolic", "Pick", 1),
        ("", "None", 0),
        ("process-stop-symbolic", "Reject", -1),
    ] {
        let content = GtkBox::new(Orientation::Horizontal, 6);
        if !icon.is_empty() {
            content.append(&Image::from_icon_name(icon));
        }
        content.append(&Label::new(Some(text)));
        let toggle = ToggleButton::builder().child(&content).build();
        if let Some((first, _)) = flags.first() {
            toggle.set_group(Some(first));
        }
        let (on_mark, syncing) = (on_mark.clone(), syncing.clone());
        toggle.connect_toggled(move |t| {
            if t.is_active() && !syncing.get() {
                on_mark(Mark::Flag(value));
            }
        });
        flag_box.append(&toggle);
        flags.push((toggle, value));
    }
    list.append(&row(&flag_box, "P · U · X"));

    // Stars: click one to rate; the same one again clears.
    let star_box = GtkBox::new(Orientation::Horizontal, 0);
    let mut star_labels = Vec::new();
    for n in 1..=5 {
        let label = Label::new(Some("☆"));
        let star = Button::builder().child(&label).tooltip_text(format!("{n} star{}", if n > 1 { "s" } else { "" })).build();
        star.add_css_class("flat");
        star.add_css_class("photon-mark-star");
        let (on_mark, state) = (on_mark.clone(), state.clone());
        star.connect_clicked(move |_| {
            let current = state.get().map_or(-1, |s| s.rating);
            on_mark(Mark::Rating(if current == n { 0 } else { n }));
        });
        star_box.append(&star);
        star_labels.push(label);
    }
    list.append(&row(&star_box, "1–5 · 0"));

    // Colour label: none or one of five.
    let color_box = GtkBox::new(Orientation::Horizontal, 2);
    let mut colors: Vec<(ToggleButton, ColorLabel)> = Vec::new();
    for color in [ColorLabel::None, ColorLabel::Red, ColorLabel::Yellow, ColorLabel::Green, ColorLabel::Blue, ColorLabel::Purple] {
        let toggle = ToggleButton::builder().child(&label_dot(color)).tooltip_text(color.display_name()).build();
        toggle.add_css_class("flat");
        toggle.add_css_class("circular");
        if let Some((first, _)) = colors.first() {
            toggle.set_group(Some(first));
        }
        let (on_mark, syncing) = (on_mark.clone(), syncing.clone());
        toggle.connect_toggled(move |t| {
            if t.is_active() && !syncing.get() {
                on_mark(Mark::Color(color));
            }
        });
        color_box.append(&toggle);
        colors.push((toggle, color));
    }
    list.append(&row(&color_box, "6–9"));

    let popover = Popover::new();
    popover.set_child(Some(&list));
    // A child of its own, not a label: no dropdown arrow.
    let button = MenuButton::builder().child(&face).popover(&popover).build();
    button.add_css_class("flat");

    let mark = MarkButton { button, flag_icon, empty, die, die_pips, dot, flags, star_labels, colors, state, syncing };
    mark.set_state(None);
    mark
}

impl MarkButton {
    /// Show `state`; `None` when there is nothing to show (no selection, or
    /// photos whose marks differ).
    pub fn set_state(&self, state: Option<MarkState>) {
        self.state.set(state);
        let s = state.unwrap_or(MarkState { flag: 0, rating: 0, color: ColorLabel::None });

        // Face: one symbol for the verdict. A reject shows ✕ whatever its
        // stars; stars mean it's kept, so they need no ✓; a pick without
        // stars shows ✓; nothing at all shows ☆. (Display only: stars don't
        // set the pick flag, so pick filters keep their meaning.)
        let rejected = s.flag == -1;
        let starred = !rejected && s.rating > 0;
        let picked = !rejected && !starred && s.flag == 1;
        self.flag_icon.set_icon_name(Some(if rejected { "process-stop-symbolic" } else { "object-select-symbolic" }));
        self.flag_icon.set_visible(rejected || picked);
        self.die_pips.set(s.rating);
        self.die.set_visible(starred);
        self.die.queue_draw();
        self.empty.set_visible(!rejected && !starred && !picked && s.color == ColorLabel::None);
        set_label_dot(&self.dot, s.color);
        self.dot.set_visible(s.color != ColorLabel::None);

        let described = match state {
            None => "Mark: pick or reject, stars, colour label".to_string(),
            Some(s) => {
                let flag = match s.flag {
                    1 => "Picked",
                    -1 => "Rejected",
                    _ => "Not flagged",
                };
                let stars = if s.rating > 0 { format!("{} ★", s.rating) } else { "no stars".into() };
                format!("{flag} · {stars} · {} — change", s.color.display_name())
            }
        };
        self.button.set_tooltip_text(Some(&described));

        // Popover controls follow, without marking.
        self.syncing.set(true);
        for (toggle, value) in &self.flags {
            toggle.set_active(state.is_some() && *value == s.flag);
        }
        for (i, label) in self.star_labels.iter().enumerate() {
            label.set_text(if (i as i32) < s.rating { "★" } else { "☆" });
        }
        for (toggle, color) in &self.colors {
            toggle.set_active(state.is_some() && *color == s.color);
        }
        self.syncing.set(false);
    }
}
