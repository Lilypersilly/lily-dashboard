// @dash-id: timers
// @dash-title: Timers
// @dash-lifecycle: stateful
//! Countdown timers and process-watcher alerts tab.
//!
//! Supports simple durations (`15m`, `2h`, `45s`) as well as watching a running PID
//! until it exits or sits at 0% CPU, firing `notify-send` + `paplay` on completion.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Label, Orientation, PolicyType, ScrolledWindow, SearchEntry, Stack, Entry,
};
use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

// --- DATA STRUCTURES ---
#[derive(Clone, Debug, PartialEq)]
enum WatchCondition {
    OnClose,
    OnIdle,
}

#[derive(Clone, Debug)]
struct ProcessJob {
    pid: String,
    name: String,
    condition: WatchCondition,
    idle_ticks: u8, // How many consecutive ticks it has been at 0% CPU
}

#[derive(Clone, Debug)]
struct TimerJob {
    title: String,
    end_time: Instant,
    #[allow(dead_code)]
    original_duration: Duration,
}

struct TimersState {
    process_jobs: Vec<ProcessJob>,
    timer_jobs: Vec<TimerJob>,
}

fn play_alert_sound(title: &str, message: &str) {
    // Send a native desktop notification
    let _ = Command::new("notify-send")
        .args(["-u", "critical", "-a", "Lily Dashboard", title, message])
        .spawn();

    // Play a standard freedesktop completion sound using paplay (PulseAudio/PipeWire)
    let _ = Command::new("paplay")
        .arg("/usr/share/srounds/feedesktop/stereo/complete.oga")
        .spawn();
}

// Parses "10m", "1h", "45s" into a Duration
// Parses compound times like "1h30m", "5m24s", "2h", "45s" into a Duration
fn parse_duration(input: &str) -> Option<Duration> {
    let s = input.trim().to_lowercase();
    let mut total_secs = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else if c == 'h' || c == 'm' || c == 's' {
            if let Ok(val) = current_num.parse::<u64>() {
                match c {
                    'h' => total_secs += val * 3600,
                    'm' => total_secs += val * 60,
                    's' => total_secs += val,
                    _ => {}
                }
                current_num.clear(); // Reset for the next number
            } else {
                return None; // Hit a letter but didn't have a valid number before it
            }
        } else if c.is_whitespace() {
            continue; // Ignore spaces so "1h 30m" works too
        } else {
            return None; // Invalid character detected
        }
    }

    // If there's a trailing number without a unit (like just "10"), reject it to enforce proper formatting
    if !current_num.is_empty() {
        return None;
    }

    if total_secs > 0 {
        Some(Duration::from_secs(total_secs))
    } else {
        None
    }
}

// Formats duration into MM:SS or HH:MM:SS
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs > 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

pub fn build_timers_ui() -> Box {
    let state = Rc::new(RefCell::new(TimersState {
        process_jobs: Vec::new(),
        timer_jobs: Vec::new(),
    }));

    // ==========================================
    // 1. MAIN APP TAB
    // ==========================================
    let app_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let top_nav = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let btn_timers = Button::builder().label("Standard Timers").css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()]).hexpand(true).build();
    let btn_processes = Button::builder().label("Process Watchers").css_classes(vec!["plan-fmt-btn".to_string()]).hexpand(true).build();
    top_nav.append(&btn_timers);
    top_nav.append(&btn_processes);
    app_box.append(&top_nav);

    let inner_stack = Stack::new();
    inner_stack.set_hhomogeneous(false);
    inner_stack.set_hexpand(true);
    inner_stack.set_vexpand(true);
    app_box.append(&inner_stack);

    // --- TIMERS PAGE ---
    let timers_page = Box::builder().orientation(Orientation::Vertical).spacing(8).build();
    
    let timer_input_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let timer_entry = Entry::builder()
        .placeholder_text("e.g. '15m', '2h', or '45s'...")
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();
    let timer_start_btn = Button::builder().label("Start Timer").css_classes(vec!["primary-btn".to_string()]).build();
    timer_input_row.append(&timer_entry);
    timer_input_row.append(&timer_start_btn);
    timers_page.append(&timer_input_row);

    let active_timers_hdr = Label::builder().label("ACTIVE TIMERS").halign(Align::Start).css_classes(vec!["fav-group-header".to_string()]).margin_top(8).build();
    timers_page.append(&active_timers_hdr);

    let timers_list = Box::builder().orientation(Orientation::Vertical).spacing(6).build();
    timers_page.append(&timers_list);

    // --- PROCESS WATCHER PAGE ---
    let process_page = Box::builder().orientation(Orientation::Vertical).spacing(8).build();
    
    let process_search = SearchEntry::builder()
        .placeholder_text("Search running apps (e.g. firefox, cargo)...")
        .css_classes(vec!["fav-search".to_string()])
        .build();
    process_page.append(&process_search);

    let active_watchers_hdr = Label::builder().label("ACTIVE WATCHERS").halign(Align::Start).css_classes(vec!["fav-group-header".to_string()]).margin_top(4).build();
    process_page.append(&active_watchers_hdr);

    let process_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();
    let process_list = Box::builder().orientation(Orientation::Vertical).spacing(6).build();
    process_scroll.set_child(Some(&process_list));
    process_page.append(&process_scroll);

    inner_stack.add_named(&timers_page, Some("timers_view"));
    inner_stack.add_named(&process_page, Some("process_view"));

    // ==========================================
    // 2. UI LOGIC & EVENTS
    // ==========================================

    // Tab Switching
    let nav_t = inner_stack.clone();
    let bt_t = btn_timers.clone();
    let bt_p = btn_processes.clone();
    btn_timers.connect_clicked(move |_| {
        bt_t.add_css_class("active");
        bt_p.remove_css_class("active");
        nav_t.set_visible_child_name("timers_view");
    });

    let nav_p = inner_stack.clone();
    let bt_t2 = btn_timers.clone();
    let bt_p2 = btn_processes.clone();
    let p_search = process_search.clone();
    btn_processes.connect_clicked(move |_| {
        bt_p2.add_css_class("active");
        bt_t2.remove_css_class("active");
        p_search.set_text(""); // Clear triggers a fresh process list load
        nav_p.set_visible_child_name("process_view");
    });

    // Handle standard timer creation
    let s_add = state.clone();
    let e_add = timer_entry.clone();
    let commit_timer = Rc::new(move || {
        let text = e_add.text().to_string();
        if let Some(dur) = parse_duration(&text) {
            s_add.borrow_mut().timer_jobs.push(TimerJob {
                title: text.clone(),
                end_time: Instant::now() + dur,
                original_duration: dur,
            });
            e_add.set_text("");
        } else {
            e_add.set_text("Invalid! Use 15m, 2h, etc.");
        }
    });

    let c_btn = commit_timer.clone();
    timer_start_btn.connect_clicked(move |_| c_btn());
    let c_ent = commit_timer.clone();
    timer_entry.connect_activate(move |_| c_ent());

    // Handle Process Searching
    let s_proc = state.clone();
    let p_list = process_list.clone();
    process_search.connect_search_changed(move |entry| {
        let query = entry.text().trim().to_lowercase();
        
        while let Some(child) = p_list.first_child() {
            p_list.remove(&child);
        }

        // Render Active Watchers First
        let state_ref = s_proc.borrow();
        for job in state_ref.process_jobs.iter() {
            let row = Box::builder().orientation(Orientation::Horizontal).spacing(6).css_classes(vec!["plan-check-row".to_string(), "selected".to_string()]).build();
            let icon = Label::builder().label("👁️").build();
            let desc = format!("Watching {} (PID {})", job.name, job.pid);
            let lbl = Label::builder().label(&desc).hexpand(true).halign(Align::Start).build();
            let cond = if job.condition == WatchCondition::OnClose { "Until Closed" } else { "Until Idle" };
            let cond_lbl = Label::builder().label(cond).css_classes(vec!["plan-subnote".to_string()]).build();
            
            let cancel_btn = Button::builder().label("×").css_classes(vec!["plan-line-del-btn".to_string()]).build();
            
            // Fix: Cleanly remove by PID and delete the row without firing fake signals
            let s_del = s_proc.clone();
            let pid_to_remove = job.pid.clone();
            let row_del = row.clone();
            let p_list_del = p_list.clone();
            
            cancel_btn.connect_clicked(move |_| {
                s_del.borrow_mut().process_jobs.retain(|j| j.pid != pid_to_remove);
                p_list_del.remove(&row_del);
            });

            row.append(&icon);
            row.append(&lbl);
            row.append(&cond_lbl);
            row.append(&cancel_btn);
            p_list.append(&row);
        }

        // Search Linux Processes
        if !query.is_empty() {
            let sep = Label::builder().label("FOUND RUNNING APPS").halign(Align::Start).css_classes(vec!["fav-group-header".to_string()]).margin_top(8).build();
            p_list.append(&sep);

            // Use `ps` to get PID, CPU%, and Command
            if let Ok(output) = Command::new("ps").args(["-e", "-o", "pid,pcpu,comm"]).output() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                
                for line in stdout.lines().skip(1) { // Skip header
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 3 {
                        let pid = parts[0];
                        let cpu = parts[1];
                        let cmd = parts[2..].join(" ");
                        
                        if cmd.to_lowercase().contains(&query) && !cmd.contains("lily-dashboard") {
                            let row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
                            
                            let btn = Button::builder().css_classes(vec!["fav-item-btn".to_string()]).hexpand(true).build();
                            let inner = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
                            let lbl = Label::builder().label(&cmd).hexpand(true).halign(Align::Start).ellipsize(gtk4::pango::EllipsizeMode::End).build();
                            let cpu_lbl = Label::builder().label(&format!("{}% CPU", cpu)).css_classes(vec!["plan-subnote".to_string()]).build();
                            inner.append(&lbl);
                            inner.append(&cpu_lbl);
                            btn.set_child(Some(&inner));

                            // Watch on Close
                            let w_close = Button::builder().label("A-Close").css_classes(vec!["plan-fmt-btn".to_string()]).build();
                            let s_add_c = s_proc.clone();
                            let p_clone1 = pid.to_string();
                            let n_clone1 = cmd.clone();
                            let entry_clone1 = entry.clone();
                            w_close.connect_clicked(move |_| {
                                s_add_c.borrow_mut().process_jobs.push(ProcessJob { pid: p_clone1.clone(), name: n_clone1.clone(), condition: WatchCondition::OnClose, idle_ticks: 0 });
                                entry_clone1.set_text(""); // Clear search to show active list
                            });

                            // Watch on Idle
                            let entry_clone2 = entry.clone();
                            let w_idle = Button::builder().label("A-Idle").css_classes(vec!["plan-fmt-btn".to_string()]).build();
                            let s_add_i = s_proc.clone();
                            let p_clone2 = pid.to_string();
                            let n_clone2 = cmd.clone();
                            w_idle.connect_clicked(move |_| {
                                s_add_i.borrow_mut().process_jobs.push(ProcessJob { pid: p_clone2.clone(), name: n_clone2.clone(), condition: WatchCondition::OnIdle, idle_ticks: 0 });
                                entry_clone2.set_text(""); 
                            });

                            row.append(&btn);
                            row.append(&w_close);
                            row.append(&w_idle);
                            p_list.append(&row);
                        }
                    }
                }
            }
        }
    });

    // ==========================================
    // 3. BACKGROUND TICKER (Every 1 Second)
    // ==========================================
    let state_ticker = state.clone();
    let tl_list = timers_list.clone();

    glib::timeout_add_local(Duration::from_millis(1000), move || {
        let mut s = state_ticker.borrow_mut();
        
        // 1. Check Standard Timers
        let mut i = 0;
        let now = Instant::now();
        while i < s.timer_jobs.len() {
            if now >= s.timer_jobs[i].end_time {
                let job = s.timer_jobs.remove(i);
                play_alert_sound("Timer Finished", &format!("Your {} timer is up!", job.title));
            } else {
                i += 1;
            }
        }

        // 2. Check Process Watchers
        let mut j = 0;
        while j < s.process_jobs.len() {
            let pid = &s.process_jobs[j].pid;
            let condition = &s.process_jobs[j].condition;
            let mut triggered = false;

            // Check if process still exists in /proc
            let proc_path = format!("/proc/{}", pid);
            if !std::path::Path::new(&proc_path).exists() {
                if *condition == WatchCondition::OnClose {
                    triggered = true;
                    play_alert_sound("Process Closed", &format!("{} has finished running.", s.process_jobs[j].name));
                } else {
                    // If we were waiting for it to go idle but it closed instead, that still counts!
                    triggered = true;
                    play_alert_sound("Process Finished", &format!("{} closed.", s.process_jobs[j].name));
                }
            } else if *condition == WatchCondition::OnIdle {
                // Read CPU usage using `ps`
                if let Ok(output) = Command::new("ps").args(["-p", pid, "-o", "pcpu="]).output() {
                    let cpu_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if let Ok(cpu_val) = cpu_str.parse::<f32>() {
                        if cpu_val < 1.0 { // Less than 1% CPU
                            s.process_jobs[j].idle_ticks += 1;
                            if s.process_jobs[j].idle_ticks >= 3 { // Must be idle for 3 consecutive seconds
                                triggered = true;
                                play_alert_sound("Process Idle", &format!("{} has dropped to 0% CPU (Finished installing/downloading).", s.process_jobs[j].name));
                            }
                        } else {
                            s.process_jobs[j].idle_ticks = 0; // Reset if it spikes again
                        }
                    }
                }
            }

            if triggered {
                s.process_jobs.remove(j);
                // We could emit a signal to refresh the UI here, but it will refresh on next interaction
            } else {
                j += 1;
            }
        }

        // 3. Update Standard Timers UI (if visible)
        if tl_list.is_visible() {
            while let Some(child) = tl_list.first_child() {
                tl_list.remove(&child);
            }
            if s.timer_jobs.is_empty() {
                let empty = Label::builder().label("No active timers.").css_classes(vec!["fav-score-badge".to_string()]).build();
                tl_list.append(&empty);
            } else {
                for (idx, job) in s.timer_jobs.iter().enumerate() {
                    let row = Box::builder().orientation(Orientation::Horizontal).spacing(6).css_classes(vec!["plan-check-row".to_string()]).build();
                    let lbl = Label::builder().label(&job.title).hexpand(true).halign(Align::Start).build();
                    
                    let remaining = job.end_time.saturating_duration_since(now);
                    let time_lbl = Label::builder().label(&format_duration(remaining)).css_classes(vec!["plan-h2".to_string()]).build();
                    
                    let cancel_btn = Button::builder().label("×").css_classes(vec!["plan-line-del-btn".to_string()]).build();
                    let s_del_timer = state_ticker.clone();
                    cancel_btn.connect_clicked(move |_| {
                        s_del_timer.borrow_mut().timer_jobs.remove(idx);
                    });

                    row.append(&lbl);
                    row.append(&time_lbl);
                    row.append(&cancel_btn);
                    tl_list.append(&row);
                }
            }
        }

        glib::ControlFlow::Continue
    });

    app_box
}

pub fn build(_ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    crate::module_api::ModuleHandle::simple(build_timers_ui())
}
