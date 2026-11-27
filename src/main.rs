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

    let update_css: Rc<dyn Fn()> = {
        let provider = provider.clone();
        let scrollbar_provider = scrollbar_provider.clone();
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
                .sidebar-sep {{ background-color: {accent1_35}; min-height: 1px; margin: 4px 10px; }}
                
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
                .sidebar-weather-lbl {{ color: #{accent2}; font-family: '{ui_font}', sans-serif; font-size: {fs_13}px; font-weight: bold; }}
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
                
                label {{ color: #{fg_main}; font-family: '{ui_font}', sans-serif; font-size: {fs_16}px; }}
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

            // Button style overrides ("tonal" uses the base CSS above)
            let pill_rad = if r_scale > 0.01 { 999 } else { 0 };
            let btn_style_css = match cfg.button_style.as_str() {
                "outlined" => format!(
                    r#"
                    .plan-fmt-btn, .plan-close-btn, .power-btn, .session-restore-btn, .ws-pill-btn, .top-tool-btn, .sticky-mini-btn, .fav-hide-btn {{ background-color: {fg_main_04}; border: 1px solid {accent1_38}; }}
                    .plan-fmt-btn:hover, .plan-close-btn:hover, .power-btn:hover, .session-restore-btn:hover, .ws-pill-btn:hover, .top-tool-btn:hover, .sticky-mini-btn:hover, .fav-hide-btn:hover {{ background-color: {accent1_15}; border-color: #{accent2}; }}
                    .plan-fmt-btn.active, .plan-close-btn.active, .power-btn.active, .ws-pill-btn.active, .sticky-mini-btn.active, .primary-btn {{ background-color: {accent1_24}; border: 1px solid #{accent2}; }}
                    .sidebar-nav-btn {{ border: 1px solid transparent; }}
                    .sidebar-nav-btn:hover {{ background-color: {accent1_15}; border-color: {accent1_38}; }}
                    .sidebar-nav-btn.selected {{ background-color: {accent1_15}; border: 1px solid #{accent2}; }}
                    .sidebar-nav-btn.selected label {{ color: #{accent2}; }}
                    .plan-delete-menu-btn, .power-btn.armed {{ background-color: transparent; border: 1px solid rgba(243, 139, 168, 0.65); }}
                    .plan-delete-menu-btn:hover {{ background-color: rgba(243, 139, 168, 0.18); }}
                    "#
                ),
                "pill" => format!(
                    r#"
                    .plan-fmt-btn, .plan-close-btn, .primary-btn, .power-btn, .session-restore-btn, .ws-pill-btn, .top-tool-btn, .sticky-mini-btn, .fav-hide-btn, .plan-delete-menu-btn, .sidebar-nav-btn {{ border-radius: {pill_rad}px; }}
                    .plan-fmt-btn.active, .plan-close-btn.active, .power-btn.active, .ws-pill-btn.active, .sticky-mini-btn.active, .primary-btn, .sidebar-nav-btn.selected {{ border: 1px solid {accent1_55}; background-color: {accent1_24}; }}
                    .sidebar-nav-btn.selected label {{ color: #{accent2}; }}
                    "#
                ),
                "solid" => format!(
                    r#"
                    .plan-fmt-btn, .plan-close-btn, .power-btn, .session-restore-btn, .ws-pill-btn, .top-tool-btn, .sticky-mini-btn, .fav-hide-btn {{ background-color: #{bg_pill}; border: 1px solid {fg_main_08}; }}
                    .plan-fmt-btn:hover, .plan-close-btn:hover, .power-btn:hover, .session-restore-btn:hover, .ws-pill-btn:hover, .top-tool-btn:hover, .sticky-mini-btn:hover, .fav-hide-btn:hover {{ background-color: {accent1_24}; }}
                    .plan-fmt-btn.active, .plan-close-btn.active, .power-btn.active, .ws-pill-btn.active, .sticky-mini-btn.active, .primary-btn, .sidebar-nav-btn.selected {{ background-color: #{accent2}; border: 1px solid #{accent2}; color: #{bg_contrast}; }}
                    .plan-fmt-btn.active label, .plan-close-btn.active label, .power-btn.active label, .power-btn.active .power-btn-lbl, .ws-pill-btn.active label, .sticky-mini-btn.active label, .primary-btn label, .sidebar-nav-btn.selected label {{ color: #{bg_contrast}; }}
                    .primary-btn:hover {{ background-color: #{accent1}; }}
                    .primary-btn:hover label {{ color: #{bg_contrast}; }}
                    .plan-delete-menu-btn, .power-btn.armed {{ background-color: #f38ba8; border: 1px solid #f38ba8; color: #{bg_contrast}; }}
                    .plan-delete-menu-btn label, .power-btn.armed .power-btn-lbl {{ color: #{bg_contrast}; }}
                    "#
                ),
                "neon" => format!(
                    r#"
                    .plan-fmt-btn, .plan-close-btn, .power-btn, .session-restore-btn, .ws-pill-btn, .top-tool-btn, .sticky-mini-btn, .fav-hide-btn {{ background-color: #{bg_recessed}; border: 1px solid {accent1_38}; }}
                    .plan-fmt-btn:hover, .plan-close-btn:hover, .power-btn:hover, .session-restore-btn:hover, .ws-pill-btn:hover, .top-tool-btn:hover, .sticky-mini-btn:hover, .fav-hide-btn:hover {{ border-color: #{accent2}; box-shadow: 0px 0px 8px {accent1_55}; }}
                    .plan-fmt-btn.active, .plan-close-btn.active, .power-btn.active, .ws-pill-btn.active, .sticky-mini-btn.active, .primary-btn, .sidebar-nav-btn.selected {{ background-color: {accent1_24}; border: 1px solid #{accent2}; box-shadow: 0px 0px 9px {accent1_55}; }}
                    .sidebar-nav-btn:hover {{ border: 1px solid {accent1_38}; box-shadow: 0px 0px 6px {accent1_38}; }}
                    .sidebar-nav-btn.selected label {{ color: #{accent2}; }}
                    .plan-delete-menu-btn, .power-btn.armed {{ background-color: rgba(243, 139, 168, 0.14); border: 1px solid #f38ba8; box-shadow: 0px 0px 8px rgba(243, 139, 168, 0.45); }}
                    "#
                ),
                "ghost" => format!(
                    r#"
                    .plan-fmt-btn, .plan-close-btn, .power-btn, .session-restore-btn, .ws-pill-btn, .top-tool-btn, .sticky-mini-btn, .fav-hide-btn {{ background-color: transparent; border: 1px solid transparent; }}
                    .plan-fmt-btn:hover, .plan-close-btn:hover, .power-btn:hover, .session-restore-btn:hover, .ws-pill-btn:hover, .top-tool-btn:hover, .sticky-mini-btn:hover, .fav-hide-btn:hover {{ background-color: {fg_main_08}; }}
                    .plan-fmt-btn.active, .plan-close-btn.active, .power-btn.active, .ws-pill-btn.active, .sticky-mini-btn.active, .primary-btn, .sidebar-nav-btn.selected {{ background-color: {fg_main_06}; border: none; border-bottom: 2px solid #{accent2}; border-radius: {r_chk}px {r_chk}px 2px 2px; }}
                    .sidebar-nav-btn.selected label {{ color: #{accent2}; }}
                    .plan-delete-menu-btn {{ background-color: transparent; }}
                    .plan-delete-menu-btn:hover {{ background-color: rgba(243, 139, 168, 0.16); }}
                    "#
                ),
                _ => String::new(),
            };

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

            let full_css = format!("{}\n{}\n{}", css, btn_style_css, preview_css);
            provider.load_from_data(&full_css);

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
        window.queue_draw();
    }
}
fn move_to_monitor(window: &ApplicationWindow, target: Option<&str>) {
    let pinned = config::get().layout.pinned_monitor;
    let mut active_name = if !pinned.trim().is_empty() {
        Some(pinned)
    } else {
        target.map(|s| s.to_string())
    };

    // Fallback to hyprctl if no explicit target provided (e.g., on first launch)
    if active_name.is_none() {
        if let Ok(output) = std::process::Command::new("hyprctl").args(["monitors", "-j"]).output() {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(monitors) = json.as_array() {
                    active_name = monitors.iter()
                        .find(|m| m.get("focused").and_then(|f| f.as_bool()).unwrap_or(false))
                        .and_then(|m| m.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string());
                }
            }
        }
    }

    let Some(target_name) = active_name else { return };
    let Some(display) = gdk::Display::default() else { return };
    let gdk_monitors = display.monitors();

    for i in 0..gdk_monitors.n_items() {
        if let Some(mon) = gdk_monitors.item(i).and_downcast::<gdk::Monitor>() {
            if let Some(conn) = mon.connector() {
                if conn == target_name {
                    window.set_monitor(&mon);
                    break;
                }
            }
        }
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
        .css_classes(vec!["dash-box".to_string()])
        .width_request(400)
        .overflow(Overflow::Hidden)
        .build();

    // Dynamically adjust closed translateX offset whenever dash_box changes width!
    let dynamic_offset_provider = CssProvider::new();
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &dynamic_offset_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER,
    );

    let last_dash_width = Rc::new(Cell::new(0));
    let last_side_right = Rc::new(Cell::new(config::get().layout.side == "right"));
    let dyn_off_prov_tick = dynamic_offset_provider.clone();
    let last_w_watch = last_dash_width.clone();
    let last_side_watch = last_side_right.clone();
    dash_box.add_tick_callback(move |box_widget, _| {
        let w = box_widget.width();
        let is_right = config::get().layout.side == "right";
        if w > 0 && (w != last_w_watch.get() || is_right != last_side_watch.get()) {
            last_w_watch.set(w);
            last_side_watch.set(is_right);
            let push_back = w + 10;
            let closed_val = if is_right {
                format!("{}px", push_back)
            } else {
                format!("-{}px", push_back)
            };
            dyn_off_prov_tick.load_from_data(&format!(
                ".drawer-container {{ transform: translateX({}); }}
                 .drawer-container.open {{ transform: translateX(0px); }}",
                closed_val
            ));
        }
        glib::ControlFlow::Continue
    });

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
        .child(&sidebar_column)
        .build();

    // Dynamic sidebar navigation rebuilt from `config.layout.sidebar_items` (tabs + "---" separators)
    let sidebar_nav = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(1)
        .margin_top(4)
        .vexpand(true)
        .build();

    let nav_buttons_rc: Rc<RefCell<Vec<(String, Button)>>> = Rc::new(RefCell::new(Vec::new()));

    let rebuild_sidebar_nav: Rc<dyn Fn()> = {
        let snav = sidebar_nav.clone();
        let stk = stack.clone();
        let nbtns = nav_buttons_rc.clone();
        Rc::new(move || {
            while let Some(c) = snav.first_child() {
                snav.remove(&c);
            }
            nbtns.borrow_mut().clear();

            let cfg = config::get();
            let active_name = stk.visible_child_name().map(|s| s.to_string());
            let compiled = modules_gen::all_modules();

            for item in &cfg.layout.sidebar_items {
                if item == "---" {
                    let sep = Box::builder()
                        .orientation(Orientation::Horizontal)
                        .height_request(1)
                        .css_classes(vec!["sidebar-sep".to_string()])
                        .build();
                    snav.append(&sep);
                } else if let Some(desc) = compiled.iter().find(|m| m.id == item.as_str()) {
                    let lbl = Label::builder()
                        .label(desc.title)
                        .halign(gtk4::Align::Start)
                        .hexpand(true)
                        .build();
                    let mut classes = vec!["sidebar-nav-btn".to_string()];
                    if active_name.as_deref() == Some(desc.id) {
                        classes.push("selected".to_string());
                    }
                    let btn = Button::builder()
                        .child(&lbl)
                        .css_classes(classes)
                        .build();
                    let stack_click = stk.clone();
                    let id_s = desc.id.to_string();
                    btn.connect_clicked(move |_| {
                        stack_click.set_visible_child_name(&id_s);
                    });
                    snav.append(&btn);
                    nbtns.borrow_mut().push((desc.id.to_string(), btn));
                }
            }
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
                    "To continue, write:  {}  and press Enter:",
                    module_loader::CONFIRM_PHRASE
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
                if pe.text().trim() == module_loader::CONFIRM_PHRASE {
                    module_loader::trust_modules(&untrusted);
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

    // Add everything to the sidebar column in order
    sidebar_column.append(&sidebar_nav);
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

    // Quick Capture & Color Picker buttons on the right side of the top Workspaces bar
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

    let dash_for_mods = dash_box.clone();
    let nav_buttons_sync = nav_buttons_rc.clone();
    let slots_sync = module_slots.clone();
    let handles_sync = loaded_handles.clone();
    let ctx_sync = module_ctx.clone();
    let mods_sync = compiled_modules.clone();

    stack.connect_visible_child_name_notify(move |stk| {
        let current = stk.visible_child_name();
        let name = current.as_deref();

        for (id, btn) in nav_buttons_sync.borrow().iter() {
            if name == Some(id.as_str()) {
                btn.add_css_class("selected");
            } else {
                btn.remove_css_class("selected");
            }
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
            }
        }
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

            // 4. Chrome toggles
            ws_w.set_visible(lay.chrome.show_top_bar);
            tr_w.set_visible(lay.chrome.show_tray);
            st_w.set_visible(lay.chrome.show_status_strip);
            sr_w.set_visible(lay.chrome.show_session_row);
            pr_w.set_visible(lay.chrome.show_power_row);
            we_w.set_visible(lay.chrome.show_weather);
            cl_w.set_visible(lay.chrome.show_clock);
            rl_w.set_visible(lay.chrome.show_reload_btn);

            // 5. Monitor pinning & hitbox refresh
            move_to_monitor(&win_l, None);
            update_input_region(&win_l, open_l.get(), db_l.width());
        })
    };
    *layout_rebuild_slot.borrow_mut() = Some(apply_layout_and_chrome.clone());

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
    let window_clone = window.clone();
    let hovered_click = is_hovered.clone();
    let open_click = is_open.clone();
    let closing_click = is_closing.clone();
    let dash_box_btn = dash_box.clone();

    trigger_btn.connect_clicked(move |btn| {
        let currently_open = open_click.get();

        if !currently_open {
            move_to_monitor(&window_clone, None);
            open_click.set(true);
            closing_click.set(false);
            media_for_toggle.set_dashboard_open(true);
            stickies_for_toggle.set_dashboard_open(true);

            // Use Exclusive keyboard interactivity while open so pressing SUPER immediately
            // steals and holds keyboard focus even when the cursor is over another window
            // (Hyprland follow_mouse won't steal it back). Closing resets to None.
            window_clone.set_keyboard_mode(KeyboardMode::Exclusive);
            // Pass the exact width of the dashboard to the hitbox
            update_input_region(&window_clone, true, dash_box_btn.width());
            window_clone.present();

            btn.add_css_class("riding");
            drawer_clone.add_css_class("open");

            if let Some(cur) = stack_for_open.visible_child_name() {
                if let Some(h) = handles_toggle.borrow().get(cur.as_str()) {
                    if let Some(ref show_cb) = h.on_show {
                        show_cb();
                    }
                }
            }

            let open_kb = open_click.clone();
            let stk_kb = stack_for_open.clone();
            let handles_kb = handles_toggle.clone();
            glib::timeout_add_local_once(Duration::from_millis(80), move || {
                if open_kb.get() {
                    if let Some(cur) = stk_kb.visible_child_name() {
                        if let Some(h) = handles_kb.borrow().get(cur.as_str()) {
                            if let Some(ref show_cb) = h.on_show {
                                show_cb();
                            }
                        }
                    }
                }
            });
        } else {
            open_click.set(false);
            closing_click.set(true);
            hovered_click.set(false);
            media_for_toggle.set_dashboard_open(false);
            stickies_for_toggle.set_dashboard_open(false);

            for h in handles_toggle.borrow().values() {
                if let Some(ref dp) = h.detail_pane {
                    dp.set_visible(false);
                }
            }
            window_clone.set_keyboard_mode(KeyboardMode::None);
            drawer_clone.remove_css_class("open");
            btn.remove_css_class("peeking");

            let btn_after = btn.clone();
            let win_after = window_clone.clone();
            let open_after = open_click.clone();
            let closing_after = closing_click.clone();

            let close_ms = config::get().layout.anim_duration_ms.max(16);
            glib::timeout_add_local_once(Duration::from_millis(close_ms), move || {
                closing_after.set(false);
                if !open_after.get() {
                    btn_after.remove_css_class("riding");
                    // Pass 0, width doesn't matter when the dashboard is closed
                    update_input_region(&win_after, false, 0);
                }
            });
        }
    });

    let key_controller = EventControllerKey::new();
    key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let btn_esc = trigger_btn.clone();
    let open_esc = is_open.clone();
    let stack_kbd = stack.clone();
    key_controller.connect_key_pressed(move |_, key, _, mods| {
        if !open_esc.get() {
            return glib::Propagation::Proceed;
        }
        let bcfg = config::get().bindings;
        if key == gdk::Key::Escape && bcfg.esc_to_close {
            btn_esc.emit_clicked();
            return glib::Propagation::Stop;
        }
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
                    let loaded_tabs: Vec<String> = config::get()
                        .layout
                        .sidebar_items
                        .into_iter()
                        .filter(|it| it != "---")
                        .collect();
                    if let Some(tab_id) = loaded_tabs.get(idx) {
                        stack_kbd.set_visible_child_name(tab_id);
                        return glib::Propagation::Stop;
                    }
                }
            }
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_controller);

    drawer_container.append(&dash_box);
    drawer_container.append(&trigger_wrapper);

    window.set_child(Some(&drawer_container));
    apply_layout_and_chrome();

    // --- THE BULLETPROOF HITBOX FIX (UPDATED FOR DYNAMIC WIDTH) ---
    let last_known_height = Rc::new(Cell::new(0));
    let last_known_width = Rc::new(Cell::new(0));
    let win_tick = window.clone();
    let open_tick = is_open.clone();
    let dash_box_tick = dash_box.clone();

    win_tick.add_tick_callback(move |win, _| {
        let current_height = win.height();
        let current_width = dash_box_tick.width();
        
        // If the height OR width changes physically, update the hitbox instantly!
        if current_height > 0 && (current_height != last_known_height.get() || current_width != last_known_width.get()) {
            last_known_height.set(current_height);
            last_known_width.set(current_width);
            let app_win = win.downcast_ref::<ApplicationWindow>().unwrap();
            update_input_region(app_win, open_tick.get(), current_width);
        }
        glib::ControlFlow::Continue
    });
    // ----------------------------------

    // --- START HYPRLAND IPC LISTENER ---
    let (tx_mon, rx_mon) = std::sync::mpsc::channel::<String>();
    
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
                            let _ = tx_mon.send(mon_name.to_string());
                        }
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
        while let Ok(mon) = rx_mon.try_recv() {
            latest_mon = Some(mon);
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
    window.connect_map(move |win| {
        update_input_region(win, open_map.get(), dash_box_map.width());
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