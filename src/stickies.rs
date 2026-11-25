//! Floating sticky notes (`lily-sticky` layer-shell windows) bound to the dashboard.
//!
//! Each sticky supports both typing and freehand Cairo drawing on the same note,
//! header-bar dragging, bottom-right corner resizing, pastel tint cycling, and
//! top-right `×` deletion. Stickies are only visible while the dashboard is open,
//! and persist to `~/.config/lily-dashboard/stickies.json` with a 1s debounced save.

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box, Button, DrawingArea, GestureDrag,
    Orientation, Overlay, PolicyType, ScrolledWindow, TextView, WrapMode,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

/// Fast (~50µs) direct query to Hyprland's IPC socket for global cursor position.
/// Avoids the surface-local coordinate feedback loop when moving/resizing layer-shell windows.
fn hypr_cursor_pos() -> Option<(i32, i32)> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
    let sock_path = format!("{}/hypr/{}/.socket.sock", runtime, sig);
    let mut stream = UnixStream::connect(&sock_path).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
    stream.write_all(b"cursorpos").ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    let (xs, ys) = out.trim().split_once(',')?;
    let x = xs.trim().parse::<i32>().ok()?;
    let y = ys.trim().parse::<i32>().ok()?;
    Some((x, y))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct StickyStroke {
    color: [f64; 3],
    width: f64,
    points: Vec<[f64; 2]>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct StickyNote {
    id: u64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    #[serde(default)]
    tint: usize,
    #[serde(default)]
    text: String,
    #[serde(default)]
    strokes: Vec<StickyStroke>,
}

#[derive(Serialize, Deserialize, Default)]
struct StickiesStore {
    next_id: u64,
    notes: Vec<StickyNote>,
}

struct LiveSticky {
    window: ApplicationWindow,
}

struct StickiesManager {
    app: Application,
    next_id: u64,
    notes: HashMap<u64, StickyNote>,
    windows: HashMap<u64, LiveSticky>,
    dashboard_open: bool,
    save_gen: u64,
}

#[derive(Clone)]
pub struct StickiesHandle {
    on_dashboard_open: Rc<dyn Fn(bool)>,
    on_new_sticky: Rc<dyn Fn()>,
}

impl StickiesHandle {
    pub fn set_dashboard_open(&self, open: bool) {
        (self.on_dashboard_open)(open);
    }

    pub fn new_sticky(&self) {
        (self.on_new_sticky)();
    }
}

const TINT_COUNT: usize = 5;
const INK_COLORS: &[(&str, [f64; 3], &str)] = &[
    ("Cream", [0.996, 0.894, 1.0], "#fee4ff"),
    ("Pink", [0.914, 0.459, 0.792], "#e975ca"),
    ("Blue", [0.537, 0.706, 0.980], "#89b4fa"),
    ("Mint", [0.651, 0.890, 0.631], "#a6e3a1"),
    ("Peach", [0.980, 0.702, 0.529], "#fab387"),
];

fn stickies_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("stickies.json");
    path
}

fn load_store() -> StickiesStore {
    if let Ok(data) = fs::read_to_string(stickies_path()) {
        if let Ok(st) = serde_json::from_str::<StickiesStore>(&data) {
            return st;
        }
    }
    StickiesStore {
        next_id: 1,
        notes: Vec::new(),
    }
}

fn save_store_now(mgr: &StickiesManager) {
    let mut notes: Vec<StickyNote> = mgr.notes.values().cloned().collect();
    notes.sort_by_key(|n| n.id);
    let st = StickiesStore {
        next_id: mgr.next_id,
        notes,
    };
    if let Ok(json) = serde_json::to_string_pretty(&st) {
        let _ = fs::write(stickies_path(), json);
    }
}

fn schedule_debounced_save(mgr_rc: &Rc<RefCell<StickiesManager>>) {
    let gen = {
        let mut mgr = mgr_rc.borrow_mut();
        mgr.save_gen = mgr.save_gen.wrapping_add(1);
        mgr.save_gen
    };
    let mgr_chk = mgr_rc.clone();
    glib::timeout_add_local_once(Duration::from_millis(900), move || {
        let mgr = mgr_chk.borrow();
        if mgr.save_gen == gen {
            save_store_now(&mgr);
        }
    });
}

fn active_gdk_monitor() -> Option<gdk::Monitor> {
    let active_name = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|json| {
            json.as_array()?.iter().find_map(|m| {
                if m.get("focused").and_then(|f| f.as_bool()).unwrap_or(false) {
                    m.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
        });

    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    if let Some(target) = active_name {
        for i in 0..monitors.n_items() {
            if let Some(mon) = monitors.item(i).and_downcast::<gdk::Monitor>() {
                if mon.connector().as_deref() == Some(target.as_str()) {
                    return Some(mon);
                }
            }
        }
    }
    monitors.item(0).and_downcast::<gdk::Monitor>()
}

fn build_sticky_window(
    mgr_rc: &Rc<RefCell<StickiesManager>>,
    note: &StickyNote,
    visible: bool,
) -> ApplicationWindow {
    let app = mgr_rc.borrow().app.clone();
    let note_id = note.id;

    let win = ApplicationWindow::builder()
        .application(&app)
        .title(format!("Lily Sticky {}", note_id))
        .build();

    win.init_layer_shell();
    win.set_namespace("lily-sticky");
    win.set_layer(Layer::Top);
    win.set_keyboard_mode(KeyboardMode::OnDemand);
    win.set_anchor(Edge::Left, true);
    win.set_anchor(Edge::Top, true);
    win.set_anchor(Edge::Right, false);
    win.set_anchor(Edge::Bottom, false);
    win.set_margin(Edge::Left, note.x.max(0));
    win.set_margin(Edge::Top, note.y.max(0));
    win.set_size_request(note.w.max(200), note.h.max(160));
    win.set_default_size(note.w.max(200), note.h.max(160));

    if let Some(mon) = active_gdk_monitor() {
        win.set_monitor(&mon);
    }

    let tint_idx = Rc::new(Cell::new(note.tint % TINT_COUNT));
    let root_card = Box::builder()
        .orientation(Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .css_classes(vec![
            "sticky-card".to_string(),
            format!("sticky-tint-{}", tint_idx.get()),
        ])
        .build();

    // --- DRAGGABLE HEADER BAR ---
    let header = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .css_classes(vec!["sticky-header".to_string()])
        .build();

    let draw_mode = Rc::new(Cell::new(false));
    let mode_btn = Button::builder()
        .label("󰏫 Type")
        .css_classes(vec!["sticky-mini-btn".to_string(), "active".to_string()])
        .tooltip_text("Switch between typing and freehand drawing")
        .build();

    let tint_btn = Button::builder()
        .label("●")
        .css_classes(vec![
            "sticky-mini-btn".to_string(),
            format!("sticky-dot-{}", tint_idx.get()),
        ])
        .tooltip_text("Cycle sticky note tint")
        .build();

    let drag_spacer = Box::builder()
        .hexpand(true)
        .build();
    drag_spacer.set_cursor_from_name(Some("grab"));

    let del_btn = Button::builder()
        .label("×")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .tooltip_text("Delete sticky note")
        .build();

    header.append(&mode_btn);
    header.append(&tint_btn);
    header.append(&drag_spacer);
    header.append(&del_btn);
    root_card.append(&header);

    // Header drag-to-move gesture (uses global Hyprland cursor coords so moving the
    // layer-shell surface never feeds back into surface-local pointer offsets)
    let cur_x = Rc::new(Cell::new(note.x.max(0)));
    let cur_y = Rc::new(Cell::new(note.y.max(0)));
    let move_start_win = Rc::new(Cell::new((note.x.max(0), note.y.max(0))));
    let move_start_cursor: Rc<Cell<Option<(i32, i32)>>> = Rc::new(Cell::new(None));
    let move_drag = GestureDrag::new();
    {
        let sw = move_start_win.clone();
        let sc = move_start_cursor.clone();
        let cx = cur_x.clone();
        let cy = cur_y.clone();
        let ds = drag_spacer.clone();
        move_drag.connect_drag_begin(move |_, _, _| {
            sw.set((cx.get(), cy.get()));
            sc.set(hypr_cursor_pos());
            ds.set_cursor_from_name(Some("grabbing"));
        });
    }
    {
        let win_m = win.clone();
        let cx = cur_x.clone();
        let cy = cur_y.clone();
        let sw = move_start_win.clone();
        let sc = move_start_cursor.clone();
        let mgr_m = mgr_rc.clone();
        move_drag.connect_drag_update(move |_, dx, dy| {
            let (nx, ny) = if let (Some((gx0, gy0)), Some((gx, gy))) = (sc.get(), hypr_cursor_pos()) {
                let (wx0, wy0) = sw.get();
                ((wx0 + (gx - gx0)).clamp(0, 3840), (wy0 + (gy - gy0)).clamp(0, 2160))
            } else {
                let dxi = dx.round() as i32;
                let dyi = dy.round() as i32;
                ((cx.get() + dxi).clamp(0, 3840), (cy.get() + dyi).clamp(0, 2160))
            };
            if nx == cx.get() && ny == cy.get() {
                return;
            }
            cx.set(nx);
            cy.set(ny);
            win_m.set_margin(Edge::Left, nx);
            win_m.set_margin(Edge::Top, ny);
            if let Some(n) = mgr_m.borrow_mut().notes.get_mut(&note_id) {
                n.x = nx;
                n.y = ny;
            }
        });
    }
    {
        let mgr_end = mgr_rc.clone();
        let ds = drag_spacer.clone();
        move_drag.connect_drag_end(move |_, _, _| {
            ds.set_cursor_from_name(Some("grab"));
            schedule_debounced_save(&mgr_end);
        });
    }
    drag_spacer.add_controller(move_drag);

    // Tint cycling
    {
        let card_t = root_card.clone();
        let ti = tint_idx.clone();
        let mgr_t = mgr_rc.clone();
        tint_btn.connect_clicked(move |btn| {
            let old = ti.get();
            let next = (old + 1) % TINT_COUNT;
            ti.set(next);
            card_t.remove_css_class(&format!("sticky-tint-{}", old));
            card_t.add_css_class(&format!("sticky-tint-{}", next));
            btn.remove_css_class(&format!("sticky-dot-{}", old));
            btn.add_css_class(&format!("sticky-dot-{}", next));
            if let Some(n) = mgr_t.borrow_mut().notes.get_mut(&note_id) {
                n.tint = next;
            }
            schedule_debounced_save(&mgr_t);
        });
    }

    // Delete button
    {
        let mgr_d = mgr_rc.clone();
        del_btn.connect_clicked(move |_| {
            let win_to_close = {
                let mut mgr = mgr_d.borrow_mut();
                mgr.notes.remove(&note_id);
                let live = mgr.windows.remove(&note_id);
                save_store_now(&mgr);
                live.map(|l| l.window)
            };
            if let Some(w) = win_to_close {
                w.set_visible(false);
                w.close();
            }
        });
    }

    // --- DRAW TOOLBAR (only shown in Draw mode) ---
    let draw_bar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .margin_start(6)
        .margin_end(6)
        .margin_top(2)
        .margin_bottom(2)
        .visible(false)
        .build();

    let active_ink = Rc::new(Cell::new(0usize));
    let active_width = Rc::new(Cell::new(3.0f64));
    let ink_buttons: Rc<RefCell<Vec<Button>>> = Rc::new(RefCell::new(Vec::new()));

    for (idx, (name, _, _)) in INK_COLORS.iter().enumerate() {
        let mut classes = vec!["sticky-mini-btn".to_string(), format!("sticky-ink-{}", idx)];
        if idx == 0 {
            classes.push("active".to_string());
        }
        let b = Button::builder()
            .label("●")
            .css_classes(classes)
            .tooltip_text(*name)
            .build();
        let ai = active_ink.clone();
        let ibs = ink_buttons.clone();
        b.connect_clicked(move |_| {
            ai.set(idx);
            for (i, other) in ibs.borrow().iter().enumerate() {
                if i == idx {
                    other.add_css_class("active");
                } else {
                    other.remove_css_class("active");
                }
            }
        });
        ink_buttons.borrow_mut().push(b.clone());
        draw_bar.append(&b);
    }

    let size_btn = Button::builder()
        .label("M")
        .css_classes(vec!["sticky-mini-btn".to_string()])
        .tooltip_text("Stroke thickness (S / M / L)")
        .build();
    {
        let aw = active_width.clone();
        size_btn.connect_clicked(move |b| {
            let cur = aw.get();
            let (next_w, lbl) = if cur < 2.5 {
                (3.0, "M")
            } else if cur < 5.0 {
                (6.0, "L")
            } else {
                (1.8, "S")
            };
            aw.set(next_w);
            b.set_label(lbl);
        });
    }

    let undo_btn = Button::builder()
        .label("↩")
        .css_classes(vec!["sticky-mini-btn".to_string()])
        .tooltip_text("Undo last stroke")
        .build();

    let clear_btn = Button::builder()
        .label("Clear")
        .css_classes(vec!["sticky-mini-btn".to_string()])
        .tooltip_text("Clear drawing strokes")
        .build();

    let bar_spacer = Box::builder().hexpand(true).build();
    draw_bar.append(&bar_spacer);
    draw_bar.append(&size_btn);
    draw_bar.append(&undo_btn);
    draw_bar.append(&clear_btn);
    root_card.append(&draw_bar);

    // --- BODY: OVERLAY OF TEXTVIEW + DRAWING CANVAS + CORNER RESIZE GRIP ---
    let body_overlay = Overlay::builder()
        .hexpand(true)
        .vexpand(true)
        .build();

    let text_view = TextView::builder()
        .wrap_mode(WrapMode::WordChar)
        .left_margin(10)
        .right_margin(10)
        .top_margin(6)
        .bottom_margin(18)
        .css_classes(vec!["sticky-text".to_string()])
        .hexpand(true)
        .vexpand(true)
        .build();
    text_view.buffer().set_text(&note.text);

    {
        let mgr_txt = mgr_rc.clone();
        text_view.buffer().connect_changed(move |buf| {
            let (start, end) = buf.bounds();
            let txt = buf.text(&start, &end, false).to_string();
            if let Some(n) = mgr_txt.borrow_mut().notes.get_mut(&note_id) {
                n.text = txt;
            }
            schedule_debounced_save(&mgr_txt);
        });
    }

    let text_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .child(&text_view)
        .build();
    body_overlay.set_child(Some(&text_scroll));

    // Freehand drawing canvas overlaid cleanly on top of text
    let strokes_rc: Rc<RefCell<Vec<StickyStroke>>> = Rc::new(RefCell::new(note.strokes.clone()));
    let canvas = DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .can_target(false) // Starts in Type mode so clicks reach TextView
        .build();
    {
        let st_draw = strokes_rc.clone();
        canvas.set_draw_func(move |_, cr, _, _| {
            cr.set_line_cap(gtk4::cairo::LineCap::Round);
            cr.set_line_join(gtk4::cairo::LineJoin::Round);
            for stroke in st_draw.borrow().iter() {
                if stroke.points.is_empty() {
                    continue;
                }
                cr.set_source_rgba(stroke.color[0], stroke.color[1], stroke.color[2], 0.92);
                cr.set_line_width(stroke.width);
                if stroke.points.len() == 1 {
                    let p = stroke.points[0];
                    cr.arc(p[0], p[1], stroke.width * 0.5, 0.0, std::f64::consts::TAU);
                    let _ = cr.fill();
                } else {
                    let first = stroke.points[0];
                    cr.move_to(first[0], first[1]);
                    for p in stroke.points.iter().skip(1) {
                        cr.line_to(p[0], p[1]);
                    }
                    let _ = cr.stroke();
                }
            }
        });
    }

    // Freehand stroke gesture on canvas (active when in Draw mode)
    let draw_drag = GestureDrag::new();
    let stroke_origin = Rc::new(Cell::new((0.0f64, 0.0f64)));
    {
        let st_b = strokes_rc.clone();
        let ai_b = active_ink.clone();
        let aw_b = active_width.clone();
        let orig_b = stroke_origin.clone();
        let cv_b = canvas.clone();
        draw_drag.connect_drag_begin(move |_, sx, sy| {
            orig_b.set((sx, sy));
            let color = INK_COLORS[ai_b.get() % INK_COLORS.len()].1;
            st_b.borrow_mut().push(StickyStroke {
                color,
                width: aw_b.get(),
                points: vec![[sx, sy]],
            });
            cv_b.queue_draw();
        });
    }
    {
        let st_u = strokes_rc.clone();
        let orig_u = stroke_origin.clone();
        let cv_u = canvas.clone();
        draw_drag.connect_drag_update(move |_, dx, dy| {
            let (sx, sy) = orig_u.get();
            let px = sx + dx;
            let py = sy + dy;
            if let Some(last) = st_u.borrow_mut().last_mut() {
                if let Some(&prev) = last.points.last() {
                    let dist2 = (px - prev[0]).powi(2) + (py - prev[1]).powi(2);
                    if dist2 >= 2.25 {
                        last.points.push([px, py]);
                        cv_u.queue_draw();
                    }
                }
            }
        });
    }
    {
        let st_e = strokes_rc.clone();
        let mgr_e = mgr_rc.clone();
        draw_drag.connect_drag_end(move |_, _, _| {
            if let Some(n) = mgr_e.borrow_mut().notes.get_mut(&note_id) {
                n.strokes = st_e.borrow().clone();
            }
            schedule_debounced_save(&mgr_e);
        });
    }
    canvas.add_controller(draw_drag);
    body_overlay.add_overlay(&canvas);

    // Undo & Clear wiring
    {
        let st_un = strokes_rc.clone();
        let cv_un = canvas.clone();
        let mgr_un = mgr_rc.clone();
        undo_btn.connect_clicked(move |_| {
            st_un.borrow_mut().pop();
            cv_un.queue_draw();
            if let Some(n) = mgr_un.borrow_mut().notes.get_mut(&note_id) {
                n.strokes = st_un.borrow().clone();
            }
            schedule_debounced_save(&mgr_un);
        });
    }
    {
        let st_cl = strokes_rc.clone();
        let cv_cl = canvas.clone();
        let mgr_cl = mgr_rc.clone();
        clear_btn.connect_clicked(move |_| {
            st_cl.borrow_mut().clear();
            cv_cl.queue_draw();
            if let Some(n) = mgr_cl.borrow_mut().notes.get_mut(&note_id) {
                n.strokes.clear();
            }
            schedule_debounced_save(&mgr_cl);
        });
    }

    // Mode toggle (Type <-> Draw)
    {
        let dm = draw_mode.clone();
        let cv = canvas.clone();
        let db = draw_bar.clone();
        let tv = text_view.clone();
        mode_btn.connect_clicked(move |b| {
            let next = !dm.get();
            dm.set(next);
            cv.set_can_target(next);
            db.set_visible(next);
            if next {
                b.set_label("󰃉 Draw");
            } else {
                b.set_label("󰏫 Type");
                tv.grab_focus();
            }
        });
    }

    root_card.append(&body_overlay);

    // Invisible 4-corner resize handles overlaid on the entire sticky card
    let win_overlay = Overlay::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&root_card)
        .build();

    let cur_w = Rc::new(Cell::new(note.w.max(200)));
    let cur_h = Rc::new(Cell::new(note.h.max(160)));

    let corners: &[(Align, Align, i32, &str, bool, bool)] = &[
        (Align::Start, Align::Start, 10, "nwse-resize", true, true),   // Top-Left
        (Align::End, Align::Start, 10, "nesw-resize", false, true),    // Top-Right
        (Align::Start, Align::End, 16, "nesw-resize", true, false),    // Bottom-Left
        (Align::End, Align::End, 16, "nwse-resize", false, false),     // Bottom-Right
    ];

    for &(halign, valign, hit_sz, cursor_name, resize_left, resize_top) in corners {
        let grip = Box::builder()
            .halign(halign)
            .valign(valign)
            .width_request(hit_sz)
            .height_request(hit_sz)
            .build();
        grip.set_cursor_from_name(Some(cursor_name));

        let start_rect = Rc::new(Cell::new((0i32, 0i32, 200i32, 160i32)));
        let start_cursor: Rc<Cell<Option<(i32, i32)>>> = Rc::new(Cell::new(None));
        let resize_drag = GestureDrag::new();
        {
            let sr = start_rect.clone();
            let sc = start_cursor.clone();
            let cx = cur_x.clone();
            let cy = cur_y.clone();
            let cw = cur_w.clone();
            let ch = cur_h.clone();
            resize_drag.connect_drag_begin(move |_, _, _| {
                sr.set((cx.get(), cy.get(), cw.get(), ch.get()));
                sc.set(hypr_cursor_pos());
            });
        }
        {
            let win_r = win.clone();
            let sr = start_rect.clone();
            let sc = start_cursor.clone();
            let cx = cur_x.clone();
            let cy = cur_y.clone();
            let cw = cur_w.clone();
            let ch = cur_h.clone();
            let mgr_r = mgr_rc.clone();
            resize_drag.connect_drag_update(move |_, dx, dy| {
                let (x0, y0, w0, h0) = sr.get();
                let (ddx, ddy) = if let (Some((gx0, gy0)), Some((gx, gy))) = (sc.get(), hypr_cursor_pos()) {
                    (gx - gx0, gy - gy0)
                } else {
                    (dx.round() as i32, dy.round() as i32)
                };

                let (nx, nw) = if resize_left {
                    let nw = (w0 - ddx).clamp(200, 760);
                    let nx = (x0 - (nw - w0)).clamp(0, 3840);
                    (nx, nw)
                } else {
                    (x0, (w0 + ddx).clamp(200, 760))
                };

                let (ny, nh) = if resize_top {
                    let nh = (h0 - ddy).clamp(160, 700);
                    let ny = (y0 - (nh - h0)).clamp(0, 2160);
                    (ny, nh)
                } else {
                    (y0, (h0 + ddy).clamp(160, 700))
                };

                if nx == cx.get() && ny == cy.get() && nw == cw.get() && nh == ch.get() {
                    return;
                }
                if nx != cx.get() {
                    cx.set(nx);
                    win_r.set_margin(Edge::Left, nx);
                }
                if ny != cy.get() {
                    cy.set(ny);
                    win_r.set_margin(Edge::Top, ny);
                }
                if nw != cw.get() || nh != ch.get() {
                    cw.set(nw);
                    ch.set(nh);
                    win_r.set_size_request(nw, nh);
                    win_r.set_default_size(nw, nh);
                }
                if let Some(n) = mgr_r.borrow_mut().notes.get_mut(&note_id) {
                    n.x = nx;
                    n.y = ny;
                    n.w = nw;
                    n.h = nh;
                }
            });
        }
        {
            let mgr_re = mgr_rc.clone();
            resize_drag.connect_drag_end(move |_, _, _| {
                schedule_debounced_save(&mgr_re);
            });
        }
        grip.add_controller(resize_drag);
        win_overlay.add_overlay(&grip);
    }

    win.set_child(Some(&win_overlay));
    win.set_visible(visible);
    win
}

pub fn init_stickies(app: &Application) -> StickiesHandle {
    let store = load_store();
    let mut notes_map = HashMap::new();
    let mut max_id = store.next_id.max(1);
    for n in store.notes {
        max_id = max_id.max(n.id + 1);
        notes_map.insert(n.id, n);
    }

    let mgr_rc = Rc::new(RefCell::new(StickiesManager {
        app: app.clone(),
        next_id: max_id,
        notes: notes_map,
        windows: HashMap::new(),
        dashboard_open: false,
        save_gen: 0,
    }));

    // Pre-build saved sticky windows (hidden until dashboard is drawn out)
    let saved_notes: Vec<StickyNote> = mgr_rc.borrow().notes.values().cloned().collect();
    for note in saved_notes {
        let win = build_sticky_window(&mgr_rc, &note, false);
        mgr_rc
            .borrow_mut()
            .windows
            .insert(note.id, LiveSticky { window: win });
    }

    let on_dashboard_open: Rc<dyn Fn(bool)> = {
        let mgr_o = mgr_rc.clone();
        Rc::new(move |open: bool| {
            let mon = if open { active_gdk_monitor() } else { None };
            // Collect windows and drop the RefMut borrow BEFORE calling set_visible(false),
            // because hiding a window mid-drag synchronously fires GestureDrag::drag_end
            // which borrows mgr_rc mutably to schedule a debounced save.
            let wins: Vec<ApplicationWindow> = {
                let mut mgr = mgr_o.borrow_mut();
                mgr.dashboard_open = open;
                mgr.windows.values().map(|l| l.window.clone()).collect()
            };
            for w in wins {
                if open {
                    if let Some(ref m) = mon {
                        w.set_monitor(m);
                    }
                    w.set_visible(true);
                } else {
                    w.set_visible(false);
                }
            }
        })
    };

    let on_new_sticky: Rc<dyn Fn()> = {
        let mgr_n = mgr_rc.clone();
        Rc::new(move || {
            let (note, is_open) = {
                let mut mgr = mgr_n.borrow_mut();
                let id = mgr.next_id;
                mgr.next_id += 1;
                let count = mgr.notes.len() as i32;
                let note = StickyNote {
                    id,
                    x: 424 + (count % 5) * 28,
                    y: 48 + (count % 5) * 28,
                    w: 270,
                    h: 250,
                    tint: (count as usize) % TINT_COUNT,
                    text: String::new(),
                    strokes: Vec::new(),
                };
                mgr.notes.insert(id, note.clone());
                save_store_now(&mgr);
                (note, mgr.dashboard_open)
            };
            let win = build_sticky_window(&mgr_n, &note, is_open);
            if is_open {
                win.present();
            }
            mgr_n
                .borrow_mut()
                .windows
                .insert(note.id, LiveSticky { window: win });
        })
    };

    StickiesHandle {
        on_dashboard_open,
        on_new_sticky,
    }
}
