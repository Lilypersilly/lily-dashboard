//! Entry point and main layer-shell window for Lily Dashboard.
//!
//! Sets up the left-edge slide-out drawer, dynamic pointer input region (click-through
//! when closed), live CSS reloading from `~/.config/theme-switch/current.conf`,
//! lazy/stateless tab lifecycle management, sidebar footer widgets, and top-bar
//! quick capture/color-picker actions.

mod config;
mod favorites;
mod launcher;
mod passwords;
mod music;
mod plans;
mod session;
mod drive;
mod locker;
mod weather;
mod timers;
mod system;
mod clipboard;
mod calendar;
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
const SLIDE_DURATION_MS: u64 = 350;

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

    let app = Application::builder().application_id(APP_ID).build();
    let reload_theme_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let reload_theme_startup = reload_theme_slot.clone();
    app.connect_startup(move |_| {
        let reload_cb = init_theme();
        *reload_theme_startup.borrow_mut() = Some(reload_cb);
    });
    app.connect_shutdown(|_| clipboard::shutdown_watchers());

    // Store the trigger button so re-running `lily-dashboard` (e.g. via SUPER keybind)
    // toggles the already-running dashboard open/closed instead of spawning a second window!
    let existing_trigger: Rc<RefCell<Option<Button>>> = Rc::new(RefCell::new(None));
    let reload_theme_act = reload_theme_slot.clone();
    app.connect_activate(move |app| {
        if let Some(btn) = existing_trigger.borrow().as_ref() {
            btn.emit_clicked();
            return;
        }

        // Apply saved Hyprland live overrides on startup if the user opted in
        let startup_cfg = config::get();
        if startup_cfg.hyprland.apply_on_startup {
            config::apply_hyprland_settings(&startup_cfg.hyprland);
        }

        session::init_session_tracker();
        clipboard::init_clipboard_watcher();
        let media_handle = media_popout::init_media_popout(app);
        let stickies_handle = stickies::init_stickies(app);

        let reload_theme = reload_theme_act
            .borrow()
            .clone()
            .unwrap_or_else(|| Rc::new(|| {}));
        let btn = build_ui(app, media_handle, stickies_handle, reload_theme);
        *existing_trigger.borrow_mut() = Some(btn);
    });

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

            let css = format!(
                r#"
                window {{ background-color: transparent; }}
                .drawer-container {{ transform: translateX(-415px); transition: transform 0.35s cubic-bezier(0.25, 0.8, 0.25, 1); }}
                .drawer-container.open {{ transform: translateX(0px); }}
                
                .dash-box {{ background-color: {bg_main_glass}; border: 2px solid #{accent1}; border-left: none; border-top-right-radius: {r_outer}px; border-bottom-right-radius: {r_outer}px; min-width: 398px; }}
                .sidebar-column {{ background-color: {bg_recessed_glass}; border-right: 1px solid #{accent1}; }}
                
                .sidebar-nav-btn {{ background-color: transparent; border: none; border-radius: {r_md}px; padding: 6px 10px; margin: 1px 4px; box-shadow: none; transition: background-color 0.18s ease, color 0.18s ease; }}
                .sidebar-nav-btn:hover {{ background-color: {accent1_15}; }}
                .sidebar-nav-btn.selected {{ background-color: {fg_main_06}; }}
                .sidebar-nav-btn label {{ color: #{fg_muted}; font-family: 'Josefin Sans', sans-serif; font-size: 15px; font-weight: bold; }}
                .sidebar-nav-btn.selected label {{ color: #{fg_main}; }}
                .sidebar-sep {{ background-color: {accent1_35}; min-height: 1px; margin: 4px 10px; }}
                
                .fav-item-btn, .music-item-album, .music-item-playlist {{ background-color: transparent; border: none; border-radius: {r_lg}px; padding: 6px 8px; transition: background-color 0.15s ease; box-shadow: none; }}
                .fav-item-btn:hover, .music-item-album:hover, .music-item-playlist:hover {{ background-color: {accent1_15}; }}
                .fav-item-btn.pinned, .music-item-album.pinned, .music-item-playlist.pinned {{ background-color: #{selection}; border: none; }}
                
                .music-art-frame {{ background-color: transparent; border: none; border-radius: {r_xs}px; min-width: 36px; min-height: 36px; box-shadow: 0px 2px 5px rgba(0, 0, 0, 0.15); }}
                .music-kind-tab-song {{ background-color: #89b4fa; border-radius: 99px; min-width: 3px; min-height: 28px; }}
                .music-kind-tab-playlist {{ background-color: #fab387; border-radius: 99px; min-width: 3px; min-height: 28px; }}
                .music-kind-tab-album {{ background-color: #a6e3a1; border-radius: 99px; min-width: 3px; min-height: 28px; }}
                
                .fav-search {{ background-color: {bg_contrast_65}; border: 1px solid transparent; border-radius: {r_md}px; color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; padding: 7px 12px; transition: background-color 0.16s ease, border-color 0.16s ease; }}
                .fav-search:focus {{ background-color: {bg_contrast_88}; border-color: {accent1_55}; }}
                
                .fav-group-header {{ color: #{accent2}; font-weight: bold; font-size: 13px; font-family: 'MesloLGL Nerd Font', monospace; margin-left: 4px; letter-spacing: 1px; }}
                .fav-score-badge {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', monospace; font-size: 12px; font-weight: bold; }}
                
                .plan-fmt-btn, .plan-close-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 6px 11px; color: #{fg_main}; font-family: 'Josefin Sans', 'MesloLGL Nerd Font', sans-serif; font-weight: bold; box-shadow: none; transition: background-color 0.16s ease, color 0.16s ease; }}
                .plan-fmt-btn label, .plan-close-btn label {{ color: #{fg_main}; font-family: 'Josefin Sans', 'MesloLGL Nerd Font', sans-serif; font-size: 14px; font-weight: bold; }}
                .plan-fmt-btn:hover, .plan-close-btn:hover {{ background-color: {fg_main_15}; }}
                .plan-fmt-btn.active, .plan-close-btn.active {{ background-color: {accent1_24}; border-color: transparent; color: #{accent2}; }}
                .plan-fmt-btn.active label, .plan-close-btn.active label {{ color: #{accent2}; }}
                .plan-fmt-btn.active:hover, .plan-close-btn.active:hover {{ background-color: {accent1_38}; }}

                .primary-btn {{ background-color: {accent1_24}; border: 1px solid transparent; border-radius: {r_md}px; padding: 7px 14px; color: #{accent2}; font-family: 'Josefin Sans', 'MesloLGL Nerd Font', sans-serif; font-weight: bold; box-shadow: none; transition: background-color 0.16s ease, color 0.16s ease; }}
                .primary-btn label {{ color: #{accent2}; font-family: 'Josefin Sans', 'MesloLGL Nerd Font', sans-serif; font-size: 14px; font-weight: bold; }}
                .primary-btn:hover {{ background-color: {accent1_38}; color: #{fg_main}; }}
                .primary-btn:hover label {{ color: #{fg_main}; }}
                .primary-btn.active {{ background-color: rgba(243, 139, 168, 0.22); color: #f38ba8; }}
                .primary-btn.active label {{ color: #f38ba8; }}
                .primary-btn.active:hover {{ background-color: rgba(243, 139, 168, 0.34); }}
                
                .session-restore-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 7px 6px; margin: 4px 8px; box-shadow: none; transition: background-color 0.16s ease; }}
                .session-restore-btn:hover {{ background-color: {fg_main_15}; }}
                .session-restore-lbl {{ color: #{accent2}; font-size: 12px; font-weight: bold; }}

                .power-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 5px 2px; min-height: 28px; box-shadow: none; transition: background-color 0.16s ease; }}
                .power-btn:hover {{ background-color: {fg_main_15}; }}
                .power-btn-lbl {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', 'Josefin Sans', sans-serif; font-size: 14px; font-weight: bold; }}
                .power-btn.active {{ background-color: {accent1_24}; }}
                .power-btn.active:hover {{ background-color: {accent1_38}; }}
                .power-btn.armed {{ background-color: rgba(243, 139, 168, 0.24); border-color: transparent; }}
                .power-btn.armed .power-btn-lbl {{ color: #f38ba8; font-family: 'Josefin Sans', sans-serif; font-size: 11px; }}

                .sidebar-footer-btn {{ background-color: transparent; border: none; border-radius: {r_bar}px; padding: 2px 3px; margin: 0px; min-height: 0px; min-width: 0px; box-shadow: none; transition: background-color 0.15s ease; }}
                .sidebar-footer-btn:hover {{ background-color: {accent1_15}; }}
                .sidebar-weather-lbl {{ color: #{accent2}; font-family: 'Josefin Sans', sans-serif; font-size: 13px; font-weight: bold; }}
                .sidebar-clock-lbl {{ color: #{fg_main}; font-family: 'MesloLGL Nerd Font', 'Josefin Sans', sans-serif; font-size: 12px; font-weight: bold; }}
                .sidebar-reload-lbl {{ color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; font-size: 11px; font-weight: bold; }}

                .sidebar-status-lbl {{ color: #{fg_muted}; font-family: 'MesloLGL Nerd Font', 'Josefin Sans', sans-serif; font-size: 11px; font-weight: bold; }}
                .sidebar-status-lbl.bat-low {{ color: #f38ba8; }}
                .notif-unread .sidebar-status-lbl {{ color: #{accent2}; }}

                .ws-top-bar {{ background-color: {bg_recessed_glass}; border-bottom: 1px solid {accent1_35}; padding: 6px 10px; }}
                .ws-pill-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_md}px; padding: 2px 9px; min-width: 24px; min-height: 22px; box-shadow: none; transition: all 0.15s ease; }}
                .ws-pill-btn label {{ color: #{fg_muted}; font-family: 'MesloLGL Nerd Font', sans-serif; font-size: 12px; font-weight: bold; }}
                .ws-pill-btn:hover {{ background-color: {fg_main_15}; }}
                .ws-pill-btn.active {{ background-color: {accent1_24}; border-color: transparent; }}
                .ws-pill-btn.active label {{ color: #{accent2}; }}
                .ws-preview-map {{ border-radius: {r_xs}px; }}
                .top-tool-btn {{ background-color: {fg_main_08}; border: 1px solid transparent; border-radius: {r_sm}px; padding: 2px 7px; min-width: 24px; min-height: 22px; box-shadow: none; transition: background-color 0.15s ease; }}
                .top-tool-btn label {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', sans-serif; font-size: 13px; }}
                .top-tool-btn:hover {{ background-color: {fg_main_15}; }}
                .kbd-pill {{ background-color: {fg_main_08}; border-radius: {r_xxs}px; padding: 3px 8px; color: #{accent2}; font-family: 'MesloLGL Nerd Font', monospace; font-size: 11px; font-weight: bold; }}

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
                .media-pill-lbl {{ color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; font-size: 13px; font-weight: bold; }}
                .media-popout-card {{ background-color: {bg_main_glass}; border: 2px solid #{accent1}; border-radius: {r_outer}px; padding: 12px 14px; }}
                .media-popout-title {{ color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; font-size: 15px; font-weight: bold; }}
                .media-art-box {{ background-color: #{bg_recessed}; border: 1px solid #{selection}; border-radius: {r_md}px; }}

                calendar.lily-calendar {{ background-color: #{bg_recessed}; border: 1px solid #{accent1}; border-radius: {r_lg}px; color: #{fg_main}; padding: 4px; font-family: 'Josefin Sans', sans-serif; font-size: 13px; }}
                calendar.lily-calendar > header {{ color: #{accent2}; font-weight: bold; border-bottom: 1px solid #{selection}; padding-bottom: 4px; }}
                calendar.lily-calendar > grid > label.day-name {{ color: #{fg_muted}; font-weight: bold; }}
                calendar.lily-calendar > grid > label.day-number {{ border-radius: {r_xs}px; padding: 3px; }}
                calendar.lily-calendar > grid > label.day-number:selected {{ background-color: #{accent2}; color: #{bg_contrast}; font-weight: bold; }}
                calendar.lily-calendar > grid > label.day-number.today {{ border: 1px solid #{accent1}; font-weight: bold; }}
                calendar.lily-calendar > grid > label.day-number.other-month {{ opacity: 0.35; }}
                
                label {{ color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; font-size: 16px; }}
                
                .trigger-wrapper {{ min-width: 56px; min-height: 130px; }}
                .trigger-btn {{ background-color: #{bg_pill}; border: 2px solid #{accent1}; border-left: none; border-top-right-radius: 100px; border-bottom-right-radius: 100px; min-width: 50px; min-height: 100px; padding: 0; transform: translateX(-36px); transition-property: transform, background-color; transition-duration: 0.2s; transition-timing-function: cubic-bezier(0.25, 0.8, 0.25, 1); }}
                .trigger-btn.peeking, .trigger-btn.riding {{ background-color: #{selection}; transform: translateX(0px); }}
                
                popover.fav-popover > contents {{ background-color: {bg_main_glass}; border: 1px solid {accent1_35}; border-radius: {r_lg}px; padding: 6px; }}
                .fav-hide-btn {{ background-color: {fg_main_08}; border: none; border-radius: {r_sm}px; padding: 6px 12px; color: #{fg_main}; box-shadow: none; transition: background-color 0.15s ease; }}
                .fav-hide-btn:hover {{ background-color: {fg_main_15}; }}
                
                .plan-detail-pane {{ background-color: {bg_main_glass}; border-left: 2px solid #{selection}; min-width: 400px; }}
                .plan-divider {{ background-color: #{selection}; border-radius: 1px; }}
                .plan-h1 {{ color: #{accent2}; font-weight: bold; font-size: 19px; }}
                .plan-h2 {{ color: #{accent2}; font-weight: bold; font-size: 16px; }}
                .plan-step {{ color: #{accent2}; font-weight: bold; font-size: 15px; }}
                .plan-body {{ color: #{fg_main}; font-size: 14px; }}
                .plan-subnote {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', monospace; font-size: 12px; }}
                .plan-checked-text {{ opacity: 0.55; text-decoration: line-through; }}
                .plan-check-row {{ background-color: #{bg_recessed}; border: 1px solid #{selection}; border-radius: {r_md}px; padding: 6px 10px; }}
                checkbutton.plan-checkbox > check {{ background-color: #{bg_pill}; border: 2px solid #{accent1}; border-radius: {r_chk}px; min-width: 18px; min-height: 18px; }}
                checkbutton.plan-checkbox:checked > check {{ background-color: #{accent2}; border-color: #{accent2}; color: #{bg_main}; }}
                
                .plan-composer-dock {{ background-color: {bg_recessed_glass}; border-top: 1px solid {accent1_35}; padding: 10px 14px; }}
                .plan-delete-menu-btn {{ background-color: rgba(243, 139, 168, 0.14); border: none; border-radius: {r_sm}px; padding: 6px 12px; color: #f38ba8; font-weight: bold; box-shadow: none; transition: background-color 0.15s ease; }}
                .plan-delete-menu-btn:hover {{ background-color: rgba(243, 139, 168, 0.26); }}
                .plan-line-del-btn {{ background-color: transparent; border: 1px solid transparent; border-radius: {r_xxs}px; padding: 0px 5px; color: #{fg_muted}; font-family: 'Josefin Sans', sans-serif; font-size: 14px; font-weight: bold; min-width: 24px; min-height: 24px; box-shadow: none; transition: background-color 0.15s ease, border-color 0.15s ease, color 0.15s ease; }}
                .plan-line-del-btn label {{ color: #{fg_muted}; font-size: 14px; font-weight: bold; }}
                .plan-line-del-btn:hover {{ background-color: rgba(243, 139, 168, 0.16); border-color: rgba(243, 139, 168, 0.42); color: #f38ba8; }}
                .plan-line-del-btn:hover label {{ color: #f38ba8; }}
                
                .plan-drag-handle {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', monospace; font-size: 13px; font-weight: bold; padding: 2px 4px; opacity: 0.7; }}
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
                .sticky-mini-btn label {{ font-family: 'MesloLGL Nerd Font', 'Josefin Sans', sans-serif; font-size: 11px; font-weight: bold; color: #{fg_main}; }}
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
                textview.sticky-text, textview.sticky-text text {{ background-color: transparent; color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; font-size: 14px; }}
                "#
            );
            
            provider.load_from_data(&css);

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
        let height = window.height().max(1080);
        let region = if !is_open {
            let center_y = height / 2;
            Region::create_rectangle(&RectangleInt::new(
                0,
                (center_y - 75).max(0),
                10,
                104,
            ))
        } else {
            // Dynamically hug the EXACT rendered width of the dashboard
            let w = if dash_width > 0 { dash_width } else { BASE_DASH_WIDTH };
            Region::create_rectangle(&RectangleInt::new(
                0,
                0,
                w + TRIGGER_WIDTH,
                height,
            ))
        };
        surface.set_input_region(&region);
        
        // Force a Wayland surface commit so the new invisible hitbox actually applies!
        window.queue_draw();
    }
}
fn move_to_monitor(window: &ApplicationWindow, target: Option<&str>) {
    let mut active_name = target.map(|s| s.to_string());

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
) -> Button {
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

    let last_dash_width = Rc::new(Cell::new(400));
    let dash_watch = dash_box.clone();
    let last_w_watch = last_dash_width.clone();
    dash_box.add_tick_callback(move |box_widget, _| {
        let w = box_widget.width();
        if w > 0 && w != last_w_watch.get() {
            last_w_watch.set(w);
            let push_back = w + 10; // Always push 15px past the exact rendered width!
            dynamic_offset_provider.load_from_data(&format!(
                ".drawer-container {{ transform: translateX(-{}px); }}
                 .drawer-container.open {{ transform: translateX(0px); }}",
                push_back
            ));
        }
        glib::ControlFlow::Continue
    });
    let _ = dash_watch;

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
        .min_content_width(110)
        .max_content_width(110)
        .width_request(110)
        .has_frame(false)
        .hexpand(false)
        .vexpand(true)
        .child(&sidebar_column)
        .build();

    // Grouped sidebar navigation with 1px horizontal dividers between groups
    let sidebar_nav = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(1)
        .margin_top(4)
        .vexpand(true)
        .build();

    let nav_groups: &[&[(&str, &str)]] = &[
        &[("run", "Run"), ("favs", "Favorites"), ("clipboard", "Clipboard")],
        &[("plans", "Plans"), ("calendar", "Calendar"), ("timers", "Timers")],
        &[("music", "Music")],
        &[("drive", "Drive"), ("passwords", "Passwords"), ("locker", "Locker")],
        &[("system", "System")],
    ];

    let mut nav_buttons: Vec<(&'static str, Button)> = Vec::new();
    let mut nav_seps: Vec<(usize, Box)> = Vec::new();
    for (gi, group) in nav_groups.iter().enumerate() {
        if gi > 0 {
            let sep = Box::builder()
                .orientation(Orientation::Horizontal)
                .height_request(1)
                .css_classes(vec!["sidebar-sep".to_string()])
                .build();
            sidebar_nav.append(&sep);
            nav_seps.push((gi, sep));
        }
        for &(id, title) in *group {
            let lbl = Label::builder()
                .label(title)
                .halign(gtk4::Align::Start)
                .hexpand(true)
                .build();
            let btn = Button::builder()
                .child(&lbl)
                .css_classes(vec!["sidebar-nav-btn".to_string()])
                .build();
            let stack_click = stack.clone();
            btn.connect_clicked(move |_| {
                stack_click.set_visible_child_name(id);
            });
            sidebar_nav.append(&btn);
            nav_buttons.push((id, btn));
        }
    }

    let apply_tab_visibility: Rc<dyn Fn()> = {
        let btns = nav_buttons.clone();
        let seps = nav_seps.clone();
        Rc::new(move || {
            let vis = config::get().visible_tabs;
            let is_vis = |id: &str| match id {
                "favs" => vis.favorites,
                "clipboard" => vis.clipboard,
                "plans" => vis.plans,
                "calendar" => vis.calendar,
                "timers" => vis.timers,
                "music" => vis.music,
                "drive" => vis.drive,
                "passwords" => vis.passwords,
                "locker" => vis.locker,
                _ => true,
            };
            for (id, btn) in &btns {
                btn.set_visible(is_vis(id));
            }
            for (gi, sep) in &seps {
                let any_in_group = nav_groups[*gi].iter().any(|(id, _)| is_vis(id));
                sep.set_visible(any_in_group);
            }
            favorites::set_scoring_enabled(vis.favorites);
            clipboard::set_watcher_enabled(vis.clipboard);
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

        btn.set_sensitive(false);
        lbl_for_reload.set_label("Building...");

        let (tx, rx) = std::sync::mpsc::channel::<Result<std::path::PathBuf, String>>();

        std::thread::spawn(move || {
            let run_cargo = |offline: bool| {
                let mut cmd = std::process::Command::new("cargo");
                cmd.args(["build", "--release"]);
                if offline {
                    cmd.arg("--offline");
                }
                cmd.current_dir(&repo_path).output()
            };

            let target_bin = repo_path.join("target/release/lily-dashboard");
            let res = match run_cargo(false) {
                Ok(out) if out.status.success() => Ok(target_bin),
                first => match run_cargo(true) {
                    Ok(out) if out.status.success() => Ok(target_bin),
                    Ok(out) => {
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
                        let msg = format!("Failed to invoke cargo ({:?}): {}", first.err(), e);
                        let _ = std::fs::write("/tmp/lily-reload.log", &msg);
                        Err(msg)
                    }
                },
            };
            let _ = tx.send(res);
        });

        let app_quit = app_for_reload.clone();
        let lbl_status = lbl_for_reload.clone();
        let btn_status = btn.clone();

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

    // Two-button Power / Restart row with two-step "Sure?" confirmation
    let power_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .margin_start(8)
        .margin_end(8)
        .margin_top(2)
        .margin_bottom(14)
        .build();

    let poweroff_lbl = Label::builder()
        .label("")
        .css_classes(vec!["power-btn-lbl".to_string()])
        .build();
    let poweroff_btn = Button::builder()
        .child(&poweroff_lbl)
        .css_classes(vec!["power-btn".to_string()])
        .tooltip_text("Power off (click again to confirm)")
        .build();

    let reboot_lbl = Label::builder()
        .label("󰜉")
        .css_classes(vec!["power-btn-lbl".to_string()])
        .build();
    let reboot_btn = Button::builder()
        .child(&reboot_lbl)
        .css_classes(vec!["power-btn".to_string()])
        .tooltip_text("Restart (click again to confirm)")
        .build();

    let armed_power: Rc<Cell<Option<u8>>> = Rc::new(Cell::new(None));
    let armed_gen: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    {
        let armed_c = armed_power.clone();
        let gen_c = armed_gen.clone();
        let po_btn = poweroff_btn.clone();
        let po_lbl = poweroff_lbl.clone();
        let rb_btn = reboot_btn.clone();
        let rb_lbl = reboot_lbl.clone();
        poweroff_btn.connect_clicked(move |_| {
            if armed_c.get() == Some(0) {
                armed_c.set(None);
                let _ = std::process::Command::new("systemctl")
                    .arg("poweroff")
                    .spawn();
                return;
            }
            armed_c.set(Some(0));
            let my_gen = gen_c.get().wrapping_add(1);
            gen_c.set(my_gen);

            po_lbl.set_label("Sure?");
            po_btn.add_css_class("armed");
            rb_lbl.set_label("󰜉");
            rb_btn.remove_css_class("armed");

            let armed_r = armed_c.clone();
            let gen_r = gen_c.clone();
            let po_btn_r = po_btn.clone();
            let po_lbl_r = po_lbl.clone();
            glib::timeout_add_local_once(Duration::from_secs(3), move || {
                if gen_r.get() == my_gen && armed_r.get() == Some(0) {
                    armed_r.set(None);
                    po_lbl_r.set_label("");
                    po_btn_r.remove_css_class("armed");
                }
            });
        });
    }

    {
        let armed_c = armed_power.clone();
        let gen_c = armed_gen.clone();
        let po_btn = poweroff_btn.clone();
        let po_lbl = poweroff_lbl.clone();
        let rb_btn = reboot_btn.clone();
        let rb_lbl = reboot_lbl.clone();
        reboot_btn.connect_clicked(move |_| {
            if armed_c.get() == Some(1) {
                armed_c.set(None);
                let _ = std::process::Command::new("systemctl")
                    .arg("reboot")
                    .spawn();
                return;
            }
            armed_c.set(Some(1));
            let my_gen = gen_c.get().wrapping_add(1);
            gen_c.set(my_gen);

            rb_lbl.set_label("Sure?");
            rb_btn.add_css_class("armed");
            po_lbl.set_label("");
            po_btn.remove_css_class("armed");

            let armed_r = armed_c.clone();
            let gen_r = gen_c.clone();
            let rb_btn_r = rb_btn.clone();
            let rb_lbl_r = rb_lbl.clone();
            glib::timeout_add_local_once(Duration::from_secs(3), move || {
                if gen_r.get() == my_gen && armed_r.get() == Some(1) {
                    armed_r.set(None);
                    rb_lbl_r.set_label("󰜉");
                    rb_btn_r.remove_css_class("armed");
                }
            });
        });
    }

    power_row.append(&poweroff_btn);
    power_row.append(&reboot_btn);

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
    let popover_guard = Rc::new(Cell::new(0usize));

    let plans_ui = plans::build_plans_ui(popover_guard.clone(), move |_expand| {
        // We don't need to manually update the hitbox here anymore!
        // The bulletproof win_tick callback perfectly handles width expansions for us automatically.
    });

    stack.add_titled(&plans_ui.list_view, Some("plans"), "Plans");

    let favs_ui = favorites::build_favorites_ui(popover_guard.clone());
    stack.add_titled(&favs_ui.root, Some("favs"), "Favorites");

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

    // Keep background app RAM-preloading on startup without building Run UI widgets until opened
    launcher::preload_configured_apps();

    let make_lazy_slot = || {
        Box::builder()
            .orientation(Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build()
    };

    let run_slot = make_lazy_slot();
    stack.add_titled(&run_slot, Some("run"), "Run");

    let passwords_slot = make_lazy_slot();
    stack.add_titled(&passwords_slot, Some("passwords"), "Passwords");

    let drive_slot = make_lazy_slot();
    stack.add_titled(&drive_slot, Some("drive"), "Drive");

    let locker_slot = make_lazy_slot();
    stack.add_titled(&locker_slot, Some("locker"), "Locker");

    let music_slot = make_lazy_slot();
    let timers_slot = make_lazy_slot();
    let calendar_slot = make_lazy_slot();
    let clipboard_slot = make_lazy_slot();
    let system_slot = make_lazy_slot();

    let run_search_slot: Rc<RefCell<Option<gtk4::SearchEntry>>> = Rc::new(RefCell::new(None));
    let system_ui_slot: Rc<RefCell<Option<system::SystemUi>>> = Rc::new(RefCell::new(None));

    let detail_tab = plans_ui.detail_view.clone();
    let dash_for_sys = dash_box.clone();
    let guard_lazy = popover_guard.clone();
    let btn_on_launch = trigger_btn.clone();
    let open_on_launch = is_open.clone();
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
            system::launch_capture(system::CaptureMode::RegionEditor, &close_cap_l);
        });
        let cap_rc = GestureClick::builder().button(3).build();
        let close_cap_r = close_drawer_cb.clone();
        cap_rc.connect_pressed(move |_, _, _, _| {
            system::launch_capture(system::CaptureMode::Monitor, &close_cap_r);
        });
        cap_btn.add_controller(cap_rc);

        let picker_btn = Button::builder()
            .label("󰈊")
            .css_classes(vec!["top-tool-btn".to_string()])
            .tooltip_text("Color Picker (copies #hex to clipboard)")
            .build();
        let close_pick = close_drawer_cb.clone();
        picker_btn.connect_clicked(move |_| {
            system::launch_color_picker(&close_pick);
        });

        top_tools.append(&sticky_btn);
        top_tools.append(&cap_btn);
        top_tools.append(&picker_btn);
        workspaces_widget.append(&top_tools);
    }

    let close_for_sys = close_drawer_cb.clone();
    let reload_theme_for_sys = reload_theme.clone();
    let apply_tabs_for_sys = apply_tab_visibility.clone();
    let run_slot_c = run_slot.clone();
    let passwords_slot_c = passwords_slot.clone();
    let drive_slot_c = drive_slot.clone();
    let locker_slot_c = locker_slot.clone();
    let music_slot_c = music_slot.clone();
    let timers_slot_c = timers_slot.clone();
    let calendar_slot_c = calendar_slot.clone();
    let clipboard_slot_c = clipboard_slot.clone();
    let system_slot_c = system_slot.clone();
    let run_search_c = run_search_slot.clone();
    let system_ui_c = system_ui_slot.clone();

    let nav_buttons_sync = nav_buttons.clone();

    stack.connect_visible_child_name_notify(move |stk| {
        let current = stk.visible_child_name();
        let name = current.as_deref();

        for (id, btn) in &nav_buttons_sync {
            if name == Some(*id) {
                btn.add_css_class("selected");
            } else {
                btn.remove_css_class("selected");
            }
        }

        if name != Some("plans") {
            detail_tab.set_visible(false);
        }
        if name != Some("system") {
            if let Some(sys) = system_ui_c.borrow().as_ref() {
                sys.detail_view.set_visible(false);
            }
        }

        // Only keep Favorites row widgets built while Favorites is the active tab
        favs_ui.set_active(name == Some("favs"));

        // Unload stateless tabs when switching away so unused modules don't sit in RAM
        if name != Some("run") {
            *run_search_c.borrow_mut() = None;
            while let Some(child) = run_slot_c.first_child() {
                run_slot_c.remove(&child);
            }
        }
        if name != Some("passwords") {
            while let Some(child) = passwords_slot_c.first_child() {
                passwords_slot_c.remove(&child);
            }
        }
        if name != Some("clipboard") {
            while let Some(child) = clipboard_slot_c.first_child() {
                clipboard_slot_c.remove(&child);
            }
        }
        if name != Some("calendar") {
            while let Some(child) = calendar_slot_c.first_child() {
                calendar_slot_c.remove(&child);
            }
        }

        match name {
            Some("run") => {
                if run_slot_c.first_child().is_none() {
                    let btn_l = btn_on_launch.clone();
                    let open_l = open_on_launch.clone();
                    let launcher_ui = launcher::build_launcher_ui(guard_lazy.clone(), move || {
                        if open_l.get() {
                            btn_l.emit_clicked();
                        }
                    });
                    run_slot_c.append(&launcher_ui.root);
                    *run_search_c.borrow_mut() = Some(launcher_ui.search_entry);
                }
                if let Some(entry) = run_search_c.borrow().as_ref() {
                    entry.grab_focus();
                }
            }
            Some("passwords") => {
                if passwords_slot_c.first_child().is_none() {
                    passwords_slot_c.append(&passwords::build_passwords_ui(guard_lazy.clone()));
                }
            }
            Some("clipboard") => {
                if clipboard_slot_c.first_child().is_none() {
                    clipboard_slot_c.append(&clipboard::build_clipboard_ui(guard_lazy.clone()));
                }
            }
            Some("calendar") => {
                if calendar_slot_c.first_child().is_none() {
                    calendar_slot_c.append(&calendar::build_calendar_ui());
                }
            }
            // Stateful tabs (active uploads, unlocked vault, playing music, active timers)
            // are lazy-built on first visit and kept alive once opened.
            Some("drive") => {
                if drive_slot_c.first_child().is_none() {
                    drive_slot_c.append(&drive::build_drive_ui());
                }
            }
            Some("locker") => {
                if locker_slot_c.first_child().is_none() {
                    locker_slot_c.append(&locker::build_locker_ui(guard_lazy.clone()));
                }
            }
            Some("music") => {
                if music_slot_c.first_child().is_none() {
                    music_slot_c.append(&music::build_music_ui());
                }
            }
            Some("timers") => {
                if timers_slot_c.first_child().is_none() {
                    timers_slot_c.append(&timers::build_timers_ui());
                }
            }
            Some("system") => {
                if system_ui_c.borrow().is_none() {
                    let sys_ui = system::build_system_ui(
                        close_for_sys.clone(),
                        reload_theme_for_sys.clone(),
                        apply_tabs_for_sys.clone(),
                    );
                    system_slot_c.append(&sys_ui.list_view);
                    dash_for_sys.append(&sys_ui.detail_view);
                    *system_ui_c.borrow_mut() = Some(sys_ui);
                }
                if let Some(sys) = system_ui_c.borrow().as_ref() {
                    sys.open_panel();
                }
            }
            _ => {}
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
    dash_box.append(&plans_ui.detail_view);

    stack.add_titled(&music_slot, Some("music"), "Music");
    stack.add_titled(&timers_slot, Some("timers"), "Timers");
    stack.add_titled(&calendar_slot, Some("calendar"), "Calendar");
    stack.add_titled(&clipboard_slot, Some("clipboard"), "Clipboard");
    stack.add_titled(&system_slot, Some("system"), "System");

    // Default to the Run tab on startup
    stack.set_visible_child_name("run");

    let win_motion = EventControllerMotion::new();
    let btn_motion = trigger_btn.clone();
    let hovered_motion = is_hovered.clone();
    let open_motion = is_open.clone();
    let closing_motion = is_closing.clone();

    win_motion.connect_enter(move |_, _, _| {
        if !open_motion.get() && !closing_motion.get() {
            hovered_motion.set(true);
            btn_motion.add_css_class("peeking");
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
    win_click.connect_pressed(move |_, _, x, _| {
        if !open_win_click.get() {
            btn_win_click.emit_clicked();
        } else {
            // Dynamically calculate the right edge of the dashboard based on actual physical width!
            let current_w = dash_box_click.width() as f64;
            let dash_edge = if current_w > 0.0 { current_w } else { BASE_DASH_WIDTH as f64 };
            if x >= dash_edge {
                btn_win_click.emit_clicked();
            }
        }
    });
    window.add_controller(win_click);

    let drawer_clone = drawer_container.clone();
    let detail_clone = plans_ui.detail_view.clone();
    let system_ui_close = system_ui_slot.clone();
    let media_for_toggle = media_handle.clone();
    let stickies_for_toggle = stickies_handle.clone();
    let run_search_open = run_search_slot.clone();
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

            window_clone.set_keyboard_mode(KeyboardMode::OnDemand);
            // Pass the exact width of the dashboard to the hitbox
            update_input_region(&window_clone, true, dash_box_btn.width());
            window_clone.present();

            btn.add_css_class("riding");
            drawer_clone.add_css_class("open");

            if stack_for_open.visible_child_name().as_deref() == Some("run") {
                if let Some(entry) = run_search_open.borrow().as_ref() {
                    entry.grab_focus();
                }
            }
        } else {
            open_click.set(false);
            closing_click.set(true);
            hovered_click.set(false);
            media_for_toggle.set_dashboard_open(false);
            stickies_for_toggle.set_dashboard_open(false);

            detail_clone.set_visible(false);
            if let Some(sys) = system_ui_close.borrow().as_ref() {
                sys.detail_view.set_visible(false);
            }
            window_clone.set_keyboard_mode(KeyboardMode::None);
            drawer_clone.remove_css_class("open");
            btn.remove_css_class("peeking");

            let btn_after = btn.clone();
            let win_after = window_clone.clone();
            let open_after = open_click.clone();
            let closing_after = closing_click.clone();

            glib::timeout_add_local_once(Duration::from_millis(SLIDE_DURATION_MS), move || {
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
    let btn_esc = trigger_btn.clone();
    let open_esc = is_open.clone();
    key_controller.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape && open_esc.get() {
            btn_esc.emit_clicked();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_controller);

    drawer_container.append(&dash_box);
    drawer_container.append(&trigger_wrapper);

    window.set_child(Some(&drawer_container));

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

    trigger_btn
}