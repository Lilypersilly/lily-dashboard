//! Shared API contract between Lily Dashboard core and drop-in modules in `modules/*.rs`.
//!
//! Every module in `modules/<name>.rs` implements:
//! ```ignore
//! pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle
//! ```
//! and optionally declares metadata comments at the top of the file:
//! ```ignore
//! // @dash-id: my_tab
//! // @dash-title: My Tab
//! // @dash-lifecycle: stateful   // stateless | stateful | visible_only
//! // @dash-hooks: init, shutdown, visibility
//! // Dashpendencies: reqwest = "0.12", chrono = "0.4"
//! ```

use gtk4::prelude::*;
use gtk4::Widget;
use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleLifecycle {
    /// Rebuilt when opened and destroyed when switching away (zero idle RAM).
    Stateless,
    /// Built lazily on first visit and kept alive for the session.
    Stateful,
    /// Built once, with `on_show` / `on_hide` called when entering/leaving the tab.
    VisibleOnly,
}

#[derive(Clone)]
pub struct ModuleContext {
    pub popover_guard: Rc<Cell<usize>>,
    pub close_drawer: Rc<dyn Fn()>,
    pub reload_theme: Rc<dyn Fn()>,
    pub apply_tabs: Rc<dyn Fn()>,
    pub switch_tab: Rc<dyn Fn(&str)>,
}

pub struct ModuleHandle {
    pub widget: Widget,
    /// Optional 400px slide-out detail pane attached to the main drawer (e.g. Plans, System).
    pub detail_pane: Option<Widget>,
    /// Optional callback invoked whenever the user switches TO this tab.
    pub on_show: Option<Rc<dyn Fn()>>,
    /// Optional callback invoked whenever the user switches AWAY from this tab.
    pub on_hide: Option<Rc<dyn Fn()>>,
}

impl ModuleHandle {
    pub fn simple(widget: impl IsA<Widget>) -> Self {
        Self {
            widget: widget.upcast(),
            detail_pane: None,
            on_show: None,
            on_hide: None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct ModuleDescriptor {
    pub id: &'static str,
    pub title: &'static str,
    pub lifecycle: ModuleLifecycle,
    pub build: fn(&ModuleContext) -> ModuleHandle,
    pub init: Option<fn()>,
    pub shutdown: Option<fn()>,
    pub on_visibility: Option<fn(bool)>,
}

// ==========================================
// DECOUPLED SIDEBAR TAB BADGES (e.g. Timers live countdown)
// ==========================================

thread_local! {
    static SIDEBAR_BADGE_CB: std::cell::RefCell<Option<Rc<dyn Fn(&str, Option<&str>)>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn register_sidebar_badge_cb(cb: Rc<dyn Fn(&str, Option<&str>)>) {
    SIDEBAR_BADGE_CB.with(|slot| {
        *slot.borrow_mut() = Some(cb);
    });
}

pub fn set_sidebar_badge(tab_id: &str, badge: Option<&str>) {
    SIDEBAR_BADGE_CB.with(|slot| {
        if let Some(ref cb) = *slot.borrow() {
            cb(tab_id, badge);
        }
    });
}

// ==========================================
// VIEWPORT OFFSCREEN GPU SHADER / SHADOW CULLER
// ==========================================
//
// `GtkBox` inside `GtkScrolledWindow` snapshots ALL children on every scroll frame even when
// 80%+ of rows are hundreds of pixels outside the visible viewport clip. With heavy user CSS
// themes (9-layer gradients, blurred inset/outset box-shadows, infinite @keyframes), offscreen
// widgets otherwise generate 1,000+ redundant GSK render nodes per frame.
//
// `attach_viewport_culler` toggles `.lily-offscreen` (paint-only: strips gradients, shadows,
// and animations without altering widget dimensions or scroll height) on children outside a
// generous ±320px overscan window, checking only on ≥140px scroll steps or content resize.

fn apply_cull_pass(scrolled: &gtk4::ScrolledWindow, content_box: &gtk4::Box) {
    let view_h = scrolled.height() as f32;
    if view_h <= 16.0 {
        return;
    }
    let min_y = -320.0_f32;
    let max_y = view_h + 320.0_f32;

    let cull_one = |w: &Widget| {
        if !w.is_visible() {
            return;
        }
        // Never cull focused widgets so keyboard / Vim navigation stays seamless
        if w.has_focus() || w.focus_child().is_some() {
            if w.has_css_class("lily-offscreen") {
                w.remove_css_class("lily-offscreen");
            }
            return;
        }
        let Some(bounds) = w.compute_bounds(scrolled) else {
            return;
        };
        let h = bounds.height();
        if h <= 0.5 {
            return;
        }
        let y = bounds.y();
        let is_off = (y + h) < min_y || y > max_y;
        if is_off {
            if !w.has_css_class("lily-settled") {
                w.add_css_class("lily-settled");
            }
            if !w.has_css_class("lily-offscreen") {
                w.add_css_class("lily-offscreen");
            }
        } else if w.has_css_class("lily-offscreen") {
            w.remove_css_class("lily-offscreen");
        }
    };

    let mut child = content_box.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        // If a child is a tall FlowBox (e.g. Wallpapers tile grid) or tall inner section Box
        // (e.g. `theme_content_box` or an expanded Settings category), cull its children
        // individually rather than treating the whole wrapper as one tall block!
        if c.height() as f32 > view_h + 360.0
            && (c.clone().downcast::<gtk4::FlowBox>().is_ok()
                || c.clone().downcast::<gtk4::Box>().is_ok())
        {
            if c.has_css_class("lily-offscreen") {
                c.remove_css_class("lily-offscreen");
            }
            let mut sub = c.first_child();
            while let Some(item) = sub {
                let next_item = item.next_sibling();
                if item.height() as f32 > view_h + 360.0
                    && item.clone().downcast::<gtk4::FlowBox>().is_ok()
                {
                    if item.has_css_class("lily-offscreen") {
                        item.remove_css_class("lily-offscreen");
                    }
                    let mut leaf = item.first_child();
                    while let Some(lf) = leaf {
                        let next_lf = lf.next_sibling();
                        cull_one(&lf);
                        leaf = next_lf;
                    }
                } else {
                    cull_one(&item);
                }
                sub = next_item;
            }
            child = next;
            continue;
        }
        cull_one(&c);
        child = next;
    }
}

pub fn refresh_viewport_culler(scrolled: &gtk4::ScrolledWindow, content_box: &gtk4::Box) {
    let sc_w = scrolled.downgrade();
    let bx_w = content_box.downgrade();
    gtk4::glib::idle_add_local_once(move || {
        if let (Some(sc), Some(bx)) = (sc_w.upgrade(), bx_w.upgrade()) {
            apply_cull_pass(&sc, &bx);
        }
    });
}

pub fn attach_viewport_culler(scrolled: &gtk4::ScrolledWindow, content_box: &gtk4::Box) {
    let vadj = scrolled.vadjustment();
    let last_cull_y = Rc::new(Cell::new(-10_000.0_f64));
    let pending_idle = Rc::new(Cell::new(false));

    {
        let sc_w = scrolled.downgrade();
        let bx_w = content_box.downgrade();
        let last_y = last_cull_y.clone();
        vadj.connect_value_changed(move |adj| {
            let val = adj.value();
            if (val - last_y.get()).abs() < 140.0 {
                return;
            }
            last_y.set(val);
            if let (Some(sc), Some(bx)) = (sc_w.upgrade(), bx_w.upgrade()) {
                apply_cull_pass(&sc, &bx);
            }
        });
    }

    let schedule_recheck = {
        let sc_w = scrolled.downgrade();
        let bx_w = content_box.downgrade();
        let last_y = last_cull_y;
        let pend = pending_idle;
        Rc::new(move |adj_val: f64| {
            if pend.get() {
                return;
            }
            pend.set(true);
            let sc_i = sc_w.clone();
            let bx_i = bx_w.clone();
            let ly = last_y.clone();
            let p_done = pend.clone();
            gtk4::glib::idle_add_local_once(move || {
                p_done.set(false);
                ly.set(adj_val);
                if let (Some(sc), Some(bx)) = (sc_i.upgrade(), bx_i.upgrade()) {
                    apply_cull_pass(&sc, &bx);
                }
            });
        })
    };

    {
        let sched = schedule_recheck.clone();
        vadj.connect_upper_notify(move |adj| {
            sched(adj.value());
        });
    }
    {
        let sched = schedule_recheck;
        vadj.connect_page_size_notify(move |adj| {
            sched(adj.value());
        });
    }
}

// ==========================================
// DECOUPLED VAULT BRIDGE (Locker <-> Passwords)
// ==========================================

struct VaultBridge {
    load_or_create: fn() -> Result<String, String>,
    save: fn(&str) -> Result<(), String>,
}

static VAULT_BRIDGE: OnceLock<Mutex<Option<VaultBridge>>> = OnceLock::new();
static RAM_WIPE_HOOKS: OnceLock<Mutex<Vec<fn()>>> = OnceLock::new();
static SOUND_PLAYER_HOOK: OnceLock<Mutex<Option<fn(&str)>>> = OnceLock::new();

pub fn register_sound_player(cb: fn(&str)) {
    let lock = SOUND_PLAYER_HOOK.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        *guard = Some(cb);
    }
}

#[allow(dead_code)]
pub fn play_sound(event: &str) {
    let lock = SOUND_PLAYER_HOOK.get_or_init(|| Mutex::new(None));
    let func = lock.lock().ok().and_then(|g| *g);
    if let Some(f) = func {
        f(event);
    }
}

pub fn register_vault_bridge(
    load_or_create: fn() -> Result<String, String>,
    save: fn(&str) -> Result<(), String>,
) {
    let lock = VAULT_BRIDGE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        *guard = Some(VaultBridge {
            load_or_create,
            save,
        });
    }
}

pub fn load_or_create_vault_passwords() -> Result<String, String> {
    let lock = VAULT_BRIDGE.get_or_init(|| Mutex::new(None));
    let func = lock
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|b| b.load_or_create));
    match func {
        Some(f) => f(),
        None => Err("Locker module is not installed in modules/.".to_string()),
    }
}

pub fn save_vault_passwords(content: &str) -> Result<(), String> {
    let lock = VAULT_BRIDGE.get_or_init(|| Mutex::new(None));
    let func = lock
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|b| b.save));
    match func {
        Some(f) => f(content),
        None => Err("Locker module is not installed in modules/.".to_string()),
    }
}

pub fn register_ram_wipe_hook(hook: fn()) {
    let lock = RAM_WIPE_HOOKS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut hooks) = lock.lock() {
        hooks.push(hook);
    }
}

pub fn trigger_ram_wipe() {
    let lock = RAM_WIPE_HOOKS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(hooks) = lock.lock() {
        for f in hooks.iter() {
            f();
        }
    }
}

// ==========================================
// SHARED QUICK-CAPTURE & COLOR PICKER
// (Works even in an "empty" build with no system.rs)
// ==========================================

static LAST_PICKED_COLOR: Mutex<Option<(u8, u8, u8)>> = Mutex::new(None);

#[allow(dead_code)]
pub fn last_picked_color() -> Option<(u8, u8, u8)> {
    LAST_PICKED_COLOR.lock().ok().and_then(|g| *g)
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum CaptureMode {
    RegionEditor,
    Monitor,
    Full,
    RegionOcr,
}

/// Closes the dashboard drawer, waits for the slide-out animation, then runs
/// the configured screenshot tool (`lumine-capture`, `hyprshot`, `grimblast`, `grim-slurp`, or custom).
pub fn launch_capture(mode: CaptureMode, close_drawer: &Rc<dyn Fn()>) {
    close_drawer();
    let tools = crate::config::get().tools;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(380));
        let shell_cmd: String = match tools.screenshot_tool.as_str() {
            "lumine-capture" => match mode {
                CaptureMode::RegionEditor => "lumine-capture".to_string(),
                CaptureMode::Monitor => "lumine-capture -m".to_string(),
                CaptureMode::Full => "lumine-capture -f".to_string(),
                CaptureMode::RegionOcr => "lumine-capture -r ocr -t c".to_string(),
            },
            "hyprshot" => match mode {
                CaptureMode::RegionEditor | CaptureMode::RegionOcr => {
                    "hyprshot -m region --clipboard-only".to_string()
                }
                CaptureMode::Monitor => "hyprshot -m output --clipboard-only".to_string(),
                CaptureMode::Full => "hyprshot -m output -m active --clipboard-only".to_string(),
            },
            "grimblast" => match mode {
                CaptureMode::RegionEditor | CaptureMode::RegionOcr => {
                    "grimblast copy area".to_string()
                }
                CaptureMode::Monitor => "grimblast copy output".to_string(),
                CaptureMode::Full => "grimblast copy screen".to_string(),
            },
            "grim-slurp" => match mode {
                CaptureMode::RegionEditor | CaptureMode::RegionOcr => {
                    "grim -g \"$(slurp)\" - | wl-copy".to_string()
                }
                CaptureMode::Monitor | CaptureMode::Full => "grim - | wl-copy".to_string(),
            },
            _ => match mode {
                CaptureMode::RegionEditor => tools.screenshot_region_cmd,
                CaptureMode::Monitor => tools.screenshot_monitor_cmd,
                CaptureMode::Full => tools.screenshot_full_cmd,
                CaptureMode::RegionOcr => tools.screenshot_ocr_cmd,
            },
        };
        if !shell_cmd.trim().is_empty() {
            let _ = Command::new("sh").args(["-c", &shell_cmd]).spawn();
        }
    });
}

pub fn launch_color_picker(close_drawer: &Rc<dyn Fn()>) {
    launch_color_picker_with_cb(close_drawer, None);
}

pub fn launch_color_picker_with_cb(
    close_drawer: &Rc<dyn Fn()>,
    on_picked: Option<Box<dyn FnOnce() + Send>>,
) {
    close_drawer();
    let tools = crate::config::get().tools;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(380));

        let picked_rgb: Option<(u8, u8, u8)> = match tools.color_picker_tool.as_str() {
            "hyprpicker" => Command::new("hyprpicker")
                .args(["-a", "-f", "hex"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| parse_hex_u8(&String::from_utf8_lossy(&o.stdout))),
            "custom" => {
                if tools.color_picker_cmd.trim().is_empty() {
                    None
                } else {
                    Command::new("sh")
                        .args(["-c", &tools.color_picker_cmd])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .and_then(|o| parse_hex_u8(&String::from_utf8_lossy(&o.stdout)))
                }
            }
            _ => (|| {
                let slurp_out = Command::new("slurp").arg("-p").output().ok()?;
                if !slurp_out.status.success() {
                    return None;
                }
                let geom = String::from_utf8_lossy(&slurp_out.stdout).trim().to_string();
                if geom.is_empty() {
                    return None;
                }
                let grim_out = Command::new("grim")
                    .args(["-g", &geom, "-t", "ppm", "-"])
                    .output()
                    .ok()?;
                if !grim_out.status.success()
                    || !grim_out.stdout.starts_with(b"P6")
                    || grim_out.stdout.len() < 3
                {
                    return None;
                }
                let len = grim_out.stdout.len();
                Some((
                    grim_out.stdout[len - 3],
                    grim_out.stdout[len - 2],
                    grim_out.stdout[len - 1],
                ))
            })(),
        };

        let Some((r, g, b)) = picked_rgb else {
            return;
        };
        let hex = format!("#{:02x}{:02x}{:02x}", r, g, b);
        let rgb = format!("rgb({}, {}, {})", r, g, b);

        if let Ok(mut guard) = LAST_PICKED_COLOR.lock() {
            *guard = Some((r, g, b));
        }

        if tools.color_picker_tool == "builtin" {
            let _ = Command::new("wl-copy").args(["--", &hex]).status();
            let _ = Command::new("notify-send")
                .args([
                    "-a",
                    "Lily Dashboard",
                    "Color Picked (Copied)",
                    &format!("{}  •  {}", hex, rgb),
                ])
                .spawn();
        }

        if let Some(cb) = on_picked {
            cb();
        }
    });
}

fn parse_hex_u8(raw: &str) -> Option<(u8, u8, u8)> {
    let clean = raw.trim().trim_start_matches('#');
    if clean.len() >= 6 {
        let r = u8::from_str_radix(&clean[0..2], 16).ok()?;
        let g = u8::from_str_radix(&clean[2..4], 16).ok()?;
        let b = u8::from_str_radix(&clean[4..6], 16).ok()?;
        Some((r, g, b))
    } else {
        None
    }
}
