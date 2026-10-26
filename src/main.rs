mod favorites;
mod launcher;
mod passwords;
mod music;
mod plans;
mod session;
mod drive;

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
    app.connect_startup(|_| load_css());

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

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data(
        "
        window {
            background-color: transparent;
        }
        
        .drawer-container {
            transform: translateX(-402px);
            transition: transform 0.35s cubic-bezier(0.25, 0.8, 0.25, 1);
        }

        .drawer-container {
            transform: translateX(-406px);
            transition: transform 0.35s cubic-bezier(0.25, 0.8, 0.25, 1);
        }

        .drawer-container.open {
            transform: translateX(0px);
        }

        .dash-box {
            background-color: #fee4ff;
            border: 2px solid #e975ca;
            border-left: none;
            border-top-right-radius: 16px;
            border-bottom-right-radius: 16px;
            min-width: 398px;
        }

        .drawer-container.open .dash-box {
            opacity: 1;
        }
        
        .sidebar-column {
            background-color: #f8d4ff;
            border-right: 2px solid #ef96d9;
        }

        stacksidebar {
            background-color: transparent;
            border-right: none;
        }

        .session-restore-btn {
            background-color: #ffc2ff;
            border: 1px solid #e975ca;
            border-radius: 10px;
            padding: 8px 6px;
            margin: 4px 8px;
        }

        .session-restore-btn:hover {
            background-color: #ef96d9;
        }

        .session-restore-lbl {
            color: #bf40a1;
            font-size: 12px;
            font-weight: bold;
        }
        
        label {
            color: #333933;
            font-family: 'Josefin Sans', sans-serif;
            font-size: 16px;
        }

        .trigger-wrapper {
            min-width: 56px;
            min-height: 130px;
        }

        .trigger-btn {
            background-color: #ffc2ff;
            border: 2px solid #e975ca;
            border-left: none;
            border-top-right-radius: 100px;
            border-bottom-right-radius: 100px;
            min-width: 50px;
            min-height: 100px;
            padding: 0;
            transform: translateX(-36px);
            transition-property: transform, background-color;
            transition-duration: 0.2s;
            transition-timing-function: cubic-bezier(0.25, 0.8, 0.25, 1);
        }
        
        .trigger-btn.peeking,
        .trigger-btn.riding {
            background-color: #ef96d9;
            transform: translateX(0px);
        }

        .fav-search {
            background-color: #f8d4ff;
            border: 2px solid #e975ca;
            border-radius: 12px;
            color: #333933;
            font-family: 'Josefin Sans', sans-serif;
            padding: 4px 8px;
        }

        .fav-group-header {
            color: #bf40a1;
            font-weight: bold;
            font-size: 13px;
            font-family: 'MesloLGL Nerd Font', monospace;
            margin-left: 4px;
        }

        .fav-item-btn {
            background-color: #ffc2ff;
            border: 1px solid #e975ca;
            border-radius: 10px;
            padding: 6px 10px;
            transition: background-color 0.15s ease;
        }

        .fav-item-btn.pinned {
            border: 2px solid #bf40a1;
        }

        .fav-item-btn:hover {
            background-color: #ef96d9;
        }

        .fav-score-badge {
            color: #bf40a1;
            font-family: 'MesloLGL Nerd Font', monospace;
            font-size: 12px;
        }

        popover.fav-popover > contents {
            background-color: #fee4ff;
            border: 2px solid #e975ca;
            border-radius: 12px;
            padding: 6px;
        }

        .fav-hide-btn {
            background-color: #ffc2ff;
            border: none;
            border-radius: 8px;
            padding: 6px 12px;
            color: #333933;
        }

        .fav-hide-btn:hover {
            background-color: #ef96d9;
        }

        .plan-detail-pane {
            background-color: #fee4ff;
            border-left: 2px solid #ef96d9;
            min-width: 400px;
        }

        .plan-divider {
            background-color: #ef96d9;
            border-radius: 1px;
        }

        .plan-close-btn {
            background-color: #ffc2ff;
            border: 1px solid #e975ca;
            border-radius: 8px;
            padding: 4px 10px;
            color: #bf40a1;
            font-weight: bold;
        }

        .plan-close-btn:hover {
            background-color: #ef96d9;
        }

        .plan-h1 {
            color: #bf40a1;
            font-weight: bold;
            font-size: 19px;
        }

        .plan-h2 {
            color: #bf40a1;
            font-weight: bold;
            font-size: 16px;
        }

        .plan-step {
            color: #bf40a1;
            font-weight: bold;
            font-size: 15px;
        }

        .plan-body {
            color: #333933;
            font-size: 14px;
        }

        .plan-subnote {
            color: #bf40a1;
            font-family: 'MesloLGL Nerd Font', monospace;
            font-size: 12px;
        }

        .plan-checked-text {
            opacity: 0.55;
            text-decoration: line-through;
        }

        .plan-check-row {
            background-color: #f8d4ff;
            border: 1px solid #ef96d9;
            border-radius: 10px;
            padding: 6px 10px;
        }

        checkbutton.plan-checkbox > check {
            background-color: #ffc2ff;
            border: 2px solid #e975ca;
            border-radius: 5px;
            min-width: 18px;
            min-height: 18px;
        }

        checkbutton.plan-checkbox:checked > check {
            background-color: #bf40a1;
            border-color: #bf40a1;
            color: #fee4ff;
        }

        .plan-composer-dock {
            background-color: #f8d4ff;
            border-top: 2px solid #ef96d9;
            padding: 10px 14px;
        }

        .plan-fmt-btn {
            background-color: #ffc2ff;
            border: 1px solid #e975ca;
            border-radius: 8px;
            padding: 3px 8px;
            color: #333933;
            font-family: 'MesloLGL Nerd Font', monospace;
            font-size: 12px;
        }

        .plan-fmt-btn:hover {
            background-color: #ef96d9;
        }

        .plan-fmt-btn.active {
            background-color: #bf40a1;
            border-color: #bf40a1;
            color: #fee4ff;
            font-weight: bold;
        }

        .plan-delete-menu-btn {
            background-color: #ffc2ff;
            border: none;
            border-radius: 8px;
            padding: 6px 12px;
            color: #bf40a1;
            font-weight: bold;
        }

        .plan-delete-menu-btn:hover {
            background-color: #ef96d9;
        }

        .plan-line-del-btn {
            background-color: #ffc2ff;
            border: 1px solid #e975ca;
            border-radius: 6px;
            padding: 1px 6px;
            color: #bf40a1;
            font-size: 11px;
            min-width: 22px;
            min-height: 22px;
        }

        .plan-line-del-btn:hover {
            background-color: #ef96d9;
        }

        .plan-drag-handle {
            color: #bf40a1;
            font-family: 'MesloLGL Nerd Font', monospace;
            font-size: 13px;
            font-weight: bold;
            padding: 2px 4px;
            opacity: 0.7;
        }

        .plan-edit-row {
            border: 1px solid transparent;
            border-radius: 8px;
            padding: 2px 4px;
            transition: background-color 0.1s ease, border-color 0.1s ease;
        }

        .plan-edit-row:hover {
            background-color: #f8d4ff;
        }

        .plan-edit-row.dragging {
            opacity: 0.4;
        }

        .plan-edit-row.drop-target {
            border: 2px dashed #bf40a1;
            background-color: #ffc2ff;
        }

        scale.music-seek {
            padding: 2px 0px;
        }

        scale.music-seek > trough {
            background-color: #ffc2ff;
            border: 1px solid #e975ca;
            border-radius: 6px;
            min-height: 6px;
        }

        scale.music-seek > trough > highlight {
            background-color: #bf40a1;
            border-radius: 6px;
        }

        scale.music-seek > trough > slider {
            background-color: #fee4ff;
            border: 2px solid #bf40a1;
            border-radius: 50%;
            min-width: 12px;
            min-height: 12px;
            margin: -4px 0px;
        }

        progressbar.drive-progress > trough {
            background-color: #ffc2ff;
            border: 1px solid #e975ca;
            border-radius: 6px;
            min-height: 14px;
        }

        progressbar.drive-progress > trough > progress {
            background-color: #bf40a1;
            border-radius: 5px;
        }
        /* --- PASTEL COLOR-CODED MUSIC WIDGETS --- */
        /* Songs = Pastel Pink (.fav-item-btn) */

        /* Albums = Pastel Blue */
        .music-item-album {
            background-color: #d0e8ff;
            border: 1px solid #75b8e9;
            border-radius: 10px;
            padding: 6px 10px;
            transition: background-color 0.15s ease;
        }

        .music-item-album:hover {
            background-color: #add6ff;
        }

        /* Playlists = Pastel Orange / Peach */
        .music-item-playlist {
            background-color: #ffe0c2;
            border: 1px solid #e9a275;
            border-radius: 10px;
            padding: 6px 10px;
            transition: background-color 0.15s ease;
        }

        .music-item-playlist:hover {
            background-color: #ffcaa0;
        }

        /* Fixed 36x36 Album Art Frame */
        .music-art-frame {
            background-color: #f8d4ff;
            border: 1px solid #e975ca;
            border-radius: 6px;
            min-width: 36px;
            min-height: 36px;
        }
        ",
    );

    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn update_input_region(window: &ApplicationWindow, is_open: bool, is_expanded: bool) {
    if let Some(surface) = window.surface() {
        let height = window.height().max(1080);
        let region = if !is_open {
            let center_y = height / 2;
            Region::create_rectangle(&RectangleInt::new(
                0,
                (center_y - 75).max(0),
                TRIGGER_WIDTH,
                150,
            ))
        } else if is_expanded {
            Region::create_rectangle(&RectangleInt::new(
                0,
                0,
                TOTAL_DASH_WIDTH + TRIGGER_WIDTH,
                height,
            ))
        } else {
            Region::create_rectangle(&RectangleInt::new(
                0,
                0,
                BASE_DASH_WIDTH + TRIGGER_WIDTH,
                height,
            ))
        };
        surface.set_input_region(&region);
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

    let stack = Stack::new();
    stack.set_hhomogeneous(false);
    stack.set_size_request(288, -1);
    stack.set_hexpand(false);
    stack.set_vexpand(true);

    let sidebar_column = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["sidebar-column".to_string()])
        .width_request(110)
        .vexpand(true)
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

    sidebar_column.append(&sidebar);
    sidebar_column.append(&reload_btn);
    sidebar_column.append(&session_btn);

    let is_hovered = Rc::new(Cell::new(false));
    let is_open = Rc::new(Cell::new(false));
    let is_expanded = Rc::new(Cell::new(false));
    let is_closing = Rc::new(Cell::new(false));
    let popover_guard = Rc::new(Cell::new(0usize));

    let win_for_plans = window.clone();
    let open_for_plans = is_open.clone();
    let expanded_for_plans = is_expanded.clone();

    let plans_ui = plans::build_plans_ui(popover_guard.clone(), move |expand| {
        expanded_for_plans.set(expand);
        update_input_region(&win_for_plans, open_for_plans.get(), expand);
    });

    stack.add_titled(&plans_ui.list_view, Some("plans"), "Plans");

    let favs_ui = favorites::build_favorites_ui(popover_guard.clone());
    stack.add_titled(&favs_ui, Some("favs"), "Favorites");

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

    let btn_on_launch = trigger_btn.clone();
    let open_on_launch = is_open.clone();
    let launcher_ui = launcher::build_launcher_ui(popover_guard.clone(), move || {
        if open_on_launch.get() {
            btn_on_launch.emit_clicked();
        }
    });
    stack.add_titled(&launcher_ui.root, Some("run"), "Run");

    let passwords_ui = passwords::build_passwords_ui(popover_guard.clone());
    stack.add_titled(&passwords_ui, Some("passwords"), "Passwords");

    let drive_ui = drive::build_drive_ui();
    stack.add_titled(&drive_ui, Some("drive"), "Drive");

    let detail_tab = plans_ui.detail_view.clone();
    let win_tab = window.clone();
    let open_tab = is_open.clone();
    let expanded_tab = is_expanded.clone();
    let run_search = launcher_ui.search_entry.clone();
    stack.connect_visible_child_name_notify(move |stk| {
        let current = stk.visible_child_name();
        if current.as_deref() != Some("plans") && expanded_tab.get() {
            expanded_tab.set(false);
            detail_tab.set_visible(false);
            update_input_region(&win_tab, open_tab.get(), false);
        }
        if current.as_deref() == Some("run") {
            run_search.grab_focus();
        }
    });

    let stack_clamp = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_width(288)
        .max_content_width(288)
        .hexpand(false)
        .vexpand(true)
        .child(&stack)
        .build();

    dash_box.append(&sidebar_column);
    dash_box.append(&stack_clamp);
    dash_box.append(&plans_ui.detail_view);

    let music_ui = music::build_music_ui();
    stack.add_titled(&music_ui, Some("music"), "Music");

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

    let win_click = GestureClick::builder().button(1).build();
    let btn_win_click = trigger_btn.clone();
    let open_win_click = is_open.clone();
    let expanded_win_click = is_expanded.clone();
    win_click.connect_pressed(move |_, _, x, _| {
        if !open_win_click.get() {
            btn_win_click.emit_clicked();
        } else {
            let dash_edge = if expanded_win_click.get() {
                TOTAL_DASH_WIDTH as f64
            } else {
                BASE_DASH_WIDTH as f64
            };
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
    let expanded_click = is_expanded.clone();
    let closing_click = is_closing.clone();

    trigger_btn.connect_clicked(move |btn| {
        let currently_open = open_click.get();

        if !currently_open {
            open_click.set(true);
            closing_click.set(false);

            window_clone.set_keyboard_mode(KeyboardMode::OnDemand);
            update_input_region(&window_clone, true, expanded_click.get());
            window_clone.present();

            btn.add_css_class("riding");
            drawer_clone.add_css_class("open");
        } else {
            open_click.set(false);
            expanded_click.set(false);
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
                    update_input_region(&win_after, false, false);
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

    window.connect_map(|win| {
        update_input_region(win, false, false);
    });

    window.present();

    trigger_btn
}