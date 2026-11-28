//! Entry point and main layer-shell window for Lily Dashboard.
//!
//! Sets up the left-edge slide-out drawer, dynamic pointer input region (click-through
//! when closed), live CSS reloading from `~/.config/theme-switch/current.conf`,
//! lazy/stateless tab lifecycle management, sidebar footer widgets, and top-bar
//! quick capture/color-picker actions.

mod config;
mod module_api;
mod module_loader;
mod modules_gen;
mod session;
mod sounds;
mod weather;
mod themes;
mod media_popout;
mod stickies;
mod tray;

use gtk4::cairo::{RectangleInt, Region};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box, Button, CssProvider, EventControllerKey,
    EventControllerMotion, GestureClick, Label, Orientation, Overflow, Stack, gdk, glib,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "com.lily.dashboard";
const BASE_DASH_WIDTH: i32 = 400;
const EXTENDED_PANE_WIDTH: i32 = 400;
const TOTAL_DASH_WIDTH: i32 = BASE_DASH_WIDTH + EXTENDED_PANE_WIDTH;
const TRIGGER_WIDTH: i32 = 56;

#[derive(Clone)]
struct DashHandle {
    trigger_btn: Button,
    is_open: Rc<Cell<bool>>,
    stack: Stack,
    reload_theme: Rc<dyn Fn()>,
}

fn main() {
    // Let Hyprland's native GPU layer_rule blur handle our surfaces instead of
    // GTK 4.24's ext-background-effect-v1 / CSS backdrop-filter (which lags during slide).
    let disable_iface = "ext_background_effect_manager_v1";
    let new_val = match std::env::var("GDK_WAYLAND_DISABLE") {
        Ok(existing) if !existing.is_empty() => {
            if existing.split(',').any(|s| s.trim() == disable_iface) {
                existing
            } else {
                format!("{},{}", existing, disable_iface)
            }
        }
        _ => disable_iface.to_string(),
    };
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("GDK_WAYLAND_DISABLE", new_val);
    }

    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gtk4::gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let reload_theme_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let reload_theme_startup = reload_theme_slot.clone();
    app.connect_startup(move |_| {
        config::ensure_bundled_fonts();
        config::ensure_builtin_styles();
        sounds::reload_active_pack();
        module_api::register_sound_player(sounds::play_by_name);
        let reload_cb = init_theme();
        *reload_theme_startup.borrow_mut() = Some(reload_cb);
    });
    app.connect_shutdown(|_| {
        for m in modules_gen::all_modules() {
            if let Some(shut) = m.shutdown {
                shut();
            }
        }
    });

    // Store the dashboard handle so re-running `lily-dashboard` (or `lily-dashboard --tab clipboard`)
    // controls the already-running instance over D-Bus instead of spawning a second window!
    let existing_handle: Rc<RefCell<Option<DashHandle>>> = Rc::new(RefCell::new(None));
    let ensure_initialized: Rc<dyn Fn(&Application) -> DashHandle> = {
        let handle_slot = existing_handle.clone();
        let reload_theme_act = reload_theme_slot.clone();
        Rc::new(move |app: &Application| {
            if let Some(h) = handle_slot.borrow().as_ref() {
                return h.clone();
            }

            // Apply saved Hyprland live overrides on startup if the user opted in
            let startup_cfg = config::get();
            if startup_cfg.hyprland.apply_on_startup {
                config::apply_hyprland_settings(&startup_cfg.hyprland);
            } else if startup_cfg.hyprland.active_border_mode != "config" {
                config::apply_hyprland_border_config(&startup_cfg.hyprland);
            }

            session::init_session_tracker();
            for m in modules_gen::all_modules() {
                if let Some(init_fn) = m.init {
                    init_fn();
                }
            }
            let media_handle = media_popout::init_media_popout(app);
            let stickies_handle = stickies::init_stickies(app);

            let reload_theme = reload_theme_act
                .borrow()
                .clone()
                .unwrap_or_else(|| Rc::new(|| {}));
            let h = build_ui(app, media_handle, stickies_handle, reload_theme);
            *handle_slot.borrow_mut() = Some(h.clone());
            h
        })
    };

    {
        let ensure_a = ensure_initialized.clone();
        let slot_a = existing_handle.clone();
        app.connect_activate(move |app| {
            let was_running = slot_a.borrow().is_some();
            let h = ensure_a(app);
            if was_running {
                h.trigger_btn.emit_clicked();
            }
        });
    }

    {
        let ensure_c = ensure_initialized.clone();
        let slot_c = existing_handle.clone();
        app.connect_command_line(move |app, cmdline| {
            let args: Vec<String> = cmdline
                .arguments()
                .into_iter()
                .skip(1)
                .map(|s| s.to_string_lossy().to_string())
                .collect();

            if args.iter().any(|a| a == "--help" || a == "-h") {
                println!(
                    "lily-dashboard — Wayland slide-out sidebar for Hyprland\n\n\
                     USAGE:\n  lily-dashboard [OPTIONS]\n\n\
                     OPTIONS:\n\
                       --toggle             Toggle dashboard open/closed (default when already running)\n\
                       --open               Open the dashboard drawer\n\
                       --close              Close the dashboard drawer\n\
                       --tab <id>           Open directly to a tab (or toggle closed if already on it)\n\
                                            IDs: run, favs, clipboard, plans, calendar, timers,\n\
                                                 music, drive, passwords, locker, system\n\
                       --theme <key>        Apply a theme globally by key or name\n\
                       --dash-only          With --theme, apply to Lily Dashboard only\n\
                       -h, --help           Show this help"
                );
                return 0.into();
            }

            let was_running = slot_c.borrow().is_some();
            let h = ensure_c(app);

            let mut target_tab: Option<String> = None;
            let mut target_theme: Option<String> = None;
            let mut dash_only = false;
            let mut want_open = false;
            let mut want_close = false;
            let mut want_toggle = false;

            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--tab" | "-t" => {
                        if let Some(v) = args.get(i + 1) {
                            target_tab = Some(v.to_lowercase());
                            i += 1;
                        }
                    }
                    "--theme" => {
                        if let Some(v) = args.get(i + 1) {
                            target_theme = Some(v.clone());
                            i += 1;
                        }
                    }
                    "--dash-only" => dash_only = true,
                    "--open" => want_open = true,
                    "--close" => want_close = true,
                    "--toggle" => want_toggle = true,
                    _ => {}
                }
                i += 1;
            }

            if let Some(t_query) = target_theme {
                let q = t_query.to_lowercase();
                if let Some(def) = themes::list_all_themes()
                    .into_iter()
                    .find(|t| t.key.to_lowercase() == q || t.name.to_lowercase() == q)
                {
                    if dash_only {
                        themes::apply_dash_only(&def);
                        (h.reload_theme)();
                    } else {
                        let rel = h.reload_theme.clone();
                        let (tx, rx) = std::sync::mpsc::channel::<()>();
                        std::thread::spawn(move || {
                            let _ = themes::apply_global(&def);
                            let _ = tx.send(());
                        });
                        glib::timeout_add_local(Duration::from_millis(80), move || {
                            if rx.try_recv().is_ok() {
                                rel();
                                glib::ControlFlow::Break
                            } else {
                                glib::ControlFlow::Continue
                            }
                        });
                    }
                }
                return 0.into();
            }

            if let Some(tab) = target_tab {
                let norm = match tab.as_str() {
                    "favorites" => "favs",
                    other => other,
                };
                let already_open = h.is_open.get();
                let cur_tab = h.stack.visible_child_name().map(|s| s.to_string());
                if already_open && cur_tab.as_deref() == Some(norm) {
                    h.trigger_btn.emit_clicked();
                } else {
                    h.stack.set_visible_child_name(norm);
                    if !already_open {
                        h.trigger_btn.emit_clicked();
                    }
                }
                return 0.into();
            }

            if want_open {
                if !h.is_open.get() {
                    h.trigger_btn.emit_clicked();
                }
            } else if want_close {
                if h.is_open.get() {
                    h.trigger_btn.emit_clicked();
                }
            } else if want_toggle || was_running {
                h.trigger_btn.emit_clicked();
            }

            0.into()
        });
    }

    app.run();
}

/// Scopes top-level CSS rules from a homemade `.css` theme under `scope_prefix` (e.g.
/// `.css-scope-cyber-scanner`) so its button in Settings -> CSS STYLE renders a live preview
/// of itself even while another CSS style is active.
fn scope_css_for_preview(scope_prefix: &str, raw_css: &str) -> String {
    // Strip /* ... */ block comments first
    let mut cleaned = String::with_capacity(raw_css.len());
    let bytes = raw_css.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            cleaned.push(bytes[i] as char);
            i += 1;
        }
    }

    let mut out = String::new();
    let mut rest = cleaned.as_str();
    while let Some(open_idx) = rest.find('{') {
        let header = rest[..open_idx].trim();
        let after_open = &rest[open_idx + 1..];
        // Find matching closing brace (handles nested @keyframes blocks cleanly)
        let mut depth = 1usize;
        let mut close_idx = None;
        for (idx, ch) in after_open.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close_idx = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(c_idx) = close_idx else {
            break;
        };
        let body = &after_open[..c_idx];
        rest = &after_open[c_idx + 1..];

        // Skip @define-color lines that may precede the selector
        let sel_header = header
            .rsplit(';')
            .next()
            .unwrap_or(header)
            .trim();
        if sel_header.is_empty() {
            continue;
        }
        if sel_header.starts_with("@keyframes") {
            out.push_str(&format!("{} {{{}}}\n", sel_header, body));
            continue;
        }
        if sel_header.starts_with('@') {
            continue;
        }
        // Only keep rules that can actually style the preview Button (`button`, `.plan-fmt-btn`, `label`),
        // skipping heavy container/control rules (`stack`, `viewport`, `entry`, `scale`, etc.) so custom
        // theme previews don't bloat global selector matching.
        let scoped: Vec<String> = sel_header
            .split(',')
            .map(|s| s.trim())
            .filter(|s| {
                !s.is_empty()
                    && (s.contains("plan-fmt-btn")
                        || s.starts_with("button")
                        || s.starts_with("label"))
            })
            .map(|s| format!("{} {}", scope_prefix, s))
            .collect();
        if !scoped.is_empty() {
            // Strip `infinite` from preview button animations so inactive theme cards in Settings
            // never keep ticking infinite shadow/gradient @keyframes in the background!
            let safe_body: String = body
                .split(';')
                .map(|decl| {
                    let d = decl.trim();
                    if d.starts_with("animation") && d.contains("infinite") {
                        ""
                    } else {
                        decl
                    }
                })
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join(";");
            out.push_str(&format!("{} {{{};}}\n", scoped.join(", "), safe_body));
        }
    }
    out
}

fn init_theme() -> Rc<dyn Fn()> {
    let display = gdk::Display::default().expect("Could not connect to a display.");
    let provider = CssProvider::new();
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Separate provider above STYLE_PROVIDER_PRIORITY_USER so our hairline scrollbars
    // beat ~/.config/gtk-4.0/gtk.css without interfering with dynamic_offset_provider
    let scrollbar_provider = CssProvider::new();
    gtk4::style_context_add_provider_for_display(
        &display,
        &scrollbar_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER + 1,
    );

    // User-editable CSS style theme provider (`~/.config/lily-dashboard/themes/css/<name>.css`)
    let user_css_provider = CssProvider::new();
    gtk4::style_context_add_provider_for_display(
        &display,
        &user_css_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER + 2,
    );

    // Structural performance guard provider above user CSS:
    // 1. Keeps internal non-scrolling layout-clamp wrappers (`sidebar_clamp`, `stack_clamp`)
    //    transparent so `viewport` rules in user CSS don't render 2× redundant full-screen shaders.
    // 2. Freezes animations & transitions inside `.dash-hibernated` while the drawer is closed
    //    offscreen so infinite `@keyframes` cost 0% GPU/CPU when closed.
    // 3. Strips offscreen gradients/shadows/animations on `.lily-offscreen` rows (>320px outside
    //    scroll viewports) without changing widget dimensions or scroll height.
    let perf_guard_provider = CssProvider::new();
    perf_guard_provider.load_from_data(
        r#"
        scrolledwindow.internal-layout-clamp,
        scrolledwindow.internal-layout-clamp > viewport {
            background: none;
            background-color: transparent;
            background-image: none;
            border: none;
            box-shadow: none;
        }
        .dash-hibernated,
        .dash-hibernated * {
            animation: none;
            transition: none;
        }
        .lily-offscreen,
        .lily-offscreen * {
            background-image: none;
            box-shadow: none;
            text-shadow: none;
            animation: none;
            transition: none;
        }
        "#,
    );
    gtk4::style_context_add_provider_for_display(
        &display,
        &perf_guard_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER + 3,
    );

    let update_css: Rc<dyn Fn()> = {
        let provider = provider.clone();
        let scrollbar_provider = scrollbar_provider.clone();
        let user_css_provider = user_css_provider.clone();
        Rc::new(move || {
            let cfg = config::get();
            let map = config::resolve_theme_colors(&cfg);

            macro_rules! get_c {
                ($k:expr, $fallback:expr) => {
                    map.get($k).map(|s| s.as_str()).unwrap_or($fallback)
                };
            }
            
            // Map the generic theme variables to the dashboard's semantic roles
            let bg_main = get_c!("bg_main", "1c161e");
            let bg_pill = get_c!("bg_pill", get_c!("bg_dark", "2a202d"));
            let bg_recessed = get_c!("bg_recessed", get_c!("bg_dark", "141016"));
            let bg_contrast = get_c!("bg_contrast", get_c!("bg_dark", "110d13"));
            
            let fg_main = get_c!("fg_main", "fee4ff");
            let fg_muted = get_c!("fg_muted", get_c!("fg_dim", "a884a6"));
            
            let accent1 = get_c!("accent", get_c!("accent1", "e975ca"));
            let accent2 = get_c!("magenta", get_c!("accent2", "ffc2ff"));
            let selection = get_c!("selection", get_c!("accent1", "bf40a1"));
            
            let hex_to_rgba = |hex: &str, alpha: f32| -> String {
                let clean = hex.trim_start_matches('#');
                if clean.len() >= 6 {
                    if let (Ok(r), Ok(g), Ok(b)) = (
                        u8::from_str_radix(&clean[0..2], 16),
                        u8::from_str_radix(&clean[2..4], 16),
                        u8::from_str_radix(&clean[4..6], 16),
                    ) {
                        return format!("rgba({}, {}, {}, {:.2})", r, g, b, alpha);
                    }
                }
                format!("rgba(128, 128, 128, {:.2})", alpha)
            };

            let glass_a = cfg.hyprland.glass_opacity.clamp(0.30, 1.0) as f32;
            let recessed_a = (glass_a + 0.07).min(1.0);
            let bg_main_glass = hex_to_rgba(bg_main, glass_a);
            let bg_recessed_glass = hex_to_rgba(bg_recessed, recessed_a);
            let bg_pill_glass = hex_to_rgba(bg_pill, recessed_a);
            let accent1_15 = hex_to_rgba(accent1, 0.15);
            let accent1_24 = hex_to_rgba(accent1, 0.24);
            let accent1_35 = hex_to_rgba(accent1, 0.35);
            let accent1_38 = hex_to_rgba(accent1, 0.38);
            let accent1_55 = hex_to_rgba(accent1, 0.55);
            let accent2_28 = hex_to_rgba(accent2, 0.28);
            let accent2_70 = hex_to_rgba(accent2, 0.70);
            let fg_main_04 = hex_to_rgba(fg_main, 0.04);
            let fg_main_06 = hex_to_rgba(fg_main, 0.06);
            let fg_main_08 = hex_to_rgba(fg_main, 0.08);
            let fg_main_15 = hex_to_rgba(fg_main, 0.15);
            let bg_contrast_65 = hex_to_rgba(bg_contrast, 0.65);
            let bg_contrast_88 = hex_to_rgba(bg_contrast, 0.88);

            // Mathematically normalized corner rounding (t in [0.0, 1.0]):
            // 1.0 = exact current max radii (matching Hyprland's max rounding proportionally)
            // 0.0 = 0px sharp corners everywhere
            let r_scale = cfg.hyprland.rounding_scale.clamp(0.0, 1.0);
            let sr = |max_px: f64| -> i32 { (max_px * r_scale).round() as i32 };
            let r_outer = sr(16.0);
            let r_lg = sr(12.0);
            let r_md = sr(10.0);
            let r_sm = sr(9.0);
            let r_xs = sr(8.0);
            let r_xxs = sr(7.0);
            let r_bar = sr(6.0);
            let r_chk = sr(5.0);
            let r_sliv = sr(4.0);

            let ui_font = if cfg.ui_font.trim().is_empty() {
                "Josefin Sans".to_string()
            } else {
                cfg.ui_font.trim().replace('\'', "")
            };
            let mono_font = if cfg.mono_font.trim().is_empty() {
                "MesloLGL Nerd Font".to_string()
            } else {
                cfg.mono_font.trim().replace('\'', "")
            };

            let fs_scale = cfg.font_scale.clamp(0.75, 1.35);
            let sf = |base: f64| -> i32 { (base * fs_scale).round().max(8.0) as i32 };
            let fs_11 = sf(11.0);
            let fs_12 = sf(12.0);
            let fs_13 = sf(13.0);
            let fs_14 = sf(14.0);
            let fs_15 = sf(15.0);
            let fs_16 = sf(16.0);
            let fs_19 = sf(19.0);

            let is_right = cfg.layout.side == "right";
            let anim_ms = cfg.layout.anim_duration_ms;
            let trig_h = match cfg.layout.trigger_size.as_str() {
                "small" => 72,
                "large" => 140,
                _ => 100,
            };
            let dash_edge_css = if is_right {
                format!("border-right: none; border-top-left-radius: {r_outer}px; border-bottom-left-radius: {r_outer}px;")
            } else {
                format!("border-left: none; border-top-right-radius: {r_outer}px; border-bottom-right-radius: {r_outer}px;")
            };
            let trig_edge_css = if is_right {
                format!("border-right: none; border-top-left-radius: 100px; border-bottom-left-radius: 100px; min-height: {trig_h}px; transform: translateX(36px);")
            } else {
                format!("border-left: none; border-top-right-radius: 100px; border-bottom-right-radius: 100px; min-height: {trig_h}px; transform: translateX(-36px);")
            };
            let closed_tx = if is_right { "415px" } else { "-415px" };
            let panel_min_w = (cfg.layout.panel_width.clamp(340, 560) - 2).max(338);

            let sidebar_border_css = if is_right {
                format!("border-left: 1px solid #{accent1}; border-right: none;")
            } else {
                format!("border-right: 1px solid #{accent1}; border-left: none;")
            };
            let detail_border_css = if is_right {
                format!("border-right: 2px solid #{selection}; border-left: none;")
            } else {
                format!("border-left: 2px solid #{selection}; border-right: none;")
            };

            let css = format!(
                r#"
                window {{ background-color: transparent; }}
                .drawer-container {{ transform: translateX({closed_tx}); transition: transform {anim_ms}ms cubic-bezier(0.25, 0.8, 0.25, 1); }}
                .drawer-container.open {{ transform: translateX(0px); }}
                
                .dash-box {{ background-color: {bg_main_glass}; border: 2px solid #{accent1}; {dash_edge_css} min-width: {panel_min_w}px; }}
                .sidebar-column {{ background-color: {bg_recessed_glass}; {sidebar_border_css} }}
                
                .sidebar-nav-btn {{ background-color: transparent; border: none; border-radius: {r_md}px; padding: 6px 10px; margin: 1px 4px; box-shadow: none; transition: background-color 0.18s ease, color 0.18s ease; }}
                .sidebar-nav-btn:hover {{ background-color: {accent1_15}; }}
                .sidebar-nav-btn.selected {{ background-color: {fg_main_06}; }}
                .sidebar-nav-btn label {{ color: #{fg_muted}; font-family: '{ui_font}', sans-serif; font-size: {fs_15}px; font-weight: bold; }}
                .sidebar-nav-btn.selected label {{ color: #{fg_main}; }}
                .sidebar-nav-btn label.sidebar-tab-badge {{ color: #{accent2}; font-family: '{mono_font}', monospace; font-size: {fs_11}px; font-weight: bold; }}
                .sidebar-sep {{ background-color: {accent1_35}; min-height: 1px; margin: 4px 10px; }}
                .sidebar-folder-btn {{ background-color: transparent; border: none; border-radius: {r_md}px; padding: 5px 8px; margin: 2px 4px 1px 4px; box-shadow: none; transition: background-color 0.16s ease; }}
                .sidebar-folder-btn:hover {{ background-color: {accent1_15}; }}
                .sidebar-folder-btn label {{ color: #{accent2}; font-family: '{ui_font}', sans-serif; font-size: {fs_13}px; font-weight: bold; letter-spacing: 0.3px; }}
                .sidebar-folder-btn.has-active {{ background-color: {accent1_15}; }}
                .sidebar-folder-btn.has-active label {{ color: #{fg_main}; }}
                .sidebar-folder-children {{ margin-left: 9px; border-left: 1.5px solid {accent1_35}; padding-left: 2px; margin-bottom: 2px; }}
                .sidebar-folder-children .sidebar-nav-btn {{ padding: 5px 8px; margin: 1px 4px 1px 2px; }}
                .sidebar-folder-children .sidebar-nav-btn label {{ font-size: {fs_14}px; }}
                
                .fav-item-btn, .music-item-album, .music-item-playlist {{ background-color: transparent; border: none; border-radius: {r_lg}px; padding: 6px 8px; transition: background-color 0.15s ease; box-shadow: none; }}
                .fav-item-btn:hover, .music-item-album:hover, .music-item-playlist:hover {{ background-color: {accent1_15}; }}
                .fav-item-btn.pinned, .music-item-album.pinned, .music-item-playlist.pinned {{ background-color: #{selection}; border: none; }}
                
                .music-art-frame {{ background-color: transparent; border: none; border-radius: {r_xs}px; min-width: 36px; min-height: 36px; box-shadow: 0px 2px 5px rgba(0, 0, 0, 0.15); }}
                .music-kind-tab-song {{ background-color: #89b4fa; border-radius: 99px; min-width: 3px; min-height: 28px; }}
                .music-kind-tab-playlist {{ background-color: #fab387; border-radius: 99px; min-width: 3px; min-height: 28px; }}
                .music-kind-tab-album {{ background-color: #a6e3a1; border-radius: 99px; min-width: 3px; min-height: 28px; }}
                
                .fav-search {{ background-color: {bg_contrast_65}; border: 1px solid transparent; border-radius: {r_md}px; color: #{fg_main}; font-family: '{ui_font}', sans-serif; font-size: {fs_14}px; padding: 7px 12px; transition: background-color 0.16s ease, border-color 0.16s ease; }}
                .fav-search:focus {{ background-color: {bg_contrast_88}; border-color: {accent1_55}; }}
                
                .fav-group-header {{ color: #{accent2}; font-weight: bold; font-size: {fs_13}px; font-family: '{mono_font}', monospace; margin-left: 4px; letter-spacing: 1px; }}
                .fav-score-badge {{ color: #{accent2}; font-family: '{mono_font}', monospace; font-size: {fs_12}px; font-weight: bold; }}
                
                .plan-fmt-btn, .plan-close-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 6px 11px; color: #{fg_main}; font-family: '{ui_font}', '{mono_font}', sans-serif; font-weight: bold; box-shadow: none; transition: background-color 0.16s ease, color 0.16s ease; }}
                .plan-fmt-btn label, .plan-close-btn label {{ color: #{fg_main}; font-family: '{ui_font}', '{mono_font}', sans-serif; font-size: {fs_14}px; font-weight: bold; }}
                .plan-fmt-btn:hover, .plan-close-btn:hover {{ background-color: {fg_main_15}; }}
                .plan-fmt-btn.active, .plan-close-btn.active {{ background-color: {accent1_24}; border-color: transparent; color: #{accent2}; }}
                .plan-fmt-btn.active label, .plan-close-btn.active label {{ color: #{accent2}; }}
                .plan-fmt-btn.active:hover, .plan-close-btn.active:hover {{ background-color: {accent1_38}; }}

                .primary-btn {{ background-color: {accent1_24}; border: 1px solid transparent; border-radius: {r_md}px; padding: 7px 14px; color: #{accent2}; font-family: '{ui_font}', '{mono_font}', sans-serif; font-weight: bold; box-shadow: none; transition: background-color 0.16s ease, color 0.16s ease; }}
                .primary-btn label {{ color: #{accent2}; font-family: '{ui_font}', '{mono_font}', sans-serif; font-size: {fs_14}px; font-weight: bold; }}
                .primary-btn:hover {{ background-color: {accent1_38}; color: #{fg_main}; }}
                .primary-btn:hover label {{ color: #{fg_main}; }}
                .primary-btn.active {{ background-color: rgba(243, 139, 168, 0.22); color: #f38ba8; }}
                .primary-btn.active label {{ color: #f38ba8; }}
                .primary-btn.active:hover {{ background-color: rgba(243, 139, 168, 0.34); }}
                
                .session-restore-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 7px 6px; margin: 4px 8px; box-shadow: none; transition: background-color 0.16s ease; }}
                .session-restore-btn:hover {{ background-color: {fg_main_15}; }}
                .session-restore-lbl {{ color: #{accent2}; font-size: {fs_12}px; font-weight: bold; }}

                .power-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 5px 1px; min-width: 0; min-height: 28px; box-shadow: none; transition: background-color 0.16s ease; }}
                .power-btn:hover {{ background-color: {fg_main_15}; }}
                .power-btn-lbl {{ color: #{accent2}; font-family: '{mono_font}', '{ui_font}', sans-serif; font-size: {fs_14}px; font-weight: bold; }}
                .power-btn.active {{ background-color: {accent1_24}; }}
                .power-btn.active:hover {{ background-color: {accent1_38}; }}
                .power-btn.armed {{ background-color: rgba(243, 139, 168, 0.24); border-color: transparent; }}
                .power-btn.armed .power-btn-lbl {{ color: #f38ba8; font-family: '{ui_font}', sans-serif; font-size: {fs_11}px; }}

                .sidebar-footer-btn {{ background-color: transparent; border: none; border-radius: {r_bar}px; padding: 2px 3px; margin: 0px; min-height: 0px; min-width: 0px; box-shadow: none; transition: background-color 0.15s ease; }}
                .sidebar-footer-btn:hover {{ background-color: {accent1_15}; }}
                .sidebar-weather-lbl {{ color: #{accent2}; font-family: '{ui_font}', '{mono_font}', sans-serif; font-size: {fs_13}px; font-weight: bold; }}
                .sidebar-clock-lbl {{ color: #{fg_main}; font-family: '{mono_font}', '{ui_font}', sans-serif; font-size: {fs_12}px; font-weight: bold; }}
                .sidebar-reload-lbl {{ color: #{fg_main}; font-family: '{ui_font}', sans-serif; font-size: {fs_11}px; font-weight: bold; }}

                .sidebar-status-lbl {{ color: #{fg_muted}; font-family: '{mono_font}', '{ui_font}', sans-serif; font-size: {fs_11}px; font-weight: bold; }}
                .sidebar-status-lbl.bat-low {{ color: #f38ba8; }}
                .notif-unread .sidebar-status-lbl {{ color: #{accent2}; }}

                .ws-top-bar {{ background-color: {bg_recessed_glass}; border-bottom: 1px solid {accent1_35}; padding: 6px 10px; }}
                .ws-pill-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 2px 9px; min-width: 24px; min-height: 22px; box-shadow: none; transition: all 0.15s ease; }}
                .ws-pill-btn label {{ color: #{fg_muted}; font-family: '{mono_font}', sans-serif; font-size: {fs_12}px; font-weight: bold; }}
                .ws-pill-btn:hover {{ background-color: {fg_main_15}; }}
                .ws-pill-btn.active {{ background-color: {accent1_24}; border-color: transparent; }}
                .ws-pill-btn.active label {{ color: #{accent2}; }}
                .ws-preview-map {{ border-radius: {r_xs}px; }}
                .top-tool-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_sm}px; padding: 2px 7px; min-width: 24px; min-height: 22px; box-shadow: none; transition: background-color 0.15s ease; }}
                .top-tool-btn label {{ color: #{accent2}; font-family: '{mono_font}', sans-serif; font-size: {fs_13}px; }}
                .top-tool-btn:hover {{ background-color: {fg_main_15}; }}
                .kbd-pill {{ background-color: {fg_main_08}; border-radius: {r_xxs}px; padding: 3px 8px; color: #{accent2}; font-family: '{mono_font}', monospace; font-size: {fs_11}px; font-weight: bold; }}
                .vim-mode-pill {{ background-color: {accent1_24}; border: 1px solid {accent1_55}; border-radius: {r_sm}px; padding: 2px 7px; color: #{accent2}; font-family: '{mono_font}', monospace; font-size: {fs_11}px; font-weight: bold; }}
                .vim-mode-pill.insert {{ background-color: #{selection}; border-color: #{accent2}; color: #{fg_main}; }}
                .vim-enabled button:focus,
                .vim-enabled .fav-item-btn:focus,
                .vim-enabled .sidebar-nav-btn:focus,
                .vim-enabled .sidebar-folder-btn:focus,
                .vim-enabled .plan-fmt-btn:focus,
                .vim-enabled .plan-close-btn:focus,
                .vim-enabled .primary-btn:focus,
                .vim-enabled .fav-hide-btn:focus,
                .vim-enabled .plan-delete-menu-btn:focus,
                .vim-enabled .power-btn:focus,
                .vim-enabled .session-restore-btn:focus,
                .vim-enabled .wall-tile-btn:focus {{
                    outline: 2px solid #{accent2};
                    outline-offset: -2px;
                    background-color: {accent1_24};
                }}

                .wall-tile-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 4px; box-shadow: none; transition: all 0.15s ease; }}
                .wall-tile-btn:hover {{ background-color: {fg_main_15}; }}
                .wall-tile-btn.active {{ border: 2px solid #{accent2}; background-color: {accent1_24}; }}

                .tray-icon-btn {{ background-color: transparent; border: 1px solid transparent; border-radius: {r_xxs}px; padding: 2px; min-width: 22px; min-height: 22px; box-shadow: none; transition: background-color 0.15s ease; }}
                .tray-icon-btn:hover {{ background-color: {fg_main_15}; }}

                .media-edge-sliver {{ background-color: #{accent2}; border-radius: {r_sliv}px; opacity: 0.85; transition: all 0.18s ease; }}
                .media-edge-sliver.paused {{ background-color: #{accent1}; opacity: 0.55; }}
                .media-edge-sliver.active {{ opacity: 0.35; }}
                .media-popout-pill {{ background-color: {bg_main_glass}; border: 2px solid #{accent1}; border-radius: {r_outer}px; padding: 6px 12px; transition: background-color 0.15s ease; }}
                .media-popout-pill:hover {{ background-color: {bg_pill_glass}; }}
                .media-pill-lbl {{ color: #{fg_main}; font-family: '{ui_font}', sans-serif; font-size: {fs_13}px; font-weight: bold; }}
                .media-popout-card {{ background-color: {bg_main_glass}; border: 2px solid #{accent1}; border-radius: {r_outer}px; padding: 12px 14px; }}
                .media-popout-title {{ color: #{fg_main}; font-family: '{ui_font}', sans-serif; font-size: {fs_15}px; font-weight: bold; }}
                .media-art-box {{ background-color: #{bg_recessed}; border: 1px solid #{selection}; border-radius: {r_md}px; }}

                calendar.lily-calendar {{ background-color: #{bg_recessed}; border: 1px solid #{accent1}; border-radius: {r_lg}px; color: #{fg_main}; padding: 4px; font-family: '{ui_font}', sans-serif; font-size: {fs_13}px; }}
                calendar.lily-calendar > header {{ color: #{accent2}; font-weight: bold; border-bottom: 1px solid #{selection}; padding-bottom: 4px; }}
                calendar.lily-calendar > grid > label.day-name {{ color: #{fg_muted}; font-weight: bold; }}
                calendar.lily-calendar > grid > label.day-number {{ border-radius: {r_xs}px; padding: 3px; }}
                calendar.lily-calendar > grid > label.day-number:selected {{ background-color: #{accent2}; color: #{bg_contrast}; font-weight: bold; }}
                calendar.lily-calendar > grid > label.day-number.today {{ border: 1px solid #{accent1}; font-weight: bold; }}
                calendar.lily-calendar > grid > label.day-number.other-month {{ opacity: 0.35; }}
                
                label {{ color: #{fg_main}; font-family: '{ui_font}', '{mono_font}', sans-serif; font-size: {fs_16}px; }}
                entry {{ background-color: #{bg_recessed}; color: #{fg_main}; caret-color: #{accent2}; border: 1px solid {accent1_35}; border-radius: {r_sm}px; padding: 6px 10px; font-family: '{ui_font}', sans-serif; font-size: {fs_14}px; box-shadow: none; }}
                entry:focus-within {{ border-color: #{accent1}; outline: none; box-shadow: none; }}
                entry > text > placeholder {{ color: #{fg_muted}; }}
                
                .trigger-wrapper {{ min-width: 50px; min-height: {trig_h}px; }}
                .trigger-btn {{ background-color: #{bg_pill}; border: 2px solid #{accent1}; {trig_edge_css} min-width: 50px; padding: 0; transition-property: transform, background-color; transition-duration: 0.2s; transition-timing-function: cubic-bezier(0.25, 0.8, 0.25, 1); }}
                .trigger-btn.peeking, .trigger-btn.riding {{ background-color: #{selection}; transform: translateX(0px); }}
                
                popover.fav-popover > contents {{ background-color: {bg_main_glass}; border: 1px solid {accent1_35}; border-radius: {r_lg}px; padding: 6px; }}
                tooltip, tooltip.background {{ background-color: #{bg_pill}; color: #{fg_main}; border: 1px solid {accent1_35}; border-radius: {r_sm}px; padding: 4px 8px; box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25); }}
                tooltip label {{ color: #{fg_main}; font-size: {fs_13}px; }}
                window.trust-dialog-win {{ background-color: #{bg_main}; color: #{fg_main}; border: 1.5px solid #{accent1}; border-radius: {r_lg}px; }}
                .trust-dialog-box {{ background-color: #{bg_main}; padding: 18px; }}
                .fav-hide-btn {{ background-color: {fg_main_08}; border: none; border-radius: {r_sm}px; padding: 6px 12px; color: #{fg_main}; box-shadow: none; transition: background-color 0.15s ease; }}
                .fav-hide-btn:hover {{ background-color: {fg_main_15}; }}
                
                .plan-detail-pane {{ background-color: {bg_main_glass}; {detail_border_css} min-width: 400px; }}
                .plan-divider {{ background-color: #{selection}; border-radius: 1px; }}
                .plan-h1 {{ color: #{accent2}; font-weight: bold; font-size: {fs_19}px; }}
                .plan-h2 {{ color: #{accent2}; font-weight: bold; font-size: {fs_16}px; }}
                .plan-step {{ color: #{accent2}; font-weight: bold; font-size: {fs_15}px; }}
                .plan-body {{ color: #{fg_main}; font-size: {fs_14}px; }}
                .plan-subnote {{ color: #{accent2}; font-family: '{mono_font}', monospace; font-size: {fs_12}px; }}
                .plan-checked-text {{ opacity: 0.55; text-decoration: line-through; }}
                .plan-check-row {{ background-color: #{bg_recessed}; border: 1px solid #{selection}; border-radius: {r_md}px; padding: 6px 10px; }}
                checkbutton.plan-checkbox > check {{ background-color: #{bg_pill}; border: 2px solid #{accent1}; border-radius: {r_chk}px; min-width: 18px; min-height: 18px; }}
                checkbutton.plan-checkbox:checked > check {{ background-color: #{accent2}; border-color: #{accent2}; color: #{bg_main}; }}
                
                .plan-composer-dock {{ background-color: {bg_recessed_glass}; border-top: 1px solid {accent1_35}; padding: 10px 14px; }}
                .plan-delete-menu-btn {{ background-color: rgba(243, 139, 168, 0.14); border: none; border-radius: {r_sm}px; padding: 6px 12px; color: #f38ba8; font-weight: bold; box-shadow: none; transition: background-color 0.15s ease; }}
                .plan-delete-menu-btn:hover {{ background-color: rgba(243, 139, 168, 0.26); }}
                .plan-line-del-btn {{ background-color: transparent; border: 1px solid transparent; border-radius: {r_xxs}px; padding: 0px 5px; color: #{fg_muted}; font-family: '{ui_font}', sans-serif; font-size: {fs_14}px; font-weight: bold; min-width: 24px; min-height: 24px; box-shadow: none; transition: background-color 0.15s ease, border-color 0.15s ease, color 0.15s ease; }}
                .plan-line-del-btn label {{ color: #{fg_muted}; font-size: {fs_14}px; font-weight: bold; }}
                .plan-line-del-btn:hover {{ background-color: rgba(243, 139, 168, 0.16); border-color: rgba(243, 139, 168, 0.42); color: #f38ba8; }}
                .plan-line-del-btn:hover label {{ color: #f38ba8; }}
                
                .plan-drag-handle {{ color: #{accent2}; font-family: '{mono_font}', monospace; font-size: {fs_13}px; font-weight: bold; padding: 2px 4px; opacity: 0.7; }}
                .plan-edit-row {{ border: 1px solid transparent; border-radius: {r_xs}px; padding: 2px 4px; transition: background-color 0.1s ease, border-color 0.1s ease; }}
                .plan-edit-row:hover {{ background-color: #{bg_recessed}; }}
                .plan-edit-row.dragging {{ opacity: 0.4; }}
                .plan-edit-row.drop-target {{ border: 2px dashed #{accent2}; background-color: #{bg_pill}; }}
                
                scale.music-seek {{ padding: 2px 0px; }}
                scale.music-seek > trough {{ background-color: {fg_main_08}; border: none; border-radius: {r_bar}px; min-height: 6px; }}
                scale.music-seek > trough > highlight {{ background-color: #{accent2}; border-radius: {r_bar}px; }}
                scale.music-seek > trough > slider {{ background-color: #{bg_main}; border: 2px solid #{accent2}; border-radius: 50%; min-width: 12px; min-height: 12px; margin: -4px 0px; }}
                
                progressbar.drive-progress > trough {{ background-color: {fg_main_08}; border: none; border-radius: {r_bar}px; min-height: 10px; }}
                progressbar.drive-progress > trough > progress {{ background-color: #{accent2}; border-radius: {r_bar}px; min-height: 10px; }}

                .sticky-card {{ background-color: {bg_main_glass}; border: 1px solid {accent1_55}; border-radius: {r_lg}px; }}
                .sticky-tint-0 {{ border-color: rgba(233, 117, 202, 0.55); }}
                .sticky-tint-1 {{ background-color: rgba(42, 36, 22, 0.72); border-color: rgba(249, 226, 175, 0.55); }}
                .sticky-tint-2 {{ background-color: rgba(22, 30, 44, 0.72); border-color: rgba(137, 180, 250, 0.55); }}
                .sticky-tint-3 {{ background-color: rgba(22, 38, 28, 0.72); border-color: rgba(166, 227, 161, 0.55); }}
                .sticky-tint-4 {{ background-color: rgba(34, 24, 44, 0.72); border-color: rgba(203, 166, 247, 0.55); }}
                .sticky-header {{ background-color: {bg_recessed_glass}; border-bottom: 1px solid {accent1_35}; border-top-left-radius: {r_lg}px; border-top-right-radius: {r_lg}px; padding: 4px 6px; }}
                .sticky-mini-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_xxs}px; padding: 2px 7px; min-width: 22px; min-height: 22px; color: #{fg_main}; box-shadow: none; transition: background-color 0.15s ease; }}
                .sticky-mini-btn label {{ font-family: '{mono_font}', '{ui_font}', sans-serif; font-size: {fs_11}px; font-weight: bold; color: #{fg_main}; }}
                .sticky-mini-btn:hover {{ background-color: {fg_main_15}; }}
                .sticky-mini-btn.active {{ background-color: {accent1_24}; }}
                .sticky-mini-btn.active label {{ color: #{accent2}; }}
                .sticky-dot-0 label {{ color: #e975ca; }}
                .sticky-dot-1 label {{ color: #f9e2af; }}
                .sticky-dot-2 label {{ color: #89b4fa; }}
                .sticky-dot-3 label {{ color: #a6e3a1; }}
                .sticky-dot-4 label {{ color: #cba6f7; }}
                .sticky-ink-0 label {{ color: #fee4ff; }}
                .sticky-ink-1 label {{ color: #e975ca; }}
                .sticky-ink-2 label {{ color: #89b4fa; }}
                .sticky-ink-3 label {{ color: #a6e3a1; }}
                .sticky-ink-4 label {{ color: #fab387; }}
                textview.sticky-text, textview.sticky-text text {{ background-color: transparent; color: #{fg_main}; font-family: '{ui_font}', sans-serif; font-size: {fs_14}px; }}
                "#
            );

            // Expose active palette as GTK @define-color variables so user CSS files in
            // `~/.config/lily-dashboard/themes/css/` automatically adapt to Dark/Light/custom themes!
            let palette_vars_css = format!(
                "@define-color bg_main #{bg_main};\n\
                 @define-color bg_pill #{bg_pill};\n\
                 @define-color bg_recessed #{bg_recessed};\n\
                 @define-color bg_contrast #{bg_contrast};\n\
                 @define-color fg_main #{fg_main};\n\
                 @define-color fg_muted #{fg_muted};\n\
                 @define-color accent #{accent1};\n\
                 @define-color accent2 #{accent2};\n\
                 @define-color selection #{selection};\n\
                 @define-color accent_15 {accent1_15};\n\
                 @define-color accent_24 {accent1_24};\n\
                 @define-color accent_38 {accent1_38};\n\
                 @define-color accent_55 {accent1_55};\n\
                 @define-color fg_04 {fg_main_04};\n\
                 @define-color fg_06 {fg_main_06};\n\
                 @define-color fg_08 {fg_main_08};\n\
                 @define-color fg_15 {fg_main_15};\n"
            );

            let pill_rad = if r_scale > 0.01 { 999 } else { 0 };
            let preview_css = format!(
                r#"
                .btn-preview-tonal {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 6px 11px; color: #{fg_main}; box-shadow: none; }}
                .btn-preview-tonal.active {{ background-color: {accent1_24}; }}
                .btn-preview-tonal.active label {{ color: #{accent2}; }}
                .btn-preview-outlined {{ background-color: {fg_main_04}; border: 1px solid {accent1_38}; border-radius: {r_md}px; padding: 6px 11px; color: #{fg_main}; box-shadow: none; }}
                .btn-preview-outlined:hover, .btn-preview-outlined.active {{ background-color: {accent1_24}; border-color: #{accent2}; }}
                .btn-preview-outlined.active label {{ color: #{accent2}; }}
                .btn-preview-pill {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {pill_rad}px; padding: 6px 13px; color: #{fg_main}; box-shadow: none; }}
                .btn-preview-pill.active {{ background-color: {accent1_24}; border: 1px solid {accent1_55}; }}
                .btn-preview-pill.active label {{ color: #{accent2}; }}
                .btn-preview-solid {{ background-color: #{accent2}; border: 1px solid #{accent2}; border-radius: {r_md}px; padding: 6px 11px; color: #{bg_contrast}; box-shadow: none; }}
                .btn-preview-solid label {{ color: #{bg_contrast}; }}
                .btn-preview-solid:hover {{ background-color: #{accent1}; }}
                .btn-preview-neon {{ background-color: #{bg_recessed}; border: 1px solid #{accent2}; border-radius: {r_md}px; padding: 6px 11px; color: #{accent2}; box-shadow: 0px 0px 9px {accent1_55}; }}
                .btn-preview-neon label {{ color: #{accent2}; }}
                .btn-preview-ghost {{ background-color: transparent; border: none; border-bottom: 2px solid {accent1_55}; border-radius: {r_chk}px {r_chk}px 2px 2px; padding: 6px 11px; color: #{fg_main}; box-shadow: none; }}
                .btn-preview-ghost.active {{ background-color: {fg_main_06}; border-bottom-color: #{accent2}; }}
                .btn-preview-ghost.active label {{ color: #{accent2}; }}
                "#
            );

            // Auto-generate scoped live self-previews for every homemade `.css` theme
            // so custom styles preview themselves right in Settings -> CSS STYLE!
            let mut custom_previews = String::new();
            for (k, _) in config::list_css_themes() {
                if matches!(k.as_str(), "tonal" | "outlined" | "pill" | "solid" | "neon" | "ghost") {
                    continue;
                }
                let raw = config::load_css_theme_source(&k);
                let scope_cls: String = k
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
                    .collect();
                custom_previews.push_str(&scope_css_for_preview(&format!(".css-scope-{}", scope_cls), &raw));
            }

            let full_css = format!("{}\n{}\n{}", palette_vars_css, css, preview_css);
            provider.load_from_data(&full_css);

            // Load active CSS style theme file from `~/.config/lily-dashboard/themes/css/<name>.css`
            // plus scoped live self-previews for homemade themes at the same USER+2 priority
            // so custom preview buttons always win over whichever stock style is active!
            // `.lily-settled` suppresses 1-shot mount animations on already-settled/offscreen rows
            // during scroll/filter while preserving `:hover` and `.active`/`.selected`/`.pinned` animations!
            let raw_style = config::load_css_theme_source(&cfg.button_style);
            let settled_guard = ".lily-settled:not(:hover):not(.active):not(.selected):not(.pinned):not(.armed), \
                                 .lily-settled *:not(:hover):not(.active):not(.selected):not(.pinned):not(.armed) { animation: none; }";
            let user_style_css = format!(
                "{}\n{}\n{}\n{}",
                palette_vars_css, raw_style, settled_guard, custom_previews
            );
            user_css_provider.load_from_data(&user_style_css);

            // Force every toplevel layer-shell window (dashboard, media popout, stickies)
            // to repaint immediately on Wayland even when not hovered/clicked.
            for w in gtk4::Window::list_toplevels() {
                w.queue_draw();
            }

            let scrollbar_css = format!(
                r#"
                scrolledwindow > scrollbar, scrolledwindow > scrollbar > range, scrolledwindow > scrollbar > range > trough,
                scrollbar, scrollbar > range, scrollbar > range > trough {{ background-color: transparent; border: none; margin: 0px; padding: 0px; min-width: 0px; min-height: 0px; box-shadow: none; }}
                scrolledwindow > scrollbar.vertical > range > trough > slider,
                scrollbar.vertical.overlay-indicator:not(.dragging):not(.hovering) > range > trough > slider,
                scrollbar.vertical > range > trough > slider {{ min-width: 2px; min-height: 24px; margin: 2px 0px; padding: 0px; border: none; border-radius: 99px; background-color: {accent2_28}; box-shadow: none; transition: background-color 0.15s ease, min-width 0.15s ease; }}
                scrolledwindow > scrollbar.vertical.hovering > range > trough > slider,
                scrolledwindow > scrollbar.vertical:hover > range > trough > slider,
                scrolledwindow > scrollbar.vertical > range > trough > slider:hover,
                scrolledwindow > scrollbar.vertical > range > trough > slider:active,
                scrollbar.vertical.hovering > range > trough > slider,
                scrollbar.vertical:hover > range > trough > slider,
                scrollbar.vertical > range > trough > slider:hover,
                scrollbar.vertical > range > trough > slider:active {{ min-width: 4px; background-color: {accent2_70}; }}
                scrolledwindow > scrollbar.horizontal > range > trough > slider,
                scrollbar.horizontal.overlay-indicator:not(.dragging):not(.hovering) > range > trough > slider,
                scrollbar.horizontal > range > trough > slider {{ min-height: 2px; min-width: 24px; margin: 0px 2px; padding: 0px; border: none; border-radius: 99px; background-color: {accent2_28}; box-shadow: none; transition: background-color 0.15s ease, min-height 0.15s ease; }}
                scrolledwindow > scrollbar.horizontal.hovering > range > trough > slider,
                scrolledwindow > scrollbar.horizontal:hover > range > trough > slider,
                scrolledwindow > scrollbar.horizontal > range > trough > slider:hover,
                scrolledwindow > scrollbar.horizontal > range > trough > slider:active,
                scrollbar.horizontal.hovering > range > trough > slider,
                scrollbar.horizontal:hover > range > trough > slider,
                scrollbar.horizontal > range > trough > slider:hover,
                scrollbar.horizontal > range > trough > slider:active {{ min-height: 4px; background-color: {accent2_70}; }}
                "#
            );
            scrollbar_provider.load_from_data(&scrollbar_css);
            sounds::reload_active_pack();
        })
    };

    // Load immediately on startup
    update_css();

    // Watch external theme file if configured
    let watch_path = config::expand_path(&config::get().paths.theme_conf_file);
    let file = gtk4::gio::File::for_path(&watch_path);
    if let Ok(monitor) = file.monitor_file(gtk4::gio::FileMonitorFlags::NONE, gtk4::gio::Cancellable::NONE) {
        let cb = update_css.clone();
        monitor.connect_changed(move |_, _, _, event| {
            if event == gtk4::gio::FileMonitorEvent::ChangesDoneHint || event == gtk4::gio::FileMonitorEvent::Created {
                cb();
            }
        });
        std::boxed::Box::leak(std::boxed::Box::new(monitor));
    }

    // Watch `~/.config/lily-dashboard/styles/` recursively (CSS, .sounds, assets/) for live hot-reload
    {
        use notify::{EventKind, RecursiveMode, Watcher};
        let styles_root = config::styles_dir();
        let (tx_st, rx_st) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let mut watcher = match notify::recommended_watcher(
                move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        if matches!(
                            ev.kind,
                            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                        ) {
                            let _ = tx_st.send(());
                        }
                    }
                },
            ) {
                Ok(w) => w,
                Err(_) => return,
            };
            if watcher.watch(&styles_root, RecursiveMode::Recursive).is_ok() {
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
        });

        let cb_st = update_css.clone();
        let mut pending_deadline: Option<std::time::Instant> = None;
        glib::timeout_add_local(Duration::from_millis(120), move || {
            let mut got_event = false;
            while rx_st.try_recv().is_ok() {
                got_event = true;
            }
            if got_event {
                pending_deadline = Some(std::time::Instant::now() + Duration::from_millis(120));
            }
            if let Some(dl) = pending_deadline {
                if std::time::Instant::now() >= dl {
                    pending_deadline = None;
                    cb_st();
                }
            }
            glib::ControlFlow::Continue
        });
    }

    update_css
}

fn update_input_region(window: &ApplicationWindow, is_open: bool, dash_width: i32) {
    if let Some(surface) = window.surface() {
        let cfg = config::get();
        let is_right = cfg.layout.side == "right";
        let trig_h = match cfg.layout.trigger_size.as_str() {
            "small" => 72,
            "large" => 140,
            _ => 100,
        };
        let height = window.height().max(1080);
        let win_w = window.width().max(TOTAL_DASH_WIDTH + TRIGGER_WIDTH);
        let region = if !is_open {
            let y = match cfg.layout.trigger_align.as_str() {
                "top" => 16,
                "bottom" => (height - trig_h - 16).max(0),
                _ => ((height - trig_h) / 2).max(0),
            };
            let x = if is_right { (win_w - 10).max(0) } else { 0 };
            Region::create_rectangle(&RectangleInt::new(x, y, 10, trig_h + 12))
        } else {
            let base_w = cfg.layout.panel_width.clamp(340, 560);
            let w = if dash_width > 0 { dash_width } else { base_w };
            let total_w = w + TRIGGER_WIDTH;
            let x = if is_right { (win_w - total_w).max(0) } else { 0 };
            Region::create_rectangle(&RectangleInt::new(x, 0, total_w, height))
        };
        surface.set_input_region(&region);
    }
}

thread_local! {
    static FOCUSED_HYPR_MON: RefCell<String> = const { RefCell::new(String::new()) };
    static APPLIED_GDK_MON: RefCell<String> = const { RefCell::new(String::new()) };
}

fn query_focused_monitor_fast() -> Option<String> {
    use std::io::{Read, Write};
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
    let sock_path = format!("{}/hypr/{}/.socket.sock", runtime, sig);
    let mut stream = std::os::unix::net::UnixStream::connect(&sock_path).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(40)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(40)));
    stream.write_all(b"j/monitors").ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let json = serde_json::from_slice::<serde_json::Value>(&buf).ok()?;
    json.as_array()?
        .iter()
        .find(|m| m.get("focused").and_then(|f| f.as_bool()).unwrap_or(false))
        .and_then(|m| m.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
}

fn move_to_monitor(window: &ApplicationWindow, target: Option<&str>) {
    if let Some(t) = target {
        if !t.is_empty() {
            FOCUSED_HYPR_MON.with(|slot| *slot.borrow_mut() = t.to_string());
        }
    }

    let pinned = config::get().layout.pinned_monitor;
    let active_name = if !pinned.trim().is_empty() {
        Some(pinned)
    } else if let Some(t) = target.filter(|s| !s.is_empty()) {
        Some(t.to_string())
    } else {
        let cached = FOCUSED_HYPR_MON.with(|slot| slot.borrow().clone());
        if !cached.is_empty() {
            Some(cached)
        } else {
            let queried = query_focused_monitor_fast();
            if let Some(ref m) = queried {
                FOCUSED_HYPR_MON.with(|slot| *slot.borrow_mut() = m.clone());
            }
            queried
        }
    };

    let Some(target_name) = active_name else { return };

    // Skip `window.set_monitor` when already bound to this connector — re-setting the same
    // monitor on a Wayland layer-shell surface triggers an unnecessary configure/repaint cycle!
    let already_applied = APPLIED_GDK_MON.with(|slot| *slot.borrow() == target_name);
    if already_applied {
        return;
    }

    let Some(display) = gdk::Display::default() else { return };
    let gdk_monitors = display.monitors();

    for i in 0..gdk_monitors.n_items() {
        if let Some(mon) = gdk_monitors.item(i).and_downcast::<gdk::Monitor>() {
            if let Some(conn) = mon.connector() {
                if conn == target_name {
                    APPLIED_GDK_MON.with(|slot| *slot.borrow_mut() = target_name);
                    window.set_monitor(&mon);
                    break;
                }
            }
        }
    }
}
/// Recursively pops down every visible `gtk4::Popover` in the widget tree (custom right-click
/// menus, power/weather/tray popovers, and GTK4's built-in Entry/TextView context menus) so
/// closing the drawer or unloading a stateless tab never leaves an orphaned Wayland popup grab.
fn popdown_all_popovers(widget: &gtk4::Widget) {
    if let Ok(pop) = widget.clone().downcast::<gtk4::Popover>() {
        if pop.is_visible() {
            pop.popdown();
        }
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        popdown_all_popovers(&c);
        child = next;
    }
}

fn build_ui(
    app: &Application,
    media_handle: media_popout::MediaPopoutHandle,
    stickies_handle: stickies::StickiesHandle,
    reload_theme: Rc<dyn Fn()>,
) -> DashHandle {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Lily Dashboard")
        .build();

    window.set_size_request(TOTAL_DASH_WIDTH + TRIGGER_WIDTH, -1);

    window.init_layer_shell();
    window.set_namespace("lily-dashboard");
    window.set_layer(Layer::Top);
    window.set_keyboard_mode(KeyboardMode::None);

    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, false);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);

    window.set_margin(Edge::Top, 8);
    window.set_margin(Edge::Bottom, 12);

    let drawer_container = Box::builder()
        .orientation(Orientation::Horizontal)
        .css_classes(vec!["drawer-container".to_string()])
        .halign(gtk4::Align::Start)
        .build();

    let dash_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .css_classes(vec!["dash-box".to_string(), "dash-hibernated".to_string()])
        .width_request(400)
        .overflow(Overflow::Hidden)
        .build();

    // Dynamically adjust closed translateX offset & Wayland input hitbox ONLY when layout
    // or surface dimensions actually change (4-frame self-terminating burst -> 0 Hz idle frame clock!).
    let dynamic_offset_provider = CssProvider::new();
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &dynamic_offset_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER,
    );

    let stack = Stack::new();
    stack.set_hhomogeneous(false);
    stack.set_size_request(288, -1);
    stack.set_hexpand(false);
    stack.set_vexpand(true);

    let sidebar_column = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["sidebar-column".to_string()])
        .vexpand(true)
        .build();

    let sidebar_clamp = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External) // <-- NEVER stretches parent!
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_width(false)                // <-- Locks width strictly!
        .min_content_width(126)
        .max_content_width(126)
        .width_request(126)
        .has_frame(false)
        .hexpand(false)
        .vexpand(true)
        .css_classes(vec!["internal-layout-clamp".to_string()])
        .child(&sidebar_column)
        .build();

    // Dynamic sidebar navigation rebuilt from `config.layout.sidebar_items`
    // (supports tabs, "---" separators, and collapsible "folder:<Name>" .. "/folder" groups)
    let sidebar_nav = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(1)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let sidebar_nav_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .overlay_scrolling(true)
        .propagate_natural_height(false)
        .has_frame(false)
        .vexpand(true)
        .child(&sidebar_nav)
        .build();
    module_api::attach_viewport_culler(&sidebar_nav_scroll, &sidebar_nav);

    let nav_buttons_rc: Rc<RefCell<Vec<(String, Button)>>> = Rc::new(RefCell::new(Vec::new()));
    let nav_folders_rc: Rc<RefCell<Vec<(Button, Box, Vec<String>)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let nav_badges_state: Rc<RefCell<std::collections::HashMap<String, String>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    let nav_badge_lbls_rc: Rc<RefCell<std::collections::HashMap<String, Label>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));

    {
        let st_b = nav_badges_state.clone();
        let lbls_b = nav_badge_lbls_rc.clone();
        module_api::register_sidebar_badge_cb(Rc::new(move |tab_id: &str, badge: Option<&str>| {
            match badge.map(|s| s.trim()).filter(|s| !s.is_empty()) {
                Some(txt) => {
                    st_b.borrow_mut().insert(tab_id.to_string(), txt.to_string());
                    if let Some(lbl) = lbls_b.borrow().get(tab_id) {
                        lbl.set_label(txt);
                        lbl.set_visible(true);
                    }
                }
                None => {
                    st_b.borrow_mut().remove(tab_id);
                    if let Some(lbl) = lbls_b.borrow().get(tab_id) {
                        lbl.set_label("");
                        lbl.set_visible(false);
                    }
                }
            }
        }));
    }

    let rebuild_sidebar_nav: Rc<dyn Fn()> = {
        let snav = sidebar_nav.clone();
        let stk = stack.clone();
        let nbtns = nav_buttons_rc.clone();
        let nfolds = nav_folders_rc.clone();
        let nbadges = nav_badges_state.clone();
        let nblbls = nav_badge_lbls_rc.clone();
        Rc::new(move || {
            while let Some(c) = snav.first_child() {
                snav.remove(&c);
            }
            nbtns.borrow_mut().clear();
            nfolds.borrow_mut().clear();
            nblbls.borrow_mut().clear();

            let cfg = config::get();
            let active_name = stk.visible_child_name().map(|s| s.to_string());
            let compiled = modules_gen::all_modules();

            let mut cur_folder: Option<(String, Button, Label, Box, Vec<String>)> = None;
            let flush_folder = |cur: &mut Option<(String, Button, Label, Box, Vec<String>)>,
                                nfolds_ref: &Rc<RefCell<Vec<(Button, Box, Vec<String>)>>>,
                                active: Option<&str>| {
                if let Some((_fname, fbtn, _flbl, fbox, child_ids)) = cur.take() {
                    let is_collapsed = !fbox.is_visible();
                    let contains_active = active
                        .map(|a| child_ids.iter().any(|cid| cid == a))
                        .unwrap_or(false);
                    if is_collapsed && contains_active {
                        fbtn.add_css_class("has-active");
                    } else {
                        fbtn.remove_css_class("has-active");
                    }
                    nfolds_ref.borrow_mut().push((fbtn, fbox, child_ids));
                }
            };

            for item in &cfg.layout.sidebar_items {
                if let Some(raw_fname) = item.strip_prefix("folder:") {
                    flush_folder(&mut cur_folder, &nfolds, active_name.as_deref());
                    let fname = if raw_fname.trim().is_empty() {
                        "Folder".to_string()
                    } else {
                        raw_fname.trim().to_string()
                    };
                    let is_collapsed = cfg
                        .layout
                        .collapsed_folders
                        .iter()
                        .any(|f| f == &fname);
                    let arrow = if is_collapsed { "▸" } else { "▾" };
                    let flbl = Label::builder()
                        .label(&format!("{} {}", arrow, fname))
                        .halign(gtk4::Align::Start)
                        .hexpand(true)
                        .ellipsize(gtk4::pango::EllipsizeMode::End)
                        .build();
                    let fbtn = Button::builder()
                        .child(&flbl)
                        .css_classes(vec!["sidebar-folder-btn".to_string()])
                        .tooltip_text(&fname)
                        .build();
                    let fbox = Box::builder()
                        .orientation(Orientation::Vertical)
                        .spacing(1)
                        .visible(!is_collapsed)
                        .css_classes(vec!["sidebar-folder-children".to_string()])
                        .build();

                    {
                        let fbox_c = fbox.clone();
                        let flbl_c = flbl.clone();
                        let fname_c = fname.clone();
                        let stk_c = stk.clone();
                        let nfolds_c = nfolds.clone();
                        fbtn.connect_clicked(move |btn| {
                            let next_vis = !fbox_c.is_visible();
                            fbox_c.set_visible(next_vis);
                            flbl_c.set_label(&format!(
                                "{} {}",
                                if next_vis { "▾" } else { "▸" },
                                fname_c
                            ));
                            config::update(|c| {
                                c.layout.collapsed_folders.retain(|x| x != &fname_c);
                                if !next_vis {
                                    c.layout.collapsed_folders.push(fname_c.clone());
                                }
                            });
                            let cur_act = stk_c.visible_child_name().map(|s| s.to_string());
                            let has_act = nfolds_c
                                .borrow()
                                .iter()
                                .find(|(b, _, _)| b == btn)
                                .map(|(_, _, ids)| {
                                    cur_act
                                        .as_deref()
                                        .map(|a| ids.iter().any(|cid| cid == a))
                                        .unwrap_or(false)
                                })
                                .unwrap_or(false);
                            if !next_vis && has_act {
                                btn.add_css_class("has-active");
                            } else {
                                btn.remove_css_class("has-active");
                            }
                        });
                    }

                    snav.append(&fbtn);
                    snav.append(&fbox);
                    cur_folder = Some((fname, fbtn, flbl, fbox, Vec::new()));
                } else if item == "/folder" {
                    flush_folder(&mut cur_folder, &nfolds, active_name.as_deref());
                } else if item == "---" {
                    let sep = Box::builder()
                        .orientation(Orientation::Horizontal)
                        .height_request(1)
                        .css_classes(vec!["sidebar-sep".to_string()])
                        .build();
                    if let Some((_, _, _, ref fbox, _)) = cur_folder {
                        fbox.append(&sep);
                    } else {
                        snav.append(&sep);
                    }
                } else if let Some(desc) = compiled.iter().find(|m| m.id == item.as_str()) {
                    let btn_row = Box::builder()
                        .orientation(Orientation::Horizontal)
                        .spacing(4)
                        .build();
                    let lbl = Label::builder()
                        .label(desc.title)
                        .halign(gtk4::Align::Start)
                        .hexpand(true)
                        .ellipsize(gtk4::pango::EllipsizeMode::End)
                        .build();
                    let cur_badge = nbadges.borrow().get(desc.id).cloned();
                    let badge_lbl = Label::builder()
                        .label(cur_badge.as_deref().unwrap_or(""))
                        .halign(gtk4::Align::End)
                        .valign(gtk4::Align::Center)
                        .visible(cur_badge.is_some())
                        .css_classes(vec!["sidebar-tab-badge".to_string()])
                        .build();
                    btn_row.append(&lbl);
                    btn_row.append(&badge_lbl);
                    nblbls.borrow_mut().insert(desc.id.to_string(), badge_lbl);

                    let mut classes = vec!["sidebar-nav-btn".to_string()];
                    if active_name.as_deref() == Some(desc.id) {
                        classes.push("selected".to_string());
                    }
                    let btn = Button::builder()
                        .child(&btn_row)
                        .css_classes(classes)
                        .build();
                    let stack_click = stk.clone();
                    let id_s = desc.id.to_string();
                    btn.connect_clicked(move |_| {
                        stack_click.set_visible_child_name(&id_s);
                    });
                    if let Some((_, _, _, ref fbox, ref mut child_ids)) = cur_folder {
                        fbox.append(&btn);
                        child_ids.push(desc.id.to_string());
                    } else {
                        snav.append(&btn);
                    }
                    nbtns.borrow_mut().push((desc.id.to_string(), btn));
                }
            }
            flush_folder(&mut cur_folder, &nfolds, active_name.as_deref());
        })
    };

    let layout_rebuild_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let power_rebuild_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let apply_tab_visibility: Rc<dyn Fn()> = {
        let rebuild_nav = rebuild_sidebar_nav.clone();
        let pwr_slot = power_rebuild_slot.clone();
        let lay_slot = layout_rebuild_slot.clone();
        Rc::new(move || {
            let items = config::get().layout.sidebar_items;
            rebuild_nav();
            for m in modules_gen::all_modules() {
                if let Some(vis_fn) = m.on_visibility {
                    vis_fn(items.iter().any(|it| it == m.id));
                }
            }
            if let Some(rebuild_pwr) = pwr_slot.borrow().as_ref() {
                rebuild_pwr();
            }
            if let Some(rebuild_lay) = lay_slot.borrow().as_ref() {
                rebuild_lay();
            }
        })
    };
    apply_tab_visibility();

    let reload_lbl = Label::builder()
        .label("Reload Dashboard")
        .halign(gtk4::Align::Start)
        .css_classes(vec!["sidebar-reload-lbl".to_string()])
        .build();

    let reload_btn = Button::builder()
        .child(&reload_lbl)
        .halign(gtk4::Align::Start)
        .css_classes(vec!["sidebar-footer-btn".to_string()])
        .tooltip_text("Rebuild (if source repo configured) and restart Lily Dashboard")
        .build();

    let app_for_reload = app.clone();
    let lbl_for_reload = reload_lbl.clone();
    let win_for_reload = window.clone();
    reload_btn.connect_clicked(move |btn| {
        let repo_dir_str = config::get().paths.source_repo_dir;
        let repo_path = config::expand_path(&repo_dir_str);
        let can_build = !repo_dir_str.trim().is_empty() && repo_path.join("Cargo.toml").is_file();

        if !can_build {
            // Prebuilt binary install: simply restart the current executable cleanly
            if let Ok(exe) = std::env::current_exe() {
                let exe_str = exe.to_string_lossy().replace('\'', "'\\''");
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("sleep 0.2 && '{}' &", exe_str))
                    .spawn();
                app_for_reload.quit();
            }
            return;
        }

        let start_build: Rc<dyn Fn(Vec<module_loader::ScannedModule>)> = {
            let btn_b = btn.clone();
            let lbl_b = lbl_for_reload.clone();
            let app_b = app_for_reload.clone();
            let repo_b = repo_path.clone();
            Rc::new(move |scanned_mods: Vec<module_loader::ScannedModule>| {
                btn_b.set_sensitive(false);
                lbl_b.set_label("Building...");

                let (tx, rx) = std::sync::mpsc::channel::<Result<std::path::PathBuf, String>>();
                let repo_bg = repo_b.clone();

                std::thread::spawn(move || {
                    let backup = match module_loader::sync_modules_to_repo(&repo_bg, &scanned_mods) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = std::fs::write("/tmp/lily-reload.log", &e);
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };

                    let run_cargo = |offline: bool| {
                        let mut cmd = std::process::Command::new("cargo");
                        cmd.args(["build", "--release"]);
                        if offline {
                            cmd.arg("--offline");
                        }
                        cmd.current_dir(&repo_bg).output()
                    };

                    let target_bin = repo_bg.join("target/release/lily-dashboard");
                    let res = match run_cargo(false) {
                        Ok(out) if out.status.success() => Ok(target_bin),
                        first => match run_cargo(true) {
                            Ok(out) if out.status.success() => Ok(target_bin),
                            Ok(out) => {
                                module_loader::restore_backup(&repo_bg, &backup);
                                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                                let _ = std::fs::write("/tmp/lily-reload.log", &stderr);
                                let last = stderr
                                    .lines()
                                    .rev()
                                    .find(|l| !l.trim().is_empty())
                                    .unwrap_or("See /tmp/lily-reload.log")
                                    .to_string();
                                Err(last)
                            }
                            Err(e) => {
                                module_loader::restore_backup(&repo_bg, &backup);
                                let msg = format!("Failed to invoke cargo ({:?}): {}", first.err(), e);
                                let _ = std::fs::write("/tmp/lily-reload.log", &msg);
                                Err(msg)
                            }
                        },
                    };
                    let _ = tx.send(res);
                });

                let app_quit = app_b.clone();
                let lbl_status = lbl_b.clone();
                let btn_status = btn_b.clone();

                glib::timeout_add_local(Duration::from_millis(100), move || {
                    if let Ok(result) = rx.try_recv() {
                        match result {
                            Ok(bin_path) => {
                                let bin_str = bin_path.to_string_lossy().replace('\'', "'\\''");
                                let _ = std::process::Command::new("sh")
                                    .arg("-c")
                                    .arg(format!("sleep 0.2 && '{}' &", bin_str))
                                    .spawn();
                                app_quit.quit();
                            }
                            Err(err_line) => {
                                lbl_status.set_label("Build failed!");
                                btn_status.set_tooltip_text(Some(&format!(
                                    "Build failed (log: /tmp/lily-reload.log): {}",
                                    err_line
                                )));
                                btn_status.set_sensitive(true);
                                let lbl_reset = lbl_status.clone();
                                glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                                    lbl_reset.set_label("Reload Dashboard");
                                });
                            }
                        }
                        return glib::ControlFlow::Break;
                    }
                    glib::ControlFlow::Continue
                });
            })
        };

        let scan = module_loader::scan_modules(&repo_path);
        if scan.untrusted.is_empty() {
            start_build(scan.modules);
            return;
        }

        // Third-party / modified module trust gate!
        let dlg = gtk4::Window::builder()
            .title("Third-Party Module Warning")
            .transient_for(&win_for_reload)
            .modal(true)
            .default_width(480)
            .resizable(false)
            .css_classes(vec!["trust-dialog-win".to_string()])
            .build();
        let vbox = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(10)
            .css_classes(vec!["trust-dialog-box".to_string()])
            .build();

        vbox.append(
            &Label::builder()
                .label("Warning: You are about to load a third-party module")
                .halign(gtk4::Align::Start)
                .wrap(true)
                .xalign(0.0)
                .css_classes(vec!["plan-h1".to_string()])
                .build(),
        );
        vbox.append(
            &Label::builder()
                .label("These are raw Rust code and therefore have access to your entire PC (at compile time and runtime). Only continue if you have read and trust the source code:")
                .halign(gtk4::Align::Start)
                .wrap(true)
                .xalign(0.0)
                .css_classes(vec!["plan-body".to_string()])
                .build(),
        );

        let list_card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(6)
            .css_classes(vec!["sys-card".to_string()])
            .build();
        for um in &scan.untrusted {
            let deps_txt = if um.dashpendencies.is_empty() {
                "no extra crates".to_string()
            } else {
                um.dashpendencies
                    .iter()
                    .map(|(k, v)| format!("{} = {}", k, v))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            list_card.append(
                &Label::builder()
                    .label(&format!(
                        "• modules/{}  [{}…]\n  deps: {}",
                        um.file_name,
                        &um.sha256[..12.min(um.sha256.len())],
                        deps_txt
                    ))
                    .halign(gtk4::Align::Start)
                    .wrap(true)
                    .xalign(0.0)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
        }
        vbox.append(&list_card);

        vbox.append(
            &Label::builder()
                .label(&format!(
                    "To continue, write:  {}  and press Enter.\n(Or write \"{}\" to disable this warning permanently.)",
                    module_loader::CONFIRM_PHRASE,
                    module_loader::CONFIRM_PHRASE_PERMANENT
                ))
                .halign(gtk4::Align::Start)
                .wrap(true)
                .xalign(0.0)
                .css_classes(vec!["plan-body".to_string()])
                .build(),
        );

        let phrase_entry = gtk4::Entry::builder()
            .placeholder_text(module_loader::CONFIRM_PHRASE)
            .hexpand(true)
            .build();
        let err_lbl = Label::builder()
            .label("")
            .halign(gtk4::Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .visible(false)
            .build();
        vbox.append(&phrase_entry);
        vbox.append(&err_lbl);

        let btn_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .halign(gtk4::Align::End)
            .build();
        let cancel_btn = Button::builder()
            .label("Cancel")
            .css_classes(vec!["fav-item-btn".to_string()])
            .build();
        let confirm_btn = Button::builder()
            .label("Trust & Rebuild")
            .css_classes(vec!["primary-btn".to_string()])
            .build();

        let dlg_cancel = dlg.clone();
        cancel_btn.connect_clicked(move |_| dlg_cancel.close());

        let do_confirm: Rc<dyn Fn()> = {
            let pe = phrase_entry.clone();
            let el = err_lbl.clone();
            let dlg_c = dlg.clone();
            let untrusted = scan.untrusted.clone();
            let all_mods = scan.modules.clone();
            let sb = start_build.clone();
            Rc::new(move || {
                let typed = pe.text().trim().to_string();
                let perm = typed == module_loader::CONFIRM_PHRASE_PERMANENT;
                if perm || typed == module_loader::CONFIRM_PHRASE {
                    if perm {
                        module_loader::trust_all_permanently();
                    } else {
                        module_loader::trust_modules(&untrusted);
                    }
                    config::update(|c| {
                        for um in &untrusted {
                            if !c.layout.sidebar_items.iter().any(|x| x == &um.id) {
                                c.layout.sidebar_items.push(um.id.clone());
                            }
                        }
                    });
                    dlg_c.close();
                    sb(all_mods.clone());
                } else {
                    el.set_label(&format!(
                        "Exact phrase required: {}",
                        module_loader::CONFIRM_PHRASE
                    ));
                    el.set_visible(true);
                }
            })
        };
        {
            let dc = do_confirm.clone();
            confirm_btn.connect_clicked(move |_| dc());
        }
        {
            let dc = do_confirm.clone();
            phrase_entry.connect_activate(move |_| dc());
        }

        btn_row.append(&cancel_btn);
        btn_row.append(&confirm_btn);
        vbox.append(&btn_row);
        dlg.set_child(Some(&vbox));
        dlg.present();
        phrase_entry.grab_focus();
    });

    // 3-button Session row: Load | Save | Auto-save toggle (OFF by default to avoid SSD wear)
    let session_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .homogeneous(true)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(2)
        .build();

    let sess_load_lbl = Label::builder()
        .label("󰦛")
        .css_classes(vec!["power-btn-lbl".to_string()])
        .build();
    let sess_load_btn = Button::builder()
        .child(&sess_load_lbl)
        .css_classes(vec!["power-btn".to_string()])
        .tooltip_text("Load saved session")
        .build();
    {
        let lbl_fb = sess_load_lbl.clone();
        sess_load_btn.connect_clicked(move |_| {
            let count = session::restore_previous_session();
            if count > 0 {
                lbl_fb.set_label(&format!("{}✓", count));
            } else {
                lbl_fb.set_label("–");
            }
            let lbl_reset = lbl_fb.clone();
            glib::timeout_add_local_once(Duration::from_millis(1400), move || {
                lbl_reset.set_label("󰦛");
            });
        });
    }

    let sess_save_lbl = Label::builder()
        .label("󰆓")
        .css_classes(vec!["power-btn-lbl".to_string()])
        .build();
    let sess_save_btn = Button::builder()
        .child(&sess_save_lbl)
        .css_classes(vec!["power-btn".to_string()])
        .tooltip_text("Save current session now")
        .build();
    {
        let lbl_fb = sess_save_lbl.clone();
        sess_save_btn.connect_clicked(move |_| {
            let count = session::save_session_now();
            if count > 0 {
                lbl_fb.set_label(&format!("{}✓", count));
            } else {
                lbl_fb.set_label("–");
            }
            let lbl_reset = lbl_fb.clone();
            glib::timeout_add_local_once(Duration::from_millis(1400), move || {
                lbl_reset.set_label("󰆓");
            });
        });
    }

    let auto_on_init = session::is_auto_save_enabled();
    let sess_auto_lbl = Label::builder()
        .label("󰁪")
        .css_classes(vec!["power-btn-lbl".to_string()])
        .build();
    let sess_auto_btn = Button::builder()
        .child(&sess_auto_lbl)
        .css_classes(if auto_on_init {
            vec!["power-btn".to_string(), "active".to_string()]
        } else {
            vec!["power-btn".to_string()]
        })
        .tooltip_text(if auto_on_init {
            "Auto-save session: ON (saves every 30s on layout change)"
        } else {
            "Auto-save session: OFF (manual save only)"
        })
        .build();
    sess_auto_btn.connect_clicked(move |btn| {
        let next = !session::is_auto_save_enabled();
        session::set_auto_save_enabled(next);
        if next {
            btn.add_css_class("active");
            btn.set_tooltip_text(Some(
                "Auto-save session: ON (saves every 30s on layout change)",
            ));
        } else {
            btn.remove_css_class("active");
            btn.set_tooltip_text(Some("Auto-save session: OFF (manual save only)"));
        }
    });

    session_row.append(&sess_load_btn);
    session_row.append(&sess_save_btn);
    session_row.append(&sess_auto_btn);

    let popover_guard = Rc::new(Cell::new(0usize));

    // Modular 1–3 button Power row (Shutdown, Reboot, Log Out, Suspend, Lock, or Dropdown Menu)
    let power_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .homogeneous(true)
        .margin_start(6)
        .margin_end(6)
        .margin_top(2)
        .margin_bottom(14)
        .build();

    let rebuild_power_row: Rc<dyn Fn()> = {
        let p_row = power_row.clone();
        let p_guard = popover_guard.clone();
        Rc::new(move || {
            while let Some(child) = p_row.first_child() {
                p_row.remove(&child);
            }

            let pcfg = config::get().power;
            let count = pcfg.count.clamp(1, 3);
            let armed_slot: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
            let armed_gen: Rc<Cell<u64>> = Rc::new(Cell::new(0));
            let slot_widgets: Rc<RefCell<Vec<(Button, Label, &'static str)>>> =
                Rc::new(RefCell::new(Vec::new()));

            let action_meta = |act: &str| -> (&'static str, &'static str, bool) {
                match act {
                    "shutdown" => ("", "Power Off (click again to confirm)", true),
                    "reboot" => ("󰜉", "Reboot (click again to confirm)", true),
                    "logout" => ("󰍃", "Log Out (click again to confirm)", true),
                    "suspend" => ("󰤄", "Suspend", false),
                    "lock" => ("󰌾", "Lock Screen", false),
                    "menu" => (" ▾", "Power Menu", false),
                    _ => ("", "Power Off (click again to confirm)", true),
                }
            };

            for idx in 0..count {
                let act = pcfg.slots[idx].clone();
                let (icon, tip, needs_confirm) = action_meta(&act);
                let lbl = Label::builder()
                    .label(icon)
                    .css_classes(vec!["power-btn-lbl".to_string()])
                    .build();
                let btn = Button::builder()
                    .child(&lbl)
                    .css_classes(vec!["power-btn".to_string()])
                    .tooltip_text(tip)
                    .build();

                slot_widgets.borrow_mut().push((btn.clone(), lbl.clone(), icon));

                if act == "menu" {
                    let guard_m = p_guard.clone();
                    let pcfg_m = pcfg.clone();
                    let btn_weak = btn.downgrade();
                    btn.connect_clicked(move |_| {
                        let Some(parent_btn) = btn_weak.upgrade() else {
                            return;
                        };
                        let pop = gtk4::Popover::builder()
                            .css_classes(vec!["fav-popover".to_string()])
                            .autohide(true)
                            .build();
                        pop.set_parent(&parent_btn);
                        pop.connect_closed(|p| {
                            let pw = p.downgrade();
                            glib::idle_add_local_once(move || {
                                if let Some(p) = pw.upgrade() {
                                    p.unparent();
                                }
                            });
                        });
                        let gm = guard_m.clone();
                        pop.connect_map(move |_| gm.set(gm.get() + 1));
                        let gu = guard_m.clone();
                        pop.connect_unmap(move |_| {
                            let g = gu.clone();
                            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                                g.set(g.get().saturating_sub(1));
                            });
                        });

                        let menu_box = Box::builder()
                            .orientation(Orientation::Vertical)
                            .spacing(4)
                            .build();

                        let items: &[(&str, &str, bool, bool)] = &[
                            ("lock", "󰌾  Lock", pcfg_m.menu_lock, false),
                            ("suspend", "󰤄  Suspend", pcfg_m.menu_suspend, false),
                            ("logout", "󰍃  Log Out", pcfg_m.menu_logout, true),
                            ("reboot", "󰜉  Reboot", pcfg_m.menu_reboot, true),
                            ("shutdown", "  Shutdown", pcfg_m.menu_shutdown, true),
                        ];

                        let mut added = 0;
                        for &(act_key, title, enabled, destructive) in items {
                            if !enabled {
                                continue;
                            }
                            added += 1;
                            let item_btn = Button::builder()
                                .label(title)
                                .css_classes(vec![if destructive {
                                    "plan-delete-menu-btn".to_string()
                                } else {
                                    "fav-hide-btn".to_string()
                                }])
                                .build();
                            let armed_item = Rc::new(Cell::new(false));
                            let pop_w = pop.downgrade();
                            item_btn.connect_clicked(move |b| {
                                if destructive && !armed_item.get() {
                                    armed_item.set(true);
                                    b.set_label(&format!("Sure? {}", title));
                                    let ai = armed_item.clone();
                                    let br = b.clone();
                                    glib::timeout_add_local_once(Duration::from_secs(3), move || {
                                        if ai.get() {
                                            ai.set(false);
                                            br.set_label(title);
                                        }
                                    });
                                    return;
                                }
                                if let Some(p) = pop_w.upgrade() {
                                    p.popdown();
                                }
                                config::run_power_action(act_key);
                            });
                            menu_box.append(&item_btn);
                        }

                        if added == 0 {
                            menu_box.append(
                                &Label::builder()
                                    .label("No menu actions enabled")
                                    .css_classes(vec!["plan-subnote".to_string()])
                                    .margin_top(4)
                                    .margin_bottom(4)
                                    .build(),
                            );
                        }

                        pop.set_child(Some(&menu_box));
                        pop.popup();
                    });
                } else if needs_confirm {
                    let armed_c = armed_slot.clone();
                    let gen_c = armed_gen.clone();
                    let all_w = slot_widgets.clone();
                    let act_s = act.clone();
                    let my_btn = btn.clone();
                    let my_lbl = lbl.clone();
                    btn.connect_clicked(move |_| {
                        if armed_c.get() == Some(idx) {
                            armed_c.set(None);
                            my_lbl.set_label(icon);
                            my_btn.remove_css_class("armed");
                            config::run_power_action(&act_s);
                            return;
                        }
                        armed_c.set(Some(idx));
                        let my_gen = gen_c.get().wrapping_add(1);
                        gen_c.set(my_gen);

                        for (i, (ob, ol, oicon)) in all_w.borrow().iter().enumerate() {
                            if i == idx {
                                ol.set_label("Sure?");
                                ob.add_css_class("armed");
                            } else {
                                ol.set_label(oicon);
                                ob.remove_css_class("armed");
                            }
                        }

                        let ar = armed_c.clone();
                        let gr = gen_c.clone();
                        let br = my_btn.clone();
                        let lr = my_lbl.clone();
                        glib::timeout_add_local_once(Duration::from_secs(3), move || {
                            if gr.get() == my_gen && ar.get() == Some(idx) {
                                ar.set(None);
                                lr.set_label(icon);
                                br.remove_css_class("armed");
                            }
                        });
                    });
                } else {
                    let act_s = act.clone();
                    btn.connect_clicked(move |_| {
                        config::run_power_action(&act_s);
                    });
                }

                p_row.append(&btn);
            }
        })
    };
    *power_rebuild_slot.borrow_mut() = Some(rebuild_power_row.clone());
    rebuild_power_row();

    // Create the weather tab AND the compact sidebar weather button
    let (weather_page, weather_sidebar_btn) = weather::build_weather_ui(&stack);
    
    // Add to stack using `add_named`. 
    // Using `add_named` (no title provided) tells GTK to intentionally HIDE it from the default text sidebar list!
    stack.add_named(&weather_page, Some("weather"));

    let weather_clock_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .build();

    let format_now = || {
        glib::DateTime::now_local()
            .ok()
            .and_then(|dt| dt.format("%H:%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "--:--".to_string())
    };
    let format_today_tip = || {
        let date_str = glib::DateTime::now_local()
            .ok()
            .and_then(|dt| dt.format("%a %d %b %Y").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let cfg = config::get();
        if cfg.tools.clock_click_mode == "custom" && !cfg.tools.clock_click_cmd.trim().is_empty() {
            format!("{}\nLeft-click: Custom clock action | Right-click: Open Calendar", date_str)
        } else {
            format!("{}\nClick: Open Calendar", date_str)
        }
    };

    let clock_lbl = Label::builder()
        .label(&format_now())
        .css_classes(vec!["sidebar-clock-lbl".to_string()])
        .build();

    let clock_btn = Button::builder()
        .child(&clock_lbl)
        .halign(gtk4::Align::End)
        .hexpand(true)
        .valign(gtk4::Align::Center)
        .css_classes(vec!["sidebar-footer-btn".to_string()])
        .tooltip_text(&format_today_tip())
        .build();

    // Left-click clock: run configured custom command or open built-in Calendar tab
    {
        let stack_clk = stack.clone();
        clock_btn.connect_clicked(move |_| {
            let cfg = config::get();
            if cfg.tools.clock_click_mode == "custom"
                && !cfg.tools.clock_click_cmd.trim().is_empty()
            {
                let _ = std::process::Command::new("sh")
                    .args(["-c", &cfg.tools.clock_click_cmd])
                    .spawn();
            } else {
                stack_clk.set_visible_child_name("calendar");
            }
        });
    }

    // Right-click clock: jump straight to the Calendar tab
    {
        let stack_cal = stack.clone();
        let rc_cal = GestureClick::builder().button(3).build();
        rc_cal.connect_pressed(move |_, _, _, _| {
            stack_cal.set_visible_child_name("calendar");
        });
        clock_btn.add_controller(rc_cal);
    }

    let clock_tick_lbl = clock_lbl.clone();
    let clock_tick_btn = clock_btn.clone();
    glib::timeout_add_seconds_local(5, move || {
        clock_tick_lbl.set_label(&format_now());
        clock_tick_btn.set_tooltip_text(Some(&format_today_tip()));
        glib::ControlFlow::Continue
    });

    weather_clock_row.append(&weather_sidebar_btn);
    weather_clock_row.append(&clock_btn);

    let footer_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .margin_start(6)
        .margin_end(6)
        .margin_bottom(8)
        .build();
    footer_box.append(&weather_clock_row);
    footer_box.append(&reload_btn);

    // Waybar replacements inside sidebar: Tray + Mouse Battery/Notifications
    let workspaces_widget = tray::build_workspaces_widget();
    let tray_widget = tray::build_system_tray();
    let status_strip = tray::build_status_strip();

    // Add everything to the sidebar column in order (module list scrolls; bottom chrome stays pinned!)
    sidebar_column.append(&sidebar_nav_scroll);
    sidebar_column.append(&tray_widget);
    sidebar_column.append(&status_strip);
    sidebar_column.append(&session_row);
    sidebar_column.append(&power_row);
    sidebar_column.append(&footer_box);

    let is_hovered = Rc::new(Cell::new(false));
    let is_open = Rc::new(Cell::new(false));
    let is_closing = Rc::new(Cell::new(false));

    let trigger_wrapper = Box::builder()
        .css_classes(vec!["trigger-wrapper".to_string()])
        .valign(gtk4::Align::Center)
        .build();

    let trigger_btn = Button::builder()
        .css_classes(vec!["trigger-btn".to_string()])
        .valign(gtk4::Align::Center)
        .can_target(false)
        .build();

    trigger_wrapper.append(&trigger_btn);

    let close_drawer_cb: Rc<dyn Fn()> = {
        let btn_c = trigger_btn.clone();
        let open_c = is_open.clone();
        Rc::new(move || {
            if open_c.get() {
                btn_c.emit_clicked();
            }
        })
    };

    // Quick Capture, Color Picker & optional Vim Mode pill on the right side of the top bar
    let vim_mode_badge = Label::builder()
        .label("NRM")
        .css_classes(vec!["vim-mode-pill".to_string()])
        .valign(gtk4::Align::Center)
        .tooltip_text("Vim Mode: NRM (Normal) / INS (Insert — Esc to exit)")
        .visible(false)
        .build();
    {
        let top_tools = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(5)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Center)
            .build();

        let sticky_btn = Button::builder()
            .label("󰠮")
            .css_classes(vec!["top-tool-btn".to_string()])
            .tooltip_text("New Sticky Note (only visible while dashboard is open)")
            .build();
        let stickies_new = stickies_handle.clone();
        sticky_btn.connect_clicked(move |_| {
            stickies_new.new_sticky();
        });

        let cap_btn = Button::builder()
            .label("󰄀")
            .css_classes(vec!["top-tool-btn".to_string()])
            .tooltip_text("Screenshot\nLeft-click: Region / Editor  •  Right-click: Active Monitor")
            .build();
        let close_cap_l = close_drawer_cb.clone();
        cap_btn.connect_clicked(move |_| {
            module_api::launch_capture(module_api::CaptureMode::RegionEditor, &close_cap_l);
        });
        let cap_rc = GestureClick::builder().button(3).build();
        let close_cap_r = close_drawer_cb.clone();
        cap_rc.connect_pressed(move |_, _, _, _| {
            module_api::launch_capture(module_api::CaptureMode::Monitor, &close_cap_r);
        });
        cap_btn.add_controller(cap_rc);

        let picker_btn = Button::builder()
            .label("󰈊")
            .css_classes(vec!["top-tool-btn".to_string()])
            .tooltip_text("Color Picker (copies #hex to clipboard)")
            .build();
        let close_pick = close_drawer_cb.clone();
        picker_btn.connect_clicked(move |_| {
            module_api::launch_color_picker(&close_pick);
        });

        top_tools.append(&vim_mode_badge);
        top_tools.append(&sticky_btn);
        top_tools.append(&cap_btn);
        top_tools.append(&picker_btn);
        workspaces_widget.append(&top_tools);
    }

    // ==========================================
    // DYNAMIC MODULAR TAB REGISTRY (`modules/*.rs`)
    // ==========================================
    let compiled_modules = modules_gen::all_modules();
    let module_slots: Rc<std::collections::HashMap<String, Box>> = {
        let mut map = std::collections::HashMap::new();
        for desc in &compiled_modules {
            let slot = Box::builder()
                .orientation(Orientation::Vertical)
                .hexpand(true)
                .vexpand(true)
                .build();
            stack.add_titled(&slot, Some(desc.id), desc.title);
            map.insert(desc.id.to_string(), slot);
        }
        if compiled_modules.is_empty() {
            let empty_box = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(8)
                .valign(gtk4::Align::Center)
                .margin_start(16)
                .margin_end(16)
                .build();
            empty_box.append(
                &Label::builder()
                    .label("No modules installed")
                    .css_classes(vec!["plan-h2".to_string()])
                    .build(),
            );
            empty_box.append(
                &Label::builder()
                    .label("Drop `.rs` modules into `modules/` and click Reload Dashboard.")
                    .wrap(true)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
            stack.add_titled(&empty_box, Some("empty"), "Empty");
        }
        Rc::new(map)
    };

    let loaded_handles: Rc<RefCell<std::collections::HashMap<String, module_api::ModuleHandle>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    let module_ctx = module_api::ModuleContext {
        popover_guard: popover_guard.clone(),
        close_drawer: close_drawer_cb.clone(),
        reload_theme: reload_theme.clone(),
        apply_tabs: apply_tab_visibility.clone(),
        switch_tab: {
            let stk_sw = stack.clone();
            Rc::new(move |id: &str| {
                stk_sw.set_visible_child_name(id);
            })
        },
    };

    // Event-driven 4-frame geometry burst synchronizer (replaces permanent VSync tick loops so
    // GdkFrameClock rests at 0 Hz whenever the dashboard is idle or closed).
    let schedule_geometry_sync: Rc<dyn Fn()> = {
        let burst_active = Rc::new(Cell::new(false));
        let burst_frames = Rc::new(Cell::new(0u8));
        let last_dash_width = Rc::new(Cell::new(0));
        let last_side_right = Rc::new(Cell::new(config::get().layout.side == "right"));
        let last_win_height = Rc::new(Cell::new(0));
        let last_hitbox_width = Rc::new(Cell::new(0));
        let last_hitbox_open = Rc::new(Cell::new(false));

        let win_g = window.clone();
        let db_g = dash_box.clone();
        let open_g = is_open.clone();
        let dyn_prov = dynamic_offset_provider.clone();

        Rc::new(move || {
            burst_frames.set(5);
            if burst_active.get() {
                return;
            }
            burst_active.set(true);

            let ba = burst_active.clone();
            let bf = burst_frames.clone();
            let ldw = last_dash_width.clone();
            let lsr = last_side_right.clone();
            let lwh = last_win_height.clone();
            let lhw = last_hitbox_width.clone();
            let lho = last_hitbox_open.clone();
            let db = db_g.clone();
            let op = open_g.clone();
            let dp = dyn_prov.clone();

            win_g.add_tick_callback(move |win, _| {
                let w = db.width();
                let h = win.height();
                let is_right = config::get().layout.side == "right";
                let open_now = op.get();

                if w > 0 && (w != ldw.get() || is_right != lsr.get()) {
                    ldw.set(w);
                    lsr.set(is_right);
                    let push_back = w + 10;
                    let closed_val = if is_right {
                        format!("{}px", push_back)
                    } else {
                        format!("-{}px", push_back)
                    };
                    dp.load_from_data(&format!(
                        ".drawer-container {{ transform: translateX({}); }}
                         .drawer-container.open {{ transform: translateX(0px); }}",
                        closed_val
                    ));
                }

                if h > 0 && (h != lwh.get() || w != lhw.get() || open_now != lho.get()) {
                    lwh.set(h);
                    lhw.set(w);
                    lho.set(open_now);
                    if let Some(app_win) = win.downcast_ref::<ApplicationWindow>() {
                        update_input_region(app_win, open_now, w);
                    }
                }

                let rem = bf.get().saturating_sub(1);
                bf.set(rem);
                if rem == 0 {
                    ba.set(false);
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        })
    };

    let dash_for_mods = dash_box.clone();
    let nav_buttons_sync = nav_buttons_rc.clone();
    let nav_folders_sync = nav_folders_rc.clone();
    let slots_sync = module_slots.clone();
    let handles_sync = loaded_handles.clone();
    let ctx_sync = module_ctx.clone();
    let mods_sync = compiled_modules.clone();
    let win_for_mods = window.clone();
    let sb_for_mods = sidebar_clamp.clone();
    let geom_for_mods = schedule_geometry_sync.clone();
    let open_for_mods = is_open.clone();
    let prev_tab_snd: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    stack.connect_visible_child_name_notify(move |stk| {
        let current = stk.visible_child_name();
        let name = current.as_deref();
        if let Some(n) = name {
            let mut prev = prev_tab_snd.borrow_mut();
            if open_for_mods.get() && prev.as_deref() != Some(n) {
                sounds::play(sounds::SoundEvent::TabSwitch);
            }
            *prev = Some(n.to_string());
        }

        let mut active_btn_widget: Option<gtk4::Widget> = None;
        for (id, btn) in nav_buttons_sync.borrow().iter() {
            if name == Some(id.as_str()) {
                btn.add_css_class("selected");
                if btn.is_mapped() {
                    active_btn_widget = Some(btn.clone().upcast());
                }
            } else {
                btn.remove_css_class("selected");
            }
        }
        for (fbtn, fbox, child_ids) in nav_folders_sync.borrow().iter() {
            let is_collapsed = !fbox.is_visible();
            let contains_active = name
                .map(|a| child_ids.iter().any(|cid| cid == a))
                .unwrap_or(false);
            if is_collapsed && contains_active {
                fbtn.add_css_class("has-active");
                active_btn_widget = Some(fbtn.clone().upcast());
            } else {
                fbtn.remove_css_class("has-active");
            }
        }
        if let Some(ref w) = active_btn_widget {
            vim_scroll_into_view(w);
        }

        // Hide inactive detail panes, invoke on_hide, and unload stateless modules
        {
            let mut handles = handles_sync.borrow_mut();
            let mut to_remove = Vec::new();
            for desc in &mods_sync {
                if name == Some(desc.id) {
                    continue;
                }
                if let Some(h) = handles.get(desc.id) {
                    if let Some(ref dp) = h.detail_pane {
                        dp.set_visible(false);
                    }
                    if let Some(ref hide_cb) = h.on_hide {
                        hide_cb();
                    }
                    if desc.lifecycle == module_api::ModuleLifecycle::Stateless {
                        if let Some(slot) = slots_sync.get(desc.id) {
                            popdown_all_popovers(slot.upcast_ref());
                            while let Some(child) = slot.first_child() {
                                slot.remove(&child);
                            }
                        }
                        if let Some(ref dp) = h.detail_pane {
                            dash_for_mods.remove(dp);
                        }
                        to_remove.push(desc.id.to_string());
                    }
                }
            }
            for id in to_remove {
                handles.remove(&id);
            }
        }

        // Build active module lazily on demand and invoke on_show
        if let Some(active_id) = name {
            if let Some(desc) = mods_sync.iter().find(|m| m.id == active_id) {
                let needs_build = !handles_sync.borrow().contains_key(active_id);
                if needs_build {
                    let handle = (desc.build)(&ctx_sync);
                    if let Some(slot) = slots_sync.get(active_id) {
                        slot.append(&handle.widget);
                    }
                    if let Some(ref dp) = handle.detail_pane {
                        let g_dp = geom_for_mods.clone();
                        let open_dp_snd = open_for_mods.clone();
                        let prev_dp_vis = Cell::new(dp.is_visible());
                        dp.connect_visible_notify(move |pane| {
                            let vis = pane.is_visible();
                            if vis != prev_dp_vis.get() {
                                prev_dp_vis.set(vis);
                                if open_dp_snd.get() {
                                    sounds::play(if vis {
                                        sounds::SoundEvent::PaneOpen
                                    } else {
                                        sounds::SoundEvent::PaneClose
                                    });
                                }
                            }
                            g_dp();
                        });
                        if config::get().layout.side == "right" {
                            dash_for_mods.prepend(dp);
                        } else {
                            dash_for_mods.append(dp);
                        }
                    }
                    handles_sync.borrow_mut().insert(active_id.to_string(), handle);
                }
                if let Some(h) = handles_sync.borrow().get(active_id) {
                    if let Some(ref show_cb) = h.on_show {
                        show_cb();
                    }
                }
                let vcfg = config::get().bindings.vim;
                if vcfg.enabled && vcfg.start_in_normal {
                    let w_n = win_for_mods.clone();
                    let s_n = stk.clone();
                    let sb_n = sb_for_mods.clone();
                    glib::idle_add_local_once(move || {
                        vim_exit_insert_to_normal(&w_n, &s_n, sb_n.upcast_ref());
                    });
                }
            }
        }
        geom_for_mods();
    });

    let stack_clamp = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_width(false)
        .min_content_width(288)
        .max_content_width(288)
        .width_request(288)
        .has_frame(false)
        .hexpand(false)
        .vexpand(true)
        .css_classes(vec!["internal-layout-clamp".to_string()])
        .child(&stack)
        .build();

    let main_col = Box::builder()
        .orientation(Orientation::Vertical)
        .width_request(398)
        .vexpand(true)
        .build();

    let body_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .vexpand(true)
        .build();

    body_row.append(&sidebar_clamp);
    body_row.append(&stack_clamp);

    main_col.append(&workspaces_widget);
    main_col.append(&body_row);

    dash_box.append(&main_col);

    // Default to the user's configured default tab (or first installed module, or "empty")
    let startup_tab = config::get().layout.default_tab;
    let chosen_tab = if compiled_modules.iter().any(|m| m.id == startup_tab.as_str()) {
        startup_tab
    } else if let Some(first) = compiled_modules.first() {
        first.id.to_string()
    } else {
        "empty".to_string()
    };
    stack.set_visible_child_name(&chosen_tab);

    // Live Layout & Chrome applier (full left/right mirror, flush trigger, monitor pin,
    // panel width, and individual chrome toggles)
    let apply_layout_and_chrome: Rc<dyn Fn()> = {
        let win_l = window.clone();
        let dc_l = drawer_container.clone();
        let db_l = dash_box.clone();
        let mc_l = main_col.clone();
        let br_l = body_row.clone();
        let sbc_l = sidebar_clamp.clone();
        let sc_l = stack_clamp.clone();
        let stk_l = stack.clone();
        let tw_l = trigger_wrapper.clone();
        let tb_l = trigger_btn.clone();
        let open_l = is_open.clone();
        let handles_l = loaded_handles.clone();
        let ws_w = workspaces_widget.clone();
        let tr_w = tray_widget.clone();
        let st_w = status_strip.clone();
        let sr_w = session_row.clone();
        let pr_w = power_row.clone();
        let we_w = weather_sidebar_btn.clone();
        let cl_w = clock_btn.clone();
        let rl_w = reload_btn.clone();
        let vb_w = vim_mode_badge.clone();
        let geom_l = schedule_geometry_sync.clone();

        Rc::new(move || {
            let cfg = config::get();
            let lay = &cfg.layout;
            let is_right = lay.side == "right";

            // 1. Anchor & mirror entire layout (trigger + detail panes + sidebar column!)
            win_l.set_anchor(Edge::Left, !is_right);
            win_l.set_anchor(Edge::Right, is_right);
            dc_l.set_halign(if is_right {
                gtk4::Align::End
            } else {
                gtk4::Align::Start
            });
            tb_l.set_halign(if is_right {
                gtk4::Align::End
            } else {
                gtk4::Align::Start
            });
            if is_right {
                dc_l.reorder_child_after(&db_l, Some(&tw_l));
                br_l.reorder_child_after(&sbc_l, Some(&sc_l));
                for h in handles_l.borrow().values() {
                    if let Some(ref dp) = h.detail_pane {
                        db_l.reorder_child_after(&mc_l, Some(dp));
                    }
                }
            } else {
                dc_l.reorder_child_after(&tw_l, Some(&db_l));
                br_l.reorder_child_after(&sc_l, Some(&sbc_l));
                for h in handles_l.borrow().values() {
                    if let Some(ref dp) = h.detail_pane {
                        db_l.reorder_child_after(dp, Some(&mc_l));
                    }
                }
            }

            // 2. Trigger vertical alignment
            let valign = match lay.trigger_align.as_str() {
                "top" => gtk4::Align::Start,
                "bottom" => gtk4::Align::End,
                _ => gtk4::Align::Center,
            };
            tw_l.set_valign(valign);
            tw_l.set_margin_top(if lay.trigger_align == "top" { 16 } else { 0 });
            tw_l.set_margin_bottom(if lay.trigger_align == "bottom" { 16 } else { 0 });

            // 3. Panel width
            let pw = lay.panel_width.clamp(340, 560);
            let stack_w = (pw - 112).max(220);
            db_l.set_width_request(pw);
            mc_l.set_width_request(pw - 2);
            sc_l.set_min_content_width(stack_w);
            sc_l.set_max_content_width(stack_w);
            sc_l.set_width_request(stack_w);
            stk_l.set_size_request(stack_w, -1);
            win_l.set_size_request((pw + 456).max(TOTAL_DASH_WIDTH) + TRIGGER_WIDTH, -1);

            // 4. Chrome toggles & Vim mode indicator
            ws_w.set_visible(lay.chrome.show_top_bar);
            tr_w.set_visible(lay.chrome.show_tray);
            st_w.set_visible(lay.chrome.show_status_strip);
            sr_w.set_visible(lay.chrome.show_session_row);
            pr_w.set_visible(lay.chrome.show_power_row);
            we_w.set_visible(lay.chrome.show_weather);
            cl_w.set_visible(lay.chrome.show_clock);
            rl_w.set_visible(lay.chrome.show_reload_btn);
            vim_sync_indicator(&win_l, &vb_w, &db_l);

            // 5. Monitor pinning & hitbox refresh
            move_to_monitor(&win_l, None);
            update_input_region(&win_l, open_l.get(), db_l.width());
            geom_l();
        })
    };
    *layout_rebuild_slot.borrow_mut() = Some(apply_layout_and_chrome.clone());

    {
        let vb_f = vim_mode_badge.clone();
        let db_f = dash_box.clone();
        window.connect_focus_widget_notify(move |win| {
            vim_sync_indicator(win, &vb_f, &db_f);
        });
    }

    let win_motion = EventControllerMotion::new();
    let btn_motion = trigger_btn.clone();
    let hovered_motion = is_hovered.clone();
    let open_motion = is_open.clone();
    let closing_motion = is_closing.clone();

    win_motion.connect_enter(move |_, _, _| {
        if !open_motion.get() && !closing_motion.get() {
            hovered_motion.set(true);
            btn_motion.add_css_class("peeking");
            if config::get().layout.trigger_hover_open {
                btn_motion.emit_clicked();
            }
        }
    });

    let btn_leave = trigger_btn.clone();
    let hovered_leave = is_hovered.clone();
    win_motion.connect_leave(move |_| {
        hovered_leave.set(false);
        let btn_delayed = btn_leave.clone();
        let hovered_check = hovered_leave.clone();
        glib::timeout_add_local_once(Duration::from_millis(100), move || {
            if !hovered_check.get() {
                btn_delayed.remove_css_class("peeking");
            }
        });
    });
    window.add_controller(win_motion);

    // DYNAMIC CLOSE-CLICK HANDLER
    let win_click = GestureClick::builder().button(1).build();
    let btn_win_click = trigger_btn.clone();
    let open_win_click = is_open.clone();
    let dash_box_click = dash_box.clone();
    let win_for_click = window.clone();
    win_click.connect_pressed(move |_, _, x, _| {
        if !open_win_click.get() {
            btn_win_click.emit_clicked();
        } else {
            let current_w = dash_box_click.width() as f64;
            let dash_edge = if current_w > 0.0 { current_w } else { BASE_DASH_WIDTH as f64 };
            let is_right = config::get().layout.side == "right";
            let clicked_trigger = if is_right {
                let win_w = win_for_click.width() as f64;
                x <= (win_w - dash_edge)
            } else {
                x >= dash_edge
            };
            if clicked_trigger {
                btn_win_click.emit_clicked();
            }
        }
    });
    window.add_controller(win_click);

    let drawer_clone = drawer_container.clone();
    let handles_toggle = loaded_handles.clone();
    let media_for_toggle = media_handle.clone();
    let stickies_for_toggle = stickies_handle.clone();
    let stack_for_open = stack.clone();
    let sidebar_for_open = sidebar_clamp.clone();
    let window_clone = window.clone();
    let hovered_click = is_hovered.clone();
    let open_click = is_open.clone();
    let closing_click = is_closing.clone();
    let dash_box_btn = dash_box.clone();
    let sidebar_col_btn = sidebar_column.clone();
    let ws_widget_btn = workspaces_widget.clone();
    let slide_gen = Rc::new(Cell::new(0u64));

    trigger_btn.connect_clicked(move |btn| {
        let currently_open = open_click.get();
        let cur_gen = slide_gen.get().wrapping_add(1);
        slide_gen.set(cur_gen);

        if !currently_open {
            move_to_monitor(&window_clone, None);
            open_click.set(true);
            closing_click.set(false);
            sounds::play(sounds::SoundEvent::DashOpen);
            media_for_toggle.set_dashboard_open(true);
            stickies_for_toggle.set_dashboard_open(true);

            // Keep `.dash-hibernated` ON during the slide transition! Because all child CSS
            // animations & transitions inside `dash_box` stay frozen while `.drawer-container`
            // animates `translateX`, GTK4 reuses `dash_box`'s cached `GskRenderNode` in O(1)
            // on every frame of the slide instead of re-shading gradients & blurring shadows!
            if !dash_box_btn.has_css_class("dash-hibernated") {
                dash_box_btn.add_css_class("dash-hibernated");
            }

            // Use Exclusive keyboard interactivity while open so pressing SUPER immediately
            // steals and holds keyboard focus even when the cursor is over another window
            // (Hyprland follow_mouse won't steal it back). Closing resets to None.
            window_clone.set_keyboard_mode(KeyboardMode::Exclusive);
            update_input_region(&window_clone, true, dash_box_btn.width());
            window_clone.present();

            if let Some(cur) = stack_for_open.visible_child_name() {
                if let Some(h) = handles_toggle.borrow().get(cur.as_str()) {
                    if let Some(ref show_cb) = h.on_show {
                        show_cb();
                    }
                }
            }
            {
                let vcfg = config::get().bindings.vim;
                if vcfg.enabled && vcfg.start_in_normal {
                    vim_exit_insert_to_normal(
                        &window_clone,
                        &stack_for_open,
                        sidebar_for_open.upcast_ref(),
                    );
                }
            }

            btn.add_css_class("riding");
            drawer_clone.add_css_class("open");

            let open_ms = config::get().layout.anim_duration_ms.max(16);
            let gen_chk = slide_gen.clone();
            let open_chk = open_click.clone();
            let db_open_done = dash_box_btn.clone();
            let sb_settle = sidebar_col_btn.clone();
            let ws_settle = ws_widget_btn.clone();
            glib::timeout_add_local_once(Duration::from_millis(open_ms), move || {
                if gen_chk.get() == cur_gen && open_chk.get() {
                    // Unfreeze interactive hover/active animations now that the slide has landed!
                    db_open_done.remove_css_class("dash-hibernated");
                    // Settle persistent chrome once after first open so future drawer toggles
                    // never re-trigger 1-shot mount @keyframes (`hud-lock-in` / `cyber-unfold`).
                    if !sb_settle.has_css_class("lily-settled") {
                        let sb_s = sb_settle.clone();
                        let ws_s = ws_settle.clone();
                        glib::timeout_add_local_once(Duration::from_millis(360), move || {
                            sb_s.add_css_class("lily-settled");
                            ws_s.add_css_class("lily-settled");
                        });
                    }
                }
            });
        } else {
            open_click.set(false);
            closing_click.set(true);
            hovered_click.set(false);
            sounds::play(sounds::SoundEvent::DashClose);
            media_for_toggle.set_dashboard_open(false);
            stickies_for_toggle.set_dashboard_open(false);

            // Freeze child animations & transitions BEFORE Frame 1 of the close slide so GTK4
            // slides the retained `GskRenderNode` in O(1) without ticking blurred box-shadows!
            dash_box_btn.add_css_class("dash-hibernated");

            // Cleanly dismiss every open Popover (right-click menus, Entry context menus, etc.)
            // BEFORE shrinking the layer-shell input region so GTK4 never keeps an orphaned
            // Wayland popup pointer grab that breaks clicking.
            popdown_all_popovers(window_clone.upcast_ref());

            for h in handles_toggle.borrow().values() {
                if let Some(ref dp) = h.detail_pane {
                    if dp.is_visible() {
                        dp.set_visible(false);
                    }
                }
                if let Some(ref hide_cb) = h.on_hide {
                    hide_cb();
                }
            }
            window_clone.set_keyboard_mode(KeyboardMode::None);
            drawer_clone.remove_css_class("open");
            btn.remove_css_class("peeking");

            let btn_after = btn.clone();
            let win_after = window_clone.clone();
            let open_after = open_click.clone();
            let closing_after = closing_click.clone();
            let gen_chk = slide_gen.clone();

            let close_ms = config::get().layout.anim_duration_ms.max(16);
            glib::timeout_add_local_once(Duration::from_millis(close_ms), move || {
                if gen_chk.get() == cur_gen {
                    closing_after.set(false);
                    if !open_after.get() {
                        btn_after.remove_css_class("riding");
                        update_input_region(&win_after, false, 0);
                    }
                }
            });
        }
    });

    let key_controller = EventControllerKey::new();
    key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let btn_esc = trigger_btn.clone();
    let open_esc = is_open.clone();
    let stack_kbd = stack.clone();
    let win_kbd = window.clone();
    let sb_kbd = sidebar_clamp.clone();
    let handles_kbd = loaded_handles.clone();
    let pending_chord: Rc<RefCell<Option<(char, std::time::Instant)>>> =
        Rc::new(RefCell::new(None));

    key_controller.connect_key_pressed(move |_, key, _, mods| {
        if !open_esc.get() {
            return glib::Propagation::Proceed;
        }
        let bcfg = config::get().bindings;

        // 1. Ctrl/Alt + 1..9 Quick Tab Jump (works in both Normal and Insert modes)
        if bcfg.tab_jump_enabled {
            let want_mod = if bcfg.tab_jump_mod.eq_ignore_ascii_case("alt") {
                mods.contains(gdk::ModifierType::ALT_MASK)
            } else {
                mods.contains(gdk::ModifierType::CONTROL_MASK)
            };
            if want_mod {
                let digit = match key {
                    gdk::Key::_1 => Some(0usize),
                    gdk::Key::_2 => Some(1),
                    gdk::Key::_3 => Some(2),
                    gdk::Key::_4 => Some(3),
                    gdk::Key::_5 => Some(4),
                    gdk::Key::_6 => Some(5),
                    gdk::Key::_7 => Some(6),
                    gdk::Key::_8 => Some(7),
                    gdk::Key::_9 => Some(8),
                    _ => None,
                };
                if let Some(idx) = digit {
                    *pending_chord.borrow_mut() = None;
                    let loaded_tabs: Vec<String> = config::get()
                        .layout
                        .sidebar_items
                        .into_iter()
                        .filter(|it| config::is_sidebar_tab_id(it))
                        .collect();
                    if let Some(tab_id) = loaded_tabs.get(idx) {
                        stack_kbd.set_visible_child_name(tab_id);
                        return glib::Propagation::Stop;
                    }
                }
            }
        }

        // 2. Classic non-Vim behavior when Vim mode is disabled
        if !bcfg.vim.enabled {
            if key == gdk::Key::Escape && bcfg.esc_to_close {
                btn_esc.emit_clicked();
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }

        // 3. Modal Vim Navigation Engine
        if vim_handle_key_press(
            key,
            mods,
            &bcfg,
            &win_kbd,
            &stack_kbd,
            sb_kbd.upcast_ref(),
            &handles_kbd,
            &btn_esc,
            &pending_chord,
        ) {
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_controller);

    // Passive global UI sound triggers across all modules (hover + click + destructive click)
    {
        fn find_interactive_widget(root: &gtk4::Widget, x: f64, y: f64) -> Option<gtk4::Widget> {
            let mut cur = root.pick(x, y, gtk4::PickFlags::DEFAULT);
            while let Some(w) = cur {
                if w == *root {
                    break;
                }
                if w.is::<Button>() || w.is::<gtk4::CheckButton>() || w.is::<gtk4::Switch>() {
                    if w.is_sensitive() && w.is_visible() {
                        return Some(w);
                    }
                    return None;
                }
                cur = w.parent();
            }
            None
        }

        let last_hovered: Rc<RefCell<Option<glib::WeakRef<gtk4::Widget>>>> =
            Rc::new(RefCell::new(None));
        let hover_motion = EventControllerMotion::new();
        {
            let db_m = dash_box.clone();
            let open_m = is_open.clone();
            let lh_m = last_hovered.clone();
            hover_motion.connect_motion(move |_, x, y| {
                if !open_m.get() || db_m.has_css_class("dash-hibernated") {
                    return;
                }
                let hit = find_interactive_widget(db_m.upcast_ref(), x, y);
                let mut slot = lh_m.borrow_mut();
                match hit {
                    Some(w) => {
                        let same = slot
                            .as_ref()
                            .and_then(|wr| wr.upgrade())
                            .map(|prev| prev == w)
                            .unwrap_or(false);
                        if !same {
                            *slot = Some(w.downgrade());
                            sounds::play(sounds::SoundEvent::ButtonHover);
                        }
                    }
                    None => {
                        *slot = None;
                    }
                }
            });
        }
        {
            let lh_l = last_hovered;
            hover_motion.connect_leave(move |_| {
                *lh_l.borrow_mut() = None;
            });
        }
        dash_box.add_controller(hover_motion);

        let click_snd = GestureClick::builder()
            .button(1)
            .propagation_phase(gtk4::PropagationPhase::Capture)
            .build();
        let db_c = dash_box.clone();
        let open_c = is_open.clone();
        click_snd.connect_pressed(move |_, _, x, y| {
            if !open_c.get() {
                return;
            }
            let Some(w) = find_interactive_widget(db_c.upcast_ref(), x, y) else {
                return;
            };
            if w.has_css_class("sidebar-nav-btn") {
                return;
            }
            if w.has_css_class("plan-delete-menu-btn")
                || w.has_css_class("plan-line-del-btn")
                || w.has_css_class("power-btn") && w.has_css_class("armed")
            {
                sounds::play(sounds::SoundEvent::DestructiveClick);
            } else {
                sounds::play(sounds::SoundEvent::ButtonClick);
            }
        });
        dash_box.add_controller(click_snd);
    }

    drawer_container.append(&dash_box);
    drawer_container.append(&trigger_wrapper);

    window.set_child(Some(&drawer_container));
    apply_layout_and_chrome();

    // Event-driven Wayland surface resize hooks (replaces permanent win_tick frame-clock polling!)
    {
        let geom_realize = schedule_geometry_sync.clone();
        window.connect_realize(move |win| {
            geom_realize();
            if let Some(surface) = win.surface() {
                let g_w = geom_realize.clone();
                surface.connect_width_notify(move |_| g_w());
                let g_h = geom_realize.clone();
                surface.connect_height_notify(move |_| g_h());
            }
        });
    }

    // --- START HYPRLAND IPC LISTENER ---
    enum HyprIpcEvent {
        Monitor(String),
        ExternalFocus,
    }
    let (tx_ipc, rx_ipc) = std::sync::mpsc::channel::<HyprIpcEvent>();

    std::thread::spawn(move || {
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default();
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
        let sock_path = format!("{}/hypr/{}/.socket2.sock", runtime_dir, signature);

        loop {
            if let Ok(stream) = std::os::unix::net::UnixStream::connect(&sock_path) {
                let reader = std::io::BufReader::new(stream);
                for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
                    if let Some(rest) = line.strip_prefix("focusedmon>>") {
                        if let Some((mon_name, _)) = rest.split_once(',') {
                            let _ = tx_ipc.send(HyprIpcEvent::Monitor(mon_name.to_string()));
                        }
                    } else if line.starts_with("activewindowv2>>")
                        || line.starts_with("workspace>>")
                        || line.starts_with("openwindow>>")
                    {
                        let _ = tx_ipc.send(HyprIpcEvent::ExternalFocus);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });

    let win_focus = window.clone();
    let open_focus = is_open.clone();
    let dash_box_ipc = dash_box.clone();

    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        let mut latest_mon: Option<String> = None;
        let mut yield_focus = false;
        while let Ok(ev) = rx_ipc.try_recv() {
            match ev {
                HyprIpcEvent::Monitor(mon) => latest_mon = Some(mon),
                HyprIpcEvent::ExternalFocus => yield_focus = true,
            }
        }

        if yield_focus && open_focus.get() {
            win_focus.set_keyboard_mode(KeyboardMode::OnDemand);
        }

        if let Some(mon_name) = latest_mon {
            move_to_monitor(&win_focus, Some(&mon_name));

            let win_region = win_focus.clone();
            let o = open_focus.get();
            let db_w = dash_box_ipc.width();

            glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                update_input_region(&win_region, o, db_w);
            });
        }
        glib::ControlFlow::Continue
    });
    // --- END HYPRLAND IPC LISTENER ---

    // Map the window hitbox dynamically so it doesn't break if you move screens while it's open
    let open_map = is_open.clone();
    let dash_box_map = dash_box.clone();
    let geom_map = schedule_geometry_sync.clone();
    window.connect_map(move |win| {
        update_input_region(win, open_map.get(), dash_box_map.width());
        geom_map();
    });

    // Ensure it's on the right screen as soon as you boot the dashboard
    move_to_monitor(&window, None);
    
    window.present();

    DashHandle {
        trigger_btn,
        is_open,
        stack,
        reload_theme,
    }
}

// ==========================================
// MODAL VIM NAVIGATION ENGINE
// ==========================================

fn vim_is_text_input(w: &gtk4::Widget) -> bool {
    let mut cur = Some(w.clone());
    for _ in 0..4 {
        let Some(node) = cur else { break };
        if node.is::<gtk4::Text>()
            || node.is::<gtk4::Entry>()
            || node.is::<gtk4::SearchEntry>()
            || node.is::<gtk4::PasswordEntry>()
            || node.is::<gtk4::SpinButton>()
            || node.is::<gtk4::TextView>()
        {
            return true;
        }
        cur = node.parent();
    }
    false
}

thread_local! {
    static VIM_PREV_INSERT_MODE: Cell<Option<bool>> = const { Cell::new(None) };
}

fn vim_sync_indicator(win: &ApplicationWindow, badge: &Label, dash_box: &Box) {
    let vcfg = config::get().bindings.vim;
    if vcfg.enabled {
        dash_box.add_css_class("vim-enabled");
    } else {
        dash_box.remove_css_class("vim-enabled");
    }
    badge.set_visible(vcfg.enabled && vcfg.show_mode_badge);
    if vcfg.enabled {
        let is_ins = GtkWindowExt::focus(win)
            .as_ref()
            .map(vim_is_text_input)
            .unwrap_or(false);
        if is_ins {
            badge.set_label("INS");
            badge.add_css_class("insert");
        } else {
            badge.set_label("NRM");
            badge.remove_css_class("insert");
        }
        let drawer_open = dash_box
            .parent()
            .map(|p| p.has_css_class("open"))
            .unwrap_or(false);
        VIM_PREV_INSERT_MODE.with(|slot| {
            if let Some(prev) = slot.get() {
                if prev != is_ins && drawer_open {
                    sounds::play(sounds::SoundEvent::VimMode);
                }
            }
            slot.set(Some(is_ins));
        });
    }
}

fn vim_find_open_popover(root: &gtk4::Widget) -> Option<gtk4::Popover> {
    if let Some(pop) = root.downcast_ref::<gtk4::Popover>() {
        if pop.is_visible() {
            return Some(pop.clone());
        }
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        if c.is_visible() {
            if let Some(found) = vim_find_open_popover(&c) {
                return Some(found);
            }
        }
        child = next;
    }
    None
}

fn vim_collect_buttons(root: &gtk4::Widget, allow_root_popover: bool, out: &mut Vec<gtk4::Widget>) {
    if !root.is_visible() || !root.is_sensitive() {
        return;
    }
    if !allow_root_popover && root.is::<gtk4::Popover>() {
        return;
    }
    if root.is::<gtk4::SearchEntry>()
        || root.is::<gtk4::Entry>()
        || root.is::<gtk4::PasswordEntry>()
        || root.is::<gtk4::SpinButton>()
        || root.is::<gtk4::TextView>()
        || root.is::<gtk4::Text>()
    {
        return;
    }
    if root.is::<gtk4::Button>()
        || root.is::<gtk4::ToggleButton>()
        || root.is::<gtk4::CheckButton>()
        || root.is::<gtk4::Switch>()
        || root.is::<gtk4::Scale>()
        || root.is::<gtk4::DropDown>()
        || root.is::<gtk4::Calendar>()
    {
        if root.can_focus() && root.is_mapped() {
            out.push(root.clone());
        }
        return;
    }
    if let Some(stk) = root.downcast_ref::<gtk4::Stack>() {
        if let Some(vis) = stk.visible_child() {
            vim_collect_buttons(&vis, false, out);
        }
        return;
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        vim_collect_buttons(&c, false, out);
        child = next;
    }
}

fn vim_collect_text_inputs(
    root: &gtk4::Widget,
    searches: &mut Vec<gtk4::Widget>,
    others: &mut Vec<gtk4::Widget>,
) {
    if !root.is_visible() || !root.is_sensitive() || root.is::<gtk4::Popover>() {
        return;
    }
    if root.is::<gtk4::SearchEntry>() {
        if root.is_mapped() {
            searches.push(root.clone());
        }
        return;
    }
    if root.is::<gtk4::Entry>()
        || root.is::<gtk4::PasswordEntry>()
        || root.is::<gtk4::SpinButton>()
        || root.is::<gtk4::TextView>()
    {
        if root.is_mapped() {
            others.push(root.clone());
        }
        return;
    }
    if let Some(stk) = root.downcast_ref::<gtk4::Stack>() {
        if let Some(vis) = stk.visible_child() {
            vim_collect_text_inputs(&vis, searches, others);
        }
        return;
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        vim_collect_text_inputs(&c, searches, others);
        child = next;
    }
}

fn vim_scroll_into_view(target: &gtk4::Widget) {
    let mut cur = target.parent();
    while let Some(p) = cur {
        if let Some(sw) = p.downcast_ref::<gtk4::ScrolledWindow>() {
            if sw.vscrollbar_policy() != gtk4::PolicyType::Never {
                if let Some(content) = sw.child() {
                    if let Some(bounds) = target.compute_bounds(&content) {
                        let vadj = sw.vadjustment();
                        let page = vadj.page_size();
                        let cur_v = vadj.value();
                        let top = bounds.y() as f64;
                        let bot = (bounds.y() + bounds.height()) as f64;
                        let pad = 12.0;
                        if top - pad < cur_v {
                            vadj.set_value((top - pad).max(vadj.lower()));
                        } else if bot + pad > cur_v + page && page > 0.0 {
                            let max_v = (vadj.upper() - page).max(vadj.lower());
                            vadj.set_value((bot + pad - page).min(max_v));
                        }
                    }
                }
                break;
            }
        }
        cur = p.parent();
    }
}

fn vim_focus_widget(target: &gtk4::Widget) {
    target.grab_focus();
    vim_scroll_into_view(target);
}

fn vim_exit_insert_to_normal(win: &ApplicationWindow, stack: &Stack, sidebar: &gtk4::Widget) {
    if let Some(tab_root) = stack.visible_child() {
        let mut btns = Vec::new();
        vim_collect_buttons(&tab_root, false, &mut btns);
        if let Some(first) = btns.first() {
            vim_focus_widget(first);
            return;
        }
    }
    let mut sb_btns = Vec::new();
    vim_collect_buttons(sidebar, false, &mut sb_btns);
    for b in &sb_btns {
        if b.has_css_class("selected") {
            vim_focus_widget(b);
            return;
        }
    }
    if let Some(first) = sb_btns.first() {
        vim_focus_widget(first);
    } else {
        GtkWindowExt::set_focus(win, None::<&gtk4::Widget>);
    }
}

fn vim_visual_rows(
    pane: &gtk4::Widget,
    items: &[gtk4::Widget],
) -> Vec<Vec<(gtk4::Widget, f64)>> {
    let mut coords: Vec<(gtk4::Widget, f64, f64)> = items
        .iter()
        .map(|w| {
            if let Some(b) = w.compute_bounds(pane) {
                (
                    w.clone(),
                    (b.x() + b.width() * 0.5) as f64,
                    (b.y() + b.height() * 0.5) as f64,
                )
            } else {
                (w.clone(), 0.0, 0.0)
            }
        })
        .collect();

    coords.sort_by(|a, b| {
        a.2.partial_cmp(&b.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut rows: Vec<(f64, Vec<(gtk4::Widget, f64)>)> = Vec::new();
    for (w, cx, cy) in coords {
        if let Some((row_y, list)) = rows.last_mut() {
            if (cy - *row_y).abs() <= 14.0 {
                list.push((w, cx));
                continue;
            }
        }
        rows.push((cy, vec![(w, cx)]));
    }
    for (_, list) in &mut rows {
        list.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    }
    rows.into_iter().map(|(_, list)| list).collect()
}

fn vim_locate_in_rows(
    focused: Option<&gtk4::Widget>,
    rows: &[Vec<(gtk4::Widget, f64)>],
) -> Option<(usize, usize, f64)> {
    let f = focused?;
    let mut cur = Some(f.clone());
    for _ in 0..4 {
        let Some(ref node) = cur else { break };
        for (ri, row) in rows.iter().enumerate() {
            for (ci, (w, cx)) in row.iter().enumerate() {
                if w == node {
                    return Some((ri, ci, *cx));
                }
            }
        }
        cur = node.parent();
    }
    None
}

enum VimMove {
    Down(usize),
    Up(usize),
    Left,
    Right,
    Top,
    Bottom,
}

fn vim_move_in_pane(
    pane: &gtk4::Widget,
    allow_root_popover: bool,
    focused: Option<&gtk4::Widget>,
    mv: VimMove,
) -> bool {
    let mut items = Vec::new();
    vim_collect_buttons(pane, allow_root_popover, &mut items);
    if items.is_empty() {
        return false;
    }
    let rows = vim_visual_rows(pane, &items);
    if rows.is_empty() {
        return false;
    }

    let pos = vim_locate_in_rows(focused, &rows);
    let pick_closest_x = |row: &[(gtk4::Widget, f64)], target_x: f64| -> gtk4::Widget {
        row.iter()
            .min_by(|a, b| {
                (a.1 - target_x)
                    .abs()
                    .partial_cmp(&(b.1 - target_x).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(w, _)| w.clone())
            .unwrap_or_else(|| row[0].0.clone())
    };

    match mv {
        VimMove::Top => {
            vim_focus_widget(&rows[0][0].0);
            true
        }
        VimMove::Bottom => {
            if let Some(last_row) = rows.last() {
                vim_focus_widget(&last_row[0].0);
            }
            true
        }
        VimMove::Down(step) => {
            if let Some((ri, _, cx)) = pos {
                let next_ri = (ri + step).min(rows.len() - 1);
                vim_focus_widget(&pick_closest_x(&rows[next_ri], cx));
            } else {
                vim_focus_widget(&rows[0][0].0);
            }
            true
        }
        VimMove::Up(step) => {
            if let Some((ri, _, cx)) = pos {
                let prev_ri = ri.saturating_sub(step);
                vim_focus_widget(&pick_closest_x(&rows[prev_ri], cx));
            } else if let Some(last_row) = rows.last() {
                vim_focus_widget(&last_row[0].0);
            }
            true
        }
        VimMove::Left => {
            if let Some((ri, ci, _)) = pos {
                if ci > 0 {
                    vim_focus_widget(&rows[ri][ci - 1].0);
                    return true;
                }
            }
            false
        }
        VimMove::Right => {
            if let Some((ri, ci, _)) = pos {
                if ci + 1 < rows[ri].len() {
                    vim_focus_widget(&rows[ri][ci + 1].0);
                    return true;
                }
            }
            false
        }
    }
}

fn vim_trigger_context_menu(focused: &gtk4::Widget, win: &ApplicationWindow) -> bool {
    let mut candidates = vec![focused.clone()];
    if let Some(p) = focused.parent() {
        candidates.push(p);
    }
    if let Some(c) = focused.first_child() {
        candidates.push(c);
    }

    for cand in candidates {
        let ctrls = cand.observe_controllers();
        for i in 0..ctrls.n_items() {
            let Some(obj) = ctrls.item(i) else { continue };
            if let Ok(gc) = obj.downcast::<gtk4::GestureClick>() {
                if GestureSingleExt::button(&gc) == 3 {
                    gc.emit_by_name::<()>("pressed", &[&1i32, &12.0f64, &12.0f64]);
                    let win_c = win.clone();
                    glib::timeout_add_local_once(Duration::from_millis(30), move || {
                        if let Some(pop) = vim_find_open_popover(win_c.upcast_ref()) {
                            let mut pop_btns = Vec::new();
                            vim_collect_buttons(pop.upcast_ref(), true, &mut pop_btns);
                            if let Some(first) = pop_btns.first() {
                                first.grab_focus();
                            }
                        }
                    });
                    return true;
                }
            }
        }
    }
    false
}

fn vim_show_help_popover(anchor: &gtk4::Widget, v: &config::VimBindsConfig) {
    // Open beside the sidebar (vertically centered over the content area) instead of GTK's
    // default `Top` placement, which pushed the cheat-sheet off the top edge of the screen.
    let is_right = config::get().layout.side == "right";
    let pop = gtk4::Popover::builder()
        .css_classes(vec!["fav-popover".to_string()])
        .autohide(true)
        .has_arrow(false)
        .position(if is_right {
            gtk4::PositionType::Left
        } else {
            gtk4::PositionType::Right
        })
        .build();
    pop.set_parent(anchor);
    let (aw, ah) = (anchor.width().max(1), anchor.height().max(1));
    pop.set_pointing_to(Some(&gdk::Rectangle::new(0, 0, aw, ah)));
    pop.set_offset(if is_right { -6 } else { 6 }, 0);
    pop.connect_closed(|p| {
        let pw = p.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(pop) = pw.upgrade() {
                pop.unparent();
            }
        });
    });

    let card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(5)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(8)
        .margin_end(8)
        .build();
    card.append(
        &Label::builder()
            .label("VIM NAVIGATION CHEAT-SHEET")
            .halign(gtk4::Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .build(),
    );

    let pairs: [(String, &str); 11] = [
        (format!("{} / {}", v.down, v.up), "Move Down / Up"),
        (format!("{} / {}", v.left, v.right), "Step Left / Right"),
        (format!("{} / {}", v.pane_left, v.pane_right), "Jump Pane ◀ / ▶"),
        (format!("{} / {}", v.next_tab, v.prev_tab), "Next / Prev Tab"),
        (format!("{} / {}", v.top, v.bottom), "Jump Top / Bottom"),
        ("Ctrl+d / Ctrl+u".to_string(), "Half-Page Scroll"),
        (format!("{} / {}", v.search, v.insert), "Search / Insert (INS)"),
        ("Ctrl+j / Ctrl+k".to_string(), "Step Search → Results"),
        ("Esc / Ctrl+[".to_string(), "Exit INS → NRM"),
        (v.context_menu.clone(), "Right-Click Menu"),
        (format!("{} / ?", v.close), "Close Pane / Help"),
    ];

    for (keys_str, desc) in pairs {
        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .build();
        row.append(
            &Label::builder()
                .label(&keys_str)
                .halign(gtk4::Align::Start)
                .css_classes(vec!["kbd-pill".to_string()])
                .build(),
        );
        row.append(
            &Label::builder()
                .label(desc)
                .halign(gtk4::Align::End)
                .hexpand(true)
                .css_classes(vec!["plan-body".to_string()])
                .build(),
        );
        card.append(&row);
    }

    pop.set_child(Some(&card));
    pop.popup();
}

fn vim_cycle_tab(stack: &Stack, delta: isize) {
    let loaded_tabs: Vec<String> = config::get()
        .layout
        .sidebar_items
        .into_iter()
        .filter(|it| config::is_sidebar_tab_id(it))
        .collect();
    if loaded_tabs.is_empty() {
        return;
    }
    let cur = stack.visible_child_name();
    let cur_idx = cur
        .as_deref()
        .and_then(|id| loaded_tabs.iter().position(|t| t == id))
        .unwrap_or(0);
    let len = loaded_tabs.len() as isize;
    let next_idx = ((cur_idx as isize + delta) % len + len) as usize % loaded_tabs.len();
    stack.set_visible_child_name(&loaded_tabs[next_idx]);
}

#[allow(clippy::too_many_arguments)]
fn vim_handle_key_press(
    key: gdk::Key,
    mods: gdk::ModifierType,
    bcfg: &config::BindingsConfig,
    win: &ApplicationWindow,
    stack: &Stack,
    sidebar: &gtk4::Widget,
    handles: &Rc<RefCell<std::collections::HashMap<String, module_api::ModuleHandle>>>,
    close_btn: &Button,
    pending_chord: &Rc<RefCell<Option<(char, std::time::Instant)>>>,
) -> bool {
    // Ignore bare modifier keys so chords aren't cleared when pressing Shift!
    if matches!(
        key,
        gdk::Key::Shift_L
            | gdk::Key::Shift_R
            | gdk::Key::Control_L
            | gdk::Key::Control_R
            | gdk::Key::Alt_L
            | gdk::Key::Alt_R
            | gdk::Key::Super_L
            | gdk::Key::Super_R
            | gdk::Key::Caps_Lock
    ) {
        return false;
    }

    let focused = GtkWindowExt::focus(win);
    let in_insert = focused.as_ref().map(vim_is_text_input).unwrap_or(false);
    let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
    let alt = mods.contains(gdk::ModifierType::ALT_MASK);
    let sup = mods.contains(gdk::ModifierType::SUPER_MASK);

    if alt || sup {
        *pending_chord.borrow_mut() = None;
        return false;
    }

    let is_esc = key == gdk::Key::Escape || (ctrl && key == gdk::Key::bracketleft);

    // --- A. OPEN POPOVER CONTEXT MENU NAVIGATION ---
    if let Some(pop) = vim_find_open_popover(win.upcast_ref()) {
        *pending_chord.borrow_mut() = None;
        let pop_w: &gtk4::Widget = pop.upcast_ref();
        if is_esc {
            let parent_row = pop.parent();
            pop.popdown();
            if let Some(p) = parent_row {
                let mut btns = Vec::new();
                vim_collect_buttons(&p, false, &mut btns);
                if let Some(b) = btns.first() {
                    b.grab_focus();
                } else if p.can_focus() {
                    p.grab_focus();
                }
            }
            return true;
        }
        if !in_insert {
            let ch_s = key.to_unicode().map(|c| c.to_string()).unwrap_or_default();
            let v = &bcfg.vim;
            if ch_s == v.close || ch_s == "?" {
                pop.popdown();
                return true;
            }
            if ch_s == v.down || key == gdk::Key::Down || (ctrl && (key == gdk::Key::j || key == gdk::Key::n)) {
                return vim_move_in_pane(pop_w, true, focused.as_ref(), VimMove::Down(1));
            }
            if ch_s == v.up || key == gdk::Key::Up || (ctrl && (key == gdk::Key::k || key == gdk::Key::p)) {
                return vim_move_in_pane(pop_w, true, focused.as_ref(), VimMove::Up(1));
            }
            if ch_s == v.bottom {
                return vim_move_in_pane(pop_w, true, focused.as_ref(), VimMove::Bottom);
            }
            if ch_s == v.top {
                return vim_move_in_pane(pop_w, true, focused.as_ref(), VimMove::Top);
            }
        }
        return false;
    }

    // --- B. INSERT MODE (inside SearchEntry / Entry / TextView) ---
    if in_insert {
        *pending_chord.borrow_mut() = None;
        if is_esc {
            vim_exit_insert_to_normal(win, stack, sidebar);
            return true;
        }
        // Telescope / fzf shortcut: Ctrl+j / Ctrl+n steps straight out of SearchEntry into results!
        if ctrl && (key == gdk::Key::j || key == gdk::Key::n) {
            vim_exit_insert_to_normal(win, stack, sidebar);
            return true;
        }
        if ctrl && (key == gdk::Key::k || key == gdk::Key::p) {
            vim_exit_insert_to_normal(win, stack, sidebar);
            return true;
        }
        return false;
    }

    // --- C. DETERMINE ORDERED PANES (Visual Left -> Right) ---
    let active_tab_id = stack.visible_child_name();
    let detail_pane: Option<gtk4::Widget> = active_tab_id.as_deref().and_then(|id| {
        handles
            .borrow()
            .get(id)
            .and_then(|h| h.detail_pane.clone())
            .filter(|dp| dp.is_visible())
    });
    let tab_pane: Option<gtk4::Widget> = stack.visible_child();

    let is_right_layout = config::get().layout.side == "right";
    let mut panes: Vec<gtk4::Widget> = Vec::with_capacity(3);
    if is_right_layout {
        if let Some(ref dp) = detail_pane {
            panes.push(dp.clone());
        }
        if let Some(ref tp) = tab_pane {
            panes.push(tp.clone());
        }
        panes.push(sidebar.clone());
    } else {
        panes.push(sidebar.clone());
        if let Some(ref tp) = tab_pane {
            panes.push(tp.clone());
        }
        if let Some(ref dp) = detail_pane {
            panes.push(dp.clone());
        }
    }

    let cur_pane_idx: usize = focused
        .as_ref()
        .and_then(|fw| panes.iter().position(|p| fw.is_ancestor(p) || fw == p))
        .or_else(|| {
            tab_pane
                .as_ref()
                .and_then(|tp| panes.iter().position(|p| p == tp))
        })
        .unwrap_or(0);
    let active_pane = &panes[cur_pane_idx];

    // --- D. NORMAL MODE: ESC & CTRL SHORTCUTS ---
    if is_esc {
        *pending_chord.borrow_mut() = None;
        if let Some(ref dp) = detail_pane {
            dp.set_visible(false);
            if let Some(id) = active_tab_id.as_deref() {
                if let Some(h) = handles.borrow().get(id) {
                    if let Some(ref hide_cb) = h.on_hide {
                        hide_cb();
                    }
                }
            }
            vim_exit_insert_to_normal(win, stack, sidebar);
            return true;
        }
        if bcfg.esc_to_close {
            close_btn.emit_clicked();
            return true;
        }
        return false;
    }

    if ctrl {
        *pending_chord.borrow_mut() = None;
        return match key {
            gdk::Key::d => vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Down(6)),
            gdk::Key::u => vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Up(6)),
            gdk::Key::j | gdk::Key::n => {
                vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Down(1))
            }
            gdk::Key::k | gdk::Key::p => {
                vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Up(1))
            }
            _ => false,
        };
    }

    // Activate focused button on Enter (in case a container widget isn't handling Return)
    if key == gdk::Key::Return || key == gdk::Key::KP_Enter {
        *pending_chord.borrow_mut() = None;
        if let Some(ref fw) = focused {
            if let Some(btn) = fw.downcast_ref::<gtk4::Button>() {
                btn.emit_clicked();
                return true;
            }
        }
        return false;
    }

    // --- E. NORMAL MODE: SINGLE-KEY & 2-KEY CHORD DISPATCH ---
    let Some(ch) = key.to_unicode().filter(|c| !c.is_control()) else {
        *pending_chord.borrow_mut() = None;
        return false;
    };

    let now = std::time::Instant::now();
    let prev_char = pending_chord
        .borrow_mut()
        .take()
        .and_then(|(pc, ts)| if now.duration_since(ts).as_millis() <= 500 { Some(pc) } else { None });

    let v = &bcfg.vim;
    let candidates: [(&str, &str); 14] = [
        (&v.down, "down"),
        (&v.up, "up"),
        (&v.left, "left"),
        (&v.right, "right"),
        (&v.pane_left, "pane_left"),
        (&v.pane_right, "pane_right"),
        (&v.next_tab, "next_tab"),
        (&v.prev_tab, "prev_tab"),
        (&v.top, "top"),
        (&v.bottom, "bottom"),
        (&v.search, "search"),
        (&v.insert, "insert"),
        (&v.context_menu, "menu"),
        (&v.close, "close"),
    ];

    let mut matched_action: Option<&str> = None;
    if let Some(pc) = prev_char {
        let two = format!("{}{}", pc, ch);
        for &(bind, act) in &candidates {
            if bind == two {
                matched_action = Some(act);
                break;
            }
        }
    }

    if matched_action.is_none() {
        let one = ch.to_string();
        for &(bind, act) in &candidates {
            if bind == one {
                matched_action = Some(act);
                break;
            }
        }
        // Check if `ch` is the first character of any configured 2-char chord (e.g. "gg")
        if matched_action.is_none() {
            let starts_chord = candidates.iter().any(|&(bind, _)| {
                let mut chars = bind.chars();
                chars.next() == Some(ch) && chars.next().is_some() && chars.next().is_none()
            });
            if starts_chord {
                *pending_chord.borrow_mut() = Some((ch, now));
                return true;
            }
        }
    }

    let Some(action) = matched_action else {
        if ch == '?' {
            vim_show_help_popover(sidebar, &bcfg.vim);
            return true;
        }
        return false;
    };

    let jump_pane = |target_idx: usize| -> bool {
        if let Some(target_pane) = panes.get(target_idx) {
            let mut btns = Vec::new();
            vim_collect_buttons(target_pane, false, &mut btns);
            if target_pane == sidebar {
                for b in &btns {
                    if b.has_css_class("selected") {
                        vim_focus_widget(b);
                        return true;
                    }
                }
            }
            if let Some(first) = btns.first() {
                vim_focus_widget(first);
                return true;
            }
        }
        false
    };

    match action {
        "down" => vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Down(1)),
        "up" => vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Up(1)),
        "left" => {
            if vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Left) {
                true
            } else if cur_pane_idx > 0 {
                jump_pane(cur_pane_idx - 1)
            } else {
                true
            }
        }
        "right" => {
            if vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Right) {
                true
            } else if cur_pane_idx + 1 < panes.len() {
                jump_pane(cur_pane_idx + 1)
            } else {
                true
            }
        }
        "pane_left" => {
            if cur_pane_idx > 0 {
                jump_pane(cur_pane_idx - 1)
            } else {
                true
            }
        }
        "pane_right" => {
            if cur_pane_idx + 1 < panes.len() {
                jump_pane(cur_pane_idx + 1)
            } else {
                true
            }
        }
        "next_tab" => {
            vim_cycle_tab(stack, 1);
            true
        }
        "prev_tab" => {
            vim_cycle_tab(stack, -1);
            true
        }
        "top" => vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Top),
        "bottom" => vim_move_in_pane(active_pane, false, focused.as_ref(), VimMove::Bottom),
        "search" | "insert" => {
            let search_first = action == "search";
            // Search active pane first, then active tab pane
            let mut searches = Vec::new();
            let mut others = Vec::new();
            vim_collect_text_inputs(active_pane, &mut searches, &mut others);
            if searches.is_empty() && others.is_empty() {
                if let Some(ref tp) = tab_pane {
                    vim_collect_text_inputs(tp, &mut searches, &mut others);
                }
            }
            let target = if search_first {
                searches.first().or_else(|| others.first())
            } else {
                // If focused row has a sibling Entry (e.g. Settings / Keybinds), prefer that!
                let sibling_entry = focused.as_ref().and_then(|fw| {
                    let parent = fw.parent()?;
                    let mut s = Vec::new();
                    let mut o = Vec::new();
                    vim_collect_text_inputs(&parent, &mut s, &mut o);
                    o.into_iter().next().or_else(|| s.into_iter().next())
                });
                sibling_entry
                    .as_ref()
                    .or_else(|| searches.first())
                    .or_else(|| others.first())
                    .cloned()
                    .as_ref()
                    .cloned()
                    .map(|w| {
                        vim_focus_widget(&w);
                    });
                return true;
            };
            if let Some(w) = target {
                vim_focus_widget(w);
            }
            true
        }
        "menu" => {
            if let Some(ref fw) = focused {
                vim_trigger_context_menu(fw, win)
            } else {
                false
            }
        }
        "close" => {
            if let Some(ref dp) = detail_pane {
                dp.set_visible(false);
                if let Some(id) = active_tab_id.as_deref() {
                    if let Some(h) = handles.borrow().get(id) {
                        if let Some(ref hide_cb) = h.on_hide {
                            hide_cb();
                        }
                    }
                }
                vim_exit_insert_to_normal(win, stack, sidebar);
            } else {
                close_btn.emit_clicked();
            }
            true
        }
        _ => false,
    }
}
