//! Waybar replacements embedded in the dashboard:
//! - Top Hyprland workspace strip with live socket updates and a hover window-switcher
//!   popover (proportional layout mini-map + click to focus / close).
//! - D-Bus `StatusNotifierWatcher` system tray for app icons.
//! - Compact mouse battery (`/sys/class/power_supply`) and SwayNC notification strip
//!   (left-click opens notifications, right-click toggles Do Not Disturb).

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, DrawingArea, EventControllerMotion, EventControllerScroll,
    EventControllerScrollFlags, FlowBox, GestureClick, Image, Label, Orientation, Popover,
    PositionType, SelectionMode,
};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use zbus::dbus_interface;

// ==========================================
// 1. HYPRLAND WORKSPACES STRIP + HOVER WINDOW SWITCHER PREVIEW
// ==========================================

#[derive(Clone, Debug, PartialEq)]
struct WorkspaceItem {
    id: i32,
    name: String,
    active: bool,
}

#[derive(Clone, Debug)]
struct WsWindow {
    address: String,
    class: String,
    title: String,
    at: [f64; 2],
    size: [f64; 2],
    floating: bool,
    focused: bool,
}

#[derive(Clone, Debug)]
struct WsPreviewData {
    ws_name: String,
    mon_name: String,
    mon_x: f64,
    mon_y: f64,
    mon_w: f64,
    mon_h: f64,
    windows: Vec<WsWindow>,
}

fn fetch_workspace_preview(ws_id: i32, ws_name: String) -> WsPreviewData {
    let mut mon_map: std::collections::HashMap<i64, (String, f64, f64, f64, f64)> =
        std::collections::HashMap::new();
    let mut fallback_mon = ("Monitor".to_string(), 0.0, 0.0, 1920.0, 1080.0);

    if let Ok(out) = Command::new("hyprctl").args(["monitors", "-j"]).output() {
        if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) {
            for m in arr {
                let id = m.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
                let name = m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Monitor")
                    .to_string();
                let x = m.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let y = m.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let raw_w = m.get("width").and_then(|v| v.as_f64()).unwrap_or(1920.0);
                let raw_h = m.get("height").and_then(|v| v.as_f64()).unwrap_or(1080.0);
                let scale = m
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.1)
                    .unwrap_or(1.0);
                let transform = m.get("transform").and_then(|v| v.as_i64()).unwrap_or(0);
                // Rotated monitors (90°/270° -> odd transform) swap logical width and height!
                let (w, h) = if transform % 2 == 1 {
                    (raw_h / scale, raw_w / scale)
                } else {
                    (raw_w / scale, raw_h / scale)
                };
                if m.get("focused").and_then(|v| v.as_bool()).unwrap_or(false) {
                    fallback_mon = (name.clone(), x, y, w, h);
                }
                mon_map.insert(id, (name, x, y, w, h));
            }
        }
    }

    let mut chosen_mon = fallback_mon;
    let mut windows = Vec::new();

    if let Ok(out) = Command::new("hyprctl").args(["clients", "-j"]).output() {
        if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) {
            for c in arr {
                let mapped = c.get("mapped").and_then(|v| v.as_bool()).unwrap_or(false);
                let hidden = c.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false);
                if !mapped || hidden {
                    continue;
                }
                let c_ws_id = c
                    .get("workspace")
                    .and_then(|w| w.get("id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-999) as i32;
                if c_ws_id != ws_id {
                    continue;
                }

                let mon_id = c.get("monitor").and_then(|v| v.as_i64()).unwrap_or(-1);
                if let Some(m_info) = mon_map.get(&mon_id) {
                    chosen_mon = m_info.clone();
                }

                let address = c
                    .get("address")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let class = c
                    .get("class")
                    .and_then(|v| v.as_str())
                    .unwrap_or("App")
                    .to_string();
                let title = c
                    .get("title")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&class)
                    .to_string();
                let at_arr = c.get("at").and_then(|v| v.as_array());
                let sz_arr = c.get("size").and_then(|v| v.as_array());
                let ax = at_arr
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let ay = at_arr
                    .and_then(|a| a.get(1))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let sw = sz_arr
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_f64())
                    .unwrap_or(200.0);
                let sh = sz_arr
                    .and_then(|a| a.get(1))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(200.0);
                let floating = c
                    .get("floating")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let focus_hist = c
                    .get("focusHistoryID")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(99);

                windows.push(WsWindow {
                    address,
                    class,
                    title,
                    at: [ax, ay],
                    size: [sw, sh],
                    floating,
                    focused: focus_hist == 0,
                });
            }
        }
    }

    // Draw tiled windows first, then floating windows on top
    windows.sort_by_key(|w| (w.floating, w.focused));

    WsPreviewData {
        ws_name,
        mon_name: chosen_mon.0,
        mon_x: chosen_mon.1,
        mon_y: chosen_mon.2,
        mon_w: chosen_mon.3.max(1.0),
        mon_h: chosen_mon.4.max(1.0),
        windows,
    }
}

fn draw_rounded_rect(cr: &gtk4::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let rad = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - rad, y + rad, rad, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - rad, y + h - rad, rad, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + rad,
        y + h - rad,
        rad,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + rad,
        y + rad,
        rad,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

fn populate_ws_preview_popover(popover: &Popover, data: WsPreviewData) {
    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .width_request(264)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();

    let hdr = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    let title = Label::builder()
        .label(&format!("WORKSPACE {}", data.ws_name.to_uppercase()))
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    let meta = Label::builder()
        .label(&format!(
            "{} • {} win",
            data.mon_name,
            data.windows.len()
        ))
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    hdr.append(&title);
    hdr.append(&meta);
    content.append(&hdr);

    if data.windows.is_empty() {
        let empty = Label::builder()
            .label("Empty workspace — click to switch")
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        content.append(&empty);
        popover.set_child(Some(&content));
        return;
    }

    // Proportional live mini-map of all windows on this workspace (handles landscape & vertical monitors)
    let is_portrait = data.mon_h > data.mon_w;
    let (map_w, map_h) = if is_portrait {
        let mh = 180.0;
        let mw = (mh * (data.mon_w / data.mon_h)).clamp(84.0, 220.0);
        (mw, mh)
    } else {
        let mw = 256.0;
        let mh = (mw * (data.mon_h / data.mon_w)).clamp(96.0, 160.0);
        (mw, mh)
    };

    // Precompute scaled rects safely (never panics even if a floating window is partially offscreen)
    let rects: Rc<Vec<(f64, f64, f64, f64, WsWindow)>> = Rc::new(
        data.windows
            .iter()
            .map(|w| {
                let rx = ((w.at[0] - data.mon_x) / data.mon_w) * map_w;
                let ry = ((w.at[1] - data.mon_y) / data.mon_h) * map_h;
                let rw = (w.size[0] / data.mon_w) * map_w;
                let rh = (w.size[1] / data.mon_h) * map_h;

                let x = rx.max(2.0).min((map_w - 16.0).max(2.0));
                let y = ry.max(2.0).min((map_h - 14.0).max(2.0));
                let avail_w = (map_w - x - 2.0).max(10.0);
                let avail_h = (map_h - y - 2.0).max(10.0);
                let cw = rw.min(avail_w).max(12.0_f64.min(avail_w));
                let ch = rh.min(avail_h).max(10.0_f64.min(avail_h));
                (x, y, cw, ch, w.clone())
            })
            .collect(),
    );

    let area = DrawingArea::builder()
        .content_width(map_w as i32)
        .content_height(map_h as i32)
        .width_request(map_w as i32)
        .height_request(map_h as i32)
        .halign(Align::Center)
        .hexpand(false)
        .css_classes(vec!["ws-preview-map".to_string()])
        .tooltip_text("Click any window preview to jump straight to it")
        .build();

    let rects_draw = rects.clone();
    area.set_draw_func(move |_, cr, w, h| {
        let wf = w as f64;
        let hf = h as f64;

        // Monitor background frame
        draw_rounded_rect(cr, 0.5, 0.5, wf - 1.0, hf - 1.0, 8.0);
        cr.set_source_rgba(0.08, 0.06, 0.09, 0.95);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.914, 0.459, 0.792, 0.45);
        cr.set_line_width(1.0);
        let _ = cr.stroke();

        // Draw each window box proportionally
        for (x, y, cw, ch, win) in rects_draw.iter() {
            let pad = 2.0;
            let bx = x + pad;
            let by = y + pad;
            let bw = (cw - pad * 2.0).max(8.0);
            let bh = (ch - pad * 2.0).max(8.0);

            draw_rounded_rect(cr, bx, by, bw, bh, 5.0);
            if win.focused {
                cr.set_source_rgba(1.0, 0.76, 1.0, 0.28);
            } else if win.floating {
                cr.set_source_rgba(0.75, 0.25, 0.63, 0.30);
            } else {
                cr.set_source_rgba(0.22, 0.16, 0.24, 0.88);
            }
            let _ = cr.fill_preserve();

            if win.focused {
                cr.set_source_rgba(1.0, 0.76, 1.0, 0.95);
                cr.set_line_width(1.6);
            } else {
                cr.set_source_rgba(0.914, 0.459, 0.792, 0.65);
                cr.set_line_width(1.0);
            }
            let _ = cr.stroke();

            // Draw concise app class label inside box if large enough
            if bw >= 28.0 && bh >= 16.0 {
                cr.save().ok();
                draw_rounded_rect(cr, bx + 2.0, by + 2.0, bw - 4.0, bh - 4.0, 3.0);
                cr.clip();
                cr.select_font_face(
                    "Josefin Sans",
                    gtk4::cairo::FontSlant::Normal,
                    gtk4::cairo::FontWeight::Bold,
                );
                cr.set_font_size(if bh >= 32.0 { 10.5 } else { 9.0 });
                cr.set_source_rgba(0.996, 0.894, 1.0, 0.92);
                let short_name: String = win.class.chars().take(16).collect();
                if let Ok(ext) = cr.text_extents(&short_name) {
                    let tx = bx + (bw - ext.width()) / 2.0 - ext.x_bearing();
                    let ty = by + (bh + ext.height()) / 2.0;
                    cr.move_to(tx.max(bx + 4.0), ty);
                    let _ = cr.show_text(&short_name);
                }
                cr.restore().ok();
            }
        }
    });

    // Click inside mini-map to focus that exact window
    let map_click = GestureClick::builder().button(1).build();
    let rects_click = rects.clone();
    let pop_map = popover.downgrade();
    map_click.connect_pressed(move |_, _, cx, cy| {
        // Check in reverse order so top-most / floating windows match first
        for (x, y, cw, ch, win) in rects_click.iter().rev() {
            if cx >= *x && cx <= x + cw && cy >= *y && cy <= y + ch {
                crate::config::hypr_focus_window(&win.address);
                if let Some(p) = pop_map.upgrade() {
                    p.popdown();
                }
                break;
            }
        }
    });
    area.add_controller(map_click);
    content.append(&area);

    // Clickable window list right beneath the mini-map
    let list_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .build();

    for win in data.windows.iter().rev().take(8) {
        let row = GtkBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();

        let mut btn_classes = vec!["fav-item-btn".to_string()];
        if win.focused {
            btn_classes.push("pinned".to_string());
        }
        let win_btn = Button::builder()
            .css_classes(btn_classes)
            .hexpand(true)
            .tooltip_text(&format!("{} — {}", win.class, win.title))
            .build();

        let inner = GtkBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();
        let icon_name = win.class.to_lowercase();
        let icon = Image::builder()
            .icon_name(&icon_name)
            .pixel_size(16)
            .build();
        let lbl = Label::builder()
            .label(&win.title)
            .halign(Align::Start)
            .hexpand(true)
            .width_chars(1)
            .max_width_chars(24)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-body".to_string()])
            .build();
        inner.append(&icon);
        inner.append(&lbl);
        win_btn.set_child(Some(&inner));

        let addr_f = win.address.clone();
        let pop_f = popover.downgrade();
        win_btn.connect_clicked(move |_| {
            crate::config::hypr_focus_window(&addr_f);
            if let Some(p) = pop_f.upgrade() {
                p.popdown();
            }
        });

        let close_btn = Button::builder()
            .label("×")
            .css_classes(vec!["plan-line-del-btn".to_string()])
            .valign(Align::Center)
            .tooltip_text("Close window")
            .build();
        let addr_c = win.address.clone();
        let row_weak = row.downgrade();
        close_btn.connect_clicked(move |_| {
            crate::config::hypr_close_window(&addr_c);
            if let Some(r) = row_weak.upgrade() {
                r.set_visible(false);
            }
        });

        row.append(&win_btn);
        row.append(&close_btn);
        list_box.append(&row);
    }

    content.append(&list_box);
    popover.set_child(Some(&content));
}

fn fetch_workspaces() -> Vec<WorkspaceItem> {
    let active_id = Command::new("hyprctl")
        .args(["activeworkspace", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v.get("id").and_then(|i| i.as_i64()))
        .unwrap_or(1) as i32;

    let mut list = Vec::new();
    if let Ok(out) = Command::new("hyprctl").args(["workspaces", "-j"]).output() {
        if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) {
            for ws in arr {
                let id = ws.get("id").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                if id <= 0 {
                    continue; // Skip special workspaces
                }
                let name = ws
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| id.to_string());
                list.push(WorkspaceItem {
                    id,
                    name,
                    active: id == active_id,
                });
            }
        }
    }

    if list.is_empty() {
        list.push(WorkspaceItem {
            id: active_id.max(1),
            name: active_id.max(1).to_string(),
            active: true,
        });
    }
    list.sort_by_key(|w| w.id);
    list
}

pub fn build_workspaces_widget() -> GtkBox {
    let bar = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .hexpand(true)
        .css_classes(vec!["ws-top-bar".to_string()])
        .build();

    let pills_row = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(5)
        .halign(Align::Start)
        .valign(Align::Center)
        .hexpand(true)
        .build();

    // Scroll anywhere on the top workspace bar to cycle workspaces
    let scroll_ctrl = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    scroll_ctrl.connect_scroll(move |_, _, dy| {
        crate::config::hypr_focus_workspace(if dy > 0.0 { "e+1" } else { "e-1" });
        glib::Propagation::Stop
    });
    bar.add_controller(scroll_ctrl);
    bar.append(&pills_row);

    // Shared hover state so moving mouse from workspace pill into preview popover keeps it open
    let active_popover: Rc<RefCell<Option<Popover>>> = Rc::new(RefCell::new(None));
    let hover_count = Rc::new(Cell::new(0usize));
    let hover_gen = Rc::new(Cell::new(0u64));
    let (tx_prev, rx_prev) = mpsc::channel::<(u64, WsPreviewData)>();
    let pending_anchor: Rc<RefCell<Option<GtkBox>>> = Rc::new(RefCell::new(None));

    // Apply asynchronously fetched workspace preview into a clean popover
    {
        let pop_slot = active_popover.clone();
        let pend_slot = pending_anchor.clone();
        let gen_r = hover_gen.clone();
        let hc_r = hover_count.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let mut latest = None;
            while let Ok(item) = rx_prev.try_recv() {
                latest = Some(item);
            }
            if let Some((req_gen, data)) = latest {
                if req_gen == gen_r.get() && hc_r.get() > 0 {
                    if let Some(anchor_box) = pend_slot.borrow().clone() {
                        if let Some(old) = pop_slot.borrow_mut().take() {
                            old.popdown();
                            old.unparent();
                        }
                        let pop = Popover::builder()
                            .css_classes(vec!["fav-popover".to_string()])
                            .autohide(false)
                            .has_arrow(true)
                            .position(PositionType::Bottom)
                            .build();
                        pop.set_parent(&anchor_box);

                        // Keep popover alive while mouse hovers over it
                        let pop_motion = EventControllerMotion::new();
                        let hc_e = hc_r.clone();
                        pop_motion.connect_enter(move |_, _, _| {
                            hc_e.set(hc_e.get() + 1);
                        });
                        let hc_l = hc_r.clone();
                        let pop_slot_l = pop_slot.clone();
                        pop_motion.connect_leave(move |_| {
                            hc_l.set(hc_l.get().saturating_sub(1));
                            let hc_chk = hc_l.clone();
                            let ps = pop_slot_l.clone();
                            glib::timeout_add_local_once(Duration::from_millis(220), move || {
                                if hc_chk.get() == 0 {
                                    if let Some(p) = ps.borrow_mut().take() {
                                        p.popdown();
                                        p.unparent();
                                    }
                                }
                            });
                        });
                        pop.add_controller(pop_motion);

                        populate_ws_preview_popover(&pop, data);
                        pop.popup();
                        *pop_slot.borrow_mut() = Some(pop);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    let (tx, rx) = mpsc::channel::<Vec<WorkspaceItem>>();

    // Event-driven Hyprland IPC listener for instant workspace updates
    std::thread::spawn(move || {
        let _ = tx.send(fetch_workspaces());

        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default();
        let runtime_dir =
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
        let sock_path = format!("{}/hypr/{}/.socket2.sock", runtime_dir, signature);

        loop {
            if let Ok(stream) = UnixStream::connect(&sock_path) {
                let reader = BufReader::new(stream);
                for line in reader.lines().map_while(Result::ok) {
                    if line.starts_with("workspace>>")
                        || line.starts_with("workspacev2>>")
                        || line.starts_with("createworkspace")
                        || line.starts_with("destroyworkspace")
                        || line.starts_with("focusedmon>>")
                        || line.starts_with("movewindow")
                    {
                        let _ = tx.send(fetch_workspaces());
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    let row_r = pills_row.clone();
    let pop_clear = active_popover.clone();
    let hc_pills = hover_count.clone();
    let gen_pills = hover_gen.clone();
    let pend_pills = pending_anchor.clone();
    let tx_prev_pills = tx_prev.clone();
    let mut last_ws: Vec<WorkspaceItem> = Vec::new();

    glib::timeout_add_local(Duration::from_millis(80), move || {
        let mut latest = None;
        while let Ok(ws) = rx.try_recv() {
            latest = Some(ws);
        }
        if let Some(items) = latest {
            if items != last_ws {
                last_ws = items.clone();
                if let Some(old) = pop_clear.borrow_mut().take() {
                    old.popdown();
                    old.unparent();
                }
                hc_pills.set(0);
                while let Some(child) = row_r.first_child() {
                    row_r.remove(&child);
                }
                for ws in items {
                    // Wrap button in a GtkBox so Popover parents to the Box (never to GtkButton,
                    // which breaks GtkButton click handling in GTK4!).
                    let pill_wrap = GtkBox::builder()
                        .orientation(Orientation::Horizontal)
                        .build();

                    let mut classes = vec!["ws-pill-btn".to_string()];
                    if ws.active {
                        classes.push("active".to_string());
                    }
                    let btn = Button::builder()
                        .label(&ws.name)
                        .css_classes(classes)
                        .build();

                    let ws_num = ws.id;
                    let pop_click = pop_clear.clone();
                    let hc_click = hc_pills.clone();
                    btn.connect_clicked(move |_| {
                        hc_click.set(0);
                        if let Some(old) = pop_click.borrow_mut().take() {
                            old.popdown();
                            old.unparent();
                        }
                        crate::config::hypr_focus_workspace(&ws_num.to_string());
                    });

                    // Hover to show live workspace window preview popover!
                    let motion = EventControllerMotion::new();
                    let ws_id = ws.id;
                    let ws_name = ws.name.clone();
                    let wrap_anchor = pill_wrap.clone();
                    let hc_e = hc_pills.clone();
                    let gen_e = gen_pills.clone();
                    let pend_e = pend_pills.clone();
                    let tx_e = tx_prev_pills.clone();

                    motion.connect_enter(move |_, _, _| {
                        hc_e.set(hc_e.get() + 1);
                        let my_gen = gen_e.get().wrapping_add(1);
                        gen_e.set(my_gen);
                        *pend_e.borrow_mut() = Some(wrap_anchor.clone());

                        let gen_chk = gen_e.clone();
                        let hc_chk = hc_e.clone();
                        let tx_bg = tx_e.clone();
                        let name_bg = ws_name.clone();
                        glib::timeout_add_local_once(Duration::from_millis(120), move || {
                            if gen_chk.get() == my_gen && hc_chk.get() > 0 {
                                std::thread::spawn(move || {
                                    let data = fetch_workspace_preview(ws_id, name_bg);
                                    let _ = tx_bg.send((my_gen, data));
                                });
                            }
                        });
                    });

                    let hc_l = hc_pills.clone();
                    let pop_l = pop_clear.clone();
                    motion.connect_leave(move |_| {
                        hc_l.set(hc_l.get().saturating_sub(1));
                        let hc_chk = hc_l.clone();
                        let ps = pop_l.clone();
                        glib::timeout_add_local_once(Duration::from_millis(220), move || {
                            if hc_chk.get() == 0 {
                                if let Some(p) = ps.borrow_mut().take() {
                                    p.popdown();
                                    p.unparent();
                                }
                            }
                        });
                    });

                    btn.add_controller(motion);
                    pill_wrap.append(&btn);
                    row_r.append(&pill_wrap);
                }
            }
        }
        glib::ControlFlow::Continue
    });

    bar
}

// ==========================================
// 2. COMPACT STATUS STRIP (MOUSE BATTERY + NOTIFICATIONS)
// ==========================================

#[derive(Clone, Debug, PartialEq)]
struct BatteryReading {
    icon: &'static str,
    pct: u8,
    tooltip: String,
    low: bool,
}

fn read_primary_battery() -> Option<BatteryReading> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    let mut mouse: Option<BatteryReading> = None;
    let mut laptop: Option<BatteryReading> = None;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let Ok(cap_str) = fs::read_to_string(path.join("capacity")) else {
            continue;
        };
        let Ok(pct) = cap_str.trim().parse::<u8>() else {
            continue;
        };
        let status = fs::read_to_string(path.join("status"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let model = fs::read_to_string(path.join("model_name"))
            .unwrap_or_else(|_| name.clone())
            .trim()
            .to_string();

        if name.starts_with("hidpp_battery") || model.to_lowercase().contains("mouse") {
            mouse = Some(BatteryReading {
                icon: "󰍽",
                pct,
                tooltip: format!("{}: {}% ({})", model, pct, status),
                low: pct <= 20,
            });
        } else if name.starts_with("BAT") {
            let icon = if status == "Charging" {
                "󰂄"
            } else if pct >= 80 {
                ""
            } else if pct >= 55 {
                ""
            } else if pct >= 30 {
                ""
            } else if pct >= 15 {
                ""
            } else {
                ""
            };
            laptop = Some(BatteryReading {
                icon,
                pct,
                tooltip: format!("{}: {}% ({})", model, pct, status),
                low: pct <= 20 && status != "Charging",
            });
        }
    }

    mouse.or(laptop)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct NotifState {
    unread: u32,
    dnd: bool,
}

pub fn build_status_strip() -> GtkBox {
    let row = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .margin_start(8)
        .margin_end(8)
        .margin_top(2)
        .margin_bottom(2)
        .build();

    // Mouse / Laptop battery indicator
    let bat_lbl = Label::builder()
        .label("")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["sidebar-status-lbl".to_string()])
        .visible(false)
        .build();

    let update_bat = {
        let lbl = bat_lbl.clone();
        move || {
            if let Some(b) = read_primary_battery() {
                lbl.set_label(&format!("{} {}%", b.icon, b.pct));
                lbl.set_tooltip_text(Some(&b.tooltip));
                lbl.set_visible(true);
                if b.low {
                    lbl.add_css_class("bat-low");
                } else {
                    lbl.remove_css_class("bat-low");
                }
            } else {
                lbl.set_visible(false);
            }
        }
    };
    update_bat();
    glib::timeout_add_seconds_local(30, move || {
        update_bat();
        glib::ControlFlow::Continue
    });

    // Notification Center badge (supports swaync, dunst, mako, or disabled via config)
    let notif_daemon = crate::config::get().tools.notification_daemon;
    let notif_lbl = Label::builder()
        .label("󰂚")
        .css_classes(vec!["sidebar-status-lbl".to_string()])
        .build();
    let notif_btn = Button::builder()
        .child(&notif_lbl)
        .halign(Align::End)
        .css_classes(vec!["sidebar-footer-btn".to_string()])
        .tooltip_text("Left-click: Notifications | Right-click: Toggle Do Not Disturb")
        .visible(notif_daemon != "none")
        .build();

    notif_btn.connect_clicked(move |_| {
        match crate::config::get().tools.notification_daemon.as_str() {
            "swaync" => {
                let _ = Command::new("swaync-client").args(["-t", "-sw"]).spawn();
            }
            "dunst" => {
                let _ = Command::new("dunstctl").arg("history-pop").spawn();
            }
            "mako" => {
                let _ = Command::new("makoctl").arg("restore").spawn();
            }
            _ => {}
        }
    });
    let right_click = GestureClick::builder().button(3).build();
    right_click.connect_pressed(move |_, _, _, _| {
        match crate::config::get().tools.notification_daemon.as_str() {
            "swaync" => {
                let _ = Command::new("swaync-client").args(["-d", "-sw"]).spawn();
            }
            "dunst" => {
                let _ = Command::new("dunstctl").args(["set-paused", "toggle"]).spawn();
            }
            "mako" => {
                let _ = Command::new("makoctl").args(["mode", "-t", "do-not-disturb"]).spawn();
            }
            _ => {}
        }
    });
    notif_btn.add_controller(right_click);

    let (tx_n, rx_n) = mpsc::channel::<NotifState>();
    let notif_daemon_bg = notif_daemon.clone();
    std::thread::spawn(move || {
        if notif_daemon_bg != "swaync" {
            return;
        }
        // Stream swaync JSON updates (zero polling!)
        if let Ok(mut child) = Command::new("swaync-client")
            .arg("-swb")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("0");
                        let unread = text.trim().parse::<u32>().unwrap_or(0);
                        let class_str = v.get("class").and_then(|x| x.as_str()).unwrap_or("");
                        let alt_str = v.get("alt").and_then(|x| x.as_str()).unwrap_or("");
                        let dnd = class_str.contains("dnd") || alt_str.contains("dnd");
                        let _ = tx_n.send(NotifState { unread, dnd });
                    }
                }
            }
            let _ = child.wait();
        }
    });

    let notif_lbl_r = notif_lbl.clone();
    let notif_btn_r = notif_btn.clone();
    glib::timeout_add_local(Duration::from_millis(200), move || {
        let mut latest = None;
        while let Ok(st) = rx_n.try_recv() {
            latest = Some(st);
        }
        if let Some(st) = latest {
            let text = if st.dnd {
                if st.unread > 0 {
                    format!("󰂛 {}", st.unread)
                } else {
                    "󰂛".to_string()
                }
            } else if st.unread > 0 {
                format!("󰂚 {}", st.unread)
            } else {
                "󰂚".to_string()
            };
            notif_lbl_r.set_label(&text);
            if st.unread > 0 {
                notif_btn_r.add_css_class("notif-unread");
            } else {
                notif_btn_r.remove_css_class("notif-unread");
            }
        }
        glib::ControlFlow::Continue
    });

    row.append(&bat_lbl);
    row.append(&notif_btn);
    row
}

// ==========================================
// 3. D-BUS SYSTEM TRAY (StatusNotifierWatcher / StatusNotifierItem)
// ==========================================

#[derive(Clone, Debug)]
struct TrayItemInfo {
    service: String,
    path: String,
    title: String,
    icon_name: String,
    icon_rgba: Option<(i32, i32, Vec<u8>)>,
}

struct SniWatcher {
    items: Arc<Mutex<Vec<String>>>,
    notify_tx: mpsc::Sender<()>,
}

#[dbus_interface(name = "org.kde.StatusNotifierWatcher")]
impl SniWatcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: zbus::MessageHeader<'_>,
        #[zbus(signal_context)] ctxt: zbus::SignalContext<'_>,
    ) {
        let entry = if service.starts_with('/') {
            let sender = header
                .sender()
                .ok()
                .flatten()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default();
            if sender.is_empty() {
                return;
            }
            format!("{}{}", sender, service)
        } else if service.contains('/') {
            service.to_string()
        } else {
            format!("{}/StatusNotifierItem", service)
        };

        let mut added = false;
        if let Ok(mut list) = self.items.lock() {
            if !list.contains(&entry) {
                list.push(entry.clone());
                added = true;
            }
        }
        if added {
            let _ = Self::status_notifier_item_registered(&ctxt, &entry).await;
            let _ = self.notify_tx.send(());
        }
    }

    async fn register_status_notifier_host(&self, _service: &str) {}

    #[dbus_interface(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default()
    }

    #[dbus_interface(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[dbus_interface(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[dbus_interface(signal)]
    async fn status_notifier_item_registered(
        ctxt: &zbus::SignalContext<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[dbus_interface(signal)]
    async fn status_notifier_item_unregistered(
        ctxt: &zbus::SignalContext<'_>,
        service: &str,
    ) -> zbus::Result<()>;
}

fn split_sni_service_path(entry: &str) -> (String, String) {
    if let Some((svc, rest)) = entry.split_once('/') {
        (svc.to_string(), format!("/{}", rest))
    } else {
        (entry.to_string(), "/StatusNotifierItem".to_string())
    }
}

fn query_sni_item(conn: &zbus::blocking::Connection, entry: &str) -> Option<TrayItemInfo> {
    let (service, path) = split_sni_service_path(entry);
    let proxy = zbus::blocking::Proxy::new(
        conn,
        service.as_str(),
        path.as_str(),
        "org.kde.StatusNotifierItem",
    )
    .ok()?;

    // Verify item is still alive by fetching Title or Id
    let title: String = proxy
        .get_property::<String>("Title")
        .or_else(|_| proxy.get_property::<String>("Id"))
        .ok()?;
    let icon_name: String = proxy
        .get_property::<String>("IconName")
        .unwrap_or_default();

    let mut icon_rgba = None;
    if let Ok(pixmaps) = proxy.get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap") {
        // Pick pixmap closest to 24px (preferring >= 16px)
        let best = pixmaps
            .into_iter()
            .filter(|(w, h, data)| *w > 0 && *h > 0 && data.len() == (*w as usize * *h as usize * 4))
            .min_by_key(|(w, _, _)| (*w - 24).abs());
        if let Some((w, h, argb)) = best {
            let mut rgba = vec![0u8; argb.len()];
            for chunk_idx in 0..(argb.len() / 4) {
                let i = chunk_idx * 4;
                rgba[i] = argb[i + 1];     // R
                rgba[i + 1] = argb[i + 2]; // G
                rgba[i + 2] = argb[i + 3]; // B
                rgba[i + 3] = argb[i];     // A
            }
            icon_rgba = Some((w, h, rgba));
        }
    }

    drop(proxy);

    Some(TrayItemInfo {
        service,
        path,
        title,
        icon_name,
        icon_rgba,
    })
}

fn fetch_external_watcher_items(conn: &zbus::blocking::Connection) -> Vec<String> {
    let Ok(proxy) = zbus::blocking::Proxy::new(
        conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    ) else {
        return Vec::new();
    };
    proxy
        .get_property::<Vec<String>>("RegisteredStatusNotifierItems")
        .unwrap_or_default()
}

pub fn build_system_tray() -> GtkBox {
    let wrapper = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .margin_start(8)
        .margin_end(8)
        .margin_top(2)
        .margin_bottom(2)
        .visible(false)
        .build();

    let flow = FlowBox::builder()
        .selection_mode(SelectionMode::None)
        .homogeneous(false)
        .max_children_per_line(4)
        .min_children_per_line(1)
        .column_spacing(4)
        .row_spacing(4)
        .halign(Align::Start)
        .build();
    wrapper.append(&flow);

    let (tx_ui, rx_ui) = mpsc::channel::<Vec<TrayItemInfo>>();

    std::thread::spawn(move || {
        let Ok(conn) = zbus::blocking::Connection::session() else {
            return;
        };

        let items_arc: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx_wake, rx_wake) = mpsc::channel::<()>();

        let watcher = SniWatcher {
            items: Arc::clone(&items_arc),
            notify_tx: tx_wake.clone(),
        };
        let _ = conn.object_server().at("/StatusNotifierWatcher", watcher);
        let mut owns_watcher = conn.request_name("org.kde.StatusNotifierWatcher").is_ok();

        let mut last_keys: Vec<String> = Vec::new();

        loop {
            // If Waybar owned the watcher earlier and was killed, take over seamlessly!
            if !owns_watcher {
                if conn.request_name("org.kde.StatusNotifierWatcher").is_ok() {
                    owns_watcher = true;
                }
            }

            let mut candidate_entries: Vec<String> = if owns_watcher {
                items_arc
                    .lock()
                    .map(|l| l.clone())
                    .unwrap_or_default()
            } else {
                fetch_external_watcher_items(&conn)
            };

            // Also deduplicate entries
            let mut seen = HashSet::new();
            candidate_entries.retain(|e| seen.insert(e.clone()));

            let mut alive_entries = Vec::new();
            let mut resolved_items = Vec::new();
            for entry in candidate_entries {
                if let Some(info) = query_sni_item(&conn, &entry) {
                    alive_entries.push(entry);
                    resolved_items.push(info);
                }
            }

            if owns_watcher {
                if let Ok(mut list) = items_arc.lock() {
                    *list = alive_entries.clone();
                }
            }

            let new_keys: Vec<String> = resolved_items
                .iter()
                .map(|i| format!("{}:{}|{}|{}", i.service, i.path, i.title, i.icon_name))
                .collect();

            if new_keys != last_keys {
                last_keys = new_keys;
                let _ = tx_ui.send(resolved_items);
            }

            let _ = rx_wake.recv_timeout(Duration::from_secs(3));
        }
    });

    let wrapper_r = wrapper.clone();
    let flow_r = flow.clone();
    glib::timeout_add_local(Duration::from_millis(250), move || {
        let mut latest = None;
        while let Ok(items) = rx_ui.try_recv() {
            latest = Some(items);
        }
        if let Some(items) = latest {
            while let Some(child) = flow_r.first_child() {
                flow_r.remove(&child);
            }
            wrapper_r.set_visible(!items.is_empty());

            for item in items {
                let img = if let Some((w, h, rgba)) = item.icon_rgba {
                    let bytes = glib::Bytes::from_owned(rgba);
                    let tex = gdk::MemoryTexture::new(
                        w,
                        h,
                        gdk::MemoryFormat::R8g8b8a8,
                        &bytes,
                        (w * 4) as usize,
                    );
                    Image::builder().paintable(&tex).pixel_size(18).build()
                } else if !item.icon_name.is_empty() {
                    Image::builder()
                        .icon_name(&item.icon_name)
                        .pixel_size(18)
                        .build()
                } else {
                    Image::builder()
                        .icon_name("application-x-executable")
                        .pixel_size(18)
                        .build()
                };

                let btn = Button::builder()
                    .child(&img)
                    .css_classes(vec!["tray-icon-btn".to_string()])
                    .tooltip_text(&item.title)
                    .build();

                // Left-click: Activate
                let svc_l = item.service.clone();
                let path_l = item.path.clone();
                btn.connect_clicked(move |_| {
                    let s = svc_l.clone();
                    let p = path_l.clone();
                    std::thread::spawn(move || {
                        if let Ok(conn) = zbus::blocking::Connection::session() {
                            if let Ok(proxy) = zbus::blocking::Proxy::new(
                                &conn,
                                s.as_str(),
                                p.as_str(),
                                "org.kde.StatusNotifierItem",
                            ) {
                                let _: Result<(), _> = proxy.call("Activate", &(0i32, 0i32));
                            }
                        }
                    });
                });

                // Right-click: ContextMenu / SecondaryActivate
                let right_click = GestureClick::builder().button(3).build();
                let svc_r = item.service.clone();
                let path_r = item.path.clone();
                right_click.connect_pressed(move |_, _, _, _| {
                    let s = svc_r.clone();
                    let p = path_r.clone();
                    std::thread::spawn(move || {
                        if let Ok(conn) = zbus::blocking::Connection::session() {
                            if let Ok(proxy) = zbus::blocking::Proxy::new(
                                &conn,
                                s.as_str(),
                                p.as_str(),
                                "org.kde.StatusNotifierItem",
                            ) {
                                let res: Result<(), _> = proxy.call("ContextMenu", &(0i32, 0i32));
                                if res.is_err() {
                                    let _: Result<(), _> =
                                        proxy.call("SecondaryActivate", &(0i32, 0i32));
                                }
                            }
                        }
                    });
                });
                btn.add_controller(right_click);

                flow_r.insert(&btn, -1);
            }
        }
        glib::ControlFlow::Continue
    });

    wrapper
}
