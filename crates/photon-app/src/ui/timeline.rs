//! Virtualized photo timeline, laid out like Google Photos / Immich:
//!
//!   * **Justified rows**: every row fills the width and keeps each photo's
//!     aspect ratio; row height varies slightly around a target.
//!   * **Date sections**: a header row per day.
//!   * **Virtualized**: rows live in a `GtkListView`, so only the rows on
//!     screen have widgets. GTK's `GridView` can't have section headers
//!     (as of GTK 4.22), and forces uniform cells, hence rows-in-a-ListView.
//!   * **Lightweight data**: the layout needs only id/hash/size/date per photo
//!     ([`TimelineItem`]), so 100k photos lay out in milliseconds.
//!   * **Async tiles**: thumbnails decode on worker threads into a small LRU
//!     of textures; a tile shows its ThumbHash placeholder until its texture
//!     is ready, and loads for rows scrolled past quickly are skipped.
//!   * **Selection by photo id**: focus/selection survive live refreshes
//!     (imports insert photos and shift every index after them).
//!   * **Selection mode**: a check circle on each tile (on hover, or always
//!     while selecting) and on each day header; in selection mode a click
//!     toggles a photo instead of opening it. Ctrl/Shift-click work anytime.
//!   * **Drag out**: dragging a tile drags its file (or the whole selection,
//!     if the tile is selected) into Files, a browser, a chat or a mail.
//!   * **Scrubber rail**: a right-edge strip with year marks; dragging jumps
//!     anywhere in the library. Positions come from the exact row heights of
//!     the layout, not from GTK's estimated scroll range.

use chrono::{DateTime, Datelike};
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use gtk4::{
    graphene, Align, Box as GtkBox, DrawingArea, EventControllerKey, EventControllerMotion,
    EventControllerScroll, EventControllerScrollFlags, Fixed, GestureClick, GestureDrag, Label,
    Button, DragSource, ListItem, ListScrollFlags, ListView, NoSelection, Orientation, Overlay,
    Picture,
    PropagationPhase, Revealer, ScrolledWindow, SignalListItemFactory,
};
use photon_core::db::{queries, Database};
use photon_core::models::{Image, TimelineItem};
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::thread;
use std::time::{Duration, Instant};

/// Space between tiles and between rows, in logical pixels.
const GAP: i32 = 4;
/// Space between the timeline and the window edges.
const MARGIN: i32 = 16;
/// How many decoded thumbnails to keep (≈1.2 MB each at 640×480).
const TEXTURE_CACHE_SIZE: usize = 160;
/// A tile must stay on screen this long before its thumbnail is decoded, so
/// flinging through thousands of rows doesn't queue thousands of decodes.
const LOAD_DELAY: Duration = Duration::from_millis(40);
/// Fixed height of a date header row.
const HEADER_HEIGHT: i32 = 48;
/// Scrubber rail: pointer-sensitive strip width, marks area width, and the
/// inset of the rail's range from the top/bottom edges.
const RAIL_WIDTH: i32 = 32;
const MARKS_WIDTH: i32 = 132;
const RAIL_INSET: f64 = 16.0;
const RAIL_THUMB_WIDTH: i32 = 20;
/// How long the rail's year marks stay visible after the last activity.
const RAIL_LINGER: Duration = Duration::from_millis(1200);
/// Clicks within this many pixels of a tile's top-left corner hit its check circle.
const CHECK_HIT: f64 = 44.0;

// ---------------------------------------------------------------------------
// Layout (pure: no GTK, unit-tested)
// ---------------------------------------------------------------------------

/// One tile: which photo, and its size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub index: usize,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header(String),
    Photos(Vec<Tile>),
}

/// Lay `items` out in justified rows of `width` pixels, aiming for rows
/// `target_height` tall, with a header row starting each day if `by_day`.
pub fn layout(items: &[TimelineItem], width: i32, target_height: i32, by_day: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let end = if by_day {
            let day = day_key(&items[start]);
            start + items[start..].iter().take_while(|i| day_key(i) == day).count()
        } else {
            items.len()
        };
        if by_day {
            rows.push(Row::Header(section_title(&items[start], end - start)));
        }
        justify(items, start..end, width, target_height, &mut rows);
        start = end;
    }
    rows
}

/// Given current rows and a focused item index, find the best tile index
/// when moving vertically (up if `!down`, down if `down`).
///
/// Skips date header rows and selects the tile in the adjacent photo row
/// whose horizontal center is closest to the current tile's horizontal center.
pub fn navigate_2d(rows: &[Row], current_index: usize, down: bool) -> Option<usize> {
    // 1. Locate row containing current_index and calculate its horizontal center.
    let mut current_row_idx = None;
    let mut current_tile_center = 0.0;

    for (r_idx, row) in rows.iter().enumerate() {
        if let Row::Photos(tiles) = row {
            let mut x = 0;
            for tile in tiles {
                if tile.index == current_index {
                    current_row_idx = Some(r_idx);
                    current_tile_center = x as f64 + (tile.width as f64) / 2.0;
                    break;
                }
                x += tile.width + GAP;
            }
            if current_row_idx.is_some() {
                break;
            }
        }
    }

    let r_idx = current_row_idx?;

    // 2. Search in target direction skipping Row::Header rows
    let is_photo_row = |i: &usize| matches!(&rows[*i], Row::Photos(t) if !t.is_empty());
    let target_row_idx = if down {
        ((r_idx + 1)..rows.len()).find(is_photo_row)?
    } else {
        (0..r_idx).rev().find(is_photo_row)?
    };

    // 3. Find the tile with closest horizontal center
    if let Row::Photos(target_tiles) = &rows[target_row_idx] {
        let mut best_tile_index = None;
        let mut best_dist = f64::MAX;
        let mut x = 0;
        for tile in target_tiles {
            let center = x as f64 + (tile.width as f64) / 2.0;
            let dist = (center - current_tile_center).abs();
            if dist < best_dist {
                best_dist = dist;
                best_tile_index = Some(tile.index);
            }
            x += tile.width + GAP;
        }
        best_tile_index
    } else {
        None
    }
}

/// For every row, the index of the header row of its section (if any).
fn header_of_rows(rows: &[Row]) -> Vec<Option<usize>> {
    let mut current = None;
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            if matches!(row, Row::Header(_)) {
                current = Some(i);
            }
            current
        })
        .collect()
}

/// Content y of every row and the total height. Exact, because header rows
/// have a fixed height and photo rows are their tiles' height plus the gap.
fn row_tops(rows: &[Row]) -> (Vec<f64>, f64) {
    let mut y = 0.0;
    let tops = rows
        .iter()
        .map(|row| {
            let top = y;
            y += match row {
                Row::Header(_) => HEADER_HEIGHT,
                Row::Photos(tiles) => tiles.first().map_or(0, |t| t.height) + GAP,
            } as f64;
            top
        })
        .collect();
    (tops, y)
}

/// Which year marks to label when they don't all fit: `marks` are
/// `(year, y)` in rail order; a year's weight is its extent on the rail (up to
/// the next mark, or `end`). Heavier years win, and no two chosen labels are
/// closer than `min_gap`. Returned in rail order.
fn pick_rail_marks(marks: &[(i32, f64)], end: f64, min_gap: f64) -> Vec<(i32, f64)> {
    let weight = |i: usize| marks.get(i + 1).map_or(end, |&(_, y)| y) - marks[i].1;
    let mut order: Vec<usize> = (0..marks.len()).collect();
    order.sort_by(|&a, &b| weight(b).total_cmp(&weight(a)));

    let mut chosen: Vec<usize> = Vec::new();
    for i in order {
        if chosen.iter().all(|&c| (marks[c].1 - marks[i].1).abs() >= min_gap) {
            chosen.push(i);
        }
    }
    chosen.sort_unstable();
    chosen.into_iter().map(|i| marks[i]).collect()
}

/// x of the rail thumb inside the marks area (centered on the rail strip).
fn thumb_x() -> f64 {
    (MARKS_WIDTH - RAIL_WIDTH / 2 - RAIL_THUMB_WIDTH / 2) as f64
}

/// Photo ids at `indices` of `items`.
fn ids_at(items: &[TimelineItem], indices: impl IntoIterator<Item = usize>) -> Vec<i64> {
    indices
        .into_iter()
        .filter_map(|i| items.get(i).map(|item| item.id))
        .collect()
}

/// Where each of `ids` now sits in `items` (ids no longer present are dropped).
fn indices_of(items: &[TimelineItem], ids: &[i64]) -> Vec<usize> {
    let position: HashMap<i64, usize> =
        items.iter().enumerate().map(|(i, item)| (item.id, i)).collect();
    ids.iter().filter_map(|id| position.get(id).copied()).collect()
}

fn justify(
    items: &[TimelineItem],
    range: std::ops::Range<usize>,
    width: i32,
    target_height: i32,
    rows: &mut Vec<Row>,
) {
    let width = width.max(1) as f64;
    let target = target_height.max(1) as f64;
    // Panoramas and slivers would make rows absurd; clamp like Google/Flickr.
    let aspect = |i: usize| items[i].display_aspect().clamp(0.4, 3.5);

    let mut row: Vec<usize> = Vec::new();
    let mut aspect_sum = 0.0;
    for i in range {
        row.push(i);
        aspect_sum += aspect(i);
        let gaps = GAP as f64 * (row.len() - 1) as f64;
        let height = (width - gaps) / aspect_sum;
        if height <= target {
            rows.push(Row::Photos(size_tiles(&row, height, Some(width), aspect)));
            row.clear();
            aspect_sum = 0.0;
        }
    }
    if !row.is_empty() {
        // A short last row keeps the target height instead of being blown up.
        rows.push(Row::Photos(size_tiles(&row, target, None, aspect)));
    }
}

fn size_tiles(
    indices: &[usize],
    height: f64,
    fill_width: Option<f64>,
    aspect: impl Fn(usize) -> f64,
) -> Vec<Tile> {
    let h = height.round().max(1.0) as i32;
    let mut tiles: Vec<Tile> = indices
        .iter()
        .map(|&index| Tile {
            index,
            width: (aspect(index) * height).round().max(1.0) as i32,
            height: h,
        })
        .collect();
    // Absorb rounding error in the last tile so rows end exactly flush.
    if let (Some(width), Some(last)) = (fill_width, tiles.len().checked_sub(1)) {
        let used: i32 = tiles.iter().map(|t| t.width).sum::<i32>() + GAP * last as i32;
        tiles[last].width = (tiles[last].width + width as i32 - used).max(1);
    }
    tiles
}

/// Camera wall-clock day; `created_at` is stored as naive time in UTC.
fn day_key(item: &TimelineItem) -> Option<i64> {
    item.created_at.map(|ts| ts.div_euclid(86_400))
}

fn section_title(first: &TimelineItem, count: usize) -> String {
    let date = first
        .created_at
        .and_then(|ts| DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.format("%A, %B %-d, %Y").to_string())
        .unwrap_or_else(|| "Unknown Date".to_string());
    let noun = if count == 1 { "photo" } else { "photos" };
    format!("{date}  ·  {count} {noun}")
}

// ---------------------------------------------------------------------------
// Texture cache
// ---------------------------------------------------------------------------

/// Least-recently-used decoded thumbnails, keyed by content hash.
struct TextureCache {
    map: HashMap<String, (gdk::Texture, u64)>,
    tick: u64,
}

impl TextureCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
        }
    }

    fn get(&mut self, hash: &str) -> Option<gdk::Texture> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(hash).map(|(tex, used)| {
            *used = tick;
            tex.clone()
        })
    }

    fn put(&mut self, hash: String, texture: gdk::Texture) {
        self.tick += 1;
        self.map.insert(hash, (texture, self.tick));
        if self.map.len() > TEXTURE_CACHE_SIZE {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, hash: &str) {
        self.map.remove(hash);
    }
}

// ---------------------------------------------------------------------------
// Widget
// ---------------------------------------------------------------------------

type ActivateFn = Box<dyn Fn(usize)>;
type SelectionFn = Box<dyn Fn(&HashSet<usize>)>;
type ModeFn = Box<dyn Fn(bool)>;
type DragFilterFn = Box<dyn Fn(Vec<Image>) -> Vec<Image>>;

struct Inner {
    root: Overlay,
    scrolled: ScrolledWindow,
    list: ListView,
    store: gio::ListStore,
    items: RefCell<Rc<Vec<TimelineItem>>>,
    rows: RefCell<Vec<Row>>,
    /// For each row, the index of its section's header row.
    header_of_row: RefCell<Vec<Option<usize>>>,
    /// Every row widget the list has created (the recycling pool).
    list_items: RefCell<Vec<glib::WeakRef<ListItem>>>,
    /// Scroll changes before this instant are programmatic (restores after a
    /// refresh), not the user scrolling: don't flash the date label.
    quiet_until: Cell<Option<Instant>>,
    date_update_pending: Cell<bool>,
    /// Content y of every row (exact: header and tile heights are fixed).
    row_tops: RefCell<Vec<f64>>,
    content_height: Cell<f64>,
    view_height: Cell<i32>,
    rail_marks: Fixed,
    rail_thumb: GtkBox,
    rail_hovered: Cell<bool>,
    rail_dragging: Cell<bool>,
    rail_drag_start: Cell<f64>,
    rail_hide_timer: RefCell<Option<glib::SourceId>>,
    jump_target: Cell<Option<usize>>,
    focused_index: Cell<Option<usize>>,
    selected_indices: RefCell<HashSet<usize>>,
    anchor_index: Cell<Option<usize>>,
    visible_tiles: RefCell<HashMap<usize, glib::WeakRef<GtkBox>>>,
    date_badge: Revealer,
    date_label: Label,
    hide_badge_timer: RefCell<Option<glib::SourceId>>,
    by_day: Cell<bool>,
    width: Cell<i32>,
    row_height: Cell<i32>,
    relayout_generation: Cell<u64>,
    cache_dir: PathBuf,
    textures: RefCell<TextureCache>,
    db: RefCell<Option<Database>>,
    on_activate: RefCell<Option<ActivateFn>>,
    on_selection_changed: RefCell<Option<SelectionFn>>,
    /// Clicks toggle photos instead of opening them.
    selection_mode: Cell<bool>,
    on_selection_mode_changed: RefCell<Option<ModeFn>>,
    /// Picks which files a drag carries for the dragged photos.
    drag_filter: RefCell<Option<DragFilterFn>>,
    undo_manager: RefCell<Option<Rc<RefCell<crate::ui::undo::UndoManager>>>>,
    on_refresh: RefCell<Option<Rc<dyn Fn()>>>,
    on_start_slideshow: RefCell<Option<Rc<dyn Fn()>>>,
}

/// The virtualized timeline widget. Cheap to clone (shared handle).
#[derive(Clone)]
pub struct Timeline {
    inner: Rc<Inner>,
}

/// A handle that doesn't keep the timeline alive; for closures owned by
/// widgets inside the timeline (which would otherwise form a cycle).
#[derive(Clone)]
pub struct WeakTimeline(Weak<Inner>);

impl WeakTimeline {
    pub fn upgrade(&self) -> Option<Timeline> {
        self.0.upgrade().map(|inner| Timeline { inner })
    }
}

impl Timeline {
    pub fn new(cache_dir: PathBuf, row_height: i32) -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let factory = SignalListItemFactory::new();
        let list = ListView::new(Some(NoSelection::new(Some(store.clone()))), Some(factory.clone()));
        list.add_css_class("photon-timeline");

        // `External`, not `Never`: rows are exactly as wide as the view, and
        // with `Never` their width requests would feed back into the view's
        // own size (every relayout making the window wider).
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::External)
            .vexpand(true)
            .hexpand(true)
            .focusable(true)
            .child(&list)
            .build();
        // Rows and tiles are not focus targets; focus is tracked by photo.
        list.set_focusable(false);

        // Floating date indicator badge on scroll
        let date_badge = Revealer::new();
        date_badge.set_transition_type(gtk4::RevealerTransitionType::Crossfade);
        date_badge.set_reveal_child(false);
        date_badge.set_halign(Align::End);
        date_badge.set_valign(Align::Start);
        date_badge.set_margin_top(14);
        date_badge.set_margin_end(28);
        date_badge.set_can_target(false);

        let badge_box = GtkBox::new(Orientation::Horizontal, 6);
        badge_box.add_css_class("card");
        badge_box.add_css_class("pill");
        badge_box.set_margin_top(2);
        badge_box.set_margin_bottom(2);
        badge_box.set_margin_start(4);
        badge_box.set_margin_end(4);

        let date_label = Label::new(None);
        date_label.set_css_classes(&["heading"]);
        badge_box.append(&date_label);
        date_badge.set_child(Some(&badge_box));

        // GTK4 has no resize signal on ordinary widgets; an invisible,
        // input-transparent DrawingArea overlaid on the view reports it.
        let size_probe = DrawingArea::new();
        size_probe.set_can_target(false);
        // Scrubber: year marks + position thumb (never takes input), under a
        // transparent strip that does. The native scrollbar is hidden; the
        // thumb shows the position instead.
        scrolled.set_vscrollbar_policy(gtk4::PolicyType::External);
        let rail_marks = Fixed::new();
        rail_marks.set_halign(Align::End);
        rail_marks.set_valign(Align::Fill);
        rail_marks.set_size_request(MARKS_WIDTH, -1);
        rail_marks.set_can_target(false);
        rail_marks.add_css_class("photon-scrubber-marks");
        let rail_thumb = GtkBox::new(Orientation::Horizontal, 0);
        rail_thumb.set_size_request(RAIL_THUMB_WIDTH, 4);
        rail_thumb.add_css_class("photon-scrubber-thumb");
        rail_marks.put(&rail_thumb, thumb_x(), RAIL_INSET);
        let rail = GtkBox::new(Orientation::Vertical, 0);
        rail.set_halign(Align::End);
        rail.set_valign(Align::Fill);
        rail.set_size_request(RAIL_WIDTH, -1);
        rail.set_cursor_from_name(Some("pointer"));
        rail.set_tooltip_text(Some("Drag to jump through your library"));

        let root = Overlay::new();
        root.set_focusable(true);
        root.set_child(Some(&scrolled));
        root.add_overlay(&size_probe);
        root.add_overlay(&date_badge);
        root.add_overlay(&rail_marks);
        root.add_overlay(&rail);

        let inner = Rc::new(Inner {
            root,
            scrolled,
            list,
            store,
            items: RefCell::new(Rc::new(Vec::new())),
            rows: RefCell::new(Vec::new()),
            header_of_row: RefCell::new(Vec::new()),
            list_items: RefCell::new(Vec::new()),
            quiet_until: Cell::new(None),
            date_update_pending: Cell::new(false),
            row_tops: RefCell::new(Vec::new()),
            content_height: Cell::new(0.0),
            view_height: Cell::new(0),
            rail_marks,
            rail_thumb,
            rail_hovered: Cell::new(false),
            rail_dragging: Cell::new(false),
            rail_drag_start: Cell::new(0.0),
            rail_hide_timer: RefCell::new(None),
            jump_target: Cell::new(None),
            focused_index: Cell::new(None),
            selected_indices: RefCell::new(HashSet::new()),
            anchor_index: Cell::new(None),
            visible_tiles: RefCell::new(HashMap::new()),
            date_badge,
            date_label,
            hide_badge_timer: RefCell::new(None),
            by_day: Cell::new(true),
            width: Cell::new(0),
            row_height: Cell::new(row_height),
            relayout_generation: Cell::new(0),
            cache_dir,
            textures: RefCell::new(TextureCache::new()),
            db: RefCell::new(None),
            on_activate: RefCell::new(None),
            on_selection_changed: RefCell::new(None),
            selection_mode: Cell::new(false),
            on_selection_mode_changed: RefCell::new(None),
            drag_filter: RefCell::new(None),
            undo_manager: RefCell::new(None),
            on_refresh: RefCell::new(None),
            on_start_slideshow: RefCell::new(None),
        });

        // Key controller for 2D keyboard navigation and multi-selection
        let key_controller = EventControllerKey::new();
        // Capture phase: arrows must reach us before the inner ListView's own
        // cursor bindings, wherever focus is inside the view.
        key_controller.set_propagation_phase(PropagationPhase::Capture);
        let weak_key = Rc::downgrade(&inner);
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if let Some(inner) = weak_key.upgrade() {
                Inner::handle_key_pressed(&inner, keyval, state)
            } else {
                glib::Propagation::Proceed
            }
        });
        inner.scrolled.add_controller(key_controller);

        // Scroll controller for Ctrl+Wheel zoom
        let scroll_controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        let weak_zoom = Rc::downgrade(&inner);
        scroll_controller.connect_scroll(move |controller, _dx, dy| {
            let state = controller.current_event_state();
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                if let Some(inner) = weak_zoom.upgrade() {
                    let old_h = inner.row_height.get();
                    let delta = if dy < 0.0 { 20 } else { -20 };
                    let new_h = (old_h + delta).clamp(110, 500);
                    if new_h != old_h {
                        inner.row_height.set(new_h);
                        let fraction = scroll_fraction(&inner.scrolled.vadjustment());
                        inner.relayout();
                        inner.restore_scroll_fraction(fraction);
                    }
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        inner.scrolled.add_controller(scroll_controller);

        // Floating date label while the user scrolls.
        let weak_scroll = Rc::downgrade(&inner);
        inner.scrolled.vadjustment().connect_value_changed(move |_| {
            let Some(inner) = weak_scroll.upgrade() else { return };
            if inner.date_update_pending.replace(true) {
                return;
            }
            // Rows at the new position are bound during the next layout; idle
            // priority runs after it. One update per frame, however many
            // scroll events arrive.
            let weak = Rc::downgrade(&inner);
            glib::idle_add_local_once(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.date_update_pending.set(false);
                    Inner::on_scrolled(&inner);
                }
            });
        });

        let weak = Rc::downgrade(&inner);
        size_probe.connect_resize(move |_, width, height| {
            if let Some(inner) = weak.upgrade() {
                if inner.view_height.replace(height) != height {
                    inner.place_rail_marks();
                }
                Inner::schedule_relayout(&inner, width - 2 * MARGIN);
            }
        });

        // Rail input: hover reveals the marks, press/drag scrubs.
        let motion = EventControllerMotion::new();
        let weak = Rc::downgrade(&inner);
        motion.connect_enter(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                inner.rail_hovered.set(true);
                Inner::activate_rail(&inner);
            }
        });
        let weak = Rc::downgrade(&inner);
        motion.connect_leave(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.rail_hovered.set(false);
                Inner::activate_rail(&inner);
            }
        });
        rail.add_controller(motion);

        let drag = GestureDrag::new();
        let weak = Rc::downgrade(&inner);
        drag.connect_drag_begin(move |_, _, y| {
            if let Some(inner) = weak.upgrade() {
                inner.rail_dragging.set(true);
                inner.rail_drag_start.set(y);
                Inner::scrub_to(&inner, y);
            }
        });
        let weak = Rc::downgrade(&inner);
        drag.connect_drag_update(move |_, _, dy| {
            if let Some(inner) = weak.upgrade() {
                Inner::scrub_to(&inner, inner.rail_drag_start.get() + dy);
            }
        });
        let weak = Rc::downgrade(&inner);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                inner.rail_dragging.set(false);
                Inner::activate_rail(&inner);
            }
        });
        rail.add_controller(drag);

        let weak = Rc::downgrade(&inner);
        factory.connect_setup(move |_, obj| {
            let item = obj.downcast_ref::<gtk4::ListItem>().expect("ListItem");
            if let Some(inner) = weak.upgrade() {
                inner.list_items.borrow_mut().push(item.downgrade());
            }
            item.set_activatable(false);
            item.set_selectable(false);
            item.set_focusable(false);
            let row = GtkBox::new(Orientation::Horizontal, GAP);
            row.set_margin_start(MARGIN);
            row.set_margin_end(MARGIN);
            item.set_child(Some(&row));
        });
        let weak = Rc::downgrade(&inner);
        factory.connect_bind(move |_, obj| {
            let item = obj.downcast_ref::<gtk4::ListItem>().expect("ListItem");
            let (Some(inner), Some(row_box), Some(data)) = (
                weak.upgrade(),
                item.child().and_downcast::<GtkBox>(),
                item.item().and_downcast::<glib::BoxedAnyObject>(),
            ) else {
                return;
            };
            Inner::bind_row(&inner, &row_box, &data.borrow::<Row>(), item.position() as usize);
        });
        factory.connect_unbind(|_, obj| {
            let item = obj.downcast_ref::<gtk4::ListItem>().expect("ListItem");
            if let Some(row_box) = item.child().and_downcast::<GtkBox>() {
                clear_children(&row_box);
            }
        });

        Self { inner }
    }

    pub fn widget(&self) -> &Overlay {
        &self.inner.root
    }

    pub fn downgrade(&self) -> WeakTimeline {
        WeakTimeline(Rc::downgrade(&self.inner))
    }

    /// Called with the photo's index into the current items when a tile is clicked or activated.
    pub fn connect_activate(&self, f: impl Fn(usize) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Box::new(f));
    }

    /// Called when the set of selected items changes.
    pub fn connect_selection_changed(&self, f: impl Fn(&HashSet<usize>) + 'static) {
        *self.inner.on_selection_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Return the currently selected item indices.
    #[allow(dead_code)]
    pub fn selected_indices(&self) -> HashSet<usize> {
        self.inner.selected_indices.borrow().clone()
    }

    /// Ids of the selected photos. Use these (not indices) for batch actions:
    /// they stay correct even if the timeline refreshes in between.
    pub fn selected_ids(&self) -> Vec<i64> {
        let items = self.inner.items.borrow();
        let selected = self.inner.selected_indices.borrow();
        ids_at(&items, selected.iter().copied())
    }

    /// Clear all selections.
    pub fn clear_selection(&self) {
        self.inner.selected_indices.borrow_mut().clear();
        Inner::update_tile_styles(&self.inner);
    }

    /// Enter or leave selection mode. Leaving it clears the selection.
    pub fn set_selection_mode(&self, on: bool) {
        Inner::set_selection_mode(&self.inner, on);
    }

    pub fn selection_mode(&self) -> bool {
        self.inner.selection_mode.get()
    }

    /// Decide which files a drag carries for the dragged photos (e.g. a
    /// RAW+JPG shot's JPG only).
    pub fn set_drag_filter(&self, f: impl Fn(Vec<Image>) -> Vec<Image> + 'static) {
        *self.inner.drag_filter.borrow_mut() = Some(Box::new(f));
    }

    /// Called when selection mode is entered or left.
    pub fn connect_selection_mode_changed(&self, f: impl Fn(bool) + 'static) {
        *self.inner.on_selection_mode_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Select all photos in the timeline.
    #[allow(dead_code)]
    pub fn select_all(&self) {
        let n_items = self.inner.items.borrow().len();
        let mut sel = self.inner.selected_indices.borrow_mut();
        sel.clear();
        for i in 0..n_items {
            sel.insert(i);
        }
        drop(sel);
        Inner::update_tile_styles(&self.inner);
    }

    /// Currently focused item index.
    #[allow(dead_code)]
    pub fn focused_index(&self) -> Option<usize> {
        self.inner.focused_index.get()
    }

    /// Show `items`. With `keep_scroll`, the view stays where it was (used for
    /// live refreshes during an import); otherwise it starts at the top.
    ///
    /// Focus and selection follow the *photos*, not their positions: a live
    /// refresh that inserts photos above them must not move the selection
    /// onto different photos.
    pub fn set_items(&self, items: Rc<Vec<TimelineItem>>, by_day: bool, keep_scroll: bool) {
        let inner = &self.inner;
        if keep_scroll {
            let old = inner.items.borrow().clone();
            let selected = ids_at(&old, inner.selected_indices.borrow().iter().copied());
            let focused = ids_at(&old, inner.focused_index.get());
            let anchor = ids_at(&old, inner.anchor_index.get());

            *inner.selected_indices.borrow_mut() = indices_of(&items, &selected).into_iter().collect();
            inner.focused_index.set(indices_of(&items, &focused).first().copied());
            inner.anchor_index.set(indices_of(&items, &anchor).first().copied());
        } else {
            inner.focused_index.set(None);
            inner.anchor_index.set(None);
            inner.selected_indices.borrow_mut().clear();
        }
        *inner.items.borrow_mut() = items;
        inner.by_day.set(by_day);

        let adj = inner.scrolled.vadjustment();
        let value = if keep_scroll { adj.value() } else { 0.0 };
        inner.relayout();
        inner.restore_scroll(value);
        Inner::update_tile_styles(inner);
    }

    pub fn set_row_height(&self, height: i32) {
        let clamped = height.clamp(110, 500);
        if self.inner.row_height.replace(clamped) != clamped {
            let fraction = scroll_fraction(&self.inner.scrolled.vadjustment());
            self.inner.relayout();
            self.inner.restore_scroll_fraction(fraction);
        }
    }

    #[allow(dead_code)]
    pub fn row_height(&self) -> i32 {
        self.inner.row_height.get()
    }

    pub fn set_db(&self, db: Database) {
        *self.inner.db.borrow_mut() = Some(db);
    }

    pub fn cull_rating_selected(&self, rating: i32) {
        Inner::cull_rating(&self.inner, rating);
    }

    pub fn cull_flag_selected(&self, flag: i32) {
        Inner::cull_flag(&self.inner, flag);
    }

    pub fn rotate_selected(&self, cw: bool) {
        Inner::rotate_selected(&self.inner, cw);
    }

    pub fn selected_items(&self) -> Vec<TimelineItem> {
        let items = self.inner.items.borrow();
        let selected = self.inner.selected_indices.borrow();
        let mut res = Vec::new();
        for &idx in selected.iter() {
            if let Some(item) = items.get(idx) {
                res.push(item.clone());
            }
        }
        res
    }

    pub fn set_undo_manager(&self, um: Rc<RefCell<crate::ui::undo::UndoManager>>) {
        *self.inner.undo_manager.borrow_mut() = Some(um);
    }

    pub fn connect_refresh(&self, f: impl Fn() + 'static) {
        *self.inner.on_refresh.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_start_slideshow(&self, f: impl Fn() + 'static) {
        *self.inner.on_start_slideshow.borrow_mut() = Some(Rc::new(f));
    }
}

impl Inner {
    fn set_selection_mode(this: &Rc<Self>, on: bool) {
        if this.selection_mode.replace(on) == on {
            return;
        }
        if on {
            this.root.add_css_class("photon-selecting");
        } else {
            this.root.remove_css_class("photon-selecting");
            this.selected_indices.borrow_mut().clear();
        }
        if let Some(cb) = this.on_selection_mode_changed.borrow().as_ref() {
            cb(on);
        }
        Self::update_tile_styles(this);
    }

    /// Toggle photo `index` in the selection (entering selection mode).
    fn toggle_selected(this: &Rc<Self>, index: usize) {
        this.focused_index.set(Some(index));
        this.anchor_index.set(Some(index));
        {
            let mut sel = this.selected_indices.borrow_mut();
            if !sel.remove(&index) {
                sel.insert(index);
            }
        }
        Self::set_selection_mode(this, true);
        Self::update_tile_styles(this);
    }

    /// Photo indices of the day section whose header is row `header`.
    fn section_indices(&self, header: usize) -> Vec<usize> {
        self.rows
            .borrow()
            .iter()
            .skip(header + 1)
            .take_while(|row| matches!(row, Row::Photos(_)))
            .flat_map(|row| match row {
                Row::Photos(tiles) => tiles.iter().map(|t| t.index).collect(),
                Row::Header(_) => Vec::new(),
            })
            .collect()
    }

    /// Select every photo of a day, or deselect them if all already are.
    fn toggle_section(this: &Rc<Self>, header: usize) {
        let indices = this.section_indices(header);
        {
            let mut sel = this.selected_indices.borrow_mut();
            if indices.iter().all(|i| sel.contains(i)) {
                for i in &indices {
                    sel.remove(i);
                }
            } else {
                sel.extend(indices.iter().copied());
            }
        }
        Self::set_selection_mode(this, true);
        Self::update_tile_styles(this);
    }

    /// Files to drag when a drag starts on photo `index`: the whole
    /// selection if the photo is part of it, else just that photo.
    fn drag_files(&self, index: usize) -> Vec<gio::File> {
        let ids = {
            let items = self.items.borrow();
            let selected = self.selected_indices.borrow();
            if selected.contains(&index) {
                let mut indices: Vec<usize> = selected.iter().copied().collect();
                indices.sort_unstable();
                ids_at(&items, indices)
            } else {
                ids_at(&items, [index])
            }
        };
        let images: Vec<Image> = {
            let db = self.db.borrow();
            let Some(conn) = db.as_ref().and_then(|db| db.conn().ok()) else { return Vec::new() };
            ids.into_iter()
                .filter_map(|id| queries::get_image(&conn, id).ok().flatten())
                .collect()
        };
        let images = match self.drag_filter.borrow().as_ref() {
            Some(filter) => filter(images),
            None => images,
        };
        images.into_iter().map(|img| gio::File::for_path(img.path)).collect()
    }

    /// Debounce width changes: relayout once the window stops resizing.
    fn schedule_relayout(this: &Rc<Self>, width: i32) {
        if width <= 0 || width == this.width.get() {
            return;
        }
        let first_layout = this.width.get() == 0;
        let generation = this.relayout_generation.get() + 1;
        this.relayout_generation.set(generation);

        let weak: Weak<Self> = Rc::downgrade(this);
        let apply = move || {
            let Some(inner) = weak.upgrade() else { return };
            if inner.relayout_generation.get() != generation {
                return;
            }
            inner.width.set(width);
            let fraction = scroll_fraction(&inner.scrolled.vadjustment());
            inner.relayout();
            inner.restore_scroll_fraction(fraction);
        };
        if first_layout {
            apply();
        } else {
            glib::timeout_add_local_once(Duration::from_millis(80), apply);
        }
    }

    fn relayout(&self) {
        let width = self.width.get();
        if width <= 0 {
            return; // laid out on first resize
        }
        let rows = layout(&self.items.borrow(), width, self.row_height.get(), self.by_day.get());
        *self.header_of_row.borrow_mut() = header_of_rows(&rows);
        let (tops, height) = row_tops(&rows);
        *self.row_tops.borrow_mut() = tops;
        self.content_height.set(height);
        // Every row is rebound after the splice; stale tile refs would point
        // at old positions.
        self.visible_tiles.borrow_mut().clear();

        let objects: Vec<glib::BoxedAnyObject> =
            rows.iter().cloned().map(glib::BoxedAnyObject::new).collect();
        *self.rows.borrow_mut() = rows;
        self.store.splice(0, self.store.n_items(), &objects);
        self.place_rail_marks();
    }

    fn update_tile_styles(this: &Rc<Self>) {
        let focus = this.focused_index.get();
        let selected = this.selected_indices.borrow();
        let items = this.items.borrow();
        let mut visible = this.visible_tiles.borrow_mut();
        visible.retain(|&idx, weak_box| {
            if let Some(frame) = weak_box.upgrade() {
                if selected.contains(&idx) {
                    frame.add_css_class("photon-tile-selected");
                } else {
                    frame.remove_css_class("photon-tile-selected");
                }

                if focus == Some(idx) {
                    frame.add_css_class("photon-tile-focused");
                } else {
                    frame.remove_css_class("photon-tile-focused");
                }

                if let Some(item) = items.get(idx) {
                    if item.flagged == -1 {
                        frame.add_css_class("photon-tile-rejected");
                        frame.remove_css_class("photon-tile-pick");
                    } else if item.flagged == 1 {
                        frame.add_css_class("photon-tile-pick");
                        frame.remove_css_class("photon-tile-rejected");
                    } else {
                        frame.remove_css_class("photon-tile-rejected");
                        frame.remove_css_class("photon-tile-pick");
                    }

                    if let Some(overlay) = frame.first_child().and_downcast::<Overlay>() {
                        if let Some(badges_box) = overlay.last_child().and_downcast::<GtkBox>() {
                            rebuild_tile_badges(&badges_box, item.rating, item.flagged, item.is_video, item.duration.as_deref(), item.missing);
                        }
                    }
                }
                true
            } else {
                false
            }
        });
        if let Some(cb) = this.on_selection_changed.borrow().as_ref() {
            cb(&selected);
        }
    }

    fn advance_focus(this: &Rc<Self>) {
        let n_items = this.items.borrow().len();
        if let Some(focus) = this.focused_index.get() {
            let next_idx = (focus + 1).min(n_items.saturating_sub(1));
            Self::move_focus(this, next_idx, false, false);
        }
    }

    fn cull_rating(this: &Rc<Self>, rating: i32) {
        let selected: Vec<usize> = {
            let sel = this.selected_indices.borrow();
            if sel.is_empty() {
                if let Some(focus) = this.focused_index.get() {
                    vec![focus]
                } else {
                    Vec::new()
                }
            } else {
                sel.iter().copied().collect()
            }
        };
        if selected.is_empty() {
            return;
        }

        let mut ids = Vec::new();
        let mut previous = Vec::new();
        {
            let mut items_clone = (**this.items.borrow()).clone();
            for &idx in &selected {
                if let Some(item) = items_clone.get_mut(idx) {
                    previous.push((item.id, item.rating));
                    item.rating = rating;
                    ids.push(item.id);
                }
            }
            *this.items.borrow_mut() = Rc::new(items_clone);
        }

        if let Some(um) = this.undo_manager.borrow().as_ref() {
            um.borrow_mut().push(crate::ui::undo::UndoAction::Rating {
                previous,
                new_rating: rating,
            });
        }

        Self::update_tile_styles(this);

        if let Some(db) = this.db.borrow().as_ref() {
            let db = db.clone();
            thread::spawn(move || {
                if let Ok(mut conn) = db.conn() {
                    if queries::batch_set_rating(&mut conn, &ids, rating).is_ok() {
                        sync_cull_to_xmp(&conn, &ids);
                    }
                }
            });
        }
    }

    fn cull_flag(this: &Rc<Self>, flag: i32) {
        let selected: Vec<usize> = {
            let sel = this.selected_indices.borrow();
            if sel.is_empty() {
                if let Some(focus) = this.focused_index.get() {
                    vec![focus]
                } else {
                    Vec::new()
                }
            } else {
                sel.iter().copied().collect()
            }
        };
        if selected.is_empty() {
            return;
        }

        let mut ids = Vec::new();
        let mut previous = Vec::new();
        {
            let mut items_clone = (**this.items.borrow()).clone();
            for &idx in &selected {
                if let Some(item) = items_clone.get_mut(idx) {
                    previous.push((item.id, item.flagged));
                    item.flagged = flag;
                    ids.push(item.id);
                }
            }
            *this.items.borrow_mut() = Rc::new(items_clone);
        }

        if let Some(um) = this.undo_manager.borrow().as_ref() {
            um.borrow_mut().push(crate::ui::undo::UndoAction::Flag {
                previous,
                new_flag: flag,
            });
        }

        Self::update_tile_styles(this);

        if let Some(db) = this.db.borrow().as_ref() {
            let db = db.clone();
            thread::spawn(move || {
                if let Ok(mut conn) = db.conn() {
                    if queries::batch_set_flag(&mut conn, &ids, flag).is_ok() {
                        sync_cull_to_xmp(&conn, &ids);
                    }
                }
            });
        }
    }

    fn rotate_selected(this: &Rc<Self>, cw: bool) {
        let selected: Vec<usize> = {
            let sel = this.selected_indices.borrow();
            if sel.is_empty() {
                if let Some(focus) = this.focused_index.get() {
                    vec![focus]
                } else {
                    Vec::new()
                }
            } else {
                sel.iter().copied().collect()
            }
        };
        if selected.is_empty() {
            return;
        }

        let mut updates: Vec<(i64, u16)> = Vec::new();
        let mut affected: Vec<(i64, String, u16)> = Vec::new();
        let mut previous = Vec::new();
        {
            let mut items_clone = (**this.items.borrow()).clone();
            for &idx in &selected {
                if let Some(item) = items_clone.get_mut(idx) {
                    previous.push((item.id, item.orientation, item.hash.clone()));
                    let next_orient = photon_core::models::rotate_orientation(item.orientation, cw);
                    item.orientation = Some(next_orient);
                    updates.push((item.id, next_orient));
                    affected.push((item.id, item.hash.clone(), next_orient));
                }
            }
            *this.items.borrow_mut() = Rc::new(items_clone);
        }

        if let Some(um) = this.undo_manager.borrow().as_ref() {
            um.borrow_mut().push(crate::ui::undo::UndoAction::Orientation {
                previous,
                cw,
            });
        }

        // Invalidate the grid textures, the disk thumbnails and the viewer's 1:1 renders
        for (_, hash, _) in &affected {
            this.textures.borrow_mut().remove(hash);
            photon_import::thumbnails::invalidate_cache(&this.cache_dir, hash);
            crate::ui::detail::invalidate_full_res(hash);
        }

        // Relayout immediately so tile aspect ratios update
        this.relayout();

        // Asynchronously update DB, sync XMP sidecars, and regenerate grid thumbnails
        if let Some(db) = this.db.borrow().as_ref() {
            let db = db.clone();
            let cache_dir = this.cache_dir.clone();
            let weak_inner = Rc::downgrade(this);
            glib::spawn_future_local(async move {
                gio::spawn_blocking(move || {
                    match db.conn() {
                        Ok(mut conn) => {
                            if let Err(e) = queries::batch_set_orientation(&mut conn, &updates) {
                                log::error!("Failed to save batch orientation: {e}");
                            } else {
                                sync_orientation_to_xmp(&conn, &updates);
                            }

                            // Regenerate grid thumbnails
                            let thumb_gen = photon_import::thumbnails::ThumbnailGenerator::new(cache_dir);
                            for &(id, _) in &updates {
                                if let Ok(Some(image)) = queries::get_image(&conn, id) {
                                    if let Err(e) = thumb_gen.ensure_grid(&image) {
                                        log::warn!("Failed to regenerate thumbnail for photo {id}: {e}");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to acquire database connection for batch orientation: {e}");
                        }
                    }
                })
                .await
                .ok();

                if let Some(inner) = weak_inner.upgrade() {
                    inner.relayout();
                }
            });
        }
    }

    /// Bring the row holding photo `item_index` into view. `ListView::scroll_to`
    /// works on exact row positions, unlike pixel offsets, which GTK only
    /// estimates for rows it hasn't measured yet.
    fn scroll_to_item(this: &Rc<Self>, item_index: usize) {
        let position = this.rows.borrow().iter().position(|row| {
            matches!(row, Row::Photos(tiles) if tiles.iter().any(|t| t.index == item_index))
        });
        if let Some(position) = position {
            this.list.scroll_to(position as u32, ListScrollFlags::NONE, None);
        }
    }

    /// The row at the top of the viewport, found among the bound row widgets.
    fn top_row(&self) -> Option<usize> {
        let mut best: Option<(f32, u32)> = None;
        for weak in self.list_items.borrow().iter() {
            let Some(item) = weak.upgrade() else { continue };
            let (Some(_), Some(child)) = (item.item(), item.child()) else { continue };
            let Some(point) = child.compute_point(&self.scrolled, &graphene::Point::new(0.0, 0.0)) else {
                continue;
            };
            let bottom = point.y() + child.height() as f32;
            // The row that straddles (or starts at) the top edge.
            if point.y() <= 1.0 && bottom > 1.0 && best.is_none_or(|(y, _)| point.y() > y) {
                best = Some((point.y(), item.position()));
            }
        }
        best.map(|(_, position)| position as usize)
    }

    /// After a scroll settles into a layout: move the rail thumb, and unless
    /// the scroll was programmatic, show the date and the rail marks.
    fn on_scrolled(this: &Rc<Self>) {
        let Some(row) = this.top_row() else { return };
        this.move_rail_thumb(row);
        if this.quiet_until.get().is_some_and(|until| Instant::now() < until) {
            return;
        }
        Self::activate_rail(this);
        if let Some(date) = this.date_of_row(row) {
            Self::show_date_badge(this, &date);
        }
    }

    /// The date (without the photo count) of the section `row` belongs to.
    fn date_of_row(&self, row: usize) -> Option<String> {
        let header = self.header_of_row.borrow().get(row).copied().flatten()?;
        match self.rows.borrow().get(header)? {
            Row::Header(title) => Some(title.split("  ·  ").next().unwrap_or(title).to_string()),
            Row::Photos(_) => None,
        }
    }

    fn show_date_badge(this: &Rc<Self>, date: &str) {
        this.date_label.set_text(date);
        this.date_badge.set_reveal_child(true);

        if let Some(timer) = this.hide_badge_timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(this);
        let id = glib::timeout_add_local_once(Duration::from_millis(900), move || {
            if let Some(inner) = weak.upgrade() {
                inner.hide_badge_timer.borrow_mut().take();
                if !inner.rail_dragging.get() {
                    inner.date_badge.set_reveal_child(false);
                }
            }
        });
        *this.hide_badge_timer.borrow_mut() = Some(id);
    }

    // ── Scrubber rail ───────────────────────────────────

    /// Rail y (in view coordinates) ↔ content y, over the inset range.
    fn rail_span(&self) -> Option<f64> {
        let span = self.view_height.get() as f64 - 2.0 * RAIL_INSET;
        (span > 0.0 && self.content_height.get() > 0.0).then_some(span)
    }

    fn rail_y_of_row(&self, row: usize) -> Option<f64> {
        let span = self.rail_span()?;
        let top = *self.row_tops.borrow().get(row)?;
        Some(RAIL_INSET + top / self.content_height.get() * span)
    }

    fn row_at_rail_y(&self, y: f64) -> Option<usize> {
        let span = self.rail_span()?;
        let fraction = ((y - RAIL_INSET) / span).clamp(0.0, 1.0);
        let target = fraction * self.content_height.get();
        let tops = self.row_tops.borrow();
        let row = tops.partition_point(|&t| t <= target).saturating_sub(1);
        (row < tops.len()).then_some(row)
    }

    /// Rebuild the year marks for the current layout and view height. At
    /// most a few dozen labels, so rebuilding is cheap.
    fn place_rail_marks(&self) {
        let mut child = self.rail_marks.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            if c != *self.rail_thumb.upcast_ref::<gtk4::Widget>() {
                self.rail_marks.remove(&c);
            }
        }

        // One candidate per year: where its first section starts on the rail.
        let rows = self.rows.borrow();
        let items = self.items.borrow();
        let mut marks: Vec<(i32, f64)> = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            if !matches!(row, Row::Header(_)) {
                continue;
            }
            let year = match rows.get(i + 1) {
                Some(Row::Photos(tiles)) => tiles
                    .first()
                    .and_then(|t| items.get(t.index)?.created_at)
                    .and_then(|ts| DateTime::from_timestamp(ts, 0))
                    .map(|dt| dt.year()),
                _ => None,
            };
            if let (Some(year), Some(y)) = (year, self.rail_y_of_row(i)) {
                if marks.last().map(|&(y0, _)| y0) != Some(year) {
                    marks.push((year, y));
                }
            }
        }
        let end = self.rail_span().map_or(0.0, |span| RAIL_INSET + span);

        for (year, y) in pick_rail_marks(&marks, end, 18.0) {
            let label = Label::new(Some(&year.to_string()));
            label.add_css_class("photon-scrubber-year");
            let (_, natural, _, _) = label.measure(Orientation::Horizontal, -1);
            let x = (MARKS_WIDTH - RAIL_WIDTH - natural - 4) as f64;
            self.rail_marks.put(&label, x, (y - 9.0).max(0.0));
        }
    }

    fn move_rail_thumb(&self, row: usize) {
        if let Some(y) = self.rail_y_of_row(row) {
            self.rail_marks.move_(&self.rail_thumb, thumb_x(), y - 2.0);
        }
    }

    /// Show the rail marks now; hide them after a pause, unless the pointer
    /// is on the rail or dragging it.
    fn activate_rail(this: &Rc<Self>) {
        this.rail_marks.add_css_class("active");
        if let Some(timer) = this.rail_hide_timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(this);
        let id = glib::timeout_add_local_once(RAIL_LINGER, move || {
            if let Some(inner) = weak.upgrade() {
                inner.rail_hide_timer.borrow_mut().take();
                if !inner.rail_hovered.get() && !inner.rail_dragging.get() {
                    inner.rail_marks.remove_css_class("active");
                }
            }
        });
        *this.rail_hide_timer.borrow_mut() = Some(id);
    }

    /// Pointer at rail `y`: preview the date there and jump the view to it.
    fn scrub_to(this: &Rc<Self>, y: f64) {
        let Some(row) = this.row_at_rail_y(y) else { return };
        Self::activate_rail(this);
        this.move_rail_thumb(row);
        if let Some(date) = this.date_of_row(row) {
            Self::show_date_badge(this, &date);
        }
        Self::jump_to_row(this, row);
    }

    /// Put `row` at the top of the view. Coalesced: during a drag only the
    /// latest target is applied, once per main-loop turn.
    fn jump_to_row(this: &Rc<Self>, row: usize) {
        if this.jump_target.replace(Some(row)).is_some() {
            return; // a jump is already scheduled; it will use the new target
        }
        let weak = Rc::downgrade(this);
        glib::idle_add_local_once(move || {
            let Some(inner) = weak.upgrade() else { return };
            let Some(row) = inner.jump_target.take() else { return };
            // Bring the row into view (exact, even for unmeasured rows) …
            inner.list.scroll_to(row as u32, ListScrollFlags::NONE, None);
            // … then, once it is laid out, align it to the top.
            let weak = Rc::downgrade(&inner);
            glib::idle_add_local_once(move || {
                let Some(inner) = weak.upgrade() else { return };
                if let Some(y) = inner.row_offset_in_view(row) {
                    let adj = inner.scrolled.vadjustment();
                    adj.set_value(adj.value() + y);
                }
            });
        });
    }

    /// Where bound row `row` currently sits relative to the top of the view.
    fn row_offset_in_view(&self, row: usize) -> Option<f64> {
        self.list_items.borrow().iter().find_map(|weak| {
            let item = weak.upgrade()?;
            item.item()?;
            if item.position() as usize != row {
                return None;
            }
            let point = item.child()?.compute_point(&self.scrolled, &graphene::Point::new(0.0, 0.0))?;
            Some(point.y() as f64)
        })
    }

    /// Programmatic scrolls for the next moment shouldn't show the date label.
    fn quiet_scroll(&self) {
        self.quiet_until.set(Some(Instant::now() + Duration::from_millis(400)));
    }

    /// Restore after the list has re-measured (its size estimate settles on idle).
    fn restore_scroll(&self, value: f64) {
        self.quiet_scroll();
        let adj = self.scrolled.vadjustment();
        adj.set_value(value);
        glib::idle_add_local_once(move || adj.set_value(value));
    }

    fn restore_scroll_fraction(&self, fraction: f64) {
        self.quiet_scroll();
        let adj = self.scrolled.vadjustment();
        glib::idle_add_local_once(move || {
            adj.set_value(fraction * (adj.upper() - adj.page_size()).max(0.0));
        });
    }

    fn move_focus(this: &Rc<Self>, target_idx: usize, is_shift: bool, is_ctrl: bool) {
        let anchor = this.anchor_index.get().unwrap_or(target_idx);
        this.focused_index.set(Some(target_idx));

        if is_shift {
            let start = anchor.min(target_idx);
            let end = anchor.max(target_idx);
            let mut sel = this.selected_indices.borrow_mut();
            sel.clear();
            for i in start..=end {
                sel.insert(i);
            }
        } else if is_ctrl {
            this.anchor_index.set(Some(target_idx));
        } else {
            this.anchor_index.set(Some(target_idx));
            let mut sel = this.selected_indices.borrow_mut();
            sel.clear();
            sel.insert(target_idx);
        }

        Self::update_tile_styles(this);
        Self::scroll_to_item(this, target_idx);
    }

    fn handle_key_pressed(
        this: &Rc<Self>,
        keyval: gdk::Key,
        state: gdk::ModifierType,
    ) -> glib::Propagation {
        let n_items = this.items.borrow().len();
        if n_items == 0 {
            return glib::Propagation::Proceed;
        }

        let is_shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let is_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let current_focus = this.focused_index.get();

        if is_ctrl {
            match keyval {
                gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => {
                    let old_h = this.row_height.get();
                    let new_h = (old_h + 30).clamp(110, 500);
                    if new_h != old_h {
                        this.row_height.set(new_h);
                        let fraction = scroll_fraction(&this.scrolled.vadjustment());
                        this.relayout();
                        this.restore_scroll_fraction(fraction);
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::minus | gdk::Key::KP_Subtract => {
                    let old_h = this.row_height.get();
                    let new_h = (old_h - 30).clamp(110, 500);
                    if new_h != old_h {
                        this.row_height.set(new_h);
                        let fraction = scroll_fraction(&this.scrolled.vadjustment());
                        this.relayout();
                        this.restore_scroll_fraction(fraction);
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::_0 | gdk::Key::KP_0 => {
                    let old_h = this.row_height.get();
                    let new_h = 200;
                    if new_h != old_h {
                        this.row_height.set(new_h);
                        let fraction = scroll_fraction(&this.scrolled.vadjustment());
                        this.relayout();
                        this.restore_scroll_fraction(fraction);
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::a | gdk::Key::A => {
                    let mut sel = this.selected_indices.borrow_mut();
                    sel.clear();
                    for i in 0..n_items {
                        sel.insert(i);
                    }
                    drop(sel);
                    Self::update_tile_styles(this);
                    return glib::Propagation::Stop;
                }
                gdk::Key::r | gdk::Key::R => {
                    Self::rotate_selected(this, true);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }

        match keyval {
            gdk::Key::_1 | gdk::Key::KP_1 | gdk::Key::exclam => {
                Self::cull_rating(this, 1);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::_2 | gdk::Key::KP_2 | gdk::Key::at => {
                Self::cull_rating(this, 2);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::_3 | gdk::Key::KP_3 | gdk::Key::numbersign => {
                Self::cull_rating(this, 3);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::_4 | gdk::Key::KP_4 | gdk::Key::dollar => {
                Self::cull_rating(this, 4);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::_5 | gdk::Key::KP_5 | gdk::Key::percent => {
                Self::cull_rating(this, 5);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::_0 | gdk::Key::KP_0 | gdk::Key::parenright | gdk::Key::grave | gdk::Key::asciitilde => {
                Self::cull_rating(this, 0);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::p | gdk::Key::P => {
                Self::cull_flag(this, 1);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::x | gdk::Key::X => {
                Self::cull_flag(this, -1);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::u | gdk::Key::U => {
                Self::cull_flag(this, 0);
                if is_shift {
                    Self::advance_focus(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::Left | gdk::Key::KP_Left => {
                let next_idx = match current_focus {
                    Some(idx) => idx.saturating_sub(1),
                    None => 0,
                };
                Self::move_focus(this, next_idx, is_shift, is_ctrl);
                glib::Propagation::Stop
            }
            gdk::Key::Right | gdk::Key::KP_Right => {
                let next_idx = match current_focus {
                    Some(idx) => (idx + 1).min(n_items.saturating_sub(1)),
                    None => 0,
                };
                Self::move_focus(this, next_idx, is_shift, is_ctrl);
                glib::Propagation::Stop
            }
            gdk::Key::Up | gdk::Key::KP_Up => {
                let next_idx = match current_focus {
                    Some(idx) => {
                        let rows = this.rows.borrow();
                        navigate_2d(&rows, idx, false).unwrap_or(idx)
                    }
                    None => 0,
                };
                Self::move_focus(this, next_idx, is_shift, is_ctrl);
                glib::Propagation::Stop
            }
            gdk::Key::Down | gdk::Key::KP_Down => {
                let next_idx = match current_focus {
                    Some(idx) => {
                        let rows = this.rows.borrow();
                        navigate_2d(&rows, idx, true).unwrap_or(idx)
                    }
                    None => 0,
                };
                Self::move_focus(this, next_idx, is_shift, is_ctrl);
                glib::Propagation::Stop
            }
            gdk::Key::Home | gdk::Key::KP_Home => {
                Self::move_focus(this, 0, is_shift, is_ctrl);
                glib::Propagation::Stop
            }
            gdk::Key::End | gdk::Key::KP_End => {
                Self::move_focus(this, n_items.saturating_sub(1), is_shift, is_ctrl);
                glib::Propagation::Stop
            }
            gdk::Key::space | gdk::Key::KP_Space => {
                if let Some(idx) = current_focus {
                    let mut sel = this.selected_indices.borrow_mut();
                    if sel.contains(&idx) {
                        sel.remove(&idx);
                    } else {
                        sel.insert(idx);
                    }
                    drop(sel);
                    Self::update_tile_styles(this);
                }
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                if let Some(idx) = current_focus {
                    if let Some(f) = this.on_activate.borrow().as_ref() {
                        f(idx);
                    }
                }
                glib::Propagation::Stop
            }
            gdk::Key::bracketleft => {
                Self::rotate_selected(this, false);
                glib::Propagation::Stop
            }
            gdk::Key::bracketright => {
                Self::rotate_selected(this, true);
                glib::Propagation::Stop
            }
            gdk::Key::F5 => {
                if let Some(ref cb) = *this.on_start_slideshow.borrow() {
                    cb();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
            gdk::Key::Escape => {
                if this.selection_mode.get() {
                    Self::set_selection_mode(this, false);
                } else {
                    this.selected_indices.borrow_mut().clear();
                    Self::update_tile_styles(this);
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    fn bind_row(this: &Rc<Self>, row_box: &GtkBox, row: &Row, position: usize) {
        clear_children(row_box);
        match row {
            Row::Header(title) => {
                let check = Button::from_icon_name("object-select-symbolic");
                check.add_css_class("circular");
                check.add_css_class("photon-day-check");
                check.set_valign(Align::End);
                check.set_margin_bottom(6);
                check.set_tooltip_text(Some("Select All Photos of This Day"));
                let weak = Rc::downgrade(this);
                check.connect_clicked(move |_| {
                    if let Some(inner) = weak.upgrade() {
                        Self::toggle_section(&inner, position);
                    }
                });

                // Find date (year, month, day) from the next photos row
                let mut day_date = None;
                {
                    let rows = this.rows.borrow();
                    let items = this.items.borrow();
                    let mut next_pos = position + 1;
                    while next_pos < rows.len() {
                        if let Row::Photos(tiles) = &rows[next_pos] {
                            if let Some(tile) = tiles.first() {
                                if let Some(item) = items.get(tile.index) {
                                    if let Some(ts) = item.created_at {
                                        if let Some(dt) = DateTime::from_timestamp(ts, 0) {
                                            day_date = Some((dt.year(), dt.month(), dt.day()));
                                        }
                                    }
                                }
                            }
                            break;
                        }
                        next_pos += 1;
                    }
                }

                let mut event_name: Option<String> = None;
                if let Some((y, m, d)) = day_date {
                    if let Some(db) = this.db.borrow().as_ref() {
                        if let Ok(conn) = db.conn() {
                            if let Ok(Some(ev)) = queries::get_event_for_day(&conn, y, m, d) {
                                event_name = Some(ev.name);
                            }
                        }
                    }
                }

                let display_title = if let Some(ref name) = event_name {
                    format!("{name} — {title}")
                } else {
                    title.clone()
                };

                let label = Label::new(Some(&display_title));
                label.set_halign(Align::Start);
                label.set_valign(Align::End);
                label.add_css_class("photon-section-header");
                label.set_margin_bottom(8);
                row_box.set_margin_bottom(0);
                row_box.set_size_request(-1, HEADER_HEIGHT);
                row_box.append(&check);
                row_box.append(&label);

                if let Some((y, m, d)) = day_date {
                    let edit_btn = Button::from_icon_name("document-edit-symbolic");
                    edit_btn.add_css_class("flat");
                    edit_btn.add_css_class("photon-day-edit-btn");
                    edit_btn.set_valign(Align::End);
                    edit_btn.set_margin_bottom(6);
                    edit_btn.set_margin_start(4);
                    edit_btn.set_tooltip_text(Some("Name or edit event for this day"));

                    let weak_inner = Rc::downgrade(this);
                    let initial = event_name.unwrap_or_default();
                    let date_str = title.split("  ·  ").next().unwrap_or(title).to_string();
                    edit_btn.connect_clicked(move |_| {
                        let weak_c = weak_inner.clone();
                        crate::ui::sidebar::prompt_text_dialog(
                            "Name Event",
                            &format!("Name event for {date_str}:"),
                            &initial,
                            "Save",
                            move |name| {
                                if let Some(inner) = weak_c.upgrade() {
                                    if let Some(db) = inner.db.borrow().as_ref() {
                                        if let Ok(conn) = db.conn() {
                                            if let Err(e) = queries::name_day_event(&conn, &name, y, m, d) {
                                                log::error!("name_day_event failed: {e}");
                                            }
                                        }
                                    }
                                    if let Some(ref cb) = *inner.on_refresh.borrow() {
                                        cb();
                                    }
                                }
                            },
                        );
                    });
                    row_box.append(&edit_btn);
                }
            }
            Row::Photos(tiles) => {
                row_box.set_margin_bottom(GAP);
                row_box.set_size_request(-1, -1);
                let items = this.items.borrow().clone();
                for tile in tiles {
                    let Some(item) = items.get(tile.index) else { continue };
                    row_box.append(&Self::make_tile(this, tile, item));
                }
            }
        }
    }

    fn make_tile(this: &Rc<Self>, tile: &Tile, item: &TimelineItem) -> GtkBox {
        let frame = GtkBox::new(Orientation::Vertical, 0);
        frame.set_size_request(tile.width, tile.height);
        frame.set_overflow(gtk4::Overflow::Hidden);
        frame.add_css_class("photon-tile");
        frame.set_cursor_from_name(Some("pointer"));

        if this.selected_indices.borrow().contains(&tile.index) {
            frame.add_css_class("photon-tile-selected");
        }
        if this.focused_index.get() == Some(tile.index) {
            frame.add_css_class("photon-tile-focused");
        }
        if item.flagged == -1 {
            frame.add_css_class("photon-tile-rejected");
        } else if item.flagged == 1 {
            frame.add_css_class("photon-tile-pick");
        }
        if item.missing {
            frame.add_css_class("photon-tile-missing");
        }

        this.visible_tiles.borrow_mut().insert(tile.index, frame.downgrade());

        let picture = Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Cover);
        picture.set_can_shrink(true);
        picture.set_vexpand(true);
        picture.set_hexpand(true);

        let overlay = Overlay::new();
        overlay.set_can_target(false);
        overlay.set_vexpand(true);
        overlay.set_hexpand(true);
        overlay.set_child(Some(&picture));

        let badges_box = GtkBox::new(Orientation::Horizontal, 4);
        badges_box.add_css_class("photon-tile-badges");
        badges_box.set_valign(Align::End);
        badges_box.set_halign(Align::Start);
        badges_box.set_can_target(false);
        rebuild_tile_badges(&badges_box, item.rating, item.flagged, item.is_video, item.duration.as_deref(), item.missing);
        overlay.add_overlay(&badges_box);

        // Shown on hover, while selecting, and on selected tiles (CSS).
        let check = gtk4::Image::from_icon_name("object-select-symbolic");
        check.add_css_class("photon-tile-check");
        check.set_halign(Align::Start);
        check.set_valign(Align::Start);
        check.set_can_target(false);
        overlay.add_overlay(&check);

        frame.append(&overlay);

        if let Some(texture) = this.textures.borrow_mut().get(&item.hash) {
            picture.set_paintable(Some(&texture));
        } else {
            if let Some(ref th_bytes) = item.thumbhash {
                if let Some(placeholder) = decode_thumbhash_texture(th_bytes) {
                    picture.set_paintable(Some(&placeholder));
                }
            }
            Self::load_texture_later(this, &picture, item.hash.clone());
        }

        let click = GestureClick::new();
        let weak = Rc::downgrade(this);
        let index = tile.index;
        click.connect_released(move |gesture, n_press, x, y| {
            let Some(inner) = weak.upgrade() else { return };
            inner.scrolled.grab_focus();

            let on_check = x < CHECK_HIT && y < CHECK_HIT;
            if inner.selection_mode.get() || on_check {
                // A double click's second press would undo the first toggle.
                if n_press == 1 {
                    let state = gesture.current_event_state();
                    if state.contains(gdk::ModifierType::SHIFT_MASK) {
                        let anchor = inner.anchor_index.get().unwrap_or(index);
                        inner.focused_index.set(Some(index));
                        inner
                            .selected_indices
                            .borrow_mut()
                            .extend(anchor.min(index)..=anchor.max(index));
                        Inner::set_selection_mode(&inner, true);
                        Inner::update_tile_styles(&inner);
                    } else {
                        Inner::toggle_selected(&inner, index);
                    }
                }
                return;
            }

            if n_press == 2 {
                if let Some(f) = inner.on_activate.borrow().as_ref() {
                    f(index);
                }
                return;
            }

            let state = gesture.current_event_state();
            let is_shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            let is_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);

            if is_shift {
                let anchor = inner.anchor_index.get().unwrap_or(index);
                inner.focused_index.set(Some(index));
                let start = anchor.min(index);
                let end = anchor.max(index);
                let mut sel = inner.selected_indices.borrow_mut();
                sel.clear();
                for i in start..=end {
                    sel.insert(i);
                }
            } else if is_ctrl {
                inner.focused_index.set(Some(index));
                inner.anchor_index.set(Some(index));
                let mut sel = inner.selected_indices.borrow_mut();
                if sel.contains(&index) {
                    sel.remove(&index);
                } else {
                    sel.insert(index);
                }
            } else {
                inner.focused_index.set(Some(index));
                inner.anchor_index.set(Some(index));
                let mut sel = inner.selected_indices.borrow_mut();
                sel.clear();
                sel.insert(index);
            }
            Inner::update_tile_styles(&inner);
        });
        frame.add_controller(click);

        let drag = DragSource::new();
        drag.set_actions(gdk::DragAction::COPY);
        let weak = Rc::downgrade(this);
        let dragged = Rc::new(Cell::new(0usize));
        let dragged_count = dragged.clone();
        drag.connect_prepare(move |drag, _, _| {
            // On a touchscreen, a drag is a scroll.
            let touch = drag
                .current_event_device()
                .is_some_and(|d| d.source() == gdk::InputSource::Touchscreen);
            if touch {
                return None;
            }
            let files = weak.upgrade()?.drag_files(index);
            if files.is_empty() {
                return None;
            }
            dragged_count.set(files.len());
            let list = gdk::FileList::from_array(&files);
            Some(gdk::ContentProvider::for_value(&list.to_value()))
        });
        let pic = picture.downgrade();
        drag.connect_drag_begin(move |_, drag| {
            let Some(paintable) = pic.upgrade().and_then(|p| p.paintable()) else { return };
            // A small thumbnail (thumbnails themselves are 640 px), with a
            // count when several photos are dragged.
            let thumb = Picture::for_paintable(&paintable);
            thumb.set_content_fit(gtk4::ContentFit::Cover);
            thumb.set_size_request(96, 96);
            thumb.set_overflow(gtk4::Overflow::Hidden);
            thumb.add_css_class("photon-drag-icon");
            let icon = Overlay::new();
            icon.set_child(Some(&thumb));
            if dragged.get() > 1 {
                let count = Label::new(Some(&dragged.get().to_string()));
                count.add_css_class("photon-drag-count");
                count.set_halign(Align::End);
                count.set_valign(Align::Start);
                icon.add_overlay(&count);
            }
            gtk4::DragIcon::for_drag(drag).set_child(Some(&icon));
        });
        frame.add_controller(drag);
        frame
    }

    fn load_texture_later(this: &Rc<Self>, picture: &Picture, hash: String) {
        let path = thumb_path(&this.cache_dir, ThumbSize::Grid, &hash);
        let weak_inner = Rc::downgrade(this);
        let weak_picture = picture.downgrade();

        glib::timeout_add_local_once(LOAD_DELAY, move || {
            // Skip rows that were scrolled past (unbound → tile destroyed).
            if weak_picture.upgrade().and_then(|p| p.root()).is_none() {
                return;
            }
            glib::spawn_future_local(async move {
                let loaded = gio::spawn_blocking(move || gdk::Texture::from_filename(&path)).await;
                let Ok(Ok(texture)) = loaded else {
                    return; // no thumbnail yet: keep the placeholder
                };
                if let Some(inner) = weak_inner.upgrade() {
                    inner.textures.borrow_mut().put(hash, texture.clone());
                }
                if let Some(picture) = weak_picture.upgrade() {
                    picture.set_paintable(Some(&texture));
                }
            });
        });
    }
}

fn clear_children(container: &GtkBox) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn rebuild_tile_badges(
    badges_box: &GtkBox,
    rating: i32,
    flagged: i32,
    is_video: bool,
    duration: Option<&str>,
    missing: bool,
) {
    clear_children(badges_box);
    if missing {
        let missing_label = Label::new(Some("⚠ Offline"));
        missing_label.add_css_class("photon-tile-badge");
        missing_label.add_css_class("photon-tile-warning-badge");
        badges_box.append(&missing_label);
    }
    if is_video {
        let label = match duration {
            Some(d) if !d.is_empty() => format!("▶ {d}"),
            _ => "▶".to_string(),
        };
        let video_label = Label::new(Some(&label));
        video_label.add_css_class("photon-tile-badge");
        video_label.add_css_class("photon-tile-video-badge");
        badges_box.append(&video_label);
    }
    if rating > 0 {
        let star_label = Label::new(Some(&format!("★ {}", rating)));
        star_label.add_css_class("photon-tile-badge");
        star_label.add_css_class("photon-tile-star-badge");
        badges_box.append(&star_label);
    }
    if flagged == 1 {
        let pick_label = Label::new(Some("✓"));
        pick_label.add_css_class("photon-tile-badge");
        pick_label.add_css_class("photon-tile-pick-badge");
        badges_box.append(&pick_label);
    } else if flagged == -1 {
        let reject_label = Label::new(Some("✕"));
        reject_label.add_css_class("photon-tile-badge");
        reject_label.add_css_class("photon-tile-reject-badge");
        badges_box.append(&reject_label);
    }
}

fn scroll_fraction(adj: &gtk4::Adjustment) -> f64 {
    let range = adj.upper() - adj.page_size();
    if range > 0.0 {
        adj.value() / range
    } else {
        0.0
    }
}

fn decode_thumbhash_texture(th: &[u8]) -> Option<gdk::Texture> {
    let (w_us, h_us, rgba) = thumbhash::thumb_hash_to_rgba(th).ok()?;
    let w = w_us as i32;
    let h = h_us as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    let bytes = glib::Bytes::from_owned(rgba);
    let stride = (w * 4) as usize;
    let mem_texture = gdk::MemoryTexture::new(
        w,
        h,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        stride,
    );
    Some(mem_texture.upcast())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn item(created_at: i64, w: u32, h: u32) -> TimelineItem {
        TimelineItem {
            id: 0,
            hash: String::new(),
            created_at: Some(created_at),
            width: Some(w),
            height: Some(h),
            orientation: None,
            thumbhash: None,
            rating: 0,
            flagged: 0,
            ..Default::default()
        }
    }

    fn photo_rows(rows: &[Row]) -> Vec<&Vec<Tile>> {
        rows.iter()
            .filter_map(|r| match r {
                Row::Photos(t) => Some(t),
                Row::Header(_) => None,
            })
            .collect()
    }

    #[test]
    fn full_rows_fill_the_width_exactly_and_last_row_keeps_target_height() {
        let items: Vec<_> = (0..10).map(|_| item(1_700_000_000, 3000, 2000)).collect();
        let rows = layout(&items, 1000, 200, false);
        let photo_rows = photo_rows(&rows);

        let (last, full) = photo_rows.split_last().unwrap();
        for row in full {
            let used: i32 = row.iter().map(|t| t.width).sum::<i32>() + GAP * (row.len() as i32 - 1);
            assert_eq!(used, 1000);
            assert!(row[0].height <= 200 && row[0].height > 100);
        }
        assert_eq!(last[0].height, 200);
        // Every photo appears exactly once, in order.
        let order: Vec<usize> = photo_rows.iter().flat_map(|r| r.iter().map(|t| t.index)).collect();
        assert_eq!(order, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn aspect_ratio_is_preserved_including_rotated_photos() {
        let mut portrait = item(0, 6000, 4000);
        portrait.orientation = Some(6); // stored landscape, displayed portrait
        let items = vec![item(0, 6000, 4000), portrait];
        let rows = layout(&items, 10_000, 300, false);
        let tiles = photo_rows(&rows)[0];
        assert_eq!(tiles[0].width, 450); // 1.5 × 300
        assert_eq!(tiles[1].width, 200); // 0.667 × 300
    }

    #[test]
    fn a_header_starts_each_day() {
        let items = vec![
            item(10 * DAY + 100, 3, 2),
            item(10 * DAY + 50, 3, 2),
            item(9 * DAY, 3, 2),
        ];
        let rows = layout(&items, 1000, 200, true);
        let headers: Vec<&String> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Header(h) => Some(h),
                Row::Photos(_) => None,
            })
            .collect();
        assert_eq!(headers.len(), 2);
        assert!(headers[0].ends_with("2 photos"));
        assert!(headers[1].ends_with("1 photo"));
        assert!(matches!(rows[0], Row::Header(_)));
    }

    #[test]
    fn lays_out_100k_photos_quickly() {
        let items: Vec<_> = (0..100_000)
            .map(|i| item(1_700_000_000 - i * 600, if i % 3 == 0 { 2000 } else { 3000 }, 2000))
            .collect();
        let start = std::time::Instant::now();
        let rows = layout(&items, 1400, 220, true);
        assert!(rows.len() > 10_000);
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    }

    #[test]
    fn selection_follows_photo_ids_when_items_shift() {
        let with_ids = |ids: &[i64]| -> Vec<TimelineItem> {
            ids.iter()
                .map(|&id| TimelineItem { id, ..item(0, 3, 2) })
                .collect()
        };
        let before = with_ids(&[10, 20, 30]);
        // An import inserted photo 15 between 10 and 20, and 30 was removed.
        let after = with_ids(&[10, 15, 20]);

        let selected = ids_at(&before, [1, 2]); // photos 20 and 30
        assert_eq!(selected, vec![20, 30]);
        assert_eq!(indices_of(&after, &selected), vec![2]); // 20 moved to index 2
    }

    #[test]
    fn crowded_rail_marks_keep_the_bigger_years() {
        // 2025 has 3 photos right above a huge 2024: 2024 must be labelled.
        let marks = [(2026, 16.0), (2025, 260.0), (2024, 266.0), (2023, 690.0), (2022, 705.0)];
        let years: Vec<i32> = pick_rail_marks(&marks, 710.0, 18.0).iter().map(|m| m.0).collect();
        assert_eq!(years, [2026, 2024, 2023]); // 2023 (15 px) outweighs 2022 (5 px)
        assert!(pick_rail_marks(&[], 100.0, 18.0).is_empty());
    }

    #[test]
    fn row_tops_are_cumulative_exact_heights() {
        let tile = |height| Tile { index: 0, width: 10, height };
        let rows = vec![
            Row::Header("a".into()),
            Row::Photos(vec![tile(200)]),
            Row::Photos(vec![tile(180)]),
        ];
        let (tops, height) = row_tops(&rows);
        let h = HEADER_HEIGHT as f64;
        let g = GAP as f64;
        assert_eq!(tops, vec![0.0, h, h + 200.0 + g]);
        assert_eq!(height, h + 200.0 + g + 180.0 + g);
    }

    #[test]
    fn every_row_knows_its_section_header() {
        let tile = |index| Tile { index, width: 10, height: 10 };
        let rows = vec![
            Row::Header("a".into()),
            Row::Photos(vec![tile(0)]),
            Row::Photos(vec![tile(1)]),
            Row::Header("b".into()),
            Row::Photos(vec![tile(2)]),
        ];
        assert_eq!(header_of_rows(&rows), vec![Some(0), Some(0), Some(0), Some(3), Some(3)]);
        assert_eq!(header_of_rows(&[Row::Photos(vec![tile(0)])]), vec![None]);
    }

    #[test]
    fn navigate_2d_jumps_correctly_across_rows_and_headers() {
        // Row 0: Tiles [0 (w: 300), 1 (w: 300), 2 (w: 400)] -> x centers: 150, 454, 808
        // Row 1: Header
        // Row 2: Tiles [3 (w: 500), 4 (w: 500)] -> x centers: 250, 754
        let rows = vec![
            Row::Photos(vec![
                Tile { index: 0, width: 300, height: 200 },
                Tile { index: 1, width: 300, height: 200 },
                Tile { index: 2, width: 400, height: 200 },
            ]),
            Row::Header("Yesterday".to_string()),
            Row::Photos(vec![
                Tile { index: 3, width: 500, height: 200 },
                Tile { index: 4, width: 500, height: 200 },
            ]),
        ];

        // Navigating down from 0 (center 150) should pick 3 (center 250 vs 754)
        assert_eq!(navigate_2d(&rows, 0, true), Some(3));
        // Navigating down from 1 (center 454) should pick 3 (|250-454|=204 vs |754-454|=300)
        assert_eq!(navigate_2d(&rows, 1, true), Some(3));
        // Navigating down from 2 (center 808) should pick 4 (|754-808|=54 vs |250-808|=558)
        assert_eq!(navigate_2d(&rows, 2, true), Some(4));

        // Navigating up from 3 (center 250) should pick 0 (|150-250|=100 vs |454-250|=204)
        assert_eq!(navigate_2d(&rows, 3, false), Some(0));
        // Navigating up from 4 (center 754) should pick 2 (|808-754|=54 vs |454-754|=300)
        assert_eq!(navigate_2d(&rows, 4, false), Some(2));

        // Top boundary: navigating up from Row 0 returns None
        assert_eq!(navigate_2d(&rows, 0, false), None);
        // Bottom boundary: navigating down from Row 2 returns None
        assert_eq!(navigate_2d(&rows, 3, true), None);
        assert_eq!(navigate_2d(&rows, 4, true), None);
    }
}

/// Write the rating and reject state of photos `ids`, as the library now has
/// them, into their XMP sidecars, so darktable and others see grid culling too.
fn sync_cull_to_xmp(conn: &rusqlite::Connection, ids: &[i64]) {
    for &id in ids {
        let Ok(Some(image)) = queries::get_image(conn, id) else { continue };
        let update = photon_import::XmpUpdate {
            rating: Some(image.rating),
            rejected: Some(image.flagged == -1),
            ..Default::default()
        };
        if let Err(e) = photon_import::write_image_xmp(conn, id, &image.path, &update) {
            log::warn!("Writing XMP for {}: {e}", image.path.display());
        }
    }
}

/// Write updated EXIF orientation of photos into their XMP sidecars.
fn sync_orientation_to_xmp(conn: &rusqlite::Connection, updates: &[(i64, u16)]) {
    for &(id, orientation) in updates {
        let Ok(Some(image)) = queries::get_image(conn, id) else { continue };
        let update = photon_import::XmpUpdate {
            orientation: Some(orientation),
            ..Default::default()
        };
        if let Err(e) = photon_import::write_image_xmp(conn, id, &image.path, &update) {
            log::warn!("Writing XMP orientation for {}: {e}", image.path.display());
        }
    }
}
