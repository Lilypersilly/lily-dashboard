// @dash-id: usb
// @dash-title: USB Drives
// @dash-lifecycle: stateful
// @dash-hooks: init, visibility
// Dashpendencies:
//! Removable USB drive manager with mount, unmount, safe eject, and UUID-based auto-mount.
//!
//! Uses `lsblk` for device discovery, `udisksctl` for passwordless mounting/ejecting,
//! and `gio::VolumeMonitor` for zero-polling hotplug detection & background auto-mounting.

use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, Orientation, PolicyType, Popover, ScrolledWindow,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;

static MONITOR_STARTED: AtomicBool = AtomicBool::new(false);
static AUTOMOUNT_ENABLED: AtomicBool = AtomicBool::new(true);
static USB_EVENT_VER: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT_AUTOMOUNT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static CONFIG_IO_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

thread_local! {
    /// Keeps the GIO VolumeMonitor alive for the entire process lifetime so hotplug
    /// signals keep firing in the background even when the USB Drives tab is closed.
    static VOL_MONITOR: RefCell<Option<gio::VolumeMonitor>> = const { RefCell::new(None) };
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct AutoMountEntry {
    #[serde(default)]
    label: String,
    #[serde(default)]
    model: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct UsbConfig {
    /// Map of partition UUID -> display info for drives that should auto-mount on plug-in
    #[serde(default)]
    automount_uuids: BTreeMap<String, AutoMountEntry>,
}

#[derive(Clone, Debug)]
struct UsbPartition {
    dev_path: String,
    parent_path: String,
    parent_model: String,
    label: String,
    uuid: String,
    size: String,
    fstype: String,
    mountpoint: String,
}

impl UsbPartition {
    fn display_name(&self) -> String {
        if !self.label.is_empty() {
            self.label.clone()
        } else if !self.parent_model.is_empty() {
            self.parent_model.clone()
        } else {
            self.dev_path
                .rsplit('/')
                .next()
                .unwrap_or(&self.dev_path)
                .to_string()
        }
    }

    fn is_mounted(&self) -> bool {
        !self.mountpoint.is_empty()
    }
}

enum UsbMsg {
    Scanned(Vec<UsbPartition>),
    ActionStatus(String),
}

// =============================================================================
// CONFIG PERSISTENCE (~/.config/lily-dashboard/usb_automount.json)
// =============================================================================

fn config_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("usb_automount.json");
    path
}

fn load_config() -> UsbConfig {
    let _guard = CONFIG_IO_LOCK.get_or_init(|| Mutex::new(())).lock();
    let path = config_path();
    if let Ok(data) = fs::read_to_string(path) {
        if let Ok(cfg) = serde_json::from_str::<UsbConfig>(&data) {
            return cfg;
        }
    }
    UsbConfig::default()
}

fn save_config(cfg: &UsbConfig) {
    let _guard = CONFIG_IO_LOCK.get_or_init(|| Mutex::new(())).lock();
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_path(), json);
    }
}

fn set_automount(uuid: &str, label: &str, model: &str, enabled: bool) {
    let mut cfg = load_config();
    if enabled {
        cfg.automount_uuids.insert(
            uuid.to_string(),
            AutoMountEntry {
                label: label.to_string(),
                model: model.to_string(),
            },
        );
    } else {
        cfg.automount_uuids.remove(uuid);
    }
    save_config(&cfg);
    bump_usb_version();
}

fn bump_usb_version() {
    USB_EVENT_VER.fetch_add(1, Ordering::Relaxed);
}

// =============================================================================
// LSBLK DISCOVERY & UDISKSCTL ACTIONS
// =============================================================================

fn val_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn val_bool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) != 0,
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn collect_mountable_children(
    node: &Value,
    parent_path: &str,
    parent_model: &str,
    out: &mut Vec<UsbPartition>,
) {
    let children = node.get("children").and_then(|c| c.as_array());
    if let Some(arr) = children {
        if !arr.is_empty() {
            for child in arr {
                collect_mountable_children(child, parent_path, parent_model, out);
            }
            return;
        }
    }

    let fstype = val_str(node.get("fstype"));
    if fstype.is_empty()
        || fstype.eq_ignore_ascii_case("swap")
        || fstype.eq_ignore_ascii_case("crypto_LUKS")
    {
        return;
    }

    let name = val_str(node.get("name"));
    let mut dev_path = val_str(node.get("path"));
    if dev_path.is_empty() && !name.is_empty() {
        dev_path = format!("/dev/{}", name);
    }
    if dev_path.is_empty() {
        return;
    }

    let label = val_str(node.get("label"));
    let uuid = val_str(node.get("uuid"));
    let size = val_str(node.get("size"));
    let mountpoint = val_str(node.get("mountpoint"));

    out.push(UsbPartition {
        dev_path,
        parent_path: parent_path.to_string(),
        parent_model: parent_model.to_string(),
        label,
        uuid,
        size,
        fstype,
        mountpoint,
    });
}

fn scan_usb_partitions() -> Vec<UsbPartition> {
    let mut out = Vec::new();
    let Ok(output) = Command::new("lsblk")
        .arg("-J")
        .arg("-o")
        .arg("NAME,PATH,LABEL,UUID,SIZE,MOUNTPOINT,FSTYPE,TRAN,RM,HOTPLUG,TYPE,MODEL")
        .output()
    else {
        return out;
    };

    if !output.status.success() {
        return out;
    }

    let Ok(root) = serde_json::from_slice::<Value>(&output.stdout) else {
        return out;
    };

    let Some(devices) = root.get("blockdevices").and_then(|d| d.as_array()) else {
        return out;
    };

    for dev in devices {
        let dev_type = val_str(dev.get("type"));
        if dev_type == "loop" || dev_type == "zram" || dev_type == "rom" {
            continue;
        }

        let tran = val_str(dev.get("tran"));
        let rm = val_bool(dev.get("rm"));
        let hotplug = val_bool(dev.get("hotplug"));

        // Match USB flash drives, external USB HDDs/SSDs, and removable SD cards
        let is_usb_or_removable = tran.eq_ignore_ascii_case("usb") || (rm && hotplug);
        if !is_usb_or_removable {
            continue;
        }

        let name = val_str(dev.get("name"));
        let mut parent_path = val_str(dev.get("path"));
        if parent_path.is_empty() && !name.is_empty() {
            parent_path = format!("/dev/{}", name);
        }
        let parent_model = val_str(dev.get("model"));

        collect_mountable_children(dev, &parent_path, &parent_model, &mut out);
    }

    out.sort_by(|a, b| {
        b.is_mounted()
            .cmp(&a.is_mounted())
            .then_with(|| a.display_name().to_lowercase().cmp(&b.display_name().to_lowercase()))
    });

    out
}

fn clean_udisks_error(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((_, tail)) = trimmed.rsplit_once(": ") {
        tail.trim().to_string()
    } else if trimmed.is_empty() {
        "Unknown udisksctl error".to_string()
    } else {
        trimmed.to_string()
    }
}

fn mount_partition(dev_path: &str, name: &str) -> Result<String, String> {
    let out = Command::new("udisksctl")
        .arg("mount")
        .arg("--no-user-interaction")
        .arg("-b")
        .arg(dev_path)
        .output()
        .map_err(|e| format!("Failed to run udisksctl: {}", e))?;

    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        bump_usb_version();
        if stdout.is_empty() {
            Ok(format!("Mounted {}", name))
        } else {
            Ok(stdout.trim_end_matches('.').to_string())
        }
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!("Mount failed: {}", clean_udisks_error(&stderr)))
    }
}

fn unmount_partition(dev_path: &str, name: &str) -> Result<String, String> {
    let out = Command::new("udisksctl")
        .arg("unmount")
        .arg("--no-user-interaction")
        .arg("-b")
        .arg(dev_path)
        .output()
        .map_err(|e| format!("Failed to run udisksctl: {}", e))?;

    if out.status.success() {
        bump_usb_version();
        Ok(format!("Unmounted {}", name))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!("Unmount failed: {}", clean_udisks_error(&stderr)))
    }
}

fn safe_eject_drive(parent_path: &str, display_name: &str) -> Result<String, String> {
    // Unmount all currently mounted partitions on this physical drive first
    let current = scan_usb_partitions();
    for part in current {
        if part.parent_path == parent_path && part.is_mounted() {
            unmount_partition(&part.dev_path, &part.display_name())?;
        }
    }

    let out = Command::new("udisksctl")
        .arg("power-off")
        .arg("--no-user-interaction")
        .arg("-b")
        .arg(parent_path)
        .output()
        .map_err(|e| format!("Failed to run udisksctl: {}", e))?;

    if out.status.success() {
        bump_usb_version();
        let _ = Command::new("notify-send")
            .arg("-a")
            .arg("Lily Dashboard")
            .arg("USB Safely Ejected")
            .arg(format!("{} is safe to unplug.", display_name))
            .spawn();
        Ok(format!("Safely ejected {} — OK to unplug!", display_name))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!("Eject failed: {}", clean_udisks_error(&stderr)))
    }
}

fn open_folder(path_str: &str) {
    if let Ok(uri) = glib::filename_to_uri(Path::new(path_str), None) {
        let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
    } else {
        let _ = Command::new("xdg-open").arg(path_str).spawn();
    }
}

fn copy_to_clipboard(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

// =============================================================================
// EVENT-DRIVEN BACKGROUND AUTO-MOUNTER
// =============================================================================

fn run_automount_check_async(delay_ms: u64) {
    if !AUTOMOUNT_ENABLED.load(Ordering::Relaxed) {
        bump_usb_version();
        return;
    }
    std::thread::spawn(move || {
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        if !AUTOMOUNT_ENABLED.load(Ordering::Relaxed) {
            bump_usb_version();
            return;
        }

        let cfg = load_config();
        if cfg.automount_uuids.is_empty() {
            bump_usb_version();
            return;
        }

        let parts = scan_usb_partitions();
        let in_flight = IN_FLIGHT_AUTOMOUNT.get_or_init(|| Mutex::new(HashSet::new()));

        for part in parts {
            if part.is_mounted() || part.uuid.is_empty() {
                continue;
            }
            if !cfg.automount_uuids.contains_key(&part.uuid) {
                continue;
            }

            // Deduplicate concurrent volume_added + drive_connected signals
            let acquired = in_flight
                .lock()
                .map(|mut set| set.insert(part.uuid.clone()))
                .unwrap_or(false);
            if !acquired {
                continue;
            }

            let res = mount_partition(&part.dev_path, &part.display_name());
            if let Ok(msg) = res {
                let _ = Command::new("notify-send")
                    .arg("-a")
                    .arg("Lily Dashboard")
                    .arg("USB Auto-Mounted")
                    .arg(&msg)
                    .spawn();
            }

            if let Ok(mut set) = in_flight.lock() {
                set.remove(&part.uuid);
            }
        }

        bump_usb_version();
    });
}

fn ensure_monitor_started() {
    if MONITOR_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    // Attach GIO VolumeMonitor once the GLib main loop is running
    glib::idle_add_local_once(move || {
        let monitor = gio::VolumeMonitor::get();

        // Trigger auto-mount ONLY when a drive/volume is plugged in (not on manual unmount!)
        monitor.connect_volume_added(move |_, _| {
            bump_usb_version();
            run_automount_check_async(600);
        });
        monitor.connect_drive_connected(move |_, _| {
            bump_usb_version();
            run_automount_check_async(750);
        });

        // Refresh UI on unmount / unplug events
        monitor.connect_volume_removed(move |_, _| {
            bump_usb_version();
        });
        monitor.connect_drive_disconnected(move |_, _| {
            bump_usb_version();
        });
        monitor.connect_mount_added(move |_, _| {
            bump_usb_version();
        });
        monitor.connect_mount_removed(move |_, _| {
            bump_usb_version();
        });

        VOL_MONITOR.with(|slot| {
            *slot.borrow_mut() = Some(monitor);
        });

        // Also check once at startup for recognized drives already plugged in at boot
        run_automount_check_async(300);
    });
}

// =============================================================================
// UI BUILDER
// =============================================================================

pub fn init() {
    let loaded = crate::config::get()
        .layout
        .sidebar_items
        .iter()
        .any(|x| x == "usb");
    AUTOMOUNT_ENABLED.store(loaded, Ordering::Relaxed);
    ensure_monitor_started();
}

pub fn set_enabled(enabled: bool) {
    AUTOMOUNT_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    ensure_monitor_started();

    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    // Header Row
    let hdr_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let title_lbl = Label::builder()
        .label("USB DRIVES")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let refresh_btn = Button::builder()
        .label("Refresh")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Rescan connected USB drives")
        .build();

    hdr_row.append(&title_lbl);
    hdr_row.append(&refresh_btn);
    root.append(&hdr_row);

    // Dependency warning if udisksctl is missing
    if !crate::config::bin_exists("udisksctl") {
        let warn_card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(3)
            .css_classes(vec!["plan-check-row".to_string()])
            .build();
        warn_card.append(
            &Label::builder()
                .label("⚠ Missing dependency: udisks2 (udisksctl)")
                .halign(Align::Start)
                .css_classes(vec!["plan-step".to_string()])
                .build(),
        );
        warn_card.append(
            &Label::builder()
                .label("Install via your package manager (e.g. sudo pacman -S udisks2).")
                .halign(Align::Start)
                .wrap(true)
                .xalign(0.0)
                .css_classes(vec!["plan-subnote".to_string()])
                .build(),
        );
        root.append(&warn_card);
    }

    // Status Banner
    let status_lbl = Label::builder()
        .label("")
        .halign(Align::Start)
        .wrap(true)
        .xalign(0.0)
        .css_classes(vec!["plan-subnote".to_string()])
        .visible(false)
        .build();
    root.append(&status_lbl);

    // Scrollable Drive List
    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();

    let list_container = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    scrolled.set_child(Some(&list_container));
    root.append(&scrolled);

    let (tx, rx) = mpsc::channel::<UsbMsg>();
    let rx_rc = Rc::new(rx);
    let is_visible = Rc::new(Cell::new(true));
    let watcher_running = Rc::new(Cell::new(false));
    let popover_guard = ctx.popover_guard.clone();
    let close_drawer = ctx.close_drawer.clone();
    let last_seen_ver = Rc::new(Cell::new(USB_EVENT_VER.load(Ordering::Relaxed)));

    let trigger_scan: Rc<dyn Fn()> = {
        let tx_s = tx.clone();
        Rc::new(move || {
            let tx_bg = tx_s.clone();
            std::thread::spawn(move || {
                let parts = scan_usb_partitions();
                let _ = tx_bg.send(UsbMsg::Scanned(parts));
            });
        })
    };

    {
        let scan = trigger_scan.clone();
        refresh_btn.connect_clicked(move |_| scan());
    }

    // Main-thread receiver & hotplug event watcher (runs ONLY while tab is visible; 0% idle CPU)
    let ensure_watcher: Rc<dyn Fn()> = {
        let vis_w = is_visible.clone();
        let wr_w = watcher_running.clone();
        let rx_w = rx_rc.clone();
        let list_weak = list_container.downgrade();
        let status_r = status_lbl.clone();
        let pg_r = popover_guard.clone();
        let close_r = close_drawer.clone();
        let tx_r = tx.clone();
        let scan_r = trigger_scan.clone();
        let ver_r = last_seen_ver.clone();

        Rc::new(move || {
            if wr_w.get() {
                return;
            }
            wr_w.set(true);
            let vis_l = vis_w.clone();
            let wr_l = wr_w.clone();
            let rx_l = rx_w.clone();
            let lw = list_weak.clone();
            let st_l = status_r.clone();
            let pg_l = pg_r.clone();
            let cl_l = close_r.clone();
            let tx_l = tx_r.clone();
            let sc_l = scan_r.clone();
            let vr_l = ver_r.clone();

            glib::timeout_add_local(Duration::from_millis(150), move || {
                let Some(list_box) = lw.upgrade() else {
                    wr_l.set(false);
                    return glib::ControlFlow::Break;
                };

                while let Ok(msg) = rx_l.try_recv() {
                    match msg {
                        UsbMsg::Scanned(parts) => {
                            render_drive_list(&list_box, &parts, &st_l, &pg_l, &cl_l, &tx_l);
                        }
                        UsbMsg::ActionStatus(text) => {
                            st_l.set_label(&text);
                            st_l.set_visible(!text.is_empty());
                            sc_l();
                        }
                    }
                }

                if !vis_l.get() {
                    wr_l.set(false);
                    return glib::ControlFlow::Break;
                }

                // Refresh UI automatically whenever GIO VolumeMonitor detects a change
                let cur_ver = USB_EVENT_VER.load(Ordering::Relaxed);
                if cur_ver != vr_l.get() && pg_l.get() == 0 {
                    vr_l.set(cur_ver);
                    sc_l();
                }

                glib::ControlFlow::Continue
            });
        })
    };

    // Initial scan & start watcher while visible
    trigger_scan();
    ensure_watcher();

    let vis_show = is_visible.clone();
    let on_show_scan = trigger_scan.clone();
    let on_show_watch = ensure_watcher.clone();
    let vis_hide = is_visible.clone();
    crate::module_api::ModuleHandle {
        widget: root.upcast::<gtk4::Widget>(),
        detail_pane: None,
        on_show: Some(Rc::new(move || {
            vis_show.set(true);
            on_show_watch();
            on_show_scan();
        })),
        on_hide: Some(Rc::new(move || {
            vis_hide.set(false);
        })),
    }
}

fn render_drive_list(
    container: &Box,
    parts: &[UsbPartition],
    status_lbl: &Label,
    popover_guard: &Rc<Cell<usize>>,
    close_drawer: &Rc<dyn Fn()>,
    tx: &mpsc::Sender<UsbMsg>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let cfg = load_config();

    let conn_hdr = Label::builder()
        .label("CONNECTED DRIVES")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(2)
        .build();
    container.append(&conn_hdr);

    if parts.is_empty() {
        let empty = Label::builder()
            .label("No USB drives detected.\nPlug in a USB drive to mount or manage it.")
            .justify(gtk4::Justification::Center)
            .wrap(true)
            .css_classes(vec!["fav-score-badge".to_string()])
            .margin_top(16)
            .margin_bottom(12)
            .build();
        container.append(&empty);
    } else {
        for part in parts {
            let is_auto = !part.uuid.is_empty() && cfg.automount_uuids.contains_key(&part.uuid);
            container.append(&build_drive_row(
                part,
                is_auto,
                status_lbl,
                popover_guard,
                close_drawer,
                tx,
            ));
        }
    }

    // Show offline recognized auto-mount drives so the user can forget old ones anytime
    let connected_uuids: HashSet<&str> = parts
        .iter()
        .filter(|p| !p.uuid.is_empty())
        .map(|p| p.uuid.as_str())
        .collect();

    let offline_auto: Vec<(&String, &AutoMountEntry)> = cfg
        .automount_uuids
        .iter()
        .filter(|(u, _)| !connected_uuids.contains(u.as_str()))
        .collect();

    if !offline_auto.is_empty() {
        let off_hdr = Label::builder()
            .label("AUTO-MOUNT (OFFLINE)")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(10)
            .build();
        container.append(&off_hdr);

        for (uuid, entry) in offline_auto {
            let row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .css_classes(vec!["plan-check-row".to_string()])
                .build();

            let col = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(1)
                .hexpand(true)
                .build();

            let title = if !entry.label.is_empty() {
                entry.label.clone()
            } else if !entry.model.is_empty() {
                entry.model.clone()
            } else {
                "USB Drive".to_string()
            };

            let name_lbl = Label::builder()
                .label(&format!("󱊟 {}", title))
                .halign(Align::Start)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .css_classes(vec!["plan-body".to_string()])
                .build();
            let sub_lbl = Label::builder()
                .label(&format!("UUID: {}", uuid))
                .halign(Align::Start)
                .ellipsize(gtk4::pango::EllipsizeMode::Middle)
                .css_classes(vec!["plan-subnote".to_string()])
                .build();

            col.append(&name_lbl);
            col.append(&sub_lbl);

            let del_btn = Button::builder()
                .label("×")
                .valign(Align::Center)
                .css_classes(vec!["plan-line-del-btn".to_string()])
                .tooltip_text("Remove from auto-mount list")
                .build();

            let uuid_del = uuid.clone();
            let title_del = title.clone();
            let tx_del = tx.clone();
            del_btn.connect_clicked(move |_| {
                set_automount(&uuid_del, "", "", false);
                let _ = tx_del.send(UsbMsg::ActionStatus(format!(
                    "Removed {} from auto-mount list.",
                    title_del
                )));
            });

            row.append(&col);
            row.append(&del_btn);
            container.append(&row);
        }
    }
}

fn build_drive_row(
    part: &UsbPartition,
    is_automount: bool,
    status_lbl: &Label,
    popover_guard: &Rc<Cell<usize>>,
    close_drawer: &Rc<dyn Fn()>,
    tx: &mpsc::Sender<UsbMsg>,
) -> Box {
    let row_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .build();

    let is_mounted = part.is_mounted();
    let mut btn_classes = vec!["fav-item-btn".to_string()];
    if is_mounted {
        btn_classes.push("pinned".to_string());
    }

    let tooltip = if is_mounted {
        format!(
            "Left-click to open folder | Right-click for Unmount, Auto-Mount & Safe Eject\n\nDevice: {}\nMount: {}\nUUID: {}",
            part.dev_path, part.mountpoint, part.uuid
        )
    } else {
        format!(
            "Left-click to mount | Right-click for Mount, Auto-Mount & Safe Eject\n\nDevice: {}\nUUID: {}",
            part.dev_path, part.uuid
        )
    };

    let btn = Button::builder()
        .css_classes(btn_classes)
        .hexpand(true)
        .tooltip_text(&tooltip)
        .build();

    let outer = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();

    let icon_lbl = Label::builder()
        .label(if is_mounted { "󱊟" } else { "󰋊" })
        .css_classes(vec!["plan-h2".to_string()])
        .valign(Align::Center)
        .build();

    let col = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();

    let top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let name_lbl = Label::builder()
        .label(&part.display_name())
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-body".to_string()])
        .build();

    let badge_text = match (is_automount, is_mounted) {
        (true, true) => "[AUTO] MOUNTED",
        (true, false) => "[AUTO]",
        (false, true) => "MOUNTED",
        (false, false) => "Mount",
    };

    let badge_lbl = Label::builder()
        .label(badge_text)
        .css_classes(vec!["fav-score-badge".to_string()])
        .valign(Align::Center)
        .build();

    top_row.append(&name_lbl);
    top_row.append(&badge_lbl);

    let sub_text = if is_mounted {
        format!(
            "{} • {} • {}",
            part.size,
            part.fstype.to_uppercase(),
            part.mountpoint
        )
    } else {
        format!(
            "{} • {} • {}",
            part.size,
            part.fstype.to_uppercase(),
            part.dev_path
        )
    };

    let sub_lbl = Label::builder()
        .label(&sub_text)
        .halign(Align::Start)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();

    col.append(&top_row);
    col.append(&sub_lbl);

    outer.append(&icon_lbl);
    outer.append(&col);
    btn.set_child(Some(&outer));

    // Left-Click: Open folder if mounted, or Mount immediately if unmounted
    {
        let part_c = part.clone();
        let st_c = status_lbl.clone();
        let close_c = close_drawer.clone();
        let tx_c = tx.clone();
        btn.connect_clicked(move |_| {
            if part_c.is_mounted() {
                open_folder(&part_c.mountpoint);
                close_c();
            } else {
                let dev = part_c.dev_path.clone();
                let name = part_c.display_name();
                st_c.set_label(&format!("Mounting {}...", name));
                st_c.set_visible(true);
                let tx_bg = tx_c.clone();
                std::thread::spawn(move || {
                    let msg = match mount_partition(&dev, &name) {
                        Ok(m) | Err(m) => m,
                    };
                    let _ = tx_bg.send(UsbMsg::ActionStatus(msg));
                });
            }
        });
    }

    // Right-Click Context Menu: Mount / Unmount | Open | Toggle Auto-Mount | Copy | Safe Eject
    let right_click = GestureClick::builder().button(3).build();
    let row_weak = row_box.downgrade();
    let part_ctx = part.clone();
    let st_ctx = status_lbl.clone();
    let guard_ctx = popover_guard.clone();
    let close_ctx = close_drawer.clone();
    let tx_ctx = tx.clone();

    right_click.connect_pressed(move |_, _, _, _| {
        let Some(parent_row) = row_weak.upgrade() else {
            return;
        };

        let popover = Popover::builder()
            .css_classes(vec!["fav-popover".to_string()])
            .autohide(true)
            .build();
        popover.set_parent(&parent_row);

        popover.connect_closed(|p| {
            let p_weak = p.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(pop) = p_weak.upgrade() {
                    pop.unparent();
                }
            });
        });

        let guard_map = guard_ctx.clone();
        popover.connect_map(move |_| {
            guard_map.set(guard_map.get() + 1);
        });
        let guard_unmap = guard_ctx.clone();
        popover.connect_unmap(move |_| {
            let g = guard_unmap.clone();
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                g.set(g.get().saturating_sub(1));
            });
        });

        let menu_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .build();

        if part_ctx.is_mounted() {
            // 1. Open in File Manager
            let open_btn = Button::builder()
                .label("Open in File Manager")
                .css_classes(vec!["fav-hide-btn".to_string()])
                .build();
            let mp = part_ctx.mountpoint.clone();
            let close_o = close_ctx.clone();
            let pop_o = popover.downgrade();
            open_btn.connect_clicked(move |_| {
                if let Some(p) = pop_o.upgrade() {
                    p.popdown();
                }
                open_folder(&mp);
                close_o();
            });
            menu_box.append(&open_btn);

            // 2. Unmount
            let unmount_btn = Button::builder()
                .label("Unmount")
                .css_classes(vec!["fav-hide-btn".to_string()])
                .build();
            let dev = part_ctx.dev_path.clone();
            let name = part_ctx.display_name();
            let st_u = st_ctx.clone();
            let tx_u = tx_ctx.clone();
            let pop_u = popover.downgrade();
            unmount_btn.connect_clicked(move |_| {
                if let Some(p) = pop_u.upgrade() {
                    p.popdown();
                }
                st_u.set_label(&format!("Unmounting {}...", name));
                st_u.set_visible(true);
                let dev_bg = dev.clone();
                let name_bg = name.clone();
                let tx_bg = tx_u.clone();
                std::thread::spawn(move || {
                    let msg = match unmount_partition(&dev_bg, &name_bg) {
                        Ok(m) | Err(m) => m,
                    };
                    let _ = tx_bg.send(UsbMsg::ActionStatus(msg));
                });
            });
            menu_box.append(&unmount_btn);
        } else {
            // 1. Mount
            let mount_btn = Button::builder()
                .label("Mount")
                .css_classes(vec!["fav-hide-btn".to_string()])
                .build();
            let dev = part_ctx.dev_path.clone();
            let name = part_ctx.display_name();
            let st_m = st_ctx.clone();
            let tx_m = tx_ctx.clone();
            let pop_m = popover.downgrade();
            mount_btn.connect_clicked(move |_| {
                if let Some(p) = pop_m.upgrade() {
                    p.popdown();
                }
                st_m.set_label(&format!("Mounting {}...", name));
                st_m.set_visible(true);
                let dev_bg = dev.clone();
                let name_bg = name.clone();
                let tx_bg = tx_m.clone();
                std::thread::spawn(move || {
                    let msg = match mount_partition(&dev_bg, &name_bg) {
                        Ok(m) | Err(m) => m,
                    };
                    let _ = tx_bg.send(UsbMsg::ActionStatus(msg));
                });
            });
            menu_box.append(&mount_btn);
        }

        // Auto-Mount Toggle (persisted by partition UUID)
        if !part_ctx.uuid.is_empty() {
            let auto_btn = Button::builder()
                .label(if is_automount {
                    "Disable Auto-Mount"
                } else {
                    "Enable Auto-Mount"
                })
                .css_classes(vec!["fav-hide-btn".to_string()])
                .build();
            let uuid = part_ctx.uuid.clone();
            let label = part_ctx.label.clone();
            let model = part_ctx.parent_model.clone();
            let disp_name = part_ctx.display_name();
            let tx_a = tx_ctx.clone();
            let pop_a = popover.downgrade();
            auto_btn.connect_clicked(move |_| {
                if let Some(p) = pop_a.upgrade() {
                    p.popdown();
                }
                let next = !is_automount;
                set_automount(&uuid, &label, &model, next);
                let msg = if next {
                    format!("Auto-mount enabled for {} (UUID saved).", disp_name)
                } else {
                    format!("Auto-mount disabled for {}.", disp_name)
                };
                let _ = tx_a.send(UsbMsg::ActionStatus(msg));
            });
            menu_box.append(&auto_btn);
        }

        // Copy Mount Path (or Device Path if unmounted)
        let copy_target = if part_ctx.is_mounted() {
            part_ctx.mountpoint.clone()
        } else {
            part_ctx.dev_path.clone()
        };
        let copy_btn = Button::builder()
            .label(if part_ctx.is_mounted() {
                "Copy Mount Path"
            } else {
                "Copy Device Path"
            })
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();
        let tx_cp = tx_ctx.clone();
        let pop_cp = popover.downgrade();
        copy_btn.connect_clicked(move |_| {
            if let Some(p) = pop_cp.upgrade() {
                p.popdown();
            }
            copy_to_clipboard(&copy_target);
            let _ = tx_cp.send(UsbMsg::ActionStatus(format!("Copied: {}", copy_target)));
        });
        menu_box.append(&copy_btn);

        // Safely Eject (Unmounts all partitions on the stick + powers off USB port)
        let eject_btn = Button::builder()
            .label("Safely Eject (Power Off)")
            .css_classes(vec!["plan-delete-menu-btn".to_string()])
            .build();
        let parent_dev = part_ctx.parent_path.clone();
        let eject_name = if !part_ctx.parent_model.is_empty() {
            part_ctx.parent_model.clone()
        } else {
            part_ctx.display_name()
        };
        let st_e = st_ctx.clone();
        let tx_e = tx_ctx.clone();
        let pop_e = popover.downgrade();
        eject_btn.connect_clicked(move |_| {
            if let Some(p) = pop_e.upgrade() {
                p.popdown();
            }
            st_e.set_label(&format!("Ejecting {}...", eject_name));
            st_e.set_visible(true);
            let p_bg = parent_dev.clone();
            let n_bg = eject_name.clone();
            let tx_bg = tx_e.clone();
            std::thread::spawn(move || {
                let msg = match safe_eject_drive(&p_bg, &n_bg) {
                    Ok(m) | Err(m) => m,
                };
                let _ = tx_bg.send(UsbMsg::ActionStatus(msg));
            });
        });
        menu_box.append(&eject_btn);

        popover.set_child(Some(&menu_box));
        popover.popup();
    });
    btn.add_controller(right_click);

    row_box.append(&btn);
    row_box
}