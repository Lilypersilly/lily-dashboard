use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, EventControllerScroll, EventControllerScrollFlags, FlowBox,
    GestureClick, Image, Label, Orientation, SelectionMode,
};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use zbus::dbus_interface;

// ==========================================
// 1. HYPRLAND WORKSPACES STRIP
// ==========================================

#[derive(Clone, Debug, PartialEq)]
struct WorkspaceItem {
    id: i32,
    name: String,
    active: bool,
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
        let dir = if dy > 0.0 { "e+1" } else { "e-1" };
        let _ = Command::new("hyprctl")
            .args(["dispatch", "workspace", dir])
            .spawn();
        glib::Propagation::Stop
    });
    bar.add_controller(scroll_ctrl);
    bar.append(&pills_row);

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
    let mut last_ws: Vec<WorkspaceItem> = Vec::new();
    glib::timeout_add_local(Duration::from_millis(80), move || {
        let mut latest = None;
        while let Ok(ws) = rx.try_recv() {
            latest = Some(ws);
        }
        if let Some(items) = latest {
            if items != last_ws {
                last_ws = items.clone();
                while let Some(child) = row_r.first_child() {
                    row_r.remove(&child);
                }
                for ws in items {
                    let mut classes = vec!["ws-pill-btn".to_string()];
                    if ws.active {
                        classes.push("active".to_string());
                    }
                    let btn = Button::builder()
                        .label(&ws.name)
                        .css_classes(classes)
                        .tooltip_text(&format!("Switch to workspace {}", ws.name))
                        .build();
                    let id_str = ws.id.to_string();
                    btn.connect_clicked(move |_| {
                        let _ = Command::new("hyprctl")
                            .args(["dispatch", "workspace", &id_str])
                            .spawn();
                    });
                    row_r.append(&btn);
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

    // Notification Center badge (swaync)
    let notif_lbl = Label::builder()
        .label("󰂚")
        .css_classes(vec!["sidebar-status-lbl".to_string()])
        .build();
    let notif_btn = Button::builder()
        .child(&notif_lbl)
        .halign(Align::End)
        .css_classes(vec!["sidebar-footer-btn".to_string()])
        .tooltip_text("Left-click: Notifications | Right-click: Toggle Do Not Disturb")
        .build();

    notif_btn.connect_clicked(move |_| {
        let _ = Command::new("swaync-client").args(["-t", "-sw"]).spawn();
    });
    let right_click = GestureClick::builder().button(3).build();
    right_click.connect_pressed(move |_, _, _, _| {
        let _ = Command::new("swaync-client").args(["-d", "-sw"]).spawn();
    });
    notif_btn.add_controller(right_click);

    let (tx_n, rx_n) = mpsc::channel::<NotifState>();
    std::thread::spawn(move || {
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
