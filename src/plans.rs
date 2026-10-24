use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, CheckButton, DragSource, DropTarget, Entry, GestureClick, Label,
    Orientation, PolicyType, Popover, ScrolledWindow,
};
use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

pub struct PlansUi {
    pub list_view: Box,
    pub detail_view: Box,
}

#[derive(Clone, Copy, PartialEq)]
enum FormatKind {
    Check,
    Bullet,
    H1,
    H2,
    Step,
    Subnote,
}

fn get_plans_dir() -> PathBuf {
    let home = glib::home_dir();
    let lower = home.join("documents").join("plans");
    if lower.exists() {
        return lower;
    }
    let upper = home.join("Documents").join("plans");
    let _ = fs::create_dir_all(&upper);
    upper
}

fn style_wrapped_label(lbl: &Label, edit_mode: bool) {
    lbl.set_wrap(true);
    lbl.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    lbl.set_max_width_chars(if edit_mode { 26 } else { 40 });
    lbl.set_halign(Align::Start);
    lbl.set_xalign(0.0);
    lbl.set_hexpand(true);
}

fn count_plan_progress(path: &Path) -> (usize, usize) {
    let Ok(content) = fs::read_to_string(path) else {
        return (0, 0);
    };
    let mut checked = 0;
    let mut total = 0;
    for line in content.lines() {
        if let Some((is_done, _)) = parse_checkbox_line(line.trim()) {
            total += 1;
            if is_done {
                checked += 1;
            }
        }
    }
    (checked, total)
}

pub fn build_plans_ui<F>(popover_guard: Rc<Cell<usize>>, on_expand: F) -> PlansUi
where
    F: Fn(bool) + 'static,
{
    let on_expand: Rc<dyn Fn(bool)> = Rc::new(on_expand);

    let list_view = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let header_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let title_lbl = Label::builder()
        .label("ARCHIVE & PLANS")
        .css_classes(vec!["fav-group-header".to_string()])
        .halign(Align::Start)
        .hexpand(true)
        .build();

    let refresh_btn = Button::builder()
        .label("Reload")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Reload plans folder")
        .build();

    header_box.append(&title_lbl);
    header_box.append(&refresh_btn);
    list_view.append(&header_box);

    let list_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let files_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    list_scroll.set_child(Some(&files_box));
    list_view.append(&list_scroll);

    let create_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_top(4)
        .build();

    let new_plan_entry = Entry::builder()
        .placeholder_text("New plan name...")
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();

    let new_plan_btn = Button::builder()
        .label("+")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Create new plan file")
        .build();

    create_box.append(&new_plan_entry);
    create_box.append(&new_plan_btn);
    list_view.append(&create_box);

    let detail_view = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["plan-detail-pane".to_string()])
        .width_request(400)
        .vexpand(true)
        .visible(false)
        .build();

    populate_plan_buttons(&files_box, &detail_view, &popover_guard, &on_expand, None);

    let files_refresh = files_box.clone();
    let detail_refresh = detail_view.clone();
    let guard_refresh = popover_guard.clone();
    let expand_refresh = on_expand.clone();
    refresh_btn.connect_clicked(move |_| {
        populate_plan_buttons(
            &files_refresh,
            &detail_refresh,
            &guard_refresh,
            &expand_refresh,
            None,
        );
    });

    let files_create = files_box.clone();
    let detail_create = detail_view.clone();
    let guard_create = popover_guard.clone();
    let expand_create = on_expand.clone();
    let entry_create = new_plan_entry.clone();

    let create_action = Rc::new(move || {
        let raw_name = entry_create.text().trim().to_string();
        if raw_name.is_empty() {
            return;
        }
        let safe_stem: String = raw_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();

        let dir = get_plans_dir();
        let file_path = dir.join(format!("{}.txt", safe_stem.trim_matches('_')));
        if !file_path.exists() {
            let initial_content = format!("# {}\n\n", raw_name.to_uppercase());
            let _ = fs::write(&file_path, initial_content);
        }

        entry_create.set_text("");
        populate_plan_buttons(
            &files_create,
            &detail_create,
            &guard_create,
            &expand_create,
            None,
        );
        render_plan_file(
            &file_path,
            &files_create,
            &detail_create,
            &guard_create,
            &expand_create,
            true,
            None,
        );
        detail_create.set_visible(true);
        expand_create(true);
    });

    let create_on_btn = create_action.clone();
    new_plan_btn.connect_clicked(move |_| create_on_btn());

    let create_on_enter = create_action.clone();
    new_plan_entry.connect_activate(move |_| create_on_enter());

    PlansUi {
        list_view,
        detail_view,
    }
}

fn populate_plan_buttons(
    files_box: &Box,
    detail_view: &Box,
    popover_guard: &Rc<Cell<usize>>,
    on_expand: &Rc<dyn Fn(bool)>,
    renaming_path: Option<PathBuf>,
) {
    while let Some(child) = files_box.first_child() {
        files_box.remove(&child);
    }

    let dir = get_plans_dir();
    let mut plans: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if ext == "txt" || ext == "md" {
                    plans.push(path);
                }
            }
        }
    }

    plans.sort();

    if plans.is_empty() {
        let empty = Label::builder()
            .label(&format!(
                "No .txt/.md files found in\n{}",
                dir.display()
            ))
            .wrap(true)
            .css_classes(vec!["fav-score-badge".to_string()])
            .margin_top(16)
            .build();
        files_box.append(&empty);
        return;
    }

    for path in plans {
        let pretty_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().replace('_', " "))
            .unwrap_or_else(|| "Plan".to_string());

        // If this plan is currently being renamed, show an inline Entry row!
        if renaming_path.as_deref() == Some(path.as_path()) {
            let rename_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(4)
                .build();

            let rename_entry = Entry::builder()
                .text(&pretty_name)
                .css_classes(vec!["fav-search".to_string()])
                .hexpand(true)
                .build();

            let save_btn = Button::builder()
                .label("Save")
                .css_classes(vec!["plan-close-btn".to_string()])
                .build();

            let cancel_btn = Button::builder()
                .label("x")
                .css_classes(vec!["plan-line-del-btn".to_string()])
                .build();

            let old_path = path.clone();
            let files_save = files_box.clone();
            let detail_save = detail_view.clone();
            let guard_save = popover_guard.clone();
            let expand_save = on_expand.clone();
            let entry_save = rename_entry.clone();

            let commit_rename = Rc::new(move || {
                let raw_new = entry_save.text().trim().to_string();
                if !raw_new.is_empty() {
                    if let Some(new_path) = rename_plan_file(&old_path, &raw_new) {
                        // If the detail pane is currently open, re-render with the new path
                        if detail_save.is_visible() {
                            render_plan_file(
                                &new_path,
                                &files_save,
                                &detail_save,
                                &guard_save,
                                &expand_save,
                                false,
                                None,
                            );
                        }
                    }
                }
                populate_plan_buttons(
                    &files_save,
                    &detail_save,
                    &guard_save,
                    &expand_save,
                    None,
                );
            });

            let commit_btn = commit_rename.clone();
            save_btn.connect_clicked(move |_| commit_btn());

            let commit_enter = commit_rename.clone();
            rename_entry.connect_activate(move |_| commit_enter());

            let files_cancel = files_box.clone();
            let detail_cancel = detail_view.clone();
            let guard_cancel = popover_guard.clone();
            let expand_cancel = on_expand.clone();
            cancel_btn.connect_clicked(move |_| {
                populate_plan_buttons(
                    &files_cancel,
                    &detail_cancel,
                    &guard_cancel,
                    &expand_cancel,
                    None,
                );
            });

            rename_row.append(&rename_entry);
            rename_row.append(&save_btn);
            rename_row.append(&cancel_btn);
            files_box.append(&rename_row);
            rename_entry.grab_focus();
            continue;
        }

        let (checked, total) = count_plan_progress(&path);
        let right_text = if total > 0 {
            format!("[{}/{}] ->", checked, total)
        } else {
            "->".to_string()
        };

        let row_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .build();

        let btn = Button::builder()
            .css_classes(vec!["fav-item-btn".to_string()])
            .hexpand(true)
            .tooltip_text("Left-click to read | Right-click to Edit / Rename / Copy / Delete")
            .build();

        let btn_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();

        let lbl = Label::builder()
            .label(&pretty_name)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();

        let arrow = Label::builder()
            .label(&right_text)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();

        btn_box.append(&lbl);
        btn_box.append(&arrow);
        btn.set_child(Some(&btn_box));

        let path_read = path.clone();
        let files_read = files_box.clone();
        let detail_read = detail_view.clone();
        let guard_read = popover_guard.clone();
        let expand_read = on_expand.clone();
        btn.connect_clicked(move |_| {
            render_plan_file(
                &path_read,
                &files_read,
                &detail_read,
                &guard_read,
                &expand_read,
                false,
                None,
            );
            detail_read.set_visible(true);
            expand_read(true);
        });

        let popover = Popover::builder()
            .css_classes(vec!["fav-popover".to_string()])
            .autohide(true)
            .build();
        popover.set_parent(&row_box);

        let guard_map = popover_guard.clone();
        popover.connect_map(move |_| {
            guard_map.set(guard_map.get() + 1);
        });
        let guard_unmap = popover_guard.clone();
        popover.connect_unmap(move |_| {
            let g = guard_unmap.clone();
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                g.set(g.get().saturating_sub(1));
            });
        });

        let menu_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .build();

        let edit_btn = Button::builder()
            .label("Edit Plan")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let rename_btn = Button::builder()
            .label("Rename Plan")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let copy_btn = Button::builder()
            .label("Copy Plan")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let delete_btn = Button::builder()
            .label("Delete Plan")
            .css_classes(vec!["plan-delete-menu-btn".to_string()])
            .build();

        menu_box.append(&edit_btn);
        menu_box.append(&rename_btn);
        menu_box.append(&copy_btn);
        menu_box.append(&delete_btn);
        popover.set_child(Some(&menu_box));

        let right_click = GestureClick::builder().button(3).build();
        let pop_open = popover.clone();
        right_click.connect_pressed(move |_, _, _, _| {
            pop_open.popup();
        });
        btn.add_controller(right_click);

        // 1. Edit Plan
        let path_edit = path.clone();
        let files_edit = files_box.clone();
        let detail_edit = detail_view.clone();
        let guard_edit = popover_guard.clone();
        let expand_edit = on_expand.clone();
        let pop_edit = popover.clone();
        edit_btn.connect_clicked(move |_| {
            pop_edit.popdown();
            render_plan_file(
                &path_edit,
                &files_edit,
                &detail_edit,
                &guard_edit,
                &expand_edit,
                true,
                None,
            );
            detail_edit.set_visible(true);
            expand_edit(true);
        });

        // 2. Rename Plan
        let path_ren = path.clone();
        let files_ren = files_box.clone();
        let detail_ren = detail_view.clone();
        let guard_ren = popover_guard.clone();
        let expand_ren = on_expand.clone();
        let pop_ren = popover.clone();
        rename_btn.connect_clicked(move |_| {
            pop_ren.popdown();
            populate_plan_buttons(
                &files_ren,
                &detail_ren,
                &guard_ren,
                &expand_ren,
                Some(path_ren.clone()),
            );
        });

        // 3. Copy Plan
        let path_copy = path.clone();
        let files_copy = files_box.clone();
        let detail_copy = detail_view.clone();
        let guard_copy = popover_guard.clone();
        let expand_copy = on_expand.clone();
        let pop_copy = popover.clone();
        copy_btn.connect_clicked(move |_| {
            pop_copy.popdown();
            let copy_path = make_copy_path(&path_copy);
            if fs::copy(&path_copy, &copy_path).is_ok() {
                populate_plan_buttons(&files_copy, &detail_copy, &guard_copy, &expand_copy, None);
            }
        });

        // 4. Delete Plan
        let path_del = path.clone();
        let files_del = files_box.clone();
        let detail_del = detail_view.clone();
        let guard_del = popover_guard.clone();
        let expand_del = on_expand.clone();
        let pop_del = popover.clone();
        delete_btn.connect_clicked(move |_| {
            pop_del.popdown();
            let _ = fs::remove_file(&path_del);
            detail_del.set_visible(false);
            expand_del(false);
            populate_plan_buttons(&files_del, &detail_del, &guard_del, &expand_del, None);
        });

        row_box.append(&btn);
        files_box.append(&row_box);
    }
}

fn rename_plan_file(old_path: &Path, new_raw_name: &str) -> Option<PathBuf> {
    let dir = old_path.parent()?;
    let ext = old_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");

    let safe_stem: String = new_raw_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();

    let clean_stem = safe_stem.trim_matches('_');
    if clean_stem.is_empty() {
        return None;
    }

    let new_path = dir.join(format!("{}.{}", clean_stem, ext));
    if new_path == old_path {
        return Some(old_path.to_path_buf());
    }

    fs::rename(old_path, &new_path).ok()?;
    Some(new_path)
}

fn make_copy_path(original: &Path) -> PathBuf {
    let dir = original.parent().unwrap_or_else(|| Path::new("."));
    let stem = original
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plan".to_string());
    let ext = original
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "txt".to_string());

    let mut candidate = dir.join(format!("{}_copy.{}", stem, ext));
    let mut counter = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{}_copy_{}.{}", stem, counter, ext));
        counter += 1;
    }
    candidate
}

fn render_plan_file(
    path: &Path,
    files_box: &Box,
    detail_view: &Box,
    popover_guard: &Rc<Cell<usize>>,
    on_expand: &Rc<dyn Fn(bool)>,
    edit_mode: bool,
    editing_line: Option<usize>,
) {
    while let Some(child) = detail_view.first_child() {
        detail_view.remove(&child);
    }

    let top_bar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_top(12)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(6)
        .build();

    let close_btn = Button::builder()
        .label("<- Close")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let expand_close = on_expand.clone();
    let detail_close = detail_view.clone();
    close_btn.connect_clicked(move |_| {
        detail_close.set_visible(false);
        expand_close(false);
    });

    let mode_toggle_btn = Button::builder()
        .label(if edit_mode { "Done" } else { "Edit" })
        .css_classes(vec![if edit_mode {
            "plan-fmt-btn".to_string()
        } else {
            "plan-close-btn".to_string()
        }])
        .build();
    if edit_mode {
        mode_toggle_btn.add_css_class("active");
    }

    let path_toggle = path.to_path_buf();
    let files_toggle = files_box.clone();
    let detail_toggle = detail_view.clone();
    let guard_toggle = popover_guard.clone();
    let expand_toggle = on_expand.clone();
    mode_toggle_btn.connect_clicked(move |_| {
        render_plan_file(
            &path_toggle,
            &files_toggle,
            &detail_toggle,
            &guard_toggle,
            &expand_toggle,
            !edit_mode,
            None,
        );
    });

    let file_badge = Label::builder()
        .label(
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        )
        .css_classes(vec!["fav-score-badge".to_string()])
        .halign(Align::End)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Start)
        .build();

    top_bar.append(&close_btn);
    top_bar.append(&mode_toggle_btn);
    top_bar.append(&file_badge);
    detail_view.append(&top_bar);

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let content_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .margin_top(6)
        .margin_bottom(16)
        .margin_start(14)
        .margin_end(14)
        .build();

    let content = fs::read_to_string(path).unwrap_or_default();

    for (line_idx, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            let spacer = Box::builder().height_request(6).build();
            content_box.append(&spacer);
            continue;
        }

        if edit_mode && editing_line == Some(line_idx) {
            let edit_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(4)
                .css_classes(vec!["plan-edit-row".to_string()])
                .build();

            let inline_entry = Entry::builder()
                .text(raw_line)
                .css_classes(vec!["fav-search".to_string()])
                .hexpand(true)
                .build();

            let save_btn = Button::builder()
                .label("Save")
                .css_classes(vec!["plan-close-btn".to_string()])
                .build();

            let cancel_btn = Button::builder()
                .label("x")
                .css_classes(vec!["plan-line-del-btn".to_string()])
                .build();

            let path_save = path.to_path_buf();
            let files_save = files_box.clone();
            let detail_save = detail_view.clone();
            let guard_save = popover_guard.clone();
            let expand_save = on_expand.clone();
            let entry_save = inline_entry.clone();

            let commit_edit = Rc::new(move || {
                let new_text = entry_save.text().to_string();
                replace_line_in_file(&path_save, line_idx, &new_text);
                populate_plan_buttons(&files_save, &detail_save, &guard_save, &expand_save, None);
                render_plan_file(
                    &path_save,
                    &files_save,
                    &detail_save,
                    &guard_save,
                    &expand_save,
                    true,
                    None,
                );
            });

            let commit_btn = commit_edit.clone();
            save_btn.connect_clicked(move |_| commit_btn());

            let commit_enter = commit_edit.clone();
            inline_entry.connect_activate(move |_| commit_enter());

            let path_cancel = path.to_path_buf();
            let files_cancel = files_box.clone();
            let detail_cancel = detail_view.clone();
            let guard_cancel = popover_guard.clone();
            let expand_cancel = on_expand.clone();
            cancel_btn.connect_clicked(move |_| {
                render_plan_file(
                    &path_cancel,
                    &files_cancel,
                    &detail_cancel,
                    &guard_cancel,
                    &expand_cancel,
                    true,
                    None,
                );
            });

            edit_row.append(&inline_entry);
            edit_row.append(&save_btn);
            edit_row.append(&cancel_btn);
            content_box.append(&edit_row);
            continue;
        }

        let item_widget: gtk4::Widget =
            if trimmed.chars().all(|c| c == '=' || c == '-') && trimmed.len() > 3 {
                Box::builder()
                    .height_request(2)
                    .margin_top(6)
                    .margin_bottom(6)
                    .hexpand(true)
                    .valign(Align::Center)
                    .css_classes(vec!["plan-divider".to_string()])
                    .build()
                    .upcast()
            } else if let Some(h1) = trimmed.strip_prefix("# ") {
                let lbl = Label::builder()
                    .label(h1)
                    .css_classes(vec!["plan-h1".to_string()])
                    .build();
                style_wrapped_label(&lbl, edit_mode);
                lbl.upcast()
            } else if let Some(h2) = trimmed.strip_prefix("## ") {
                let lbl = Label::builder()
                    .label(h2)
                    .css_classes(vec!["plan-h2".to_string()])
                    .margin_top(6)
                    .build();
                style_wrapped_label(&lbl, edit_mode);
                lbl.upcast()
            } else if let Some((is_checked, item_text)) = parse_checkbox_line(trimmed) {
                let chk_row = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .spacing(8)
                    .hexpand(true)
                    .css_classes(vec!["plan-check-row".to_string()])
                    .build();

                let check_btn = CheckButton::builder()
                    .active(is_checked)
                    .valign(Align::Start)
                    .css_classes(vec!["plan-checkbox".to_string()])
                    .build();

                let lbl = Label::builder()
                    .label(item_text)
                    .css_classes(vec!["plan-body".to_string()])
                    .build();
                style_wrapped_label(&lbl, edit_mode);

                if is_checked {
                    lbl.add_css_class("plan-checked-text");
                }

                let file_path = path.to_path_buf();
                let files_chk = files_box.clone();
                let detail_chk = detail_view.clone();
                let guard_chk = popover_guard.clone();
                let expand_chk = on_expand.clone();
                let lbl_clone = lbl.clone();
                check_btn.connect_toggled(move |btn| {
                    let active = btn.is_active();
                    if active {
                        lbl_clone.add_css_class("plan-checked-text");
                    } else {
                        lbl_clone.remove_css_class("plan-checked-text");
                    }
                    toggle_line_in_file(&file_path, line_idx, active);
                    populate_plan_buttons(&files_chk, &detail_chk, &guard_chk, &expand_chk, None);
                });

                chk_row.append(&check_btn);
                chk_row.append(&lbl);
                chk_row.upcast()
            } else if let Some(bullet) = trimmed
                .strip_prefix("* ")
                .or_else(|| trimmed.strip_prefix("- "))
            {
                let lbl = Label::builder()
                    .label(&format!("- {}", bullet))
                    .css_classes(vec!["plan-body".to_string()])
                    .margin_start(6)
                    .build();
                style_wrapped_label(&lbl, edit_mode);
                lbl.upcast()
            } else {
                let is_indented = raw_line.starts_with("  ") || raw_line.starts_with('\t');
                let is_step = trimmed.starts_with("Step ");
                let lbl = Label::builder()
                    .label(trimmed)
                    .css_classes(vec![if is_step {
                        "plan-step".to_string()
                    } else if is_indented {
                        "plan-subnote".to_string()
                    } else {
                        "plan-body".to_string()
                    }])
                    .margin_start(if is_indented { 28 } else { 0 })
                    .margin_top(if is_step { 4 } else { 0 })
                    .build();
                style_wrapped_label(&lbl, edit_mode);
                lbl.upcast()
            };

        if edit_mode {
            let line_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(4)
                .css_classes(vec!["plan-edit-row".to_string()])
                .build();

            let drag_handle = Label::builder()
                .label("::")
                .valign(Align::Center)
                .css_classes(vec!["plan-drag-handle".to_string()])
                .tooltip_text("Drag up or down to reorder")
                .build();

            let edit_line_btn = Button::builder()
                .label("e")
                .valign(Align::Center)
                .css_classes(vec!["plan-line-del-btn".to_string()])
                .tooltip_text("Edit this line in-place (or double-click row)")
                .build();

            let del_line_btn = Button::builder()
                .label("x")
                .valign(Align::Center)
                .css_classes(vec!["plan-line-del-btn".to_string()])
                .tooltip_text("Delete this line")
                .build();

            let path_ed = path.to_path_buf();
            let files_ed = files_box.clone();
            let detail_ed = detail_view.clone();
            let guard_ed = popover_guard.clone();
            let expand_ed = on_expand.clone();
            edit_line_btn.connect_clicked(move |_| {
                render_plan_file(
                    &path_ed,
                    &files_ed,
                    &detail_ed,
                    &guard_ed,
                    &expand_ed,
                    true,
                    Some(line_idx),
                );
            });

            let dbl_click = GestureClick::builder().button(1).build();
            let path_dbl = path.to_path_buf();
            let files_dbl = files_box.clone();
            let detail_dbl = detail_view.clone();
            let guard_dbl = popover_guard.clone();
            let expand_dbl = on_expand.clone();
            dbl_click.connect_pressed(move |_, n_press, _, _| {
                if n_press == 2 {
                    render_plan_file(
                        &path_dbl,
                        &files_dbl,
                        &detail_dbl,
                        &guard_dbl,
                        &expand_dbl,
                        true,
                        Some(line_idx),
                    );
                }
            });
            line_row.add_controller(dbl_click);

            let path_del = path.to_path_buf();
            let files_del = files_box.clone();
            let detail_del = detail_view.clone();
            let guard_del = popover_guard.clone();
            let expand_del = on_expand.clone();
            del_line_btn.connect_clicked(move |_| {
                delete_line_from_file(&path_del, line_idx);
                populate_plan_buttons(&files_del, &detail_del, &guard_del, &expand_del, None);
                render_plan_file(
                    &path_del,
                    &files_del,
                    &detail_del,
                    &guard_del,
                    &expand_del,
                    true,
                    None,
                );
            });

            let drag_source = DragSource::builder()
                .actions(gdk::DragAction::MOVE)
                .build();

            let src_idx = line_idx as u32;
            drag_source.connect_prepare(move |_, _, _| {
                Some(gdk::ContentProvider::for_value(&src_idx.to_value()))
            });

            let row_drag_begin = line_row.clone();
            drag_source.connect_drag_begin(move |_, _| {
                row_drag_begin.add_css_class("dragging");
            });

            let row_drag_end = line_row.clone();
            drag_source.connect_drag_end(move |_, _, _| {
                row_drag_end.remove_css_class("dragging");
            });
            line_row.add_controller(drag_source);

            let drop_target = DropTarget::new(u32::static_type(), gdk::DragAction::MOVE);

            let row_enter = line_row.clone();
            drop_target.connect_enter(move |_, _, _| {
                row_enter.add_css_class("drop-target");
                gdk::DragAction::MOVE
            });

            let row_leave = line_row.clone();
            drop_target.connect_leave(move |_| {
                row_leave.remove_css_class("drop-target");
            });

            let path_drop = path.to_path_buf();
            let files_drop = files_box.clone();
            let detail_drop = detail_view.clone();
            let guard_drop = popover_guard.clone();
            let expand_drop = on_expand.clone();
            let row_drop = line_row.clone();
            let dst_idx = line_idx;

            drop_target.connect_drop(move |_, value, _, _| {
                row_drop.remove_css_class("drop-target");
                if let Ok(from_idx) = value.get::<u32>() {
                    let from = from_idx as usize;
                    if from != dst_idx {
                        move_line_in_file(&path_drop, from, dst_idx);
                        let path_re = path_drop.clone();
                        let files_re = files_drop.clone();
                        let detail_re = detail_drop.clone();
                        let guard_re = guard_drop.clone();
                        let expand_re = expand_drop.clone();
                        glib::idle_add_local_once(move || {
                            render_plan_file(
                                &path_re,
                                &files_re,
                                &detail_re,
                                &guard_re,
                                &expand_re,
                                true,
                                None,
                            );
                        });
                        return true;
                    }
                }
                false
            });
            line_row.add_controller(drop_target);

            line_row.append(&drag_handle);
            line_row.append(&item_widget);
            line_row.append(&edit_line_btn);
            line_row.append(&del_line_btn);
            content_box.append(&line_row);
        } else {
            content_box.append(&item_widget);
        }
    }

    scrolled.set_child(Some(&content_box));
    detail_view.append(&scrolled);

    if edit_mode {
        let composer = build_composer_dock(path, files_box, detail_view, popover_guard, on_expand);
        detail_view.append(&composer);
    }
}

fn build_composer_dock(
    path: &Path,
    files_box: &Box,
    detail_view: &Box,
    popover_guard: &Rc<Cell<usize>>,
    on_expand: &Rc<dyn Fn(bool)>,
) -> Box {
    let dock = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["plan-composer-dock".to_string()])
        .build();

    let toolbar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .build();

    let active_format = Rc::new(Cell::new(FormatKind::Check));

    let entry = Entry::builder()
        .placeholder_text("Add checklist item (* [ ] ...)")
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();

    let modes: [(&str, FormatKind, &str); 6] = [
        ("[ ]", FormatKind::Check, "Add checklist item (* [ ] ...)"),
        ("-", FormatKind::Bullet, "Add bullet point (* ...)"),
        ("H1", FormatKind::H1, "Add main title (# ...)"),
        ("H2", FormatKind::H2, "Add section header (## ...)"),
        ("Step", FormatKind::Step, "Add workflow step (Step 1: ...)"),
        ("Note", FormatKind::Subnote, "Add indented note (      ...)"),
    ];

    let mut mode_buttons: Vec<Button> = Vec::new();

    for (label, kind, _) in modes {
        let mut classes = vec!["plan-fmt-btn".to_string()];
        if kind == FormatKind::Check {
            classes.push("active".to_string());
        }
        let btn = Button::builder()
            .label(label)
            .css_classes(classes)
            .build();
        mode_buttons.push(btn);
    }

    for (idx, (_, kind, placeholder)) in modes.iter().enumerate() {
        let btn = &mode_buttons[idx];
        let all_btns = mode_buttons.clone();
        let fmt_cell = active_format.clone();
        let entry_clone = entry.clone();
        let kind_val = *kind;
        let ph_text = placeholder.to_string();

        btn.connect_clicked(move |clicked_btn| {
            fmt_cell.set(kind_val);
            for b in &all_btns {
                b.remove_css_class("active");
            }
            clicked_btn.add_css_class("active");
            entry_clone.set_placeholder_text(Some(&ph_text));
            entry_clone.grab_focus();
        });

        toolbar.append(btn);
    }

    let div_btn = Button::builder()
        .label("---")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Insert horizontal divider line")
        .build();

    let path_div = path.to_path_buf();
    let files_div = files_box.clone();
    let detail_div = detail_view.clone();
    let guard_div = popover_guard.clone();
    let expand_div = on_expand.clone();
    div_btn.connect_clicked(move |_| {
        append_line_to_file(
            &path_div,
            "================================================================================",
        );
        render_plan_file(
            &path_div,
            &files_div,
            &detail_div,
            &guard_div,
            &expand_div,
            true,
            None,
        );
    });
    toolbar.append(&div_btn);

    let input_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let add_btn = Button::builder()
        .label("+ Add")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    input_row.append(&entry);
    input_row.append(&add_btn);

    let path_submit = path.to_path_buf();
    let files_submit = files_box.clone();
    let detail_submit = detail_view.clone();
    let guard_submit = popover_guard.clone();
    let expand_submit = on_expand.clone();
    let fmt_submit = active_format.clone();
    let entry_submit = entry.clone();

    let submit_action = Rc::new(move || {
        let text = entry_submit.text().trim().to_string();
        if text.is_empty() {
            return;
        }

        let formatted_line = match fmt_submit.get() {
            FormatKind::Check => format!("* [ ] {}", text),
            FormatKind::Bullet => format!("* {}", text),
            FormatKind::H1 => format!("\n# {}", text),
            FormatKind::H2 => format!("\n## {}", text),
            FormatKind::Step => {
                if text.starts_with("Step ") {
                    format!("\n{}", text)
                } else {
                    format!("\nStep {}", text)
                }
            }
            FormatKind::Subnote => {
                format!("      ({})", text.trim_matches(|c| c == '(' || c == ')'))
            }
        };

        append_line_to_file(&path_submit, &formatted_line);
        populate_plan_buttons(
            &files_submit,
            &detail_submit,
            &guard_submit,
            &expand_submit,
            None,
        );
        render_plan_file(
            &path_submit,
            &files_submit,
            &detail_submit,
            &guard_submit,
            &expand_submit,
            true,
            None,
        );
    });

    let submit_click = submit_action.clone();
    add_btn.connect_clicked(move |_| submit_click());

    let submit_enter = submit_action.clone();
    entry.connect_activate(move |_| submit_enter());

    dock.append(&toolbar);
    dock.append(&input_row);
    dock
}

fn append_line_to_file(path: &Path, line: &str) {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let needs_newline = !existing.is_empty() && !existing.ends_with('\n');

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        if needs_newline {
            let _ = writeln!(file);
        }
        let _ = writeln!(file, "{}", line);
    }
}

fn replace_line_in_file(path: &Path, target_line: usize, new_text: &str) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };

    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    if let Some(line) = lines.get_mut(target_line) {
        *line = new_text.to_string();
        let mut output = lines.join("\n");
        if content.ends_with('\n') && !output.is_empty() {
            output.push('\n');
        }
        let _ = fs::write(path, output);
    }
}

fn delete_line_from_file(path: &Path, target_line: usize) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };

    let mut lines: Vec<&str> = content.lines().collect();
    if target_line < lines.len() {
        lines.remove(target_line);
        let mut output = lines.join("\n");
        if content.ends_with('\n') && !output.is_empty() {
            output.push('\n');
        }
        let _ = fs::write(path, output);
    }
}

fn move_line_in_file(path: &Path, from_idx: usize, to_idx: usize) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };

    let mut lines: Vec<&str> = content.lines().collect();
    if from_idx < lines.len() && to_idx < lines.len() && from_idx != to_idx {
        let item = lines.remove(from_idx);
        lines.insert(to_idx, item);
        let mut output = lines.join("\n");
        if content.ends_with('\n') && !output.is_empty() {
            output.push('\n');
        }
        let _ = fs::write(path, output);
    }
}

fn parse_checkbox_line(trimmed: &str) -> Option<(bool, &str)> {
    for prefix in ["* [ ] ", "- [ ] "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some((false, rest));
        }
    }
    for prefix in ["* [x] ", "- [x] ", "* [X] ", "- [X] "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some((true, rest));
        }
    }
    None
}

fn toggle_line_in_file(path: &Path, target_line: usize, checked: bool) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };

    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    if let Some(line) = lines.get_mut(target_line) {
        if checked {
            *line = line.replacen("[ ]", "[x]", 1);
        } else {
            *line = line.replacen("[x]", "[ ]", 1).replacen("[X]", "[ ]", 1);
        }

        let mut output = lines.join("\n");
        if content.ends_with('\n') {
            output.push('\n');
        }
        let _ = fs::write(path, output);
    }
}