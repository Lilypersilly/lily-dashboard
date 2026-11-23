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

use gtk4::cairo::{RectangleInt, Region};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box, Button, CssProvider, EventControllerKey,
    EventControllerMotion, GestureClick, Label, Orientation, Overflow, Stack, StackSidebar, gdk,
    glib,
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
    

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| init_theme());

    // Store the trigger button so re-running `lily-dashboard` (e.g. via SUPER keybind)
    // toggles the already-running dashboard open/closed instead of spawning a second window!
    let existing_trigger: Rc<RefCell<Option<Button>>> = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        if let Some(btn) = existing_trigger.borrow().as_ref() {
            btn.emit_clicked();
            return;
        }

        session::init_session_tracker();

        let btn = build_ui(app);
        *existing_trigger.borrow_mut() = Some(btn);
    });

    app.run();
}

fn init_theme() {
    let provider = CssProvider::new();
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let update_css = {
        let provider = provider.clone();
        move || {
            let mut map = std::collections::HashMap::new();
            let mut path = glib::user_config_dir();
            path.push("theme-switch");
            path.push("current.conf");
            
            if let Ok(content) = std::fs::read_to_string(&path) {
                for line in content.lines() {
                    let line = line.split('#').next().unwrap_or("").trim();
                    if let Some((k, v)) = line.split_once('=') {
                        map.insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
            }

            macro_rules! get_c {
                ($k:expr, $fallback:expr) => {
                    map.get($k).map(|s| s.as_str()).unwrap_or($fallback)
                };
            }
            
            // Map the generic theme variables to the dashboard's semantic roles
            let bg_main = get_c!("bg_main", "fee4ff");
            let bg_pill = get_c!("bg_pill", get_c!("bg_dark", "ffc2ff"));
            let bg_recessed = get_c!("bg_recessed", get_c!("bg_dark", "f8d4ff"));
            let bg_contrast = get_c!("bg_contrast", get_c!("bg_dark", "322a32"));
            
            let fg_main = get_c!("fg_main", "333933");
            let fg_muted = get_c!("fg_muted", get_c!("fg_dim", "856d8a"));
            
            let accent1 = get_c!("accent", get_c!("accent1", "e975ca"));
            let accent2 = get_c!("magenta", get_c!("accent2", "bf40a1"));
            let selection = get_c!("selection", get_c!("accent1", "ef96d9"));
            
            let hex_to_rgba = |hex: &str, alpha: f32| -> String {
                let clean = hex.trim_start_matches('#');
                if clean.len() >= 6 {
                    if let (Ok(r), Ok(g), Ok(b)) = (
                        u8::from_str_radix(&clean[0..2], 16),
                        u8::from_str_radix(&clean[2..4], 16),
                        u8::from_str_radix(&clean[4..6], 16),
                    ) {
                        return format!("rgba({}, {}, {}, {})", r, g, b, alpha);
                    }
                }
                format!("rgba(128, 128, 128, {})", alpha)
            };

            let accent1_15 = hex_to_rgba(accent1, 0.15);
            let fg_main_06 = hex_to_rgba(fg_main, 0.06);
            let bg_contrast_50 = hex_to_rgba(bg_contrast, 0.50);
            let bg_contrast_80 = hex_to_rgba(bg_contrast, 0.80);

            let css = format!(
                r#"
                window {{ background-color: transparent; }}
                .drawer-container {{ transform: translateX(-415px); transition: transform 0.35s cubic-bezier(0.25, 0.8, 0.25, 1); opacity: 0.95; }}
                .drawer-container.open {{ transform: translateX(0px); opacity: 0.95; }}
                
                .dash-box {{ background-color: #{bg_main}; border: 2px solid #{accent1}; border-left: none; border-top-right-radius: 16px; border-bottom-right-radius: 16px; min-width: 398px; }}
                .sidebar-column {{ background-color: #{bg_recessed}; border-right: 1px solid #{accent1}; }}
                
                stacksidebar {{ background-color: transparent; }}
                stacksidebar list {{ background-color: transparent; }}
                stacksidebar row {{ padding: 8px 10px; border-radius: 12px; margin: 2px 4px; transition: background-color 0.2s ease, color 0.2s ease; }}
                stacksidebar row:hover {{ background-color: {accent1_15}; }}
                stacksidebar row:checked, stacksidebar row.selected {{ background-color: {fg_main_06}; }}
                stacksidebar label {{ color: #{fg_muted}; font-family: 'Josefin Sans', sans-serif; font-size: 16px; font-weight: bold; }}
                stacksidebar row:checked label, stacksidebar row.selected label {{ color: #{fg_main}; }}
                
                .fav-item-btn, .music-item-album, .music-item-playlist {{ background-color: transparent; border: none; border-radius: 12px; padding: 6px 8px; transition: background-color 0.15s ease; box-shadow: none; }}
                .fav-item-btn:hover, .music-item-album:hover, .music-item-playlist:hover {{ background-color: {accent1_15}; }}
                .fav-item-btn.pinned, .music-item-album.pinned, .music-item-playlist.pinned {{ background-color: #{selection}; border: none; }}
                
                .music-art-frame {{ background-color: transparent; border: none; border-radius: 8px; min-width: 36px; min-height: 36px; box-shadow: 0px 2px 5px rgba(0, 0, 0, 0.15); }}
                
                .fav-search {{ background-color: {bg_contrast_50}; border: 1px solid #{accent1}; border-radius: 12px; color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; padding: 6px 12px; }}
                .fav-search:focus {{ background-color: {bg_contrast_80}; }}
                
                .fav-group-header {{ color: #{accent2}; font-weight: bold; font-size: 13px; font-family: 'MesloLGL Nerd Font', monospace; margin-left: 4px; letter-spacing: 1px; }}
                .fav-score-badge {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', monospace; font-size: 12px; font-weight: bold; }}
                
                .plan-fmt-btn, .plan-close-btn {{ background-color: #{bg_pill}; border: 1px solid #{accent1}; border-radius: 8px; padding: 4px 10px; color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; font-weight: bold; transition: all 0.15s ease; }}
                .plan-fmt-btn:hover, .plan-close-btn:hover {{ background-color: #{selection}; }}
                .plan-fmt-btn.active, .plan-close-btn.active {{ background-color: #{accent2}; border-color: #{accent2}; color: #{bg_contrast}; }}
                
                .session-restore-btn {{ background-color: transparent; border: 1px solid #{accent1}; border-radius: 10px; padding: 8px 6px; margin: 4px 8px; transition: background-color 0.15s ease; }}
                .session-restore-btn:hover {{ background-color: {accent1_15}; }}
                .session-restore-lbl {{ color: #{accent2}; font-size: 12px; font-weight: bold; }}
                
                label {{ color: #{fg_main}; font-family: 'Josefin Sans', sans-serif; font-size: 16px; }}
                
                .trigger-wrapper {{ min-width: 56px; min-height: 130px; }}
                .trigger-btn {{ background-color: #{bg_pill}; border: 2px solid #{accent1}; border-left: none; border-top-right-radius: 100px; border-bottom-right-radius: 100px; min-width: 50px; min-height: 100px; padding: 0; transform: translateX(-36px); transition-property: transform, background-color; transition-duration: 0.2s; transition-timing-function: cubic-bezier(0.25, 0.8, 0.25, 1); }}
                .trigger-btn.peeking, .trigger-btn.riding {{ background-color: #{selection}; transform: translateX(0px); }}
                
                popover.fav-popover > contents {{ background-color: #{bg_main}; border: 2px solid #{accent1}; border-radius: 12px; padding: 6px; }}
                .fav-hide-btn {{ background-color: #{bg_pill}; border: none; border-radius: 8px; padding: 6px 12px; color: #{fg_main}; }}
                .fav-hide-btn:hover {{ background-color: #{selection}; }}
                
                .plan-detail-pane {{ background-color: #{bg_main}; border-left: 2px solid #{selection}; min-width: 400px; }}
                .plan-divider {{ background-color: #{selection}; border-radius: 1px; }}
                .plan-h1 {{ color: #{accent2}; font-weight: bold; font-size: 19px; }}
                .plan-h2 {{ color: #{accent2}; font-weight: bold; font-size: 16px; }}
                .plan-step {{ color: #{accent2}; font-weight: bold; font-size: 15px; }}
                .plan-body {{ color: #{fg_main}; font-size: 14px; }}
                .plan-subnote {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', monospace; font-size: 12px; }}
                .plan-checked-text {{ opacity: 0.55; text-decoration: line-through; }}
                .plan-check-row {{ background-color: #{bg_recessed}; border: 1px solid #{selection}; border-radius: 10px; padding: 6px 10px; }}
                checkbutton.plan-checkbox > check {{ background-color: #{bg_pill}; border: 2px solid #{accent1}; border-radius: 5px; min-width: 18px; min-height: 18px; }}
                checkbutton.plan-checkbox:checked > check {{ background-color: #{accent2}; border-color: #{accent2}; color: #{bg_main}; }}
                
                .plan-composer-dock {{ background-color: #{bg_recessed}; border-top: 2px solid #{selection}; padding: 10px 14px; }}
                .plan-delete-menu-btn {{ background-color: #{bg_pill}; border: none; border-radius: 8px; padding: 6px 12px; color: #{accent2}; font-weight: bold; }}
                .plan-delete-menu-btn:hover {{ background-color: #{selection}; }}
                .plan-line-del-btn {{ background-color: #{bg_pill}; border: 1px solid #{accent1}; border-radius: 6px; padding: 1px 6px; color: #{accent2}; font-size: 11px; min-width: 22px; min-height: 22px; }}
                .plan-line-del-btn:hover {{ background-color: #{selection}; }}
                
                .plan-drag-handle {{ color: #{accent2}; font-family: 'MesloLGL Nerd Font', monospace; font-size: 13px; font-weight: bold; padding: 2px 4px; opacity: 0.7; }}
                .plan-edit-row {{ border: 1px solid transparent; border-radius: 8px; padding: 2px 4px; transition: background-color 0.1s ease, border-color 0.1s ease; }}
                .plan-edit-row:hover {{ background-color: #{bg_recessed}; }}
                .plan-edit-row.dragging {{ opacity: 0.4; }}
                .plan-edit-row.drop-target {{ border: 2px dashed #{accent2}; background-color: #{bg_pill}; }}
                
                scale.music-seek {{ padding: 2px 0px; }}
                scale.music-seek > trough {{ background-color: #{bg_pill}; border: 1px solid #{accent1}; border-radius: 6px; min-height: 6px; }}
                scale.music-seek > trough > highlight {{ background-color: #{accent2}; border-radius: 6px; }}
                scale.music-seek > trough > slider {{ background-color: #{bg_main}; border: 2px solid #{accent2}; border-radius: 50%; min-width: 12px; min-height: 12px; margin: -4px 0px; }}
                
                progressbar.drive-progress > trough {{ background-color: #{bg_pill}; border: 1px solid #{accent1}; border-radius: 6px; min-height: 14px; }}
                progressbar.drive-progress > trough > progress {{ background-color: #{accent2}; border-radius: 5px; }}
                "#
            );
            
            // Fix 1: Pass the string directly instead of converting to bytes
            provider.load_from_data(&css);
        }
    };

    // Load it immediately on startup
    update_css();

    // Attach FileMonitor so it live-updates instantly when you run theme-switch!
    let mut path = glib::user_config_dir();
    path.push("theme-switch");
    path.push("current.conf");
    let file = gtk4::gio::File::for_path(&path);
    
    if let Ok(monitor) = file.monitor_file(gtk4::gio::FileMonitorFlags::NONE, gtk4::gio::Cancellable::NONE) {
        monitor.connect_changed(move |_, _, _, event| {
            if event == gtk4::gio::FileMonitorEvent::ChangesDoneHint || event == gtk4::gio::FileMonitorEvent::Created {
                update_css();
            }
        });
        
        // Fix 2: Explicitly tell Rust to use the memory Box, not the GTK UI Box!
        std::boxed::Box::leak(std::boxed::Box::new(monitor)); 
    }
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
fn build_ui(app: &Application) -> Button {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Lily Dashboard")
        .build();

    window.set_size_request(TOTAL_DASH_WIDTH + TRIGGER_WIDTH, -1);

    window.init_layer_shell();
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

    let sidebar = StackSidebar::new();
    sidebar.set_stack(&stack);
    sidebar.set_vexpand(true);

    let reload_lbl = Label::builder()
        .label("Reload\ndashboard")
        .justify(gtk4::Justification::Center)
        .css_classes(vec!["session-restore-lbl".to_string()])
        .build();

    let reload_btn = Button::builder()
        .child(&reload_lbl)
        .css_classes(vec!["session-restore-btn".to_string()])
        .tooltip_text("Recompile release binary and restart Lily Dashboard")
        .build();

    let app_for_reload = app.clone();
    let lbl_for_reload = reload_lbl.clone();
    reload_btn.connect_clicked(move |btn| {
        btn.set_sensitive(false);
        lbl_for_reload.set_label("Building...");

        let (tx, rx) = std::sync::mpsc::channel::<bool>();

        std::thread::spawn(move || {
            let status = std::process::Command::new("cargo")
                .args(["build", "--release"])
                .current_dir("/home/lilypersilly/lily-dashboard")
                .status();

            let ok = status.map(|s| s.success()).unwrap_or(false);
            let _ = tx.send(ok);
        });

        let app_quit = app_for_reload.clone();
        let lbl_status = lbl_for_reload.clone();
        let btn_status = btn.clone();

        glib::timeout_add_local(Duration::from_millis(100), move || {
            if let Ok(success) = rx.try_recv() {
                if success {
                    let _ = std::process::Command::new("sh")
                        .arg("-c")
                        .arg("sleep 0.2 && /home/lilypersilly/lily-dashboard/target/release/lily-dashboard &")
                        .spawn();
                    app_quit.quit();
                } else {
                    lbl_status.set_label("Build\nfailed!");
                    btn_status.set_sensitive(true);
                    let lbl_reset = lbl_status.clone();
                    glib::timeout_add_local_once(Duration::from_millis(2000), move || {
                        lbl_reset.set_label("Reload\ndashboard");
                    });
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    });

    let session_lbl = Label::builder()
        .label("Load previous\nsession")
        .justify(gtk4::Justification::Center)
        .css_classes(vec!["session-restore-lbl".to_string()])
        .build();

    let session_btn = Button::builder()
        .child(&session_lbl)
        .css_classes(vec!["session-restore-btn".to_string()])
        .tooltip_text("Restore windows and workspaces from your previous Hyprland session")
        .build();

    let lbl_feedback = session_lbl.clone();
    session_btn.connect_clicked(move |_| {
        let count = session::restore_previous_session();
        if count > 0 {
            lbl_feedback.set_label(&format!("Restored {}!", count));
        } else {
            lbl_feedback.set_label("No session\nfound");
        }
        let lbl_reset = lbl_feedback.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            lbl_reset.set_label("Load previous\nsession");
        });
    });

    // Create the weather tab AND the small sidebar widget button
    let (weather_page, weather_sidebar_btn) = weather::build_weather_ui(&stack);
    
    // Add to stack using `add_named`. 
    // Using `add_named` (no title provided) tells GTK to intentionally HIDE it from the default text sidebar list!
    stack.add_named(&weather_page, Some("weather"));

    // Add everything to the sidebar column in order
    sidebar_column.append(&sidebar);
    sidebar_column.append(&weather_sidebar_btn); // <-- The cute weather widget goes here!
    sidebar_column.append(&reload_btn);
    sidebar_column.append(&session_btn);

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

    let run_search_slot: Rc<RefCell<Option<gtk4::SearchEntry>>> = Rc::new(RefCell::new(None));

    let detail_tab = plans_ui.detail_view.clone();
    let guard_lazy = popover_guard.clone();
    let btn_on_launch = trigger_btn.clone();
    let open_on_launch = is_open.clone();
    let run_slot_c = run_slot.clone();
    let passwords_slot_c = passwords_slot.clone();
    let drive_slot_c = drive_slot.clone();
    let locker_slot_c = locker_slot.clone();
    let music_slot_c = music_slot.clone();
    let timers_slot_c = timers_slot.clone();
    let run_search_c = run_search_slot.clone();

    stack.connect_visible_child_name_notify(move |stk| {
        let current = stk.visible_child_name();
        let name = current.as_deref();

        if name != Some("plans") {
            detail_tab.set_visible(false);
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

    dash_box.append(&sidebar_clamp);
    dash_box.append(&stack_clamp);
    dash_box.append(&plans_ui.detail_view);

    stack.add_titled(&music_slot, Some("music"), "Music");
    stack.add_titled(&timers_slot, Some("timers"), "Timers");

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

            window_clone.set_keyboard_mode(KeyboardMode::OnDemand);
            // Pass the exact width of the dashboard to the hitbox
            update_input_region(&window_clone, true, dash_box_btn.width());
            window_clone.present();

            btn.add_css_class("riding");
            drawer_clone.add_css_class("open");
        } else {
            open_click.set(false);
            closing_click.set(true);
            hovered_click.set(false);

            detail_clone.set_visible(false);
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