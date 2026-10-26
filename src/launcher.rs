use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Image, Label, Orientation, PolicyType, Popover,
    ScrolledWindow, SearchEntry,
};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Default)]
struct LauncherConfig {
    #[serde(default)]
    pinned_apps: HashSet<String>,
}

#[derive(Clone)]
struct AppEntry {
    name: String,
    description: String,
    exec_name: String,
    exec_cmdline: String,
    app_info: gio::AppInfo,
}

pub struct LauncherUi {
    pub root: Box,
    pub search_entry: SearchEntry,
}

fn config_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("launcher.json");
    path
}

fn load_pinned_apps() -> HashSet<String> {
    if let Ok(data) = fs::read_to_string(config_path()) {
        if let Ok(cfg) = serde_json::from_str::<LauncherConfig>(&data) {
            return cfg.pinned_apps;
        }
    }
    HashSet::new()
}

fn save_pinned_apps(pinned: &HashSet<String>) {
    let cfg = LauncherConfig {
        pinned_apps: pinned.clone(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&cfg) {
        let _ = fs::write(config_path(), json);
    }
}

fn clean_exec_cmdline(raw: &str) -> String {
    // Strip standard freedesktop %u, %U, %f, %F field codes for Hyprland dispatch
    raw.split_whitespace()
        .filter(|part| !part.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ")
}

fn load_installed_apps() -> Vec<AppEntry> {
    let mut apps: Vec<AppEntry> = gio::AppInfo::all()
        .into_iter()
        .filter(|app| app.should_show())
        .map(|app| {
            let name = app.display_name().to_string();
            let description = app
                .description()
                .map(|d| d.to_string())
                .unwrap_or_default();
            let exec_name = app
                .executable()
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let exec_cmdline = app
                .commandline()
                .map(|c| clean_exec_cmdline(&c.to_string_lossy()))
                .unwrap_or_else(|| exec_name.clone());

            AppEntry {
                name,
                description,
                exec_name,
                exec_cmdline,
                app_info: app,
            }
        })
        .collect();

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

pub fn build_launcher_ui<F>(popover_guard: Rc<Cell<usize>>, on_launch: F) -> LauncherUi
where
    F: Fn() + 'static,
{
    let on_launch: Rc<dyn Fn()> = Rc::new(on_launch);

    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let search_entry = SearchEntry::builder()
        .placeholder_text("Run app or command...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let list_container = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    scrolled.set_child(Some(&list_container));
    root.append(&search_entry);
    root.append(&scrolled);

    let apps = Rc::new(RefCell::new(load_installed_apps()));
    let pinned = Rc::new(RefCell::new(load_pinned_apps()));

    populate_app_list(
        &list_container,
        &apps,
        &pinned,
        "",
        &popover_guard,
        &on_launch,
    );

    let list_filter = list_container.clone();
    let apps_filter = apps.clone();
    let pinned_filter = pinned.clone();
    let guard_filter = popover_guard.clone();
    let launch_filter = on_launch.clone();
    search_entry.connect_search_changed(move |entry| {
        let query = entry.text().trim().to_lowercase();
        populate_app_list(
            &list_filter,
            &apps_filter,
            &pinned_filter,
            &query,
            &guard_filter,
            &launch_filter,
        );
    });

    let apps_activate = apps.clone();
    let pinned_activate = pinned.clone();
    let launch_activate = on_launch.clone();
    search_entry.connect_activate(move |entry| {
        let raw_query = entry.text().trim().to_string();
        if raw_query.is_empty() {
            return;
        }
        let query_lower = raw_query.to_lowercase();
        let pin_set = pinned_activate.borrow();

        let mut candidates: Vec<AppEntry> = apps_activate
            .borrow()
            .iter()
            .filter(|app| {
                app.name.to_lowercase().contains(&query_lower)
                    || app.exec_name.to_lowercase().contains(&query_lower)
                    || app.description.to_lowercase().contains(&query_lower)
            })
            .cloned()
            .collect();

        candidates.sort_by(|a, b| {
            let a_pin = pin_set.contains(&a.name);
            let b_pin = pin_set.contains(&b.name);
            b_pin.cmp(&a_pin)
        });

        if let Some(app) = candidates.first() {
            let _ = app.app_info.launch(&[], None::<&gio::AppLaunchContext>);
        } else {
            let _ = Command::new("sh").arg("-c").arg(&raw_query).spawn();
        }

        entry.set_text("");
        launch_activate();
    });

    LauncherUi {
        root,
        search_entry,
    }
}

fn populate_app_list(
    container: &Box,
    apps_rc: &Rc<RefCell<Vec<AppEntry>>>,
    pinned_rc: &Rc<RefCell<HashSet<String>>>,
    query: &str,
    popover_guard: &Rc<Cell<usize>>,
    on_launch: &Rc<dyn Fn()>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let header = Label::builder()
        .label("APPLICATIONS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(2)
        .build();
    container.append(&header);

    let mut filtered: Vec<AppEntry> = apps_rc
        .borrow()
        .iter()
        .filter(|app| {
            if query.is_empty() {
                return true;
            }
            app.name.to_lowercase().contains(query)
                || app.exec_name.to_lowercase().contains(query)
                || app.description.to_lowercase().contains(query)
        })
        .cloned()
        .collect();

    {
        let pin_set = pinned_rc.borrow();
        filtered.sort_by(|a, b| {
            let a_pin = pin_set.contains(&a.name);
            let b_pin = pin_set.contains(&b.name);
            b_pin
                .cmp(&a_pin)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    let match_count = filtered.len();

    for app in filtered {
        let is_pinned = pinned_rc.borrow().contains(&app.name);

        let row_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .build();

        let mut btn_classes = vec!["fav-item-btn".to_string()];
        if is_pinned {
            btn_classes.push("pinned".to_string());
        }

        let btn = Button::builder()
            .css_classes(btn_classes)
            .hexpand(true)
            .tooltip_text("Left-click to launch | Right-click to Pin or Send to Workspace")
            .build();

        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();

        if let Some(icon) = app.app_info.icon() {
            let img = Image::from_gicon(&icon);
            img.set_pixel_size(20);
            row.append(&img);
        }

        let name_lbl = Label::builder()
            .label(&app.name)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();

        let badge = if is_pinned { "[PIN] ->" } else { "->" };
        let arrow = Label::builder()
            .label(badge)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();

        row.append(&name_lbl);
        row.append(&arrow);
        btn.set_child(Some(&row));

        let app_info_clone = app.app_info.clone();
        let launch_cb = on_launch.clone();
        btn.connect_clicked(move |_| {
            let _ = app_info_clone.launch(&[], None::<&gio::AppLaunchContext>);
            launch_cb();
        });

        // Right-Click Context Menu: Pin App | Launch on Workspace 1..5
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

        let pin_btn = Button::builder()
            .label(if is_pinned { "Unpin App" } else { "Pin App to Top" })
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let ws_label = Label::builder()
            .label("Open on Workspace:")
            .css_classes(vec!["fav-score-badge".to_string()])
            .margin_top(4)
            .build();

        let ws_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();

        for ws_num in 1..=5 {
            let ws_btn = Button::builder()
                .label(&format!("{}", ws_num))
                .css_classes(vec!["plan-fmt-btn".to_string()])
                .build();

            let cmd_ws = app.exec_cmdline.clone();
            let pop_ws = popover.clone();
            ws_btn.connect_clicked(move |_| {
                pop_ws.popdown();
                let lua_dispatch = format!(
                    "hl.dsp.exec_cmd(\"[workspace {} silent] {}\")",
                    ws_num,
                    cmd_ws.replace('"', "\\\"")
                );
                let _ = Command::new("hyprctl")
                    .args(["dispatch", &lua_dispatch])
                    .spawn();
            });
            ws_row.append(&ws_btn);
        }

        menu_box.append(&pin_btn);
        menu_box.append(&ws_label);
        menu_box.append(&ws_row);
        popover.set_child(Some(&menu_box));

        let right_click = GestureClick::builder().button(3).build();
        let pop_open = popover.clone();
        right_click.connect_pressed(move |_, _, _, _| {
            pop_open.popup();
        });
        btn.add_controller(right_click);

        let app_name = app.name.clone();
        let container_pin = container.clone();
        let apps_pin = apps_rc.clone();
        let pinned_pin = pinned_rc.clone();
        let query_pin = query.to_string();
        let guard_pin = popover_guard.clone();
        let launch_pin = on_launch.clone();
        let pop_pin = popover.clone();

        pin_btn.connect_clicked(move |_| {
            pop_pin.popdown();
            {
                let mut set = pinned_pin.borrow_mut();
                if set.contains(&app_name) {
                    set.remove(&app_name);
                } else {
                    set.insert(app_name.clone());
                }
                save_pinned_apps(&set);
            }
            populate_app_list(
                &container_pin,
                &apps_pin,
                &pinned_pin,
                &query_pin,
                &guard_pin,
                &launch_pin,
            );
        });

        row_box.append(&btn);
        container.append(&row_box);
    }

    if match_count == 0 && !query.is_empty() {
        let cmd_btn = Button::builder()
            .css_classes(vec!["fav-item-btn".to_string()])
            .hexpand(true)
            .build();

        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();

        let lbl = Label::builder()
            .label(&format!("Run command: {}", query))
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();

        let arrow = Label::builder()
            .label("->")
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();

        row.append(&lbl);
        row.append(&arrow);
        cmd_btn.set_child(Some(&row));

        let cmd_str = query.to_string();
        let launch_cb = on_launch.clone();
        cmd_btn.connect_clicked(move |_| {
            let _ = Command::new("sh").arg("-c").arg(&cmd_str).spawn();
            launch_cb();
        });

        container.append(&cmd_btn);
    }
}