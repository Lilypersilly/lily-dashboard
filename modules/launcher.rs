// @dash-id: run
// @dash-title: Run
// @dash-lifecycle: stateless
// @dash-hooks: init
//! Default `Run` application launcher tab.
//!
//! Reads installed `.desktop` entries via `gio::AppInfo`, supports instant filtering,
//! pinning favorites, and optional page-cache preloading for selected binaries.

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
    name_lower: String,
    desc_lower: String,
    exec_name: String,
    exec_lower: String,
    exec_path: PathBuf,
    exec_cmdline: String,
    app_info: gio::AppInfo,
}

thread_local! {
    static INSTALLED_APPS_CACHE: RefCell<Option<(std::time::Instant, Vec<AppEntry>)>> =
        const { RefCell::new(None) };
}

struct CachedAppRow {
    app: AppEntry,
    row_box: Box,
    btn: Button,
    arrow_lbl: Label,
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
    if let Some(cached) = INSTALLED_APPS_CACHE.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|(ts, _)| ts.elapsed() < Duration::from_secs(25))
            .map(|(_, list)| list.clone())
    }) {
        return cached;
    }

    let mut apps: Vec<AppEntry> = gio::AppInfo::all()
        .into_iter()
        .filter(|app| app.should_show())
        .map(|app| {
            let name = app.display_name().to_string();
            let name_lower = name.to_lowercase();
            let desc_lower = app
                .description()
                .map(|d| d.to_lowercase())
                .unwrap_or_default();
            let exec_path = app.executable();
            let exec_name = exec_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let exec_lower = exec_name.to_lowercase();
            let exec_cmdline = app
                .commandline()
                .map(|c| clean_exec_cmdline(&c.to_string_lossy()))
                .unwrap_or_else(|| exec_name.clone());

            AppEntry {
                name,
                name_lower,
                desc_lower,
                exec_name,
                exec_lower,
                exec_path,
                exec_cmdline,
                app_info: app,
            }
        })
        .collect();

    apps.sort_by(|a, b| a.name_lower.cmp(&b.name_lower));
    INSTALLED_APPS_CACHE.with(|slot| {
        *slot.borrow_mut() = Some((std::time::Instant::now(), apps.clone()));
    });
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
        .placeholder_text("Run app, command, or = math...")
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

    let math_top_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    let preload_header = Label::builder()
        .label("PRELOADED IN RAM")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(2)
        .visible(false)
        .build();
    let preload_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    let apps_header = Label::builder()
        .label("APPLICATIONS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    let apps_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    let fallback_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();

    list_container.append(&math_top_box);
    list_container.append(&preload_header);
    list_container.append(&preload_box);
    list_container.append(&apps_header);
    list_container.append(&apps_box);
    list_container.append(&fallback_box);

    scrolled.set_child(Some(&list_container));
    crate::module_api::attach_viewport_culler(&scrolled, &apps_box);
    root.append(&search_entry);
    root.append(&scrolled);

    let installed = load_installed_apps();
    let cfg = load_config();

    let apps = Rc::new(RefCell::new(installed.clone()));
    let pinned = Rc::new(RefCell::new(cfg.pinned_apps));
    let preloaded = Rc::new(RefCell::new(cfg.preloaded_apps));
    let query_state = Rc::new(RefCell::new(String::new()));
    let refresh_slot: Rc<RefCell<Option<Rc<dyn Fn(bool)>>>> = Rc::new(RefCell::new(None));

    // Pre-build one reusable GTK row per installed app so search keystrokes toggle visibility
    // and reorder existing widgets in-place (zero widget allocations, zero icon reloads, and
    // zero mount-animation storms while typing!).
    let cached_rows: Rc<Vec<CachedAppRow>> = {
        let mut rows = Vec::with_capacity(installed.len());
        for (idx, app) in installed.iter().enumerate() {
            let row = build_cached_app_row(
                app,
                &pinned,
                &preloaded,
                &popover_guard,
                &on_launch,
                &refresh_slot,
                false,
            );
            if idx >= 18 {
                row.row_box.add_css_class("lily-settled");
                row.row_box.add_css_class("lily-offscreen");
            }
            apps_box.append(&row.row_box);
            rows.push(row);
        }
        Rc::new(rows)
    };

    let last_order: Rc<RefCell<Vec<usize>>> =
        Rc::new(RefCell::new((0..cached_rows.len()).collect()));
    let last_preload_sig: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let refresh_list: Rc<dyn Fn(bool)> = {
        let sc_r = scrolled.clone();
        let math_top_r = math_top_box.clone();
        let pre_hdr_r = preload_header.clone();
        let pre_box_r = preload_box.clone();
        let apps_hdr_r = apps_header.clone();
        let apps_box_r = apps_box.clone();
        let fall_box_r = fallback_box.clone();
        let rows_r = cached_rows.clone();
        let apps_r = apps.clone();
        let pin_r = pinned.clone();
        let pre_r = preloaded.clone();
        let q_r = query_state.clone();
        let order_r = last_order.clone();
        let pre_sig_r = last_preload_sig.clone();
        let guard_r = popover_guard.clone();
        let launch_r = on_launch.clone();
        let ref_slot_r = refresh_slot.clone();

        Rc::new(move |is_initial: bool| {
            while let Some(c) = math_top_r.first_child() {
                math_top_r.remove(&c);
            }
            while let Some(c) = fall_box_r.first_child() {
                fall_box_r.remove(&c);
            }

            let raw_q = q_r.borrow().clone();
            let query_trimmed = raw_q.trim();
            let explicit_math = query_trimmed.starts_with('=');
            let query_owned = query_trimmed.to_lowercase();
            let query = query_owned.as_str();

            // 0. Explicit `= expr` quick-math card right at the very top
            if explicit_math {
                if let Some((expr_disp, ans)) = eval_quick_math(query_trimmed) {
                    append_math_card(&math_top_r, &expr_disp, &ans);
                    math_top_r.set_visible(true);
                } else {
                    math_top_r.set_visible(false);
                }
            } else {
                math_top_r.set_visible(false);
            }

            // 1. Preloaded section (only when search is empty and preloaded set is non-empty)
            if query.is_empty() && !pre_r.borrow().is_empty() {
                let mut cur_sig: Vec<String> = pre_r.borrow().iter().cloned().collect();
                cur_sig.sort();
                if *pre_sig_r.borrow() != cur_sig {
                    *pre_sig_r.borrow_mut() = cur_sig;
                    while let Some(c) = pre_box_r.first_child() {
                        pre_box_r.remove(&c);
                    }
                    let pre_list: Vec<AppEntry> = apps_r
                        .borrow()
                        .iter()
                        .filter(|a| pre_r.borrow().contains(&a.name))
                        .cloned()
                        .collect();
                    for app in pre_list {
                        let prow = build_cached_app_row(
                            &app,
                            &pin_r,
                            &pre_r,
                            &guard_r,
                            &launch_r,
                            &ref_slot_r,
                            true,
                        );
                        if !is_initial {
                            prow.row_box.add_css_class("lily-settled");
                        }
                        pre_box_r.append(&prow.row_box);
                    }
                }
                pre_hdr_r.set_visible(true);
                pre_box_r.set_visible(true);
            } else {
                pre_hdr_r.set_visible(false);
                pre_box_r.set_visible(false);
            }

            if explicit_math {
                apps_hdr_r.set_visible(false);
                apps_box_r.set_visible(false);
                fall_box_r.set_visible(false);
                return;
            }

            apps_hdr_r.set_visible(true);
            apps_box_r.set_visible(true);

            let new_order: Vec<usize> = {
                let pin_set = pin_r.borrow();
                if query.is_empty() {
                    let mut idxs: Vec<usize> = (0..rows_r.len()).collect();
                    idxs.sort_by(|&ia, &ib| {
                        let a = &rows_r[ia].app;
                        let b = &rows_r[ib].app;
                        let a_pin = pin_set.contains(&a.name);
                        let b_pin = pin_set.contains(&b.name);
                        b_pin.cmp(&a_pin).then_with(|| ia.cmp(&ib))
                    });
                    idxs
                } else {
                    let mut scored: Vec<(i32, usize)> = rows_r
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, r)| {
                            let s = app_match_score(
                                &r.app,
                                query,
                                pin_set.contains(&r.app.name),
                            )?;
                            Some((s, idx))
                        })
                        .collect();
                    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    scored.into_iter().map(|(_, idx)| idx).collect()
                }
            };

            let mut matched = vec![false; rows_r.len()];
            for &idx in &new_order {
                matched[idx] = true;
            }

            for (idx, row) in rows_r.iter().enumerate() {
                if !matched[idx] && row.row_box.is_visible() {
                    row.row_box.set_visible(false);
                }
            }

            {
                let pin_set = pin_r.borrow();
                let pre_set = pre_r.borrow();
                for (vis_pos, &idx) in new_order.iter().enumerate() {
                    let row = &rows_r[idx];
                    let is_pin = pin_set.contains(&row.app.name);
                    let is_pre = pre_set.contains(&row.app.name);

                    if is_pin != row.btn.has_css_class("pinned") {
                        if is_pin {
                            row.btn.add_css_class("pinned");
                        } else {
                            row.btn.remove_css_class("pinned");
                        }
                    }

                    let badge = if is_pin && is_pre {
                        "[PIN+RAM] ->"
                    } else if is_pin {
                        "[PIN] ->"
                    } else if is_pre {
                        "[RAM] ->"
                    } else {
                        "->"
                    };
                    if row.arrow_lbl.label() != badge {
                        row.arrow_lbl.set_label(badge);
                    }

                    if !is_initial {
                        if !row.row_box.has_css_class("lily-settled") {
                            row.row_box.add_css_class("lily-settled");
                        }
                        if vis_pos < 20 && row.row_box.has_css_class("lily-offscreen") {
                            row.row_box.remove_css_class("lily-offscreen");
                        }
                    }

                    if !row.row_box.is_visible() {
                        row.row_box.set_visible(true);
                    }
                }
            }

            if *order_r.borrow() != new_order {
                let mut prev: Option<&Box> = None;
                for &idx in &new_order {
                    let rb = &rows_r[idx].row_box;
                    apps_box_r.reorder_child_after(rb, prev);
                    prev = Some(rb);
                }
                *order_r.borrow_mut() = new_order.clone();
            }

            if new_order.is_empty() && !query.is_empty() {
                if let Some((expr_disp, ans)) = eval_quick_math(query_trimmed) {
                    append_math_card(&fall_box_r, &expr_disp, &ans);
                }

                let cmd_btn = Button::builder()
                    .css_classes(vec!["fav-item-btn".to_string()])
                    .hexpand(true)
                    .build();

                let row = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .spacing(6)
                    .build();

                let lbl = Label::builder()
                    .label(&format!("Run command: {}", query_trimmed))
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

                let cmd_str = query_trimmed.to_string();
                let launch_cb = launch_r.clone();
                cmd_btn.connect_clicked(move |_| {
                    let _ = Command::new("sh").arg("-c").arg(&cmd_str).spawn();
                    launch_cb();
                });

                fall_box_r.append(&cmd_btn);
                fall_box_r.set_visible(true);
            } else {
                fall_box_r.set_visible(false);
            }

            sc_r.vadjustment().set_value(0.0);
            crate::module_api::refresh_viewport_culler(&sc_r, &apps_box_r);
        })
    };
    *refresh_slot.borrow_mut() = Some(refresh_list.clone());
    refresh_list(true);

    let q_filter = query_state.clone();
    let ref_filter = refresh_list.clone();
    search_entry.connect_search_changed(move |entry| {
        *q_filter.borrow_mut() = entry.text().trim().to_string();
        ref_filter(false);
    });

    let apps_activate = apps.clone();
    let pinned_activate = pinned.clone();
    let launch_activate = on_launch.clone();
    search_entry.connect_activate(move |entry| {
        let raw_query = entry.text().trim().to_string();
        if raw_query.is_empty() {
            return;
        }
        // Explicit `= expr` always evaluates & copies math first
        if raw_query.starts_with('=') {
            if let Some((_, ans)) = eval_quick_math(&raw_query) {
                copy_math_result(&ans);
                entry.set_text("");
                launch_activate();
                return;
            }
        }

        let query_lower = raw_query.to_lowercase();
        let pin_set = pinned_activate.borrow();

        let mut scored: Vec<(i32, AppEntry)> = apps_activate
            .borrow()
            .iter()
            .filter_map(|app| {
                let s = app_match_score(app, &query_lower, pin_set.contains(&app.name))?;
                Some((s, app.clone()))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name_lower.cmp(&b.1.name_lower))
        });

        if let Some((_, app)) = scored.first() {
            let _ = app.app_info.launch(&[], None::<&gio::AppLaunchContext>);
        } else if let Some((_, ans)) = eval_quick_math(&raw_query) {
            copy_math_result(&ans);
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

/// Scores an app against a non-empty lowercase query so exact/prefix/word-start name & binary
/// matches always rank above mid-word or description-only matches (e.g. `"tor"` puts `Tor Browser`
/// at #1 instead of `"text ediTOR"` / `"moniTOR"`).
fn app_match_score(app: &AppEntry, q: &str, is_pinned: bool) -> Option<i32> {
    let name = app.name_lower.as_str();
    let exec = app.exec_lower.as_str();
    let desc = app.desc_lower.as_str();

    let word_starts = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric())
            .any(|w| !w.is_empty() && w.starts_with(q))
    };

    let mut score = if name == q || exec == q {
        1000
    } else if name.starts_with(q) {
        900 - (name.len().min(60) as i32)
    } else if exec.starts_with(q) {
        830 - (exec.len().min(40) as i32)
    } else if word_starts(name) {
        760 - (name.len().min(50) as i32)
    } else if let Some(pos) = name.find(q) {
        560 - (pos.min(40) as i32)
    } else if let Some(pos) = exec.find(q) {
        460 - (pos.min(40) as i32)
    } else if word_starts(desc) {
        240
    } else if desc.contains(q) {
        100
    } else {
        return None;
    };

    if is_pinned {
        score += 35;
    }
    Some(score)
}

fn copy_math_result(ans: &str) {
    if let Some(display) = gtk4::gdk::Display::default() {
        display.clipboard().set_text(ans);
    }
    let _ = Command::new("wl-copy").args(["--", ans]).spawn();
}

fn append_math_card(container: &Box, expr_disp: &str, ans: &str) {
    container.append(
        &Label::builder()
            .label("QUICK MATH")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(2)
            .build(),
    );

    let btn = Button::builder()
        .css_classes(vec!["fav-item-btn".to_string(), "pinned".to_string()])
        .hexpand(true)
        .tooltip_text("Click or press Enter to copy result")
        .build();
    let col = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .build();
    let top = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    let ans_lbl = Label::builder()
        .label(&format!("= {}", ans))
        .halign(Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-h2".to_string()])
        .build();
    let hint_lbl = Label::builder()
        .label("Copy")
        .halign(Align::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    top.append(&ans_lbl);
    top.append(&hint_lbl);

    let expr_lbl = Label::builder()
        .label(expr_disp)
        .halign(Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    col.append(&top);
    col.append(&expr_lbl);
    btn.set_child(Some(&col));

    let ans_owned = ans.to_string();
    let hint_c = hint_lbl.clone();
    btn.connect_clicked(move |_| {
        copy_math_result(&ans_owned);
        hint_c.set_label("Copied!");
        let h_reset = hint_c.clone();
        glib::timeout_add_local_once(Duration::from_millis(1000), move || {
            h_reset.set_label("Copy");
        });
    });
    container.append(&btn);
}

#[derive(Clone, Debug, PartialEq)]
enum MathTok {
    Num(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
    Bang,
}

fn eval_quick_math(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    let explicit = trimmed.starts_with('=');
    let expr_str = trimmed.strip_prefix('=').unwrap_or(trimmed).trim();
    if expr_str.is_empty() || expr_str.starts_with('/') {
        return None;
    }

    let cleaned = expr_str
        .replace('×', "*")
        .replace('÷', "/")
        .replace('−', "-")
        .replace('π', "pi")
        .replace('τ', "tau")
        .replace('√', "sqrt");

    let chars: Vec<char> = cleaned.chars().collect();
    let mut toks: Vec<MathTok> = Vec::new();
    let mut i = 0usize;
    let mut has_op_or_fn = explicit;

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '0' && i + 2 < chars.len() && (chars[i + 1] == 'x' || chars[i + 1] == 'X') {
            i += 2;
            let start = i;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i == start {
                return None;
            }
            let hex_s: String = chars[start..i].iter().collect();
            let v = u64::from_str_radix(&hex_s, 16).ok()? as f64;
            has_op_or_fn = true;
            toks.push(MathTok::Num(v));
            continue;
        }
        if c == '0' && i + 2 < chars.len() && (chars[i + 1] == 'b' || chars[i + 1] == 'B') {
            i += 2;
            let start = i;
            while i < chars.len() && (chars[i] == '0' || chars[i] == '1') {
                i += 1;
            }
            if i == start {
                return None;
            }
            let bin_s: String = chars[start..i].iter().collect();
            let v = u64::from_str_radix(&bin_s, 2).ok()? as f64;
            has_op_or_fn = true;
            toks.push(MathTok::Num(v));
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
            let start = i;
            let mut dot = false;
            while i < chars.len() {
                if chars[i].is_ascii_digit() {
                    i += 1;
                } else if chars[i] == '.' && !dot {
                    dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let num_s: String = chars[start..i].iter().collect();
            toks.push(MathTok::Num(num_s.parse::<f64>().ok()?));
            continue;
        }
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let id: String = chars[start..i].iter().collect::<String>().to_lowercase();
            has_op_or_fn = true;
            toks.push(MathTok::Ident(id));
            continue;
        }
        match c {
            '+' | '-' | '*' | '/' | '%' | '^' => {
                has_op_or_fn = true;
                toks.push(MathTok::Op(c));
            }
            '(' => {
                has_op_or_fn = true;
                toks.push(MathTok::LParen);
            }
            ')' => toks.push(MathTok::RParen),
            '!' => {
                has_op_or_fn = true;
                toks.push(MathTok::Bang);
            }
            _ => return None,
        }
        i += 1;
    }

    if !has_op_or_fn || toks.is_empty() {
        return None;
    }

    // Insert implicit multiplication (e.g. `2pi`, `3(4+5)`, `(2)(3)`)
    let mut full: Vec<MathTok> = Vec::with_capacity(toks.len() + 4);
    let is_fn = |s: &str| {
        matches!(
            s,
            "sqrt" | "cbrt" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "ln" | "log" | "log2" | "abs" | "round" | "floor" | "ceil"
        )
    };
    for tok in toks {
        if let Some(prev) = full.last() {
            let ins = match (prev, &tok) {
                (MathTok::Num(_), MathTok::Ident(_) | MathTok::LParen) => true,
                (MathTok::RParen | MathTok::Bang, MathTok::Num(_) | MathTok::Ident(_) | MathTok::LParen) => true,
                (MathTok::Ident(id), MathTok::LParen) => !is_fn(id),
                _ => false,
            };
            if ins {
                full.push(MathTok::Op('*'));
            }
        }
        full.push(tok);
    }

    fn p_add(t: &[MathTok], p: &mut usize) -> Option<f64> {
        let mut v = p_mul(t, p)?;
        while let Some(MathTok::Op(op @ ('+' | '-'))) = t.get(*p) {
            let o = *op;
            *p += 1;
            let r = p_mul(t, p)?;
            v = if o == '+' { v + r } else { v - r };
        }
        Some(v)
    }
    fn p_mul(t: &[MathTok], p: &mut usize) -> Option<f64> {
        let mut v = p_un(t, p)?;
        while let Some(MathTok::Op(op @ ('*' | '/' | '%'))) = t.get(*p) {
            let o = *op;
            *p += 1;
            let r = p_un(t, p)?;
            if (o == '/' || o == '%') && r == 0.0 {
                return None;
            }
            v = match o {
                '*' => v * r,
                '/' => v / r,
                _ => v % r,
            };
        }
        Some(v)
    }
    fn p_un(t: &[MathTok], p: &mut usize) -> Option<f64> {
        match t.get(*p) {
            Some(MathTok::Op('-')) => {
                *p += 1;
                Some(-p_un(t, p)?)
            }
            Some(MathTok::Op('+')) => {
                *p += 1;
                p_un(t, p)
            }
            _ => p_pow(t, p),
        }
    }
    fn p_pow(t: &[MathTok], p: &mut usize) -> Option<f64> {
        let lhs = p_post(t, p)?;
        if matches!(t.get(*p), Some(MathTok::Op('^'))) {
            *p += 1;
            let rhs = p_un(t, p)?;
            Some(lhs.powf(rhs))
        } else {
            Some(lhs)
        }
    }
    fn p_post(t: &[MathTok], p: &mut usize) -> Option<f64> {
        let mut v = p_prim(t, p)?;
        while matches!(t.get(*p), Some(MathTok::Bang)) {
            *p += 1;
            if v < 0.0 || v > 170.0 || (v - v.round()).abs() > 1e-9 {
                return None;
            }
            let n = v.round() as u64;
            let mut acc = 1.0f64;
            for k in 2..=n {
                acc *= k as f64;
            }
            v = acc;
        }
        Some(v)
    }
    fn p_prim(t: &[MathTok], p: &mut usize) -> Option<f64> {
        match t.get(*p)?.clone() {
            MathTok::Num(n) => {
                *p += 1;
                Some(n)
            }
            MathTok::LParen => {
                *p += 1;
                let v = p_add(t, p)?;
                if !matches!(t.get(*p), Some(MathTok::RParen)) {
                    return None;
                }
                *p += 1;
                Some(v)
            }
            MathTok::Ident(name) => {
                *p += 1;
                if matches!(t.get(*p), Some(MathTok::LParen)) {
                    *p += 1;
                    let arg = p_add(t, p)?;
                    if !matches!(t.get(*p), Some(MathTok::RParen)) {
                        return None;
                    }
                    *p += 1;
                    Some(match name.as_str() {
                        "sqrt" => arg.sqrt(),
                        "cbrt" => arg.cbrt(),
                        "sin" => arg.sin(),
                        "cos" => arg.cos(),
                        "tan" => arg.tan(),
                        "asin" => arg.asin(),
                        "acos" => arg.acos(),
                        "atan" => arg.atan(),
                        "ln" => arg.ln(),
                        "log" => arg.log10(),
                        "log2" => arg.log2(),
                        "abs" => arg.abs(),
                        "round" => arg.round(),
                        "floor" => arg.floor(),
                        "ceil" => arg.ceil(),
                        _ => return None,
                    })
                } else {
                    match name.as_str() {
                        "pi" => Some(std::f64::consts::PI),
                        "e" => Some(std::f64::consts::E),
                        "tau" => Some(std::f64::consts::TAU),
                        _ => None,
                    }
                }
            }
            _ => None,
        }
    }

    let mut pos = 0usize;
    let val = p_add(&full, &mut pos)?;
    if pos != full.len() || !val.is_finite() {
        return None;
    }
    let clean = if val.abs() < 1e-13 { 0.0 } else { val };
    let formatted = if (clean - clean.round()).abs() < 1e-10 && clean.abs() < 1e15 {
        format!("{}", clean.round() as i64)
    } else if clean.abs() >= 1e12 || (clean != 0.0 && clean.abs() < 1e-6) {
        format!("{:.6e}", clean)
    } else {
        let s = format!("{:.8}", clean);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    Some((expr_str.to_string(), formatted))
}

#[allow(clippy::too_many_arguments)]
fn build_cached_app_row(
    app: &AppEntry,
    pinned_rc: &Rc<RefCell<HashSet<String>>>,
    preloaded_rc: &Rc<RefCell<HashSet<String>>>,
    popover_guard: &Rc<Cell<usize>>,
    on_launch: &Rc<dyn Fn()>,
    refresh_slot: &Rc<RefCell<Option<Rc<dyn Fn(bool)>>>>,
    is_preload_section: bool,
) -> CachedAppRow {
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
    let pinned_ctx = pinned_rc.clone();
    let preloaded_ctx = preloaded_rc.clone();
    let guard_ctx = popover_guard.clone();
    let ref_ctx = refresh_slot.clone();

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
                let rules = format!("workspace {} silent", ws_num);
                crate::config::hypr_exec_with_rules(&rules, &cmd_ws);
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
            let pinned_pin = pinned_ctx.clone();
            let preloaded_pin = preloaded_ctx.clone();
            let ref_pin = ref_ctx.clone();
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
                if let Some(ref cb) = *ref_pin.borrow() {
                    cb(false);
                }
            });
        }

        // Preload button handler
        {
            let app_clone = app_ctx.clone();
            let pinned_pre = pinned_ctx.clone();
            let preloaded_pre = preloaded_ctx.clone();
            let ref_pre = ref_ctx.clone();
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

                if let Some(ref cb) = *ref_pre.borrow() {
                    cb(false);
                }
            });
        }

        popover.popup();
    });
    btn.add_controller(right_click);

    row_box.append(&btn);
    CachedAppRow {
        app: app.clone(),
        row_box,
        btn,
        arrow_lbl: arrow,
    }
}

pub fn init() {
    preload_configured_apps();
}

pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    let close_cb = ctx.close_drawer.clone();
    let ui = build_launcher_ui(ctx.popover_guard.clone(), move || close_cb());
    let entry = ui.search_entry.clone();
    let root_settle = ui.root.clone();
    crate::module_api::ModuleHandle {
        widget: ui.root.upcast(),
        detail_pane: None,
        on_show: Some(std::rc::Rc::new(move || {
            entry.grab_focus();
            if !root_settle.has_css_class("lily-settled") {
                let r_s = root_settle.clone();
                let dur = crate::config::get().layout.anim_duration_ms + 360;
                glib::timeout_add_local_once(Duration::from_millis(dur), move || {
                    r_s.add_css_class("lily-settled");
                });
            }
        })),
        on_hide: None,
    }
}
