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
//!     of textures; a tile shows a neutral placeholder until its texture is
//!     ready, and loads for rows scrolled past quickly are skipped.

use chrono::DateTime;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use gtk4::{
    Align, Box as GtkBox, DrawingArea, EventControllerKey, GestureClick, Label, ListView,
    NoSelection, Orientation, Overlay, Picture, Revealer, ScrolledWindow, SignalListItemFactory,
};
use photon_core::models::TimelineItem;
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Space between tiles and between rows, in logical pixels.
const GAP: i32 = 4;
/// Space between the timeline and the window edges.
const MARGIN: i32 = 16;
/// How many decoded thumbnails to keep (≈1.2 MB each at 640×480).
const TEXTURE_CACHE_SIZE: usize = 160;
/// A tile must stay on screen this long before its thumbnail is decoded, so
/// flinging through thousands of rows doesn't queue thousands of decodes.
const LOAD_DELAY: Duration = Duration::from_millis(40);

const CSS: &str = "
.photon-timeline, .photon-timeline > row, .photon-timeline > row:hover,
.photon-timeline > row:selected {
    background: none;
    padding: 0;
}
.photon-tile {
    background-color: alpha(currentColor, 0.08);
    border-radius: 8px;
    transition: opacity 150ms ease, outline-color 150ms ease, box-shadow 150ms ease;
    outline: 3px solid transparent;
    outline-offset: -3px;
}
.photon-tile:hover picture { opacity: 0.88; }
.photon-tile.photon-tile-selected {
    outline: 3px solid @accent_bg_color;
    outline-offset: -3px;
    box-shadow: 0 0 0 2px alpha(@accent_bg_color, 0.4);
}
.photon-tile.photon-tile-focused {
    outline: 3px solid @accent_color;
    outline-offset: -3px;
}
.photon-tile.photon-tile-selected.photon-tile-focused {
    outline: 3px solid @accent_bg_color;
    outline-offset: -3px;
    box-shadow: 0 0 0 3px alpha(@accent_bg_color, 0.6);
}
.photon-section-header {
    font-weight: 700;
    font-size: 1.15em;
    letter-spacing: -0.2px;
}
";

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
    let target_row_idx = if down {
        let mut target = None;
        for i in (r_idx + 1)..rows.len() {
            if let Row::Photos(tiles) = &rows[i] {
                if !tiles.is_empty() {
                    target = Some(i);
                    break;
                }
            }
        }
        target?
    } else {
        let mut target = None;
        for i in (0..r_idx).rev() {
            if let Row::Photos(tiles) = &rows[i] {
                if !tiles.is_empty() {
                    target = Some(i);
                    break;
                }
            }
        }
        target?
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
}

// ---------------------------------------------------------------------------
// Widget
// ---------------------------------------------------------------------------

type ActivateFn = Box<dyn Fn(usize)>;
type SelectionFn = Box<dyn Fn(&HashSet<usize>)>;

struct Inner {
    root: Overlay,
    scrolled: ScrolledWindow,
    store: gio::ListStore,
    items: RefCell<Rc<Vec<TimelineItem>>>,
    rows: RefCell<Vec<Row>>,
    row_y_offsets: RefCell<Vec<i32>>,
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
    on_activate: RefCell<Option<ActivateFn>>,
    on_selection_changed: RefCell<Option<SelectionFn>>,
}

/// The virtualized timeline widget. Cheap to clone (shared handle).
#[derive(Clone)]
pub struct Timeline {
    inner: Rc<Inner>,
}

impl Timeline {
    pub fn new(cache_dir: PathBuf, row_height: i32) -> Self {
        install_css();

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
        let root = Overlay::new();
        root.set_focusable(true);
        root.set_child(Some(&scrolled));
        root.add_overlay(&size_probe);
        root.add_overlay(&date_badge);

        let inner = Rc::new(Inner {
            root,
            scrolled,
            store,
            items: RefCell::new(Rc::new(Vec::new())),
            rows: RefCell::new(Vec::new()),
            row_y_offsets: RefCell::new(Vec::new()),
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
            on_activate: RefCell::new(None),
            on_selection_changed: RefCell::new(None),
        });

        // Key controller for 2D keyboard navigation and multi-selection
        let key_controller = EventControllerKey::new();
        let weak_key = Rc::downgrade(&inner);
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if let Some(inner) = weak_key.upgrade() {
                Inner::handle_key_pressed(&inner, keyval, state)
            } else {
                glib::Propagation::Proceed
            }
        });
        inner.scrolled.add_controller(key_controller);

        // Update floating date pill on scroll
        let weak_scroll = Rc::downgrade(&inner);
        inner.scrolled.vadjustment().connect_value_changed(move |adj| {
            if let Some(inner) = weak_scroll.upgrade() {
                let offsets = inner.row_y_offsets.borrow();
                if offsets.is_empty() {
                    return;
                }
                let val = adj.value() as i32;
                let row_idx = match offsets.binary_search(&val) {
                    Ok(idx) => idx,
                    Err(idx) => idx.saturating_sub(1),
                };

                let rows = inner.rows.borrow();
                let active_header = (0..=row_idx.min(rows.len().saturating_sub(1)))
                    .rev()
                    .find_map(|i| match &rows[i] {
                        Row::Header(h) => Some(h.clone()),
                        _ => None,
                    });

                if let Some(header_text) = active_header {
                    // Extract just the date part before the separator
                    let date_part = header_text.split("  ·  ").next().unwrap_or(&header_text);
                    inner.date_label.set_text(date_part);
                    inner.date_badge.set_reveal_child(true);

                    if let Some(timer) = inner.hide_badge_timer.borrow_mut().take() {
                        timer.remove();
                    }
                    let weak_timer = Rc::downgrade(&inner);
                    let source_id = glib::timeout_add_local_once(Duration::from_millis(900), move || {
                        if let Some(inner) = weak_timer.upgrade() {
                            inner.date_badge.set_reveal_child(false);
                        }
                    });
                    *inner.hide_badge_timer.borrow_mut() = Some(source_id);
                }
            }
        });

        let weak = Rc::downgrade(&inner);
        size_probe.connect_resize(move |_, width, _| {
            if let Some(inner) = weak.upgrade() {
                Inner::schedule_relayout(&inner, width - 2 * MARGIN);
            }
        });

        factory.connect_setup(|_, obj| {
            let item = obj.downcast_ref::<gtk4::ListItem>().expect("ListItem");
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
            Inner::bind_row(&inner, &row_box, &data.borrow::<Row>());
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

    /// Called with the photo's index into the current items when a tile is clicked or activated.
    pub fn connect_activate(&self, f: impl Fn(usize) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Box::new(f));
    }

    /// Called when the set of selected items changes.
    #[allow(dead_code)]
    pub fn connect_selection_changed(&self, f: impl Fn(&HashSet<usize>) + 'static) {
        *self.inner.on_selection_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Return the currently selected item indices.
    #[allow(dead_code)]
    pub fn selected_indices(&self) -> HashSet<usize> {
        self.inner.selected_indices.borrow().clone()
    }

    /// Clear all selections.
    #[allow(dead_code)]
    pub fn clear_selection(&self) {
        self.inner.selected_indices.borrow_mut().clear();
        Inner::update_tile_styles(&self.inner);
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
    pub fn set_items(&self, items: Rc<Vec<TimelineItem>>, by_day: bool, keep_scroll: bool) {
        let len = items.len();
        *self.inner.items.borrow_mut() = items;
        self.inner.by_day.set(by_day);
        if !keep_scroll {
            self.inner.focused_index.set(None);
            self.inner.anchor_index.set(None);
            self.inner.selected_indices.borrow_mut().clear();
        } else {
            self.inner.selected_indices.borrow_mut().retain(|&idx| idx < len);
            if let Some(idx) = self.inner.focused_index.get() {
                if idx >= len {
                    self.inner.focused_index.set(None);
                }
            }
            if let Some(idx) = self.inner.anchor_index.get() {
                if idx >= len {
                    self.inner.anchor_index.set(None);
                }
            }
        }
        let adj = self.inner.scrolled.vadjustment();
        let value = if keep_scroll { adj.value() } else { 0.0 };
        self.inner.relayout();
        restore_scroll(&adj, value);
        Inner::update_tile_styles(&self.inner);
    }

    pub fn set_row_height(&self, height: i32) {
        if self.inner.row_height.replace(height) != height {
            let adj = self.inner.scrolled.vadjustment();
            let fraction = scroll_fraction(&adj);
            self.inner.relayout();
            restore_scroll_fraction(&adj, fraction);
        }
    }
}

impl Inner {
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
            let adj = inner.scrolled.vadjustment();
            let fraction = scroll_fraction(&adj);
            inner.relayout();
            restore_scroll_fraction(&adj, fraction);
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

        let mut y = 0;
        let mut offsets = Vec::with_capacity(rows.len());
        for row in &rows {
            offsets.push(y);
            let row_h = match row {
                Row::Header(_) => 48,
                Row::Photos(tiles) => tiles.first().map(|t| t.height).unwrap_or(0) + GAP,
            };
            y += row_h;
        }
        *self.row_y_offsets.borrow_mut() = offsets;

        let objects: Vec<glib::BoxedAnyObject> =
            rows.iter().cloned().map(glib::BoxedAnyObject::new).collect();
        *self.rows.borrow_mut() = rows;
        self.store.splice(0, self.store.n_items(), &objects);
    }

    fn update_tile_styles(this: &Rc<Self>) {
        let focus = this.focused_index.get();
        let selected = this.selected_indices.borrow();
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
                true
            } else {
                false
            }
        });
        if let Some(cb) = this.on_selection_changed.borrow().as_ref() {
            cb(&selected);
        }
    }

    fn scroll_to_item(this: &Rc<Self>, item_index: usize) {
        let rows = this.rows.borrow();
        let offsets = this.row_y_offsets.borrow();
        for (r_idx, row) in rows.iter().enumerate() {
            if let Row::Photos(tiles) = row {
                if tiles.iter().any(|t| t.index == item_index) {
                    let y = offsets.get(r_idx).copied().unwrap_or(0);
                    let h = tiles.first().map(|t| t.height).unwrap_or(0) + GAP;
                    let adj = this.scrolled.vadjustment();
                    let current_val = adj.value();
                    let page_size = adj.page_size();
                    let target_top = y as f64;
                    let target_bottom = (y + h) as f64;

                    if target_top < current_val {
                        adj.set_value(target_top.max(0.0));
                    } else if target_bottom > current_val + page_size {
                        adj.set_value((target_bottom - page_size).max(0.0));
                    }
                    break;
                }
            }
        }
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

        match keyval {
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
            gdk::Key::Escape => {
                this.selected_indices.borrow_mut().clear();
                Self::update_tile_styles(this);
                glib::Propagation::Stop
            }
            gdk::Key::a | gdk::Key::A if is_ctrl => {
                let mut sel = this.selected_indices.borrow_mut();
                sel.clear();
                for i in 0..n_items {
                    sel.insert(i);
                }
                drop(sel);
                Self::update_tile_styles(this);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    fn bind_row(this: &Rc<Self>, row_box: &GtkBox, row: &Row) {
        clear_children(row_box);
        match row {
            Row::Header(title) => {
                let label = Label::new(Some(title));
                label.set_halign(Align::Start);
                label.add_css_class("photon-section-header");
                label.set_margin_top(18);
                label.set_margin_bottom(8);
                row_box.set_margin_bottom(0);
                row_box.append(&label);
            }
            Row::Photos(tiles) => {
                row_box.set_margin_bottom(GAP);
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

        this.visible_tiles.borrow_mut().insert(tile.index, frame.downgrade());

        let picture = Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Cover);
        picture.set_can_shrink(true);
        picture.set_vexpand(true);
        frame.append(&picture);

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
        click.connect_released(move |gesture, n_press, _, _| {
            let Some(inner) = weak.upgrade() else { return };
            inner.scrolled.grab_focus();

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

fn scroll_fraction(adj: &gtk4::Adjustment) -> f64 {
    let range = adj.upper() - adj.page_size();
    if range > 0.0 {
        adj.value() / range
    } else {
        0.0
    }
}

/// Restore after the list has re-measured (its size estimate settles on idle).
fn restore_scroll(adj: &gtk4::Adjustment, value: f64) {
    adj.set_value(value);
    let adj = adj.clone();
    glib::idle_add_local_once(move || adj.set_value(value));
}

fn restore_scroll_fraction(adj: &gtk4::Adjustment, fraction: f64) {
    let adj = adj.clone();
    glib::idle_add_local_once(move || {
        adj.set_value(fraction * (adj.upper() - adj.page_size()).max(0.0));
    });
}

fn install_css() {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let Some(display) = gdk::Display::default() else { return };
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(CSS);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
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

