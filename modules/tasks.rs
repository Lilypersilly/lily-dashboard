// @dash-id: tasks
// @dash-title: Tasks
// @dash-lifecycle: stateful
// Dashpendencies:
//! Live CPU & RAM process monitor with search, sort modes, freeze toggle,
//! and right-click graceful (`SIGTERM`) / force (`SIGKILL`) termination.
//!
//! Runs `ps` off the GTK main thread and pauses its 1.5s refresh loop whenever
//! the tab is hidden or the dashboard drawer is closed (0% idle CPU).

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, Orientation, PolicyType, Popover, ProgressBar,
    ScrolledWindow, SearchEntry,
};
use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Debug)]
struct ProcInfo {
    pid: String,
    cpu: f64,
    mem: f64,
    name: String,
}

fn get_processes(sort_by: &str, query: &str) -> Vec<ProcInfo> {
    let sort_flag = if sort_by == "cpu" { "-pcpu" } else { "-pmem" };
    let mut results = Vec::new();
    let q = query.trim().to_lowercase();

    if let Ok(out) = Command::new("ps")
        .args(["-eo", "pid,pcpu,pmem,comm", "--sort", sort_flag])
        .output()
    {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let pid = parts[0].to_string();
                let cpu = parts[1].parse::<f64>().unwrap_or(0.0);
                let mem = parts[2].parse::<f64>().unwrap_or(0.0);
                let name = parts[3..].join(" ");

                if !q.is_empty() && !name.to_lowercase().contains(&q) && !pid.contains(&q) {
                    continue;
                }

                results.push(ProcInfo {
                    pid,
                    cpu,
                    mem,
                    name,
                });

                if results.len() >= 30 {
                    break;
                }
            }
        }
    }
    results
}

pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    // 1. Header
    root.append(
        &Label::builder()
            .label("PROCESS MONITOR")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .build(),
    );

    // 2. Toolbar (Sort Modes + Freeze)
    let toolbar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let sort_cpu_btn = Button::builder()
        .label("Sort: CPU")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .build();
    let sort_ram_btn = Button::builder()
        .label("Sort: RAM")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    let freeze_btn = Button::builder()
        .label("Freeze")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Pause live updates so you can click jumping items")
        .hexpand(true)
        .halign(Align::End)
        .build();

    toolbar.append(&sort_cpu_btn);
    toolbar.append(&sort_ram_btn);
    toolbar.append(&freeze_btn);
    root.append(&toolbar);

    // 3. Search Bar
    let search_entry = SearchEntry::builder()
        .placeholder_text("Search by app name or PID...")
        .css_classes(vec!["fav-search".to_string()])
        .build();
    root.append(&search_entry);

    // 4. Scrollable List Container
    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let list_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    scrolled.set_child(Some(&list_box));
    root.append(&scrolled);

    // State Variables
    let sort_mode = Rc::new(RefCell::new("cpu".to_string()));
    let search_query = Rc::new(RefCell::new(String::new()));
    let is_frozen = Rc::new(Cell::new(false));
    let is_visible = Rc::new(Cell::new(true));
    let timer_running = Rc::new(Cell::new(false));
    let scan_in_flight = Rc::new(Cell::new(false));
    let scan_queued = Rc::new(Cell::new(false));
    let pump_running = Rc::new(Cell::new(false));
    let popover_guard = ctx.popover_guard.clone();

    let (tx, rx) = mpsc::channel::<Vec<ProcInfo>>();
    let rx_rc = Rc::new(rx);

    let trigger_scan: Rc<dyn Fn()> = {
        let sm_rc = sort_mode.clone();
        let sq_rc = search_query.clone();
        let inflight_rc = scan_in_flight.clone();
        let queued_rc = scan_queued.clone();
        let pump_rc = pump_running.clone();
        let tx_s = tx.clone();
        let rx_s = rx_rc.clone();
        let list_weak = list_box.downgrade();
        let pg_rc = popover_guard.clone();

        let self_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let self_weak = self_slot.clone();

        let cb: Rc<dyn Fn()> = Rc::new(move || {
            if inflight_rc.get() {
                queued_rc.set(true);
                return;
            }
            inflight_rc.set(true);
            let mode = sm_rc.borrow().clone();
            let query = sq_rc.borrow().clone();
            let tx_bg = tx_s.clone();
            std::thread::spawn(move || {
                let procs = get_processes(&mode, &query);
                let _ = tx_bg.send(procs);
            });

            if !pump_rc.get() {
                pump_rc.set(true);
                let rx_p = rx_s.clone();
                let lw = list_weak.clone();
                let pg = pg_rc.clone();
                let inf = inflight_rc.clone();
                let qd = queued_rc.clone();
                let pr = pump_rc.clone();
                let sw = self_weak.clone();
                glib::timeout_add_local(Duration::from_millis(40), move || {
                    let Some(lb) = lw.upgrade() else {
                        inf.set(false);
                        pr.set(false);
                        return glib::ControlFlow::Break;
                    };
                    if let Ok(procs) = rx_p.try_recv() {
                        inf.set(false);
                        if pg.get() == 0 {
                            while let Some(child) = lb.first_child() {
                                lb.remove(&child);
                            }
                            if procs.is_empty() {
                                lb.append(
                                    &Label::builder()
                                        .label("No matching processes found.")
                                        .css_classes(vec!["plan-subnote".to_string()])
                                        .margin_top(20)
                                        .build(),
                                );
                            } else {
                                for p in &procs {
                                    lb.append(&build_proc_row(p, &pg, &sw));
                                }
                            }
                        }
                        if qd.replace(false) {
                            if let Some(ref f) = *sw.borrow() {
                                f();
                            }
                        }
                        if !inf.get() {
                            pr.set(false);
                            return glib::ControlFlow::Break;
                        }
                    }
                    glib::ControlFlow::Continue
                });
            }
        });

        *self_slot.borrow_mut() = Some(cb.clone());
        cb
    };

    let ensure_live_timer: Rc<dyn Fn()> = {
        let vis = is_visible.clone();
        let tr = timer_running.clone();
        let frz = is_frozen.clone();
        let pg = popover_guard.clone();
        let scan = trigger_scan.clone();
        Rc::new(move || {
            if tr.get() || !vis.get() {
                return;
            }
            tr.set(true);
            let vis_t = vis.clone();
            let tr_t = tr.clone();
            let frz_t = frz.clone();
            let pg_t = pg.clone();
            let scan_t = scan.clone();
            glib::timeout_add_local(Duration::from_millis(1500), move || {
                if !vis_t.get() {
                    tr_t.set(false);
                    return glib::ControlFlow::Break;
                }
                if !frz_t.get() && pg_t.get() == 0 {
                    scan_t();
                }
                glib::ControlFlow::Continue
            });
        })
    };

    // Wire up Search Bar
    {
        let sq = search_query.clone();
        let scan = trigger_scan.clone();
        search_entry.connect_search_changed(move |entry| {
            *sq.borrow_mut() = entry.text().to_string();
            scan();
        });
    }

    // Wire up Toolbar Buttons
    {
        let sm = sort_mode.clone();
        let b_cpu = sort_cpu_btn.clone();
        let b_ram = sort_ram_btn.clone();
        let scan = trigger_scan.clone();
        sort_cpu_btn.connect_clicked(move |_| {
            *sm.borrow_mut() = "cpu".to_string();
            b_cpu.add_css_class("active");
            b_ram.remove_css_class("active");
            scan();
        });
    }
    {
        let sm = sort_mode.clone();
        let b_cpu = sort_cpu_btn.clone();
        let b_ram = sort_ram_btn.clone();
        let scan = trigger_scan.clone();
        sort_ram_btn.connect_clicked(move |_| {
            *sm.borrow_mut() = "mem".to_string();
            b_ram.add_css_class("active");
            b_cpu.remove_css_class("active");
            scan();
        });
    }
    {
        let f = is_frozen.clone();
        let scan = trigger_scan.clone();
        freeze_btn.connect_clicked(move |b| {
            let next = !f.get();
            f.set(next);
            if next {
                b.add_css_class("active");
                b.set_label("Unfreeze");
            } else {
                b.remove_css_class("active");
                b.set_label("Freeze");
                scan();
            }
        });
    }

    // Initial load & start timer while visible
    trigger_scan();
    ensure_live_timer();

    let search_focus = search_entry.clone();
    let vis_show = is_visible.clone();
    let scan_show = trigger_scan.clone();
    let timer_show = ensure_live_timer.clone();
    let vis_hide = is_visible.clone();

    crate::module_api::ModuleHandle {
        widget: root.upcast::<gtk4::Widget>(),
        detail_pane: None,
        on_show: Some(Rc::new(move || {
            vis_show.set(true);
            scan_show();
            timer_show();
            search_focus.grab_focus();
        })),
        on_hide: Some(Rc::new(move || {
            vis_hide.set(false);
        })),
    }
}

fn build_proc_row(
    proc: &ProcInfo,
    popover_guard: &Rc<Cell<usize>>,
    rescan_weak: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) -> Box {
    let row_box = Box::builder().orientation(Orientation::Horizontal).build();
    let btn = Button::builder()
        .css_classes(vec!["fav-item-btn".to_string()])
        .hexpand(true)
        .tooltip_text("Right-click to Terminate / Force Kill")
        .build();

    let inner = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .build();

    // Left Col (Name & PID)
    let left_col = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    let name_lbl = Label::builder()
        .label(&proc.name)
        .halign(Align::Start)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-body".to_string()])
        .build();
    let pid_lbl = Label::builder()
        .label(&format!("PID: {}", proc.pid))
        .halign(Align::Start)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    left_col.append(&name_lbl);
    left_col.append(&pid_lbl);
    inner.append(&left_col);

    // Middle Col (CPU Progress Bar)
    let cpu_col = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .width_request(76)
        .valign(Align::Center)
        .build();
    let cpu_lbl = Label::builder()
        .label(&format!("{:.1}% CPU", proc.cpu))
        .halign(Align::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    let cpu_bar = ProgressBar::builder()
        .fraction((proc.cpu / 100.0).clamp(0.0, 1.0))
        .css_classes(vec!["drive-progress".to_string()])
        .build();
    cpu_col.append(&cpu_lbl);
    cpu_col.append(&cpu_bar);
    inner.append(&cpu_col);

    // Right Col (RAM Progress Bar)
    let mem_col = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .width_request(76)
        .valign(Align::Center)
        .build();
    let mem_lbl = Label::builder()
        .label(&format!("{:.1}% RAM", proc.mem))
        .halign(Align::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    let mem_bar = ProgressBar::builder()
        .fraction((proc.mem / 100.0).clamp(0.0, 1.0))
        .css_classes(vec!["drive-progress".to_string()])
        .build();
    mem_col.append(&mem_lbl);
    mem_col.append(&mem_bar);
    inner.append(&mem_col);

    btn.set_child(Some(&inner));

    // Right-Click Context Menu for Killing Processes
    let right_click = GestureClick::builder().button(3).build();
    let row_weak = row_box.downgrade();
    let guard_ctx = popover_guard.clone();
    let pid_ctx = proc.pid.clone();
    let rescan_ctx = rescan_weak.clone();

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

        let term_btn = Button::builder()
            .label("Terminate (SIGTERM)")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();
        let kill_btn = Button::builder()
            .label("Force Kill (SIGKILL)")
            .css_classes(vec!["plan-delete-menu-btn".to_string()])
            .build();

        let pop_t = popover.downgrade();
        let pid_t = pid_ctx.clone();
        let rs_t = rescan_ctx.clone();
        term_btn.connect_clicked(move |_| {
            if let Some(p) = pop_t.upgrade() {
                p.popdown();
            }
            let _ = Command::new("kill").arg("-15").arg(&pid_t).spawn();
            let rs_after = rs_t.clone();
            glib::timeout_add_local_once(Duration::from_millis(250), move || {
                if let Some(ref f) = *rs_after.borrow() {
                    f();
                }
            });
        });

        let pop_k = popover.downgrade();
        let pid_k = pid_ctx.clone();
        let rs_k = rescan_ctx.clone();
        kill_btn.connect_clicked(move |_| {
            if let Some(p) = pop_k.upgrade() {
                p.popdown();
            }
            let _ = Command::new("kill").arg("-9").arg(&pid_k).spawn();
            let rs_after = rs_k.clone();
            glib::timeout_add_local_once(Duration::from_millis(250), move || {
                if let Some(ref f) = *rs_after.borrow() {
                    f();
                }
            });
        });

        menu_box.append(&term_btn);
        menu_box.append(&kill_btn);
        popover.set_child(Some(&menu_box));
        popover.popup();
    });

    btn.add_controller(right_click);
    row_box.append(&btn);
    row_box
}
