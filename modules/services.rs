// @dash-id: services
// @dash-title: Services
// @dash-lifecycle: stateful
// Dashpendencies:
//! Systemd service manager with pinning, Start/Stop/Restart/Freeze/Kill controls,
//! and a 400px slide-out detail pane with live `journalctl` logs.

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, Orientation, PolicyType, Popover, ScrolledWindow,
    SearchEntry,
};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    System,
    User,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FilterMode {
    All,
    Active,
    Failed,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct ServicesConfig {
    #[serde(default)]
    pinned_system: BTreeSet<String>,
    #[serde(default)]
    pinned_user: BTreeSet<String>,
}

impl ServicesConfig {
    fn is_pinned(&self, scope: Scope, unit: &str) -> bool {
        match scope {
            Scope::System => self.pinned_system.contains(unit),
            Scope::User => self.pinned_user.contains(unit),
        }
    }

    fn toggle_pin(&mut self, scope: Scope, unit: &str) -> bool {
        let set = match scope {
            Scope::System => &mut self.pinned_system,
            Scope::User => &mut self.pinned_user,
        };
        if set.contains(unit) {
            set.remove(unit);
            false
        } else {
            set.insert(unit.to_string());
            true
        }
    }
}

#[derive(Clone, Debug)]
struct ServiceEntry {
    unit: String,
    short_name: String,
    active_state: String,
    sub_state: String,
    boot_state: String,
    frozen: bool,
    description: String,
}

impl ServiceEntry {
    fn is_running(&self) -> bool {
        self.active_state == "active" || self.active_state == "reloading"
    }

    fn is_failed(&self) -> bool {
        self.active_state == "failed"
    }

    fn status_badge(&self, is_pinned: bool) -> String {
        let state = if self.frozen {
            "PAUSED"
        } else if self.is_failed() {
            "FAILED"
        } else if self.is_running() {
            "ACTIVE"
        } else if self.active_state == "activating" {
            "STARTING"
        } else if self.active_state == "deactivating" {
            "STOPPING"
        } else {
            "STOPPED"
        };

        if is_pinned {
            format!("[PIN] {}", state)
        } else {
            state.to_string()
        }
    }
}

#[derive(Clone, Debug)]
struct ServiceDetail {
    scope: Scope,
    unit: String,
    short_name: String,
    active_state: String,
    sub_state: String,
    freezer_state: String,
    boot_state: String,
    main_pid: String,
    memory_str: String,
    since: String,
    description: String,
    logs: String,
}

enum SvcMsg {
    ListLoaded {
        scope: Scope,
        entries: Vec<ServiceEntry>,
    },
    DetailLoaded(ServiceDetail),
    ActionDone {
        scope: Scope,
        unit: String,
        message: String,
    },
}

// =============================================================================
// CONFIG PERSISTENCE (~/.config/lily-dashboard/services.json)
// =============================================================================

fn config_path() -> PathBuf {
    let mut p = glib::user_config_dir();
    p.push("lily-dashboard");
    let _ = fs::create_dir_all(&p);
    p.push("services.json");
    p
}

fn load_config() -> ServicesConfig {
    let path = config_path();
    if let Ok(data) = fs::read_to_string(path) {
        if let Ok(cfg) = serde_json::from_str::<ServicesConfig>(&data) {
            return cfg;
        }
    }
    ServicesConfig::default()
}

fn save_config(cfg: &ServicesConfig) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_path(), json);
    }
}

fn copy_to_clipboard(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

fn format_mem_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GiB", b / GB)
    } else if b >= MB {
        format!("{:.1} MiB", b / MB)
    } else if b >= KB {
        format!("{:.0} KiB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

// =============================================================================
// SYSTEMCTL & JOURNALCTL BACKEND (runs on background threads)
// =============================================================================

fn systemctl_base(scope: Scope) -> Command {
    let mut cmd = Command::new("systemctl");
    if scope == Scope::User {
        cmd.arg("--user");
    }
    cmd
}

fn fetch_services(scope: Scope) -> Vec<ServiceEntry> {
    let mut map: BTreeMap<String, ServiceEntry> = BTreeMap::new();

    // 1. Installed unit files (catches stopped & disabled services like plexmediaserver!)
    if let Ok(out) = systemctl_base(scope)
        .arg("list-unit-files")
        .arg("--type=service")
        .arg("--no-pager")
        .arg("--no-legend")
        .arg("--plain")
        .output()
    {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let mut parts = line.split_whitespace();
            let Some(unit) = parts.next() else {
                continue;
            };
            // Skip uninstantiated template units (e.g. getty@.service)
            if !unit.ends_with(".service") || unit.contains("@.") {
                continue;
            }
            let boot_state = parts.next().unwrap_or("unknown").to_string();
            let short_name = unit.strip_suffix(".service").unwrap_or(unit).to_string();
            map.insert(
                unit.to_string(),
                ServiceEntry {
                    unit: unit.to_string(),
                    short_name,
                    active_state: "inactive".to_string(),
                    sub_state: "dead".to_string(),
                    boot_state,
                    frozen: false,
                    description: String::new(),
                },
            );
        }
    }

    // 2. Loaded / active / failed units with live state & descriptions
    let mut active_units: Vec<String> = Vec::new();
    if let Ok(out) = systemctl_base(scope)
        .arg("list-units")
        .arg("--type=service")
        .arg("--all")
        .arg("--no-pager")
        .arg("--no-legend")
        .arg("--plain")
        .output()
    {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 4 {
                continue;
            }
            let unit = parts[0];
            if !unit.ends_with(".service") {
                continue;
            }
            let active_state = parts[2].to_string();
            let sub_state = parts[3].to_string();
            let description = if parts.len() > 4 {
                parts[4..].join(" ")
            } else {
                String::new()
            };

            if active_state == "active" {
                active_units.push(unit.to_string());
            }

            let short_name = unit.strip_suffix(".service").unwrap_or(unit).to_string();
            map.entry(unit.to_string())
                .and_modify(|e| {
                    e.active_state = active_state.clone();
                    e.sub_state = sub_state.clone();
                    if !description.is_empty() {
                        e.description = description.clone();
                    }
                })
                .or_insert_with(|| ServiceEntry {
                    unit: unit.to_string(),
                    short_name,
                    active_state,
                    sub_state,
                    boot_state: "static".to_string(),
                    frozen: false,
                    description,
                });
        }
    }

    // 3. Batch-query cgroup FreezerState for active units so paused services show PAUSED
    if !active_units.is_empty() {
        let mut cmd = systemctl_base(scope);
        cmd.arg("show")
            .arg("--property=Id,FreezerState")
            .arg("--no-pager");
        for u in &active_units {
            cmd.arg(u);
        }
        if let Ok(out) = cmd.output() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut cur_id = String::new();
            let mut frozen_set: HashSet<String> = HashSet::new();
            for line in stdout.lines() {
                let t = line.trim();
                if t.is_empty() {
                    cur_id.clear();
                } else if let Some(id) = t.strip_prefix("Id=") {
                    cur_id = id.trim().to_string();
                } else if let Some(fs) = t.strip_prefix("FreezerState=") {
                    if fs.trim().eq_ignore_ascii_case("frozen") && !cur_id.is_empty() {
                        frozen_set.insert(cur_id.clone());
                    }
                }
            }
            for u in frozen_set {
                if let Some(entry) = map.get_mut(&u) {
                    entry.frozen = true;
                }
            }
        }
    }

    map.into_values().collect()
}

fn fetch_service_detail(scope: Scope, unit: &str) -> ServiceDetail {
    let short_name = unit.strip_suffix(".service").unwrap_or(unit).to_string();
    let mut detail = ServiceDetail {
        scope,
        unit: unit.to_string(),
        short_name,
        active_state: "unknown".to_string(),
        sub_state: "unknown".to_string(),
        freezer_state: "running".to_string(),
        boot_state: "unknown".to_string(),
        main_pid: "—".to_string(),
        memory_str: "—".to_string(),
        since: "—".to_string(),
        description: String::new(),
        logs: String::new(),
    };

    if let Ok(out) = systemctl_base(scope)
        .arg("show")
        .arg(unit)
        .arg("--property=ActiveState,SubState,FreezerState,UnitFileState,MainPID,MemoryCurrent,ActiveEnterTimestamp,Description")
        .arg("--no-pager")
        .output()
    {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Some(v) = line.strip_prefix("ActiveState=") {
                detail.active_state = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("SubState=") {
                detail.sub_state = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("FreezerState=") {
                detail.freezer_state = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("UnitFileState=") {
                let s = v.trim();
                if !s.is_empty() {
                    detail.boot_state = s.to_string();
                }
            } else if let Some(v) = line.strip_prefix("MainPID=") {
                let s = v.trim();
                if !s.is_empty() && s != "0" {
                    detail.main_pid = s.to_string();
                }
            } else if let Some(v) = line.strip_prefix("MemoryCurrent=") {
                if let Ok(bytes) = v.trim().parse::<u64>() {
                    if bytes > 0 && bytes < u64::MAX {
                        detail.memory_str = format_mem_bytes(bytes);
                    }
                }
            } else if let Some(v) = line.strip_prefix("ActiveEnterTimestamp=") {
                let s = v.trim();
                if !s.is_empty() {
                    detail.since = s.to_string();
                }
            } else if let Some(v) = line.strip_prefix("Description=") {
                detail.description = v.trim().to_string();
            }
        }
    }

    // Grab last 45 journalctl lines for this unit
    let mut jcmd = Command::new("journalctl");
    if scope == Scope::User {
        jcmd.arg("--user");
    }
    jcmd.arg("-u")
        .arg(unit)
        .arg("-n")
        .arg("45")
        .arg("--no-pager")
        .arg("--output=short");

    if let Ok(out) = jcmd.output() {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            detail.logs = if err.is_empty() {
                "No recent journal entries found.".to_string()
            } else {
                err
            };
        } else {
            detail.logs = text;
        }
    } else {
        detail.logs = "Could not invoke journalctl.".to_string();
    }

    detail
}

fn run_systemctl_action(
    scope: Scope,
    action: &str,
    unit: &str,
    extra_args: &[&str],
) -> Result<String, String> {
    let mut cmd = match scope {
        Scope::User => {
            let mut c = Command::new("systemctl");
            c.arg("--user");
            c
        }
        Scope::System => {
            // pkexec cleanly pops up Hyprland's graphical Polkit password agent
            if crate::config::bin_exists("pkexec") {
                let mut c = Command::new("pkexec");
                c.arg("systemctl");
                c
            } else {
                Command::new("systemctl")
            }
        }
    };

    cmd.arg(action);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg(unit);

    let out = cmd
        .output()
        .map_err(|e| format!("Failed to run systemctl: {}", e))?;

    let short = unit.strip_suffix(".service").unwrap_or(unit);
    if out.status.success() {
        let verb = match action {
            "start" => "Started",
            "stop" => "Stopped",
            "restart" => "Restarted",
            "freeze" => "Paused (frozen)",
            "thaw" => "Resumed (thawed)",
            "kill" => "Sent SIGKILL to",
            "enable" => "Enabled at boot:",
            "disable" => "Disabled at boot:",
            _ => "Updated",
        };
        Ok(format!("{} {}", verb, short))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let code = out.status.code().unwrap_or(1);
        if code == 126 || stderr.to_lowercase().contains("dismissed") {
            Err("Authentication cancelled.".to_string())
        } else if stderr.is_empty() {
            Err(format!("Failed to {} {} (exit {})", action, short, code))
        } else {
            let first = stderr.lines().next().unwrap_or(&stderr);
            Err(first.to_string())
        }
    }
}

// =============================================================================
// UI BUILDER
// =============================================================================

pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    // --- LEFT PANE (Main sidebar tab) ---
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

    let hdr_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let title_lbl = Label::builder()
        .label("SERVICES")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let refresh_btn = Button::builder()
        .label("Refresh")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Reload systemd service list")
        .build();

    hdr_row.append(&title_lbl);
    hdr_row.append(&refresh_btn);
    root.append(&hdr_row);

    // Scope Toggle: System vs User
    let scope_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();
    let btn_system = Button::builder()
        .label("System")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .build();
    let btn_user = Button::builder()
        .label("User")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    scope_row.append(&btn_system);
    scope_row.append(&btn_user);
    root.append(&scope_row);

    // Filter Toggle: All / Active / Failed
    let filter_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();
    let btn_f_all = Button::builder()
        .label("All")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .build();
    let btn_f_active = Button::builder()
        .label("Active")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    let btn_f_failed = Button::builder()
        .label("Failed")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    filter_row.append(&btn_f_all);
    filter_row.append(&btn_f_active);
    filter_row.append(&btn_f_failed);
    root.append(&filter_row);

    // Search Bar
    let search_entry = SearchEntry::builder()
        .placeholder_text("Filter services (e.g. plex, docker, ssh)...")
        .css_classes(vec!["fav-search".to_string()])
        .build();
    root.append(&search_entry);

    // Status Banner
    let status_lbl = Label::builder()
        .label("")
        .halign(Align::Start)
        .wrap(true)
        .xalign(0.0)
        .css_classes(vec!["plan-subnote".to_string()])
        .visible(false)
        .build();
    root.append(&status_lbl);

    // Scrollable Service List
    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();

    let list_container = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    scrolled.set_child(Some(&list_container));
    crate::module_api::attach_viewport_culler(&scrolled, &list_container);
    root.append(&scrolled);

    // --- RIGHT 400PX SLIDE-OUT DETAIL PANE ---
    let detail_view = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["plan-detail-pane".to_string()])
        .width_request(400)
        .vexpand(true)
        .visible(false)
        .build();

    // Shared State
    let (tx, rx) = mpsc::channel::<SvcMsg>();
    let rx_rc = Rc::new(rx);
    let in_flight = Rc::new(Cell::new(0usize));
    let pump_running = Rc::new(Cell::new(false));
    let ensure_pump_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    let cur_scope = Rc::new(Cell::new(Scope::System));
    let cur_filter = Rc::new(Cell::new(FilterMode::All));
    let cur_query = Rc::new(RefCell::new(String::new()));
    let cached_entries: Rc<RefCell<Vec<ServiceEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let cfg_rc = Rc::new(RefCell::new(load_config()));
    let selected_unit: Rc<RefCell<Option<(Scope, String)>>> = Rc::new(RefCell::new(None));
    let popover_guard = ctx.popover_guard.clone();

    let trigger_list_scan: Rc<dyn Fn(Scope)> = {
        let tx_s = tx.clone();
        let inf = in_flight.clone();
        let ep = ensure_pump_slot.clone();
        Rc::new(move |scope: Scope| {
            inf.set(inf.get() + 1);
            if let Some(ref start_pump) = *ep.borrow() {
                start_pump();
            }
            let tx_bg = tx_s.clone();
            std::thread::spawn(move || {
                let entries = fetch_services(scope);
                let _ = tx_bg.send(SvcMsg::ListLoaded { scope, entries });
            });
        })
    };

    let open_detail: Rc<dyn Fn(Scope, String)> = {
        let sel = selected_unit.clone();
        let dv = detail_view.clone();
        let tx_d = tx.clone();
        let inf = in_flight.clone();
        let ep = ensure_pump_slot.clone();
        Rc::new(move |scope: Scope, unit: String| {
            *sel.borrow_mut() = Some((scope, unit.clone()));
            dv.set_visible(true);
            inf.set(inf.get() + 1);
            if let Some(ref start_pump) = *ep.borrow() {
                start_pump();
            }
            let tx_bg = tx_d.clone();
            std::thread::spawn(move || {
                let detail = fetch_service_detail(scope, &unit);
                let _ = tx_bg.send(SvcMsg::DetailLoaded(detail));
            });
        })
    };

    let trigger_action: Rc<dyn Fn(Scope, String, &'static str, &'static [&'static str])> = {
        let st_a = status_lbl.clone();
        let tx_a = tx.clone();
        let inf = in_flight.clone();
        let ep = ensure_pump_slot.clone();
        Rc::new(
            move |scope: Scope,
                  unit: String,
                  action: &'static str,
                  extra: &'static [&'static str]| {
                let short = unit.strip_suffix(".service").unwrap_or(&unit).to_string();
                st_a.set_label(&format!("Running {} on {}...", action, short));
                st_a.set_visible(true);
                inf.set(inf.get() + 1);
                if let Some(ref start_pump) = *ep.borrow() {
                    start_pump();
                }
                let tx_bg = tx_a.clone();
                std::thread::spawn(move || {
                    let message = match run_systemctl_action(scope, action, &unit, extra) {
                        Ok(m) | Err(m) => m,
                    };
                    let _ = tx_bg.send(SvcMsg::ActionDone {
                        scope,
                        unit,
                        message,
                    });
                });
            },
        )
    };

    let rerender_list: Rc<dyn Fn()> = {
        let box_r = list_container.clone();
        let entries_r = cached_entries.clone();
        let scope_r = cur_scope.clone();
        let filter_r = cur_filter.clone();
        let query_r = cur_query.clone();
        let cfg_r = cfg_rc.clone();
        let sel_r = selected_unit.clone();
        let pg_r = popover_guard.clone();
        let open_r = open_detail.clone();
        let act_r = trigger_action.clone();
        let st_r = status_lbl.clone();

        let self_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let self_weak = self_slot.clone();

        let cb: Rc<dyn Fn()> = Rc::new(move || {
            let scope = scope_r.get();
            let filter = filter_r.get();
            let query = query_r.borrow().clone();
            let entries = entries_r.borrow();
            let cfg = cfg_r.borrow().clone();
            let sel_unit = sel_r
                .borrow()
                .as_ref()
                .filter(|(s, _)| *s == scope)
                .map(|(_, u)| u.clone());

            render_service_list(
                &box_r,
                &entries,
                scope,
                filter,
                &query,
                &cfg,
                sel_unit.as_deref(),
                &pg_r,
                &cfg_r,
                &st_r,
                &open_r,
                &act_r,
                &self_weak,
            );
        });

        *self_slot.borrow_mut() = Some(cb.clone());
        cb
    };

    // Wire Scope Buttons
    {
        let sc = cur_scope.clone();
        let b_sys = btn_system.clone();
        let b_usr = btn_user.clone();
        let scan = trigger_list_scan.clone();
        btn_system.connect_clicked(move |_| {
            sc.set(Scope::System);
            b_sys.add_css_class("active");
            b_usr.remove_css_class("active");
            scan(Scope::System);
        });
    }
    {
        let sc = cur_scope.clone();
        let b_sys = btn_system.clone();
        let b_usr = btn_user.clone();
        let scan = trigger_list_scan.clone();
        btn_user.connect_clicked(move |_| {
            sc.set(Scope::User);
            b_usr.add_css_class("active");
            b_sys.remove_css_class("active");
            scan(Scope::User);
        });
    }

    // Wire Filter Buttons
    let wire_filter = |btn: &Button,
                       mode: FilterMode,
                       all_b: [Button; 3],
                       f_cell: Rc<Cell<FilterMode>>,
                       re: Rc<dyn Fn()>| {
        btn.connect_clicked(move |clicked| {
            f_cell.set(mode);
            for b in &all_b {
                b.remove_css_class("active");
            }
            clicked.add_css_class("active");
            re();
        });
    };
    let f_btns = [btn_f_all.clone(), btn_f_active.clone(), btn_f_failed.clone()];
    wire_filter(&btn_f_all, FilterMode::All, f_btns.clone(), cur_filter.clone(), rerender_list.clone());
    wire_filter(&btn_f_active, FilterMode::Active, f_btns.clone(), cur_filter.clone(), rerender_list.clone());
    wire_filter(&btn_f_failed, FilterMode::Failed, f_btns, cur_filter.clone(), rerender_list.clone());

    // Wire Search Bar
    {
        let q = cur_query.clone();
        let re = rerender_list.clone();
        search_entry.connect_search_changed(move |entry| {
            *q.borrow_mut() = entry.text().trim().to_lowercase();
            re();
        });
    }

    // Wire Refresh Button
    {
        let sc = cur_scope.clone();
        let scan = trigger_list_scan.clone();
        let sel = selected_unit.clone();
        let dv = detail_view.clone();
        let open_d = open_detail.clone();
        refresh_btn.connect_clicked(move |_| {
            scan(sc.get());
            if dv.is_visible() {
                if let Some((s, u)) = sel.borrow().clone() {
                    open_d(s, u);
                }
            }
        });
    }

    // On-demand main-thread receiver pump (runs ONLY while background jobs are in flight; 0% idle CPU)
    {
        let list_weak = list_container.downgrade();
        let rx_p = rx_rc.clone();
        let inf_p = in_flight.clone();
        let pr_p = pump_running.clone();
        let entries_r = cached_entries.clone();
        let scope_r = cur_scope.clone();
        let re_r = rerender_list.clone();
        let dv_r = detail_view.clone();
        let sel_r = selected_unit.clone();
        let cfg_r = cfg_rc.clone();
        let st_r = status_lbl.clone();
        let scan_r = trigger_list_scan.clone();
        let open_r = open_detail.clone();
        let act_r = trigger_action.clone();

        *ensure_pump_slot.borrow_mut() = Some(Rc::new(move || {
            if pr_p.get() {
                return;
            }
            pr_p.set(true);
            let lw = list_weak.clone();
            let rx_l = rx_p.clone();
            let inf_l = inf_p.clone();
            let pr_l = pr_p.clone();
            let entries_l = entries_r.clone();
            let scope_l = scope_r.clone();
            let re_l = re_r.clone();
            let dv_l = dv_r.clone();
            let sel_l = sel_r.clone();
            let cfg_l = cfg_r.clone();
            let st_l = st_r.clone();
            let scan_l = scan_r.clone();
            let open_l = open_r.clone();
            let act_l = act_r.clone();

            glib::timeout_add_local(Duration::from_millis(50), move || {
                if lw.upgrade().is_none() {
                    inf_l.set(0);
                    pr_l.set(false);
                    return glib::ControlFlow::Break;
                }

                while let Ok(msg) = rx_l.try_recv() {
                    inf_l.set(inf_l.get().saturating_sub(1));
                    match msg {
                        SvcMsg::ListLoaded { scope, entries } => {
                            if scope == scope_l.get() {
                                *entries_l.borrow_mut() = entries;
                                re_l();
                            }
                        }
                        SvcMsg::DetailLoaded(detail) => {
                            let matches_open = sel_l
                                .borrow()
                                .as_ref()
                                .map(|(s, u)| *s == detail.scope && *u == detail.unit)
                                .unwrap_or(false);
                            if matches_open && dv_l.is_visible() {
                                render_detail_pane(
                                    &dv_l,
                                    &detail,
                                    &cfg_l,
                                    &sel_l,
                                    &st_l,
                                    &open_l,
                                    &act_l,
                                    &re_l,
                                );
                            }
                        }
                        SvcMsg::ActionDone {
                            scope,
                            unit,
                            message,
                        } => {
                            st_l.set_label(&message);
                            st_l.set_visible(true);
                            if scope == scope_l.get() {
                                scan_l(scope);
                            }
                            let detail_matches = dv_l.is_visible()
                                && sel_l
                                    .borrow()
                                    .as_ref()
                                    .map(|(s, u)| *s == scope && *u == unit)
                                    .unwrap_or(false);
                            if detail_matches {
                                open_l(scope, unit);
                            }
                        }
                    }
                }

                if inf_l.get() == 0 {
                    pr_l.set(false);
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            });
        }));
    }

    // Initial scan (starts on-demand pump automatically)
    trigger_list_scan(Scope::System);

    let show_scope = cur_scope.clone();
    let show_scan = trigger_list_scan.clone();
    let show_search = search_entry.clone();
    crate::module_api::ModuleHandle {
        widget: root.upcast::<gtk4::Widget>(),
        detail_pane: Some(detail_view.upcast::<gtk4::Widget>()),
        on_show: Some(Rc::new(move || {
            show_scan(show_scope.get());
            show_search.grab_focus();
        })),
        on_hide: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_service_list(
    container: &Box,
    entries: &[ServiceEntry],
    scope: Scope,
    filter: FilterMode,
    query: &str,
    cfg_snapshot: &ServicesConfig,
    selected_unit: Option<&str>,
    popover_guard: &Rc<Cell<usize>>,
    cfg_rc: &Rc<RefCell<ServicesConfig>>,
    status_lbl: &Label,
    open_detail: &Rc<dyn Fn(Scope, String)>,
    trigger_action: &Rc<dyn Fn(Scope, String, &'static str, &'static [&'static str])>,
    rerender_weak: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let matches = |e: &ServiceEntry| -> bool {
        let mode_ok = match filter {
            FilterMode::All => true,
            FilterMode::Active => e.is_running() || e.frozen,
            FilterMode::Failed => e.is_failed(),
        };
        if !mode_ok {
            return false;
        }
        if query.is_empty() {
            return true;
        }
        e.short_name.to_lowercase().contains(query)
            || e.unit.to_lowercase().contains(query)
            || e.description.to_lowercase().contains(query)
    };

    let mut pinned: Vec<&ServiceEntry> = Vec::new();
    let mut others: Vec<&ServiceEntry> = Vec::new();

    for e in entries {
        if !matches(e) {
            continue;
        }
        if cfg_snapshot.is_pinned(scope, &e.unit) {
            pinned.push(e);
        } else {
            others.push(e);
        }
    }

    let sort_cmp = |a: &&ServiceEntry, b: &&ServiceEntry| {
        b.is_failed()
            .cmp(&a.is_failed())
            .then_with(|| b.frozen.cmp(&a.frozen))
            .then_with(|| b.is_running().cmp(&a.is_running()))
            .then_with(|| a.short_name.to_lowercase().cmp(&b.short_name.to_lowercase()))
    };

    pinned.sort_by(sort_cmp);
    others.sort_by(sort_cmp);

    if !pinned.is_empty() {
        container.append(
            &Label::builder()
                .label("PINNED SERVICES")
                .halign(Align::Start)
                .css_classes(vec!["fav-group-header".to_string()])
                .margin_top(2)
                .build(),
        );
        for item in &pinned {
            container.append(&build_service_row(
                item,
                scope,
                true,
                selected_unit == Some(item.unit.as_str()),
                popover_guard,
                cfg_rc,
                status_lbl,
                open_detail,
                trigger_action,
                rerender_weak,
            ));
        }
    }

    let cap = if query.is_empty() { 60 } else { 100 };
    if !others.is_empty() {
        let hdr_text = if others.len() > cap {
            format!("ALL SERVICES (showing {} of {} — search to filter)", cap, others.len())
        } else {
            format!("SERVICES ({})", others.len())
        };
        container.append(
            &Label::builder()
                .label(&hdr_text)
                .halign(Align::Start)
                .css_classes(vec!["fav-group-header".to_string()])
                .margin_top(if pinned.is_empty() { 2 } else { 8 })
                .build(),
        );
        for item in others.into_iter().take(cap) {
            container.append(&build_service_row(
                item,
                scope,
                false,
                selected_unit == Some(item.unit.as_str()),
                popover_guard,
                cfg_rc,
                status_lbl,
                open_detail,
                trigger_action,
                rerender_weak,
            ));
        }
    }

    if pinned.is_empty() && container.first_child().is_none() {
        container.append(
            &Label::builder()
                .label(if query.is_empty() {
                    "No matching services."
                } else {
                    "No services match your search."
                })
                .justify(gtk4::Justification::Center)
                .css_classes(vec!["fav-score-badge".to_string()])
                .margin_top(20)
                .build(),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn build_service_row(
    item: &ServiceEntry,
    scope: Scope,
    is_pinned: bool,
    is_selected: bool,
    popover_guard: &Rc<Cell<usize>>,
    cfg_rc: &Rc<RefCell<ServicesConfig>>,
    status_lbl: &Label,
    open_detail: &Rc<dyn Fn(Scope, String)>,
    trigger_action: &Rc<dyn Fn(Scope, String, &'static str, &'static [&'static str])>,
    rerender_weak: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) -> Box {
    let row_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .build();

    let mut classes = vec!["fav-item-btn".to_string()];
    if is_selected || (is_pinned && item.is_running()) {
        classes.push("pinned".to_string());
    }

    let tooltip = format!(
        "{}\nState: {} ({}){}\nBoot: {}\n\nLeft-click for inspector & logs | Right-click for quick actions",
        item.unit,
        item.active_state,
        item.sub_state,
        if item.frozen { " [PAUSED]" } else { "" },
        item.boot_state
    );

    let btn = Button::builder()
        .css_classes(classes)
        .hexpand(true)
        .tooltip_text(&tooltip)
        .build();

    let col = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .build();

    let top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let glyph = if item.frozen {
        "󰏤"
    } else if item.is_failed() {
        "󰀨"
    } else if item.is_running() {
        "●"
    } else {
        "○"
    };

    let name_lbl = Label::builder()
        .label(&format!("{} {}", glyph, item.short_name))
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-body".to_string()])
        .build();

    let badge_lbl = Label::builder()
        .label(&item.status_badge(is_pinned))
        .css_classes(vec!["fav-score-badge".to_string()])
        .valign(Align::Center)
        .build();

    top_row.append(&name_lbl);
    top_row.append(&badge_lbl);
    col.append(&top_row);

    let sub_text = if !item.description.is_empty() {
        item.description.clone()
    } else {
        format!("boot: {}", item.boot_state)
    };
    let sub_lbl = Label::builder()
        .label(&sub_text)
        .halign(Align::Start)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    col.append(&sub_lbl);

    btn.set_child(Some(&col));

    // Left-Click: Open 400px Inspector & Logs Pane
    {
        let open_c = open_detail.clone();
        let unit_c = item.unit.clone();
        let re_c = rerender_weak.clone();
        btn.connect_clicked(move |_| {
            open_c(scope, unit_c.clone());
            if let Some(ref f) = *re_c.borrow() {
                f();
            }
        });
    }

    // Right-Click Quick Action Context Menu
    let right_click = GestureClick::builder().button(3).build();
    let row_weak = row_box.downgrade();
    let item_ctx = item.clone();
    let pg_ctx = popover_guard.clone();
    let cfg_ctx = cfg_rc.clone();
    let st_ctx = status_lbl.clone();
    let act_ctx = trigger_action.clone();
    let re_ctx = rerender_weak.clone();

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
            let pw = p.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(pop) = pw.upgrade() {
                    pop.unparent();
                }
            });
        });

        let gm = pg_ctx.clone();
        popover.connect_map(move |_| gm.set(gm.get() + 1));
        let gu = pg_ctx.clone();
        popover.connect_unmap(move |_| {
            let g = gu.clone();
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                g.set(g.get().saturating_sub(1));
            });
        });

        let menu_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .build();

        let add_action_btn = |label: &str,
                              css: &str,
                              action: &'static str,
                              extra: &'static [&'static str]| {
            let b = Button::builder()
                .label(label)
                .css_classes(vec![css.to_string()])
                .build();
            let pop_w = popover.downgrade();
            let act_c = act_ctx.clone();
            let u_c = item_ctx.unit.clone();
            b.connect_clicked(move |_| {
                if let Some(p) = pop_w.upgrade() {
                    p.popdown();
                }
                act_c(scope, u_c.clone(), action, extra);
            });
            menu_box.append(&b);
        };

        // 1. Start or Stop
        if item_ctx.is_running() {
            add_action_btn("Stop", "fav-hide-btn", "stop", &[]);
        } else {
            add_action_btn("Start", "fav-hide-btn", "start", &[]);
        }

        // 2. Restart
        add_action_btn("Restart", "fav-hide-btn", "restart", &[]);

        // 3. Pause (Freeze) / Resume (Thaw)
        if item_ctx.frozen {
            add_action_btn("Resume (Thaw)", "fav-hide-btn", "thaw", &[]);
        } else if item_ctx.is_running() {
            add_action_btn("Pause (Freeze)", "fav-hide-btn", "freeze", &[]);
        }

        // 4. Pin / Unpin
        let pin_btn = Button::builder()
            .label(if is_pinned { "Unpin" } else { "Pin to Top" })
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();
        {
            let pop_w = popover.downgrade();
            let cfg_p = cfg_ctx.clone();
            let u_p = item_ctx.unit.clone();
            let short_p = item_ctx.short_name.clone();
            let st_p = st_ctx.clone();
            let re_p = re_ctx.clone();
            pin_btn.connect_clicked(move |_| {
                if let Some(p) = pop_w.upgrade() {
                    p.popdown();
                }
                let now_pinned = {
                    let mut cfg = cfg_p.borrow_mut();
                    let res = cfg.toggle_pin(scope, &u_p);
                    save_config(&cfg);
                    res
                };
                let msg = if now_pinned {
                    format!("Pinned {} to top.", short_p)
                } else {
                    format!("Unpinned {}.", short_p)
                };
                st_p.set_label(&msg);
                st_p.set_visible(true);
                if let Some(ref f) = *re_p.borrow() {
                    f();
                }
            });
        }
        menu_box.append(&pin_btn);

        // 5. Boot Enable / Disable
        if item_ctx.boot_state == "enabled" {
            add_action_btn("Disable at Boot", "fav-hide-btn", "disable", &[]);
        } else if item_ctx.boot_state == "disabled" {
            add_action_btn("Enable at Boot", "fav-hide-btn", "enable", &[]);
        }

        // 6. Force Kill (SIGKILL)
        if item_ctx.is_running() || item_ctx.frozen {
            add_action_btn(
                "Force Kill (SIGKILL)",
                "plan-delete-menu-btn",
                "kill",
                &["-s", "SIGKILL"],
            );
        }

        popover.set_child(Some(&menu_box));
        popover.popup();
    });
    btn.add_controller(right_click);

    row_box.append(&btn);
    row_box
}

#[allow(clippy::too_many_arguments)]
fn render_detail_pane(
    detail_view: &Box,
    detail: &ServiceDetail,
    cfg_rc: &Rc<RefCell<ServicesConfig>>,
    selected_unit: &Rc<RefCell<Option<(Scope, String)>>>,
    status_lbl: &Label,
    open_detail: &Rc<dyn Fn(Scope, String)>,
    trigger_action: &Rc<dyn Fn(Scope, String, &'static str, &'static [&'static str])>,
    rerender_list: &Rc<dyn Fn()>,
) {
    while let Some(child) = detail_view.first_child() {
        detail_view.remove(&child);
    }

    let is_running = detail.active_state == "active" || detail.active_state == "reloading";
    let is_frozen = detail.freezer_state.eq_ignore_ascii_case("frozen");
    let is_pinned = cfg_rc.borrow().is_pinned(detail.scope, &detail.unit);

    // Top Header Bar
    let top_bar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_top(12)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(6)
        .build();

    let title_lbl = Label::builder()
        .label(&detail.short_name)
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-h1".to_string()])
        .tooltip_text(&detail.unit)
        .build();

    let ref_btn = Button::builder()
        .label("Refresh")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();
    {
        let open_r = open_detail.clone();
        let sc = detail.scope;
        let u = detail.unit.clone();
        ref_btn.connect_clicked(move |_| open_r(sc, u.clone()));
    }

    let close_btn = Button::builder()
        .label("×")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .tooltip_text("Close inspector")
        .build();
    {
        let dv = detail_view.clone();
        let sel = selected_unit.clone();
        let re = rerender_list.clone();
        close_btn.connect_clicked(move |_| {
            dv.set_visible(false);
            *sel.borrow_mut() = None;
            re();
        });
    }

    top_bar.append(&title_lbl);
    top_bar.append(&ref_btn);
    top_bar.append(&close_btn);
    detail_view.append(&top_bar);

    let body = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(14)
        .hexpand(true)
        .vexpand(true)
        .build();

    // 1. Metadata & Status Card
    let info_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .css_classes(vec!["sys-card".to_string()])
        .build();

    if !detail.description.is_empty() {
        info_card.append(
            &Label::builder()
                .label(&detail.description)
                .halign(Align::Start)
                .wrap(true)
                .xalign(0.0)
                .css_classes(vec!["plan-body".to_string()])
                .build(),
        );
    }

    let state_text = if is_frozen {
        format!("PAUSED (frozen) • {} ({})", detail.active_state, detail.sub_state)
    } else {
        format!("{} ({})", detail.active_state, detail.sub_state)
    };

    let add_kv = |k: &str, v: &str| {
        let r = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();
        r.append(
            &Label::builder()
                .label(k)
                .halign(Align::Start)
                .css_classes(vec!["plan-subnote".to_string()])
                .build(),
        );
        r.append(
            &Label::builder()
                .label(v)
                .halign(Align::End)
                .hexpand(true)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .css_classes(vec!["fav-score-badge".to_string()])
                .build(),
        );
        info_card.append(&r);
    };

    add_kv("Unit", &detail.unit);
    add_kv("State", &state_text);
    add_kv("Boot", &detail.boot_state);
    add_kv("PID / RAM", &format!("{}  •  {}", detail.main_pid, detail.memory_str));
    if detail.since != "—" {
        add_kv("Since", &detail.since);
    }
    body.append(&info_card);

    // 2. Primary Action Buttons
    let row1 = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();

    let start_stop_btn = Button::builder()
        .label(if is_running { "Stop" } else { "Start" })
        .css_classes(vec!["primary-btn".to_string()])
        .tooltip_text(if is_running {
            "Cleanly stop service (prevents auto-restart)"
        } else {
            "Start service"
        })
        .build();
    {
        let act = trigger_action.clone();
        let sc = detail.scope;
        let u = detail.unit.clone();
        let verb = if is_running { "stop" } else { "start" };
        start_stop_btn.connect_clicked(move |_| act(sc, u.clone(), verb, &[]));
    }

    let restart_btn = Button::builder()
        .label("Restart")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    {
        let act = trigger_action.clone();
        let sc = detail.scope;
        let u = detail.unit.clone();
        restart_btn.connect_clicked(move |_| act(sc, u.clone(), "restart", &[]));
    }

    row1.append(&start_stop_btn);
    row1.append(&restart_btn);
    body.append(&row1);

    let row2 = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();

    let freeze_btn = Button::builder()
        .label(if is_frozen {
            "Resume (Thaw)"
        } else {
            "Pause (Freeze)"
        })
        .css_classes(if is_frozen {
            vec!["plan-fmt-btn".to_string(), "active".to_string()]
        } else {
            vec!["plan-fmt-btn".to_string()]
        })
        .sensitive(is_running || is_frozen)
        .tooltip_text("Freeze/thaw all processes in this service via cgroup v2")
        .build();
    {
        let act = trigger_action.clone();
        let sc = detail.scope;
        let u = detail.unit.clone();
        let verb = if is_frozen { "thaw" } else { "freeze" };
        freeze_btn.connect_clicked(move |_| act(sc, u.clone(), verb, &[]));
    }

    let kill_btn = Button::builder()
        .label("Force Kill (9)")
        .css_classes(vec!["plan-delete-menu-btn".to_string()])
        .sensitive(is_running || is_frozen)
        .tooltip_text("Send SIGKILL to every process in this service")
        .build();
    {
        let act = trigger_action.clone();
        let sc = detail.scope;
        let u = detail.unit.clone();
        kill_btn.connect_clicked(move |_| act(sc, u.clone(), "kill", &["-s", "SIGKILL"]));
    }

    row2.append(&freeze_btn);
    row2.append(&kill_btn);
    body.append(&row2);

    // Row 3: Pin + Boot Toggle + Copy Unit Name
    let row3 = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();

    let pin_btn = Button::builder()
        .label(if is_pinned { "Pinned ✓" } else { "Pin" })
        .css_classes(if is_pinned {
            vec!["plan-fmt-btn".to_string(), "active".to_string()]
        } else {
            vec!["plan-fmt-btn".to_string()]
        })
        .build();
    {
        let cfg_p = cfg_rc.clone();
        let sc = detail.scope;
        let u = detail.unit.clone();
        let open_p = open_detail.clone();
        let re_p = rerender_list.clone();
        pin_btn.connect_clicked(move |_| {
            {
                let mut cfg = cfg_p.borrow_mut();
                cfg.toggle_pin(sc, &u);
                save_config(&cfg);
            }
            re_p();
            open_p(sc, u.clone());
        });
    }

    let boot_btn = Button::builder()
        .label(if detail.boot_state == "enabled" {
            "Boot: ON"
        } else if detail.boot_state == "disabled" {
            "Boot: OFF"
        } else {
            &detail.boot_state
        })
        .css_classes(if detail.boot_state == "enabled" {
            vec!["plan-fmt-btn".to_string(), "active".to_string()]
        } else {
            vec!["plan-fmt-btn".to_string()]
        })
        .sensitive(detail.boot_state == "enabled" || detail.boot_state == "disabled")
        .build();
    {
        let act = trigger_action.clone();
        let sc = detail.scope;
        let u = detail.unit.clone();
        let verb = if detail.boot_state == "enabled" {
            "disable"
        } else {
            "enable"
        };
        boot_btn.connect_clicked(move |_| act(sc, u.clone(), verb, &[]));
    }

    let copy_logs_btn = Button::builder()
        .label("Copy Logs")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    {
        let logs_c = detail.logs.clone();
        let st_c = status_lbl.clone();
        copy_logs_btn.connect_clicked(move |b| {
            copy_to_clipboard(&logs_c);
            st_c.set_label("Copied recent journal logs to clipboard.");
            st_c.set_visible(true);
            b.set_label("Copied!");
            let br = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                br.set_label("Copy Logs");
            });
        });
    }

    row3.append(&pin_btn);
    row3.append(&boot_btn);
    row3.append(&copy_logs_btn);
    body.append(&row3);

    // 3. Recent Journal Logs
    body.append(
        &Label::builder()
            .label("RECENT LOGS (JOURNALCTL)")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(4)
            .build(),
    );

    let log_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .css_classes(vec!["sys-card".to_string()])
        .build();

    let log_lbl = Label::builder()
        .label(&detail.logs)
        .halign(Align::Start)
        .valign(Align::Start)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .selectable(true)
        .css_classes(vec!["plan-subnote".to_string()])
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    log_scroll.set_child(Some(&log_lbl));
    body.append(&log_scroll);

    // Auto-scroll journal logs to the newest line at the bottom
    let adj = log_scroll.vadjustment();
    glib::idle_add_local_once(move || {
        adj.set_value((adj.upper() - adj.page_size()).max(0.0));
    });

    detail_view.append(&body);
}