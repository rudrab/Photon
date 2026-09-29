//! Fullscreen slideshow for Photon:
//!
//!   * Fullscreen dark view of selected photos or the current timeline.
//!   * Uses Large previews with asynchronous preloading.
//!   * Configurable 3s / 5s / 10s auto-advance interval.
//!   * Space to pause/resume, arrow keys to step.
//!   * Auto-hiding OSD control bar on mouse inactivity.
//!   * Activated by F5 or menu.

use gtk4::prelude::*;
use gtk4::{
    gdk, glib, Align, Box as GtkBox, Button, EventControllerKey, EventControllerMotion,
    GestureClick, Label, Orientation, Overlay, Picture, Revealer, ToggleButton, Window,
};
use photon_core::models::TimelineItem;
use photon_import::thumbnails::{thumb_path, ThumbSize};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

pub fn start(
    parent: &impl IsA<gtk4::Window>,
    items: Vec<TimelineItem>,
    cache_dir: PathBuf,
) {
    if items.is_empty() {
        return;
    }

    let window = Window::builder()
        .title("Slideshow — Photon")
        .transient_for(parent)
        .modal(true)
        .decorated(false)
        .build();

    window.fullscreen();
    window.add_css_class("photon-slideshow");

    let root_overlay = Overlay::new();
    root_overlay.set_vexpand(true);
    root_overlay.set_hexpand(true);

    let picture = Picture::new();
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    root_overlay.set_child(Some(&picture));

    // ── Floating OSD Toolbar ────────────────────────────
    let osd_revealer = Revealer::new();
    osd_revealer.set_transition_type(gtk4::RevealerTransitionType::SlideUp);
    osd_revealer.set_valign(Align::End);
    osd_revealer.set_halign(Align::Center);
    osd_revealer.set_margin_bottom(24);
    osd_revealer.set_reveal_child(true);

    let osd_bar = GtkBox::new(Orientation::Horizontal, 8);
    osd_bar.add_css_class("photon-selection-bar");
    osd_bar.set_margin_start(16);
    osd_bar.set_margin_end(16);
    osd_bar.set_margin_top(8);
    osd_bar.set_margin_bottom(8);

    let prev_btn = Button::from_icon_name("go-previous-symbolic");
    prev_btn.add_css_class("flat");
    prev_btn.set_tooltip_text(Some("Previous slide (←)"));

    let play_pause_btn = Button::from_icon_name("media-playback-pause-symbolic");
    play_pause_btn.add_css_class("flat");
    play_pause_btn.set_tooltip_text(Some("Pause / Play (Space)"));

    let next_btn = Button::from_icon_name("go-next-symbolic");
    next_btn.add_css_class("flat");
    next_btn.set_tooltip_text(Some("Next slide (→)"));

    let interval_box = GtkBox::new(Orientation::Horizontal, 0);
    interval_box.add_css_class("linked");

    let btn_3s = ToggleButton::with_label("3s");
    let btn_5s = ToggleButton::with_label("5s");
    let btn_10s = ToggleButton::with_label("10s");
    btn_5s.set_active(true);
    btn_3s.set_group(Some(&btn_5s));
    btn_10s.set_group(Some(&btn_5s));

    interval_box.append(&btn_3s);
    interval_box.append(&btn_5s);
    interval_box.append(&btn_10s);

    let counter_label = Label::new(None);
    counter_label.add_css_class("heading");
    counter_label.set_margin_start(8);
    counter_label.set_margin_end(8);

    let close_btn = Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.set_tooltip_text(Some("Exit Slideshow (Esc / F5)"));

    osd_bar.append(&prev_btn);
    osd_bar.append(&play_pause_btn);
    osd_bar.append(&next_btn);
    osd_bar.append(&interval_box);
    osd_bar.append(&counter_label);
    osd_bar.append(&close_btn);

    osd_revealer.set_child(Some(&osd_bar));
    root_overlay.add_overlay(&osd_revealer);
    window.set_child(Some(&root_overlay));

    // ── Slideshow State ─────────────────────────────────
    let current_idx = Rc::new(Cell::new(0usize));
    let is_playing = Rc::new(Cell::new(true));
    let interval_secs = Rc::new(Cell::new(5u64));
    let timer_source: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let osd_hide_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    let items = Rc::new(items);
    let total = items.len();

    // Show slide helper
    let show_slide: Rc<dyn Fn(usize)> = {
        let picture = picture.clone();
        let counter_label = counter_label.clone();
        let items = items.clone();
        let cache_dir = cache_dir.clone();
        Rc::new(move |idx: usize| {
            if idx >= items.len() {
                return;
            }
            counter_label.set_text(&format!("{} / {}", idx + 1, total));
            let item = &items[idx];
            load_slide_image(&picture, item, &cache_dir);

            // Preload next image into OS cache / disk
            if idx + 1 < items.len() {
                let next_item = &items[idx + 1];
                let next_large = thumb_path(&cache_dir, ThumbSize::Large, &next_item.hash);
                if !next_large.exists() {
                    let next_grid = thumb_path(&cache_dir, ThumbSize::Grid, &next_item.hash);
                    let _ = next_grid.exists();
                }
            }
        })
    };

    // Auto-advance scheduling
    let schedule_timer: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    {
        let show_slide = show_slide.clone();
        let current_idx = current_idx.clone();
        let is_playing = is_playing.clone();
        let interval_secs = interval_secs.clone();
        let schedule_timer_in = schedule_timer.clone();
        let timer_source = timer_source.clone();
        let total = total;

        let schedule_fn = Rc::new(move || {
            if let Some(s) = timer_source.borrow_mut().take() {
                s.remove();
            }
            if !is_playing.get() {
                return;
            }
            let secs = interval_secs.get();
            let show_slide_c = show_slide.clone();
            let current_idx_c = current_idx.clone();
            let timer_source_c = timer_source.clone();
            let total = total;
            let schedule_again = schedule_timer_in.clone();

            let id = glib::timeout_add_local_once(Duration::from_secs(secs), move || {
                let next = (current_idx_c.get() + 1) % total;
                current_idx_c.set(next);
                show_slide_c(next);
                timer_source_c.borrow_mut().take();
                if let Some(f) = schedule_again.borrow().as_ref() {
                    f();
                }
            });
            *timer_source.borrow_mut() = Some(id);
        });
        *schedule_timer.borrow_mut() = Some(schedule_fn.clone());
        schedule_fn();
    }

    // Initial slide
    show_slide(0);

    // OSD auto-hide on inactivity
    let show_osd = {
        let osd_revealer = osd_revealer.clone();
        let osd_hide_timer = osd_hide_timer.clone();
        let window = window.clone();
        Rc::new(move || {
            osd_revealer.set_reveal_child(true);
            window.set_cursor_from_name(Some("default"));
            if let Some(t) = osd_hide_timer.borrow_mut().take() {
                t.remove();
            }
            let osd = osd_revealer.clone();
            let timer_ref = osd_hide_timer.clone();
            let win = window.clone();
            let id = glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                osd.set_reveal_child(false);
                win.set_cursor_from_name(Some("none"));
                timer_ref.borrow_mut().take();
            });
            *osd_hide_timer.borrow_mut() = Some(id);
        })
    };
    show_osd();

    let motion = EventControllerMotion::new();
    let so = show_osd.clone();
    motion.connect_motion(move |_, _, _| {
        so();
    });
    window.add_controller(motion);

    // Click anywhere toggles play/pause
    let click = GestureClick::new();
    let ip = is_playing.clone();
    let pb = play_pause_btn.clone();
    let st = schedule_timer.clone();
    click.connect_released(move |_, n_press, _, _| {
        if n_press == 1 {
            let playing = !ip.get();
            ip.set(playing);
            if playing {
                pb.set_icon_name("media-playback-pause-symbolic");
                if let Some(f) = st.borrow().as_ref() {
                    f();
                }
            } else {
                pb.set_icon_name("media-playback-start-symbolic");
            }
        }
    });
    picture.add_controller(click);

    // Step next/prev
    let step = {
        let current_idx = current_idx.clone();
        let show_slide = show_slide.clone();
        let st = schedule_timer.clone();
        let total = total;
        Rc::new(move |delta: isize| {
            let cur = current_idx.get() as isize;
            let next = (cur + delta).rem_euclid(total as isize) as usize;
            current_idx.set(next);
            show_slide(next);
            if let Some(f) = st.borrow().as_ref() {
                f();
            }
        })
    };

    let step_p = step.clone();
    prev_btn.connect_clicked(move |_| step_p(-1));

    let step_n = step.clone();
    next_btn.connect_clicked(move |_| step_n(1));

    let ip = is_playing.clone();
    let pb = play_pause_btn.clone();
    let st = schedule_timer.clone();
    play_pause_btn.connect_clicked(move |_| {
        let playing = !ip.get();
        ip.set(playing);
        if playing {
            pb.set_icon_name("media-playback-pause-symbolic");
            if let Some(f) = st.borrow().as_ref() {
                f();
            }
        } else {
            pb.set_icon_name("media-playback-start-symbolic");
        }
    });

    let is_secs = interval_secs.clone();
    let st = schedule_timer.clone();
    btn_3s.connect_toggled(glib::clone!(
        #[strong] is_secs,
        #[strong] st,
        move |b| {
            if b.is_active() {
                is_secs.set(3);
                if let Some(f) = st.borrow().as_ref() {
                    f();
                }
            }
        }
    ));

    let is_secs = interval_secs.clone();
    let st = schedule_timer.clone();
    btn_5s.connect_toggled(glib::clone!(
        #[strong] is_secs,
        #[strong] st,
        move |b| {
            if b.is_active() {
                is_secs.set(5);
                if let Some(f) = st.borrow().as_ref() {
                    f();
                }
            }
        }
    ));

    let is_secs = interval_secs.clone();
    let st = schedule_timer.clone();
    btn_10s.connect_toggled(glib::clone!(
        #[strong] is_secs,
        #[strong] st,
        move |b| {
            if b.is_active() {
                is_secs.set(10);
                if let Some(f) = st.borrow().as_ref() {
                    f();
                }
            }
        }
    ));

    let win_c = window.clone();
    close_btn.connect_clicked(move |_| {
        win_c.close();
    });

    // Keyboard Controller
    let key_ctrl = EventControllerKey::new();
    let win_k = window.clone();
    let step_k = step.clone();
    let ip_k = is_playing.clone();
    let pb_k = play_pause_btn.clone();
    let st_k = schedule_timer.clone();
    let b3 = btn_3s.clone();
    let b5 = btn_5s.clone();
    let b10 = btn_10s.clone();
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        match key {
            gdk::Key::Escape | gdk::Key::F5 | gdk::Key::q | gdk::Key::Q => {
                win_k.close();
                glib::Propagation::Stop
            }
            gdk::Key::space => {
                let playing = !ip_k.get();
                ip_k.set(playing);
                if playing {
                    pb_k.set_icon_name("media-playback-pause-symbolic");
                    if let Some(f) = st_k.borrow().as_ref() {
                        f();
                    }
                } else {
                    pb_k.set_icon_name("media-playback-start-symbolic");
                }
                glib::Propagation::Stop
            }
            gdk::Key::Left | gdk::Key::Up | gdk::Key::Page_Up => {
                step_k(-1);
                glib::Propagation::Stop
            }
            gdk::Key::Right | gdk::Key::Down | gdk::Key::Page_Down => {
                step_k(1);
                glib::Propagation::Stop
            }
            gdk::Key::_3 => {
                b3.set_active(true);
                glib::Propagation::Stop
            }
            gdk::Key::_5 => {
                b5.set_active(true);
                glib::Propagation::Stop
            }
            gdk::Key::_0 | gdk::Key::_1 => {
                b10.set_active(true);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(key_ctrl);

    // Teardown cleanup on destroy
    let ts_clean = timer_source.clone();
    let osd_clean = osd_hide_timer.clone();
    window.connect_destroy(move |_| {
        if let Some(s) = ts_clean.borrow_mut().take() {
            s.remove();
        }
        if let Some(s) = osd_clean.borrow_mut().take() {
            s.remove();
        }
    });

    window.present();
}

fn load_slide_image(picture: &Picture, item: &TimelineItem, cache_dir: &Path) {
    let large = thumb_path(cache_dir, ThumbSize::Large, &item.hash);
    if large.exists() {
        picture.set_filename(Some(&large));
    } else {
        let grid = thumb_path(cache_dir, ThumbSize::Grid, &item.hash);
        if grid.exists() {
            picture.set_filename(Some(&grid));
        }
    }
}
