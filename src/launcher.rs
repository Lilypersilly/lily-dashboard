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
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Default, Clone)]
struct LauncherConfig {
    #[serde(default)]
    pinned_apps: HashSet<String>,
    #[serde(default)]
    preloaded_apps: HashSet<String>,
}

#[derive(Clone)]
struct AppEntry {
    name: String,
    description: String,
    exec_name: String,
    exec_path: PathBuf,
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

fn load_config() -> LauncherConfig {
    if let Ok(data) = fs::read_to_string(config_path()) {
        if let Ok(cfg) = serde_json::from_str::<LauncherConfig>(&data) {
            return cfg;
        }
    }
    LauncherConfig::default()
}

fn save_config(pinned: &HashSet<String>, preloaded: &HashSet<String>) {
    let cfg = LauncherConfig {
        pinned_apps: pinned.clone(),
        preloaded_apps: preloaded.clone(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&cfg) {
        let _ = fs::write(config_path(), json);
    }
}

fn clean_exec_cmdline(raw: &str) -> String {
    raw.split_whitespace()
        .filter(|part| !part.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_binary_path(exec_path: &Path) -> Option<PathBuf> {
    if exec_path.is_absolute() && exec_path.exists() {
        return Some(exec_path.to_path_buf());
    }
    let name = exec_path.file_name()?.to_str()?;
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(name);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

/// Reads a file into a scratch buffer so the Linux kernel caches its pages in RAM
fn warm_file_into_ram(path: &Path) {
    if let Ok(mut f) = File::open(path) {
        let mut buf = vec![0u8; 256 * 1024]; // 256 KB chunks
        while let Ok(n) = f.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    }
}

/// Recursively warms a directory (like ~/.mozilla/firefox or /usr/lib/firefox) into RAM
fn warm_dir_into_ram(dir: &Path, max_files: &mut usize) {
    if *max_files == 0 || !dir.exists() {
        return;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if *max_files == 0 {
                break;
            }
            let p = entry.path();
            if p.is_dir() {
                warm_dir_into_ram(&p, max_files);
            } else if p.is_file() {
                // Only warm files under 150MB so we don't accidentally read huge video downloads
                if let Ok(meta) = p.metadata() {
                    if meta.len() < 150 * 1024 * 1024 {
                        warm_file_into_ram(&p);
                        *max_files = max_files.saturating_sub(1);
                    }
                }
            }
        }
    }
}

/// Preloads an application in the background:
/// 1. Pulls the binary, its `/usr/lib/<app>` directory, and all `ldd` shared libraries into RAM.
/// 2. For browsers (Firefox/Zen/LibreWolf), also pre-warms the SQLite profile in RAM.
/// 3. For apps with native daemon modes (Thunar, Nautilus, Steam), spawns their silent daemon.
fn preload_app_in_background(app: AppEntry) {
    std::thread::spawn(move || {
        let bin_lower = app.exec_name.to_lowercase();

        // 1. Resolve full binary path and warm it + all its shared libraries (`ldd`) into RAM
        if let Some(bin_path) = resolve_binary_path(&app.exec_path) {
            warm_file_into_ram(&bin_path);

            if let Ok(output) = Command::new("ldd").arg(&bin_path).output() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    // Parse `libxul.so => /usr/lib/firefox/libxul.so (0x0000...)`
                    if let Some((_, right)) = line.split_once("=>") {
                        if let Some(so_path_str) = right.split_whitespace().next() {
                            let so_path = Path::new(so_path_str);
                            if so_path.exists() {
                                warm_file_into_ram(so_path);
                            }
                        }
                    }
                }
            }
        }

        // 2. Warm `/usr/lib/<app_name>` (e.g., `/usr/lib/firefox/libxul.so` is 160MB+!)
        let lib_dir = PathBuf::from(format!("/usr/lib/{}", bin_lower));
        if lib_dir.exists() {
            let mut budget = 250;
            warm_dir_into_ram(&lib_dir, &mut budget);
        }

        // 3. App-specific profile warming & silent background daemons
        let home = glib::home_dir();
        if bin_lower.contains("firefox") {
            let mut budget = 300;
            warm_dir_into_ram(&home.join(".mozilla/firefox"), &mut budget);
        } else if bin_lower.contains("zen") {
            let mut budget = 300;
            warm_dir_into_ram(&home.join(".zen"), &mut budget);
        } else if bin_lower.contains("librewolf") {
            let mut budget = 300;
            warm_dir_into_ram(&home.join(".librewolf"), &mut budget);
        } else if bin_lower == "thunar" {
            let _ = Command::new("thunar")
                .arg("--daemon")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        } else if bin_lower == "nautilus" {
            let _ = Command::new("nautilus")
                .arg("--gapplication-service")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    });
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
            let exec_path = app.executable();
            let exec_name = exec_path
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
                exec_path,
                exec_cmdline,
                app_info: app,
            }
        })
        .collect();

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

/// Runs on dashboard startup so configured Preload Apps warm up in RAM even while
/// the Run tab UI itself is lazily constructed on demand.
pub fn preload_configured_apps() {
    let cfg = load_config();
    if cfg.preloaded_apps.is_empty() {
        return;
    }
    let installed = load_installed_apps();
    for app in installed {
        if cfg.preloaded_apps.contains(&app.name) {
            preload_app_in_background(app);
        }
    }
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

    let installed = load_installed_apps();
    let cfg = load_config();

    let apps = Rc::new(RefCell::new(installed));
    let pinned = Rc::new(RefCell::new(cfg.pinned_apps));
    let preloaded = Rc::new(RefCell::new(cfg.preloaded_apps));

    populate_app_list(
        &list_container,
        &apps,
        &pinned,
        &preloaded,
        "",
        &popover_guard,
        &on_launch,
    );

    let list_filter = list_container.clone();
    let apps_filter = apps.clone();
    let pinned_filter = pinned.clone();
    let preloaded_filter = preloaded.clone();
    let guard_filter = popover_guard.clone();
    let launch_filter = on_launch.clone();
    search_entry.connect_search_changed(move |entry| {
        let query = entry.text().trim().to_lowercase();
        populate_app_list(
            &list_filter,
            &apps_filter,
            &pinned_filter,
            &preloaded_filter,
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
    preloaded_rc: &Rc<RefCell<HashSet<String>>>,
    query: &str,
    popover_guard: &Rc<Cell<usize>>,
    on_launch: &Rc<dyn Fn()>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    // 1. If search is empty and we have preloaded apps, show the PRELOADED section at the top!
    if query.is_empty() && !preloaded_rc.borrow().is_empty() {
        let preload_header = Label::builder()
            .label("PRELOADED IN RAM")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(2)
            .build();
        container.append(&preload_header);

        let preloaded_list: Vec<AppEntry> = apps_rc
            .borrow()
            .iter()
            .filter(|a| preloaded_rc.borrow().contains(&a.name))
            .cloned()
            .collect();

        for app in preloaded_list {
            let row = build_app_row(
                &app,
                container,
                apps_rc,
                pinned_rc,
                preloaded_rc,
                query,
                popover_guard,
                on_launch,
                true,
            );
            container.append(&row);
        }
    }

    let header = Label::builder()
        .label("APPLICATIONS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
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
        let row = build_app_row(
            &app,
            container,
            apps_rc,
            pinned_rc,
            preloaded_rc,
            query,
            popover_guard,
            on_launch,
            false,
        );
        container.append(&row);
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

#[allow(clippy::too_many_arguments)]
fn build_app_row(
    app: &AppEntry,
    container: &Box,
    apps_rc: &Rc<RefCell<Vec<AppEntry>>>,
    pinned_rc: &Rc<RefCell<HashSet<String>>>,
    preloaded_rc: &Rc<RefCell<HashSet<String>>>,
    query: &str,
    popover_guard: &Rc<Cell<usize>>,
    on_launch: &Rc<dyn Fn()>,
    is_preload_section: bool,
) -> Box {
    let is_pinned = pinned_rc.borrow().contains(&app.name);
    let is_preloaded = preloaded_rc.borrow().contains(&app.name);

    let row_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .build();

    let mut btn_classes = vec!["fav-item-btn".to_string()];
    if is_pinned || is_preload_section {
        btn_classes.push("pinned".to_string());
    }

    let btn = Button::builder()
        .css_classes(btn_classes)
        .hexpand(true)
        .tooltip_text("Left-click to launch | Right-click to Pin, Preload, or Send to Workspace")
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

    let badge = if is_preload_section {
        "[RAM] ->"
    } else if is_pinned && is_preloaded {
        "[PIN+RAM] ->"
    } else if is_pinned {
        "[PIN] ->"
    } else if is_preloaded {
        "[RAM] ->"
    } else {
        "->"
    };

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

    // Lazily build Right-Click Context Menu on demand and unparent on close to avoid
    // allocating hundreds of Popovers upfront and prevent GTK reference-cycle leaks.
    let right_click = GestureClick::builder().button(3).build();
    let row_weak = row_box.downgrade();
    let app_ctx = app.clone();
    let container_ctx = container.clone();
    let apps_ctx = apps_rc.clone();
    let pinned_ctx = pinned_rc.clone();
    let preloaded_ctx = preloaded_rc.clone();
    let query_ctx = query.to_string();
    let guard_ctx = popover_guard.clone();
    let launch_ctx = on_launch.clone();

    right_click.connect_pressed(move |_, _, _, _| {
        let Some(parent_row) = row_weak.upgrade() else {
            return;
        };

        let popover = Popover::builder()
            .css_classes(vec!["fav-popover".to_string()])
            .autohide(true)
            .build();
        popover.set_parent(&parent_row);

        popover.connect_closed(|p| {
            let p_weak = p.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(pop) = p_weak.upgrade() {
                    pop.unparent();
                }
            });
        });

        let guard_map = guard_ctx.clone();
        popover.connect_map(move |_| {
            guard_map.set(guard_map.get() + 1);
        });
        let guard_unmap = guard_ctx.clone();
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

        let cur_pinned = pinned_ctx.borrow().contains(&app_ctx.name);
        let cur_preloaded = preloaded_ctx.borrow().contains(&app_ctx.name);

        let pin_btn = Button::builder()
            .label(if cur_pinned { "Unpin App" } else { "Pin App to Top" })
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let preload_btn = Button::builder()
            .label(if cur_preloaded {
                "Remove from Preload"
            } else {
                "Preload in Background"
            })
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

            let cmd_ws = app_ctx.exec_cmdline.clone();
            let pop_ws = popover.downgrade();
            ws_btn.connect_clicked(move |_| {
                if let Some(p) = pop_ws.upgrade() {
                    p.popdown();
                }
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
        menu_box.append(&preload_btn);
        menu_box.append(&ws_label);
        menu_box.append(&ws_row);
        popover.set_child(Some(&menu_box));

        // Pin button handler
        {
            let app_name = app_ctx.name.clone();
            let container_pin = container_ctx.clone();
            let apps_pin = apps_ctx.clone();
            let pinned_pin = pinned_ctx.clone();
            let preloaded_pin = preloaded_ctx.clone();
            let query_pin = query_ctx.clone();
            let guard_pin = guard_ctx.clone();
            let launch_pin = launch_ctx.clone();
            let pop_pin = popover.downgrade();

            pin_btn.connect_clicked(move |_| {
                if let Some(p) = pop_pin.upgrade() {
                    p.popdown();
                }
                {
                    let mut set = pinned_pin.borrow_mut();
                    if set.contains(&app_name) {
                        set.remove(&app_name);
                    } else {
                        set.insert(app_name.clone());
                    }
                    save_config(&set, &preloaded_pin.borrow());
                }
                populate_app_list(
                    &container_pin,
                    &apps_pin,
                    &pinned_pin,
                    &preloaded_pin,
                    &query_pin,
                    &guard_pin,
                    &launch_pin,
                );
            });
        }

        // Preload button handler
        {
            let app_clone = app_ctx.clone();
            let container_pre = container_ctx.clone();
            let apps_pre = apps_ctx.clone();
            let pinned_pre = pinned_ctx.clone();
            let preloaded_pre = preloaded_ctx.clone();
            let query_pre = query_ctx.clone();
            let guard_pre = guard_ctx.clone();
            let launch_pre = launch_ctx.clone();
            let pop_pre = popover.downgrade();

            preload_btn.connect_clicked(move |_| {
                if let Some(p) = pop_pre.upgrade() {
                    p.popdown();
                }
                let added = {
                    let mut set = preloaded_pre.borrow_mut();
                    let is_added = if set.contains(&app_clone.name) {
                        set.remove(&app_clone.name);
                        false
                    } else {
                        set.insert(app_clone.name.clone());
                        true
                    };
                    save_config(&pinned_pre.borrow(), &set);
                    is_added
                };

                if added {
                    preload_app_in_background(app_clone.clone());
                }

                populate_app_list(
                    &container_pre,
                    &apps_pre,
                    &pinned_pre,
                    &preloaded_pre,
                    &query_pre,
                    &guard_pre,
                    &launch_pre,
                );
            });
        }

        popover.popup();
    });
    btn.add_controller(right_click);

    row_box.append(&btn);
    row_box
}