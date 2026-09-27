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
    Align, Box as GtkBox, DrawingArea, GestureClick, Label, ListView, NoSelection, Orientation,
    Overlay, Picture, ScrolledWindow, SignalListItemFactory,
};
use photon_core::models::TimelineItem;
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    transition: opacity 150ms ease;
}
.photon-tile:hover picture { opacity: 0.88; }
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

struct Inner {
    root: Overlay,
    scrolled: ScrolledWindow,
    store: gio::ListStore,
    items: RefCell<Rc<Vec<TimelineItem>>>,
    by_day: Cell<bool>,
    width: Cell<i32>,
    row_height: Cell<i32>,
    relayout_generation: Cell<u64>,
    cache_dir: PathBuf,
    textures: RefCell<TextureCache>,
    on_activate: RefCell<Option<ActivateFn>>,
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
            .child(&list)
            .build();

        // GTK4 has no resize signal on ordinary widgets; an invisible,
        // input-transparent DrawingArea overlaid on the view reports it.
        let size_probe = DrawingArea::new();
        size_probe.set_can_target(false);
        let root = Overlay::new();
        root.set_child(Some(&scrolled));
        root.add_overlay(&size_probe);

        let inner = Rc::new(Inner {
            root,
            scrolled,
            store,
            items: RefCell::new(Rc::new(Vec::new())),
            by_day: Cell::new(true),
            width: Cell::new(0),
            row_height: Cell::new(row_height),
            relayout_generation: Cell::new(0),
            cache_dir,
            textures: RefCell::new(TextureCache::new()),
            on_activate: RefCell::new(None),
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

    /// Called with the photo's index into the current items when a tile is clicked.
    pub fn connect_activate(&self, f: impl Fn(usize) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Box::new(f));
    }

    /// Show `items`. With `keep_scroll`, the view stays where it was (used for
    /// live refreshes during an import); otherwise it starts at the top.
    pub fn set_items(&self, items: Rc<Vec<TimelineItem>>, by_day: bool, keep_scroll: bool) {
        *self.inner.items.borrow_mut() = items;
        self.inner.by_day.set(by_day);
        let adj = self.inner.scrolled.vadjustment();
        let value = if keep_scroll { adj.value() } else { 0.0 };
        self.inner.relayout();
        restore_scroll(&adj, value);
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
        let objects: Vec<glib::BoxedAnyObject> =
            rows.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.store.splice(0, self.store.n_items(), &objects);
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
                    row_box.append(&Self::make_tile(this, tile, &item.hash));
                }
            }
        }
    }

    fn make_tile(this: &Rc<Self>, tile: &Tile, hash: &str) -> GtkBox {
        let frame = GtkBox::new(Orientation::Vertical, 0);
        frame.set_size_request(tile.width, tile.height);
        frame.set_overflow(gtk4::Overflow::Hidden);
        frame.add_css_class("photon-tile");
        frame.set_cursor_from_name(Some("pointer"));

        let picture = Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Cover);
        picture.set_can_shrink(true);
        picture.set_vexpand(true);
        frame.append(&picture);

        if let Some(texture) = this.textures.borrow_mut().get(hash) {
            picture.set_paintable(Some(&texture));
        } else {
            Self::load_texture_later(this, &picture, hash.to_string());
        }

        let click = GestureClick::new();
        let weak = Rc::downgrade(this);
        let index = tile.index;
        click.connect_released(move |_, _, _, _| {
            if let Some(inner) = weak.upgrade() {
                if let Some(f) = inner.on_activate.borrow().as_ref() {
                    f(index);
                }
            }
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
}
