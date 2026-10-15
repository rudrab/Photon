//! Batch Tagging Popover for selected photos (Shotwell / Lightroom style).
//!
//! Allows viewing tags present across a selection (with partial counts like
//! "wedding · 120 of 300"), adding existing or new hierarchical tags
//! (e.g. "Places|France|Paris"), and batch removing tags.
//! Operations apply shot-wide (RAW+JPG companions), sync to XMP sidecars
//! in the background, and integrate with 1-action undo/redo.

use crate::ui::selection_bar::Context;
use crate::ui::undo::{self, UndoAction};
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Entry, FlowBox, Label, ListBox, ListBoxRow, Orientation,
    PolicyType, Popover, ScrolledWindow,
};
use photon_core::db::queries;
use std::cell::RefCell;
use std::rc::Rc;
use std::thread;

/// Show the batch tagging popover anchored to `target` for `image_ids`.
pub fn show_batch_tags_popover(
    target: &impl IsA<gtk4::Widget>,
    image_ids: Vec<i64>,
    ctx: &Context,
) {
    if image_ids.is_empty() {
        return;
    }

    let popover = Popover::new();
    popover.set_parent(target);
    popover.set_autohide(true);

    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_size_request(340, -1);

    // ── Header: Title & Selection Count ──────────────────────────
    let header_box = GtkBox::new(Orientation::Horizontal, 8);
    let title = Label::new(Some("Tags"));
    title.add_css_class("heading");
    title.set_halign(Align::Start);
    title.set_hexpand(true);
    header_box.append(&title);

    let total_selected_shots = if let Ok(conn) = ctx.db.conn() {
        queries::count_distinct_shots(&conn, &image_ids).unwrap_or(image_ids.len())
    } else {
        image_ids.len()
    };

    let count_label = Label::new(Some(&format!(
        "{} photo{}",
        total_selected_shots,
        if total_selected_shots == 1 { "" } else { "s" }
    )));
    count_label.add_css_class("dim-label");
    count_label.set_halign(Align::End);
    header_box.append(&count_label);
    root.append(&header_box);

    // ── Input & Add Button ───────────────────────────────────────
    let input_box = GtkBox::new(Orientation::Horizontal, 6);
    let entry = Entry::new();
    entry.set_placeholder_text(Some("+ Add tag (e.g. Places|Paris)..."));
    entry.set_hexpand(true);
    input_box.append(&entry);

    let add_btn = Button::from_icon_name("list-add-symbolic");
    add_btn.set_tooltip_text(Some("Add Tag"));
    add_btn.add_css_class("suggested-action");
    input_box.append(&add_btn);
    root.append(&input_box);

    // ── Autocomplete Suggestions List (revealed when typing) ─────
    let suggestions_scroll = ScrolledWindow::new();
    suggestions_scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
    suggestions_scroll.set_max_content_height(140);
    suggestions_scroll.set_propagate_natural_height(true);
    suggestions_scroll.set_visible(false);

    let suggestions_list = ListBox::new();
    suggestions_list.add_css_class("boxed-list");
    suggestions_scroll.set_child(Some(&suggestions_list));
    root.append(&suggestions_scroll);

    // ── Applied Tags Section ─────────────────────────────────────
    let section_label = Label::new(Some("Applied Tags"));
    section_label.add_css_class("caption-heading");
    section_label.add_css_class("dim-label");
    section_label.set_halign(Align::Start);
    root.append(&section_label);

    let chips_scroll = ScrolledWindow::new();
    chips_scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
    chips_scroll.set_max_content_height(180);
    chips_scroll.set_propagate_natural_height(true);

    let chips_container = GtkBox::new(Orientation::Vertical, 4);
    chips_scroll.set_child(Some(&chips_container));
    root.append(&chips_scroll);

    // Fetch all known tags for autocomplete
    let all_tags_with_paths = Rc::new(RefCell::new(Vec::<(i64, String, String)>::new()));
    if let Ok(conn) = ctx.db.conn() {
        if let Ok(tags) = queries::get_all_tags_with_paths(&conn) {
            let mut list = Vec::new();
            for (t, path) in tags {
                if let Some(id) = t.id {
                    list.push((id, t.name, path));
                }
            }
            *all_tags_with_paths.borrow_mut() = list;
        }
    }

    // Refresh chips function
    let render_chips: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let render_self = render_chips.clone();

    let render_action = {
        let db = ctx.db.clone();
        let chips_container = chips_container.clone();
        let image_ids = image_ids.clone();
        let total_selected = total_selected_shots;
        let ctx = ctx.clone();
        let render_self = render_self.clone();

        Rc::new(move || {
            while let Some(child) = chips_container.first_child() {
                chips_container.remove(&child);
            }

            let Ok(conn) = db.conn() else { return };
            let tags = queries::get_tags_for_images(&conn, &image_ids).unwrap_or_default();

            if tags.is_empty() {
                let empty_lbl = Label::new(Some("No tags applied yet"));
                empty_lbl.add_css_class("dim-label");
                empty_lbl.set_halign(Align::Start);
                empty_lbl.set_margin_start(4);
                empty_lbl.set_margin_top(4);
                chips_container.append(&empty_lbl);
                return;
            }

            let flow = FlowBox::new();
            flow.set_selection_mode(gtk4::SelectionMode::None);
            flow.set_max_children_per_line(10);
            flow.set_row_spacing(4);
            flow.set_column_spacing(4);

            for (tag, path, count) in tags {
                let Some(tag_id) = tag.id else { continue };
                let is_partial = count < total_selected;

                let chip = GtkBox::new(Orientation::Horizontal, 4);
                chip.add_css_class("tag-chip");
                if is_partial {
                    chip.add_css_class("partial");
                }

                // Label formatted as "tag · count of total" or "tag · count"
                let count_text = if is_partial {
                    format!("#{} · {} of {}", tag.name, count, total_selected)
                } else {
                    format!("#{} · {}", tag.name, total_selected)
                };
                let chip_label = Label::new(Some(&count_text));
                chip.append(&chip_label);

                // Tooltip with hierarchical path and details
                let tooltip = format!(
                    "Path: {}\nApplied to {} of {} selected photo{}",
                    path,
                    count,
                    total_selected,
                    if total_selected == 1 { "" } else { "s" }
                );
                chip.set_tooltip_text(Some(&tooltip));

                // Remove button
                let rm_btn = Button::from_icon_name("window-close-symbolic");
                rm_btn.add_css_class("flat");
                rm_btn.add_css_class("tag-remove-btn");
                rm_btn.set_tooltip_text(Some(&format!("Remove '#{}' from selection", tag.name)));

                let db_rm = db.clone();
                let sel_ids = image_ids.clone();
                let t_name = tag.name.clone();
                let ctx_rm = ctx.clone();
                let render_call = render_self.clone();

                rm_btn.connect_clicked(move |_| {
                    let Ok(mut conn) = db_rm.conn() else { return };
                    // Shot-wide expansion
                    let expanded_ids = queries::expand_shot_image_ids(&conn, &sel_ids).unwrap_or_else(|_| sel_ids.clone());
                    match queries::batch_untag_images(&mut conn, &expanded_ids, tag_id) {
                        Ok(_removed) => {
                            let removed_shots = queries::count_distinct_shots(&conn, &expanded_ids).unwrap_or(sel_ids.len());
                            // Record undo action
                            ctx_rm.undo_manager.borrow_mut().push(UndoAction::TagRemove {
                                image_ids: expanded_ids.clone(),
                                tag_id,
                                tag_name: t_name.clone(),
                            });

                            // Background XMP sync
                            let db_bg = db_rm.clone();
                            let ids_sync = expanded_ids.clone();
                            thread::spawn(move || {
                                if let Ok(conn) = db_bg.conn() {
                                    for id in ids_sync {
                                        undo::sync_tags_xmp(&conn, id);
                                    }
                                }
                            });

                            (ctx_rm.share.notify)(&format!(
                                "Removed tag '#{}' from {} photo(s)",
                                t_name, removed_shots
                            ));
                            (ctx_rm.on_library_changed)();

                            if let Some(r) = render_call.borrow().as_ref() {
                                r();
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to untag images: {e}");
                        }
                    }
                });

                chip.append(&rm_btn);
                flow.append(&chip);
            }

            chips_container.append(&flow);
        })
    };

    *render_chips.borrow_mut() = Some(render_action.clone());
    render_action();

    // ── Helper: Apply tag by text ────────────────────────────────
    let apply_tag = {
        let db = ctx.db.clone();
        let image_ids = image_ids.clone();
        let ctx = ctx.clone();
        let render_chips = render_chips.clone();
        let entry = entry.clone();
        let suggestions_scroll = suggestions_scroll.clone();
        let all_tags = all_tags_with_paths.clone();

        Rc::new(move |raw_text: String| {
            let text = raw_text.trim().to_string();
            if text.is_empty() {
                return;
            }

            let Ok(mut conn) = db.conn() else { return };
            let expanded_ids = queries::expand_shot_image_ids(&conn, &image_ids).unwrap_or_else(|_| image_ids.clone());

            match queries::ensure_tag(&conn, &text) {
                Ok(tag_id) => {
                    match queries::batch_tag_images(&mut conn, &expanded_ids, tag_id) {
                        Ok(_added) => {
                            ctx.undo_manager.borrow_mut().push(UndoAction::TagAdd {
                                image_ids: expanded_ids.clone(),
                                tag_id,
                                tag_name: text.clone(),
                            });

                            // Background XMP sync
                            let db_bg = db.clone();
                            let ids_sync = expanded_ids.clone();
                            thread::spawn(move || {
                                if let Ok(conn) = db_bg.conn() {
                                    for id in ids_sync {
                                        undo::sync_tags_xmp(&conn, id);
                                    }
                                }
                            });

                            // Refresh all_tags cache if new tag was created
                            if let Ok(updated_tags) = queries::get_all_tags_with_paths(&conn) {
                                let mut list = Vec::new();
                                for (t, path) in updated_tags {
                                    if let Some(id) = t.id {
                                        list.push((id, t.name, path));
                                    }
                                }
                                *all_tags.borrow_mut() = list;
                            }

                            let added_shots = queries::count_distinct_shots(&conn, &expanded_ids).unwrap_or(image_ids.len());
                            (ctx.share.notify)(&format!(
                                "Added tag '#{}' to {} photo(s)",
                                text, added_shots
                            ));
                            (ctx.on_library_changed)();

                            entry.set_text("");
                            suggestions_scroll.set_visible(false);

                            if let Some(r) = render_chips.borrow().as_ref() {
                                r();
                            }
                        }
                        Err(e) => log::error!("Failed to batch tag images: {e}"),
                    }
                }
                Err(e) => log::error!("Failed to ensure tag '{text}': {e}"),
            }
        })
    };

    // Connect Entry Activate (Enter key)
    let apply_entry = apply_tag.clone();
    entry.connect_activate(move |e| {
        apply_entry(e.text().to_string());
    });

    // Connect Add Button Click
    let apply_btn = apply_tag.clone();
    let entry_btn = entry.clone();
    add_btn.connect_clicked(move |_| {
        apply_btn(entry_btn.text().to_string());
    });

    // ── Connect Autocomplete Suggestions on Entry Changed ────────
    let all_tags_clone = all_tags_with_paths.clone();
    let sugg_scroll = suggestions_scroll.clone();
    let sugg_list = suggestions_list.clone();
    let apply_sugg = apply_tag.clone();

    entry.connect_changed(move |e| {
        let query = e.text().trim().to_lowercase();
        while let Some(child) = sugg_list.first_child() {
            sugg_list.remove(&child);
        }

        if query.is_empty() {
            sugg_scroll.set_visible(false);
            return;
        }

        let tags = all_tags_clone.borrow();
        let mut matches = 0;

        for (_id, name, path) in tags.iter() {
            if name.to_lowercase().contains(&query) || path.to_lowercase().contains(&query) {
                let row = ListBoxRow::new();
                let box_row = GtkBox::new(Orientation::Horizontal, 8);
                box_row.set_margin_start(8);
                box_row.set_margin_end(8);
                box_row.set_margin_top(4);
                box_row.set_margin_bottom(4);

                let icon = gtk4::Image::from_icon_name("tag-symbolic");
                box_row.append(&icon);

                let display_text = if path != name {
                    format!("{} ({})", name, path)
                } else {
                    name.clone()
                };
                let lbl = Label::new(Some(&display_text));
                lbl.set_halign(Align::Start);
                lbl.set_hexpand(true);
                lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                box_row.append(&lbl);

                row.set_child(Some(&box_row));

                let tag_target = path.clone();
                let apply_row = apply_sugg.clone();
                row.connect_activate(move |_| {
                    apply_row(tag_target.clone());
                });

                sugg_list.append(&row);
                matches += 1;
                if matches >= 8 {
                    break;
                }
            }
        }

        sugg_scroll.set_visible(matches > 0);
    });

    // Handle suggestion row click/activation
    suggestions_list.connect_row_activated(|_, row| {
        row.emit_activate();
    });

    popover.set_child(Some(&root));
    popover.popup();
}
