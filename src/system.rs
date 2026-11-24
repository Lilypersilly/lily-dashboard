//! System tab and 400px slide-out detail pane.
//!
//! Hosts Sound (`pactl`), Network (`nmcli`), Themes (`theme-switch`), Wallpapers
//! (`swww` with paired vertical crops and RAM-only thumbnails), Displays (`hyprctl`),
//! Keybinds (live parser for `~/.config/hypr/binds.lua`), and Overview (CPU/RAM/GPU
//! stats, `lumine-capture` screenshots, and a `slurp`+`grim` screen color picker).

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, DrawingArea, Entry, FlowBox, Label, Orientation, Overflow, Picture,
    PolicyType, ProgressBar, Scale, ScrolledWindow, SelectionMode, Stack, StackTransitionType,
};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::fs;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

static LAST_PICKED_COLOR: Mutex<Option<(u8, u8, u8)>> = Mutex::new(None);

#[derive(Clone, Copy, Debug)]
pub enum CaptureMode {
    RegionEditor,
    Monitor,
    Full,
    RegionOcr,
}

/// Closes the dashboard drawer, waits for the 350ms slide-out animation, then runs
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

/// Wayland color picker: uses built-in `slurp -p` + `grim -t ppm -` + `wl-copy`,
/// `hyprpicker`, or a custom command from `config.json`.
pub fn launch_color_picker(close_drawer: &Rc<dyn Fn()>) {
    launch_color_picker_inner(close_drawer, None);
}

fn launch_color_picker_inner(
    close_drawer: &Rc<dyn Fn()>,
    on_picked: Option<mpsc::Sender<SystemMsg>>,
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

        if let Some(tx) = on_picked {
            let _ = tx.send(SystemMsg::ColorPicked);
        }
    });
}

fn parse_hex_u8(raw: &str) -> Option<(u8, u8, u8)> {
    let clean = raw.trim().trim_start_matches('#');
    if clean.len() >= 6 {
        let r = u8::from_str_radix(&clean[0..2], 16).ok()?;
        let g = u8::from_str_radix(&clean[2..4], 16).ok()?;
        let b = u8::from_str_radix(&clean[4..6], 16).ok()?;
        return Some((r, g, b));
    }
    None
}

pub struct SystemUi {
    pub list_view: Box,
    pub detail_view: Box,
    open_default_cb: Rc<dyn Fn()>,
}

impl SystemUi {
    pub fn open_panel(&self) {
        (self.open_default_cb)();
    }
}

#[derive(Clone, Debug)]
struct AudioDevice {
    name: String,
    description: String,
    volume_pct: f64,
    muted: bool,
    is_default: bool,
}

#[derive(Clone, Debug)]
struct AudioStream {
    index: u64,
    app_name: String,
    media_name: String,
    volume_pct: f64,
    muted: bool,
}

#[derive(Clone, Debug, Default)]
struct AudioSnapshot {
    sinks: Vec<AudioDevice>,
    sources: Vec<AudioDevice>,
    streams: Vec<AudioStream>,
}

#[derive(Clone, Debug)]
struct ActiveConn {
    name: String,
    kind: String,
    device: String,
    ip: String,
}

#[derive(Clone, Debug)]
struct WifiNet {
    in_use: bool,
    ssid: String,
    signal: u8,
    security: String,
}

#[derive(Clone, Debug)]
struct SavedConn {
    name: String,
    kind: String,
}

#[derive(Clone, Debug, Default)]
struct NetworkSnapshot {
    wifi_enabled: bool,
    has_wifi_device: bool,
    active: Vec<ActiveConn>,
    wifi_list: Vec<WifiNet>,
    saved_inactive: Vec<SavedConn>,
}

#[derive(Clone, Debug)]
struct MonitorInfo {
    name: String,
    width: i64,
    height: i64,
    refresh: f64,
    scale: f64,
    workspace: String,
    focused: bool,
}

#[derive(Clone, Debug, Default)]
struct OverviewSnapshot {
    cpu_pct: f64,
    mem_used_gb: f64,
    mem_total_gb: f64,
    mem_frac: f64,
    uptime: String,
    gpu_name: String,
    gpu_temp: Option<u32>,
    gpu_util: Option<u32>,
    gpu_vram_used: Option<u32>,
    gpu_vram_total: Option<u32>,
    dnd_enabled: Option<bool>,
}

#[derive(Clone, Debug)]
struct ThemeEntry {
    key: String,
    name: String,
    mode: String,
    swatches: [(f64, f64, f64); 4],
    is_active: bool,
}

#[derive(Clone, Debug)]
struct WallpaperEntry {
    path: String,
    name: String,
    has_vertical_pair: bool,
    is_active: bool,
}

#[derive(Clone, Debug)]
struct KeybindEntry {
    group: String,
    keys: String,
    desc: String,
    raw: String,
}

enum SystemMsg {
    Audio(AudioSnapshot),
    Network(NetworkSnapshot),
    NetworkAction(String),
    Displays(Vec<MonitorInfo>),
    Keybinds(Vec<KeybindEntry>),
    Overview(OverviewSnapshot),
    ColorPicked,
    Themes(Vec<ThemeEntry>),
    ThemeApplied(String),
    Wallpapers(Vec<WallpaperEntry>),
    WallpaperThumb {
        path: String,
        bytes: glib::Bytes,
        has_alpha: bool,
        rowstride: usize,
        width: i32,
        height: i32,
    },
    WallpaperApplied(String),
}

// ==========================================
// DATA FETCHERS (run on background threads)
// ==========================================

fn parse_pactl_volume(val: &Value) -> f64 {
    let Some(vol_obj) = val.get("volume").and_then(|v| v.as_object()) else {
        return 0.0;
    };
    let mut sum = 0.0;
    let mut count = 0usize;
    for ch in vol_obj.values() {
        if let Some(pct_str) = ch.get("value_percent").and_then(|v| v.as_str()) {
            if let Ok(num) = pct_str.trim().trim_end_matches('%').parse::<f64>() {
                sum += num;
                count += 1;
            }
        }
    }
    if count > 0 {
        (sum / count as f64).round()
    } else {
        0.0
    }
}

fn fetch_audio_snapshot() -> AudioSnapshot {
    let mut snap = AudioSnapshot::default();

    let default_sink = Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let default_source = Command::new("pactl")
        .arg("get-default-source")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if let Ok(out) = Command::new("pactl").args(["-f", "json", "list", "sinks"]).output() {
        if let Ok(arr) = serde_json::from_slice::<Vec<Value>>(&out.stdout) {
            for item in arr {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let desc = item
                    .get("description")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&name)
                    .to_string();
                let muted = item.get("mute").and_then(|v| v.as_bool()).unwrap_or(false);
                let volume_pct = parse_pactl_volume(&item);
                let is_default = name == default_sink;
                snap.sinks.push(AudioDevice {
                    name,
                    description: desc,
                    volume_pct,
                    muted,
                    is_default,
                });
            }
        }
    }

    if let Ok(out) = Command::new("pactl").args(["-f", "json", "list", "sources"]).output() {
        if let Ok(arr) = serde_json::from_slice::<Vec<Value>>(&out.stdout) {
            for item in arr {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() || name.ends_with(".monitor") {
                    continue;
                }
                let desc = item
                    .get("description")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&name)
                    .to_string();
                let muted = item.get("mute").and_then(|v| v.as_bool()).unwrap_or(false);
                let volume_pct = parse_pactl_volume(&item);
                let is_default = name == default_source;
                snap.sources.push(AudioDevice {
                    name,
                    description: desc,
                    volume_pct,
                    muted,
                    is_default,
                });
            }
        }
    }

    if let Ok(out) = Command::new("pactl")
        .args(["-f", "json", "list", "sink-inputs"])
        .output()
    {
        if let Ok(arr) = serde_json::from_slice::<Vec<Value>>(&out.stdout) {
            for item in arr {
                let Some(index) = item.get("index").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let props = item.get("properties");
                let app_name = props
                    .and_then(|p| p.get("application.name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("App")
                    .to_string();
                let media_name = props
                    .and_then(|p| p.get("media.name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let muted = item.get("mute").and_then(|v| v.as_bool()).unwrap_or(false);
                let volume_pct = parse_pactl_volume(&item);
                snap.streams.push(AudioStream {
                    index,
                    app_name,
                    media_name,
                    volume_pct,
                    muted,
                });
            }
        }
    }

    snap
}

fn split_nmcli_terse(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == ':' {
            fields.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    fields.push(cur);
    fields
}

fn fetch_device_ip(dev: &str) -> String {
    if dev.is_empty() || dev == "--" {
        return String::new();
    }
    if let Ok(out) = Command::new("nmcli")
        .args(["-t", "-f", "IP4.ADDRESS", "dev", "show", dev])
        .output()
    {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Some((_, val)) = line.split_once(':') {
                let ip = val.split('/').next().unwrap_or(val).trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    String::new()
}

fn fetch_network_snapshot(rescan_wifi: bool) -> NetworkSnapshot {
    let mut snap = NetworkSnapshot::default();

    if let Ok(out) = Command::new("nmcli").args(["radio", "wifi"]).output() {
        snap.wifi_enabled = String::from_utf8_lossy(&out.stdout)
            .trim()
            .eq_ignore_ascii_case("enabled");
    }

    if let Ok(out) = Command::new("nmcli")
        .args(["-t", "-f", "TYPE", "dev"])
        .output()
    {
        snap.has_wifi_device = String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == "wifi");
    }

    let mut active_names = std::collections::HashSet::new();
    if let Ok(out) = Command::new("nmcli")
        .args(["-t", "-f", "NAME,TYPE,DEVICE", "con", "show", "--active"])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts = split_nmcli_terse(line);
            if parts.len() >= 3 {
                let name = parts[0].clone();
                let kind = parts[1].clone();
                let device = parts[2].clone();
                if kind == "loopback" || name == "lo" {
                    continue;
                }
                let ip = fetch_device_ip(&device);
                active_names.insert(name.clone());
                snap.active.push(ActiveConn {
                    name,
                    kind,
                    device,
                    ip,
                });
            }
        }
    }

    if let Ok(out) = Command::new("nmcli")
        .args(["-t", "-f", "NAME,TYPE", "con", "show"])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts = split_nmcli_terse(line);
            if parts.len() >= 2 {
                let name = parts[0].clone();
                let kind = parts[1].clone();
                if kind == "loopback" || name == "lo" || active_names.contains(&name) {
                    continue;
                }
                snap.saved_inactive.push(SavedConn { name, kind });
            }
        }
    }

    if snap.has_wifi_device && snap.wifi_enabled {
        let mut args = vec![
            "-t",
            "-f",
            "IN-USE,SSID,SIGNAL,SECURITY",
            "dev",
            "wifi",
            "list",
        ];
        if rescan_wifi {
            args.push("--rescan");
            args.push("yes");
        } else {
            args.push("--rescan");
            args.push("no");
        }
        if let Ok(out) = Command::new("nmcli").args(&args).output() {
            let mut seen = std::collections::HashSet::new();
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let parts = split_nmcli_terse(line);
                if parts.len() >= 4 {
                    let in_use = parts[0].trim() == "*";
                    let ssid = parts[1].trim().to_string();
                    if ssid.is_empty() || !seen.insert(ssid.clone()) {
                        continue;
                    }
                    let signal = parts[2].trim().parse::<u8>().unwrap_or(0);
                    let security = parts[3].trim().to_string();
                    snap.wifi_list.push(WifiNet {
                        in_use,
                        ssid,
                        signal,
                        security,
                    });
                }
            }
            snap.wifi_list.sort_by(|a, b| {
                b.in_use
                    .cmp(&a.in_use)
                    .then_with(|| b.signal.cmp(&a.signal))
            });
        }
    }

    snap
}

fn fetch_displays_snapshot() -> Vec<MonitorInfo> {
    let mut monitors = Vec::new();
    if let Ok(out) = Command::new("hyprctl").args(["monitors", "-j"]).output() {
        if let Ok(arr) = serde_json::from_slice::<Vec<Value>>(&out.stdout) {
            for m in arr {
                let name = m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Monitor")
                    .to_string();
                let width = m.get("width").and_then(|v| v.as_i64()).unwrap_or(0);
                let height = m.get("height").and_then(|v| v.as_i64()).unwrap_or(0);
                let refresh = m
                    .get("refreshRate")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(60.0);
                let scale = m.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let workspace = m
                    .get("activeWorkspace")
                    .and_then(|w| w.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let focused = m.get("focused").and_then(|v| v.as_bool()).unwrap_or(false);
                monitors.push(MonitorInfo {
                    name,
                    width,
                    height,
                    refresh,
                    scale,
                    workspace,
                    focused,
                });
            }
        }
    }
    monitors
}

fn read_cpu_times() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total: u64 = nums.iter().sum();
    Some((idle, total))
}

fn fetch_overview_snapshot(prev_cpu: &mut Option<(u64, u64)>) -> OverviewSnapshot {
    let mut snap = OverviewSnapshot::default();

    if let Some((idle, total)) = read_cpu_times() {
        if let Some((p_idle, p_total)) = *prev_cpu {
            let dt = total.saturating_sub(p_total);
            let di = idle.saturating_sub(p_idle);
            if dt > 0 {
                snap.cpu_pct = ((dt.saturating_sub(di)) as f64 / dt as f64) * 100.0;
            }
        }
        *prev_cpu = Some((idle, total));
    }

    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        let mut total_kb = 0u64;
        let mut avail_kb = 0u64;
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total_kb = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                avail_kb = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
        }
        if total_kb > 0 {
            let used_kb = total_kb.saturating_sub(avail_kb);
            snap.mem_total_gb = total_kb as f64 / (1024.0 * 1024.0);
            snap.mem_used_gb = used_kb as f64 / (1024.0 * 1024.0);
            snap.mem_frac = (used_kb as f64 / total_kb as f64).clamp(0.0, 1.0);
        }
    }

    if let Ok(up) = fs::read_to_string("/proc/uptime") {
        if let Some(secs_f) = up.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()) {
            let secs = secs_f as u64;
            let days = secs / 86400;
            let hrs = (secs % 86400) / 3600;
            let mins = (secs % 3600) / 60;
            snap.uptime = if days > 0 {
                format!("{}d {}h {}m", days, hrs, mins)
            } else if hrs > 0 {
                format!("{}h {}m", hrs, mins)
            } else {
                format!("{}m", mins)
            };
        }
    }

    if let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,temperature.gpu,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if out.status.success() {
            let line = String::from_utf8_lossy(&out.stdout);
            if let Some(first) = line.lines().next() {
                let parts: Vec<&str> = first.split(',').map(|s| s.trim()).collect();
                if parts.len() >= 5 {
                    snap.gpu_name = parts[0].to_string();
                    snap.gpu_temp = parts[1].parse().ok();
                    snap.gpu_util = parts[2].parse().ok();
                    snap.gpu_vram_used = parts[3].parse().ok();
                    snap.gpu_vram_total = parts[4].parse().ok();
                }
            }
        }
    }

    if let Ok(out) = Command::new("swaync-client").arg("-D").output() {
        if out.status.success() {
            let txt = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
            snap.dnd_enabled = Some(txt == "true");
        }
    }

    snap
}

fn parse_hex_rgb(hex: &str) -> (f64, f64, f64) {
    let clean = hex.trim().trim_start_matches('#');
    if clean.len() >= 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&clean[0..2], 16),
            u8::from_str_radix(&clean[2..4], 16),
            u8::from_str_radix(&clean[4..6], 16),
        ) {
            return (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
        }
    }
    (0.5, 0.5, 0.5)
}

fn parse_theme_conf(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let clean = line.split('#').next().unwrap_or("").trim();
            if let Some((k, v)) = clean.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

fn fetch_themes_snapshot() -> Vec<ThemeEntry> {
    let cfg = crate::config::get();
    let theme_conf_path = crate::config::expand_path(&cfg.paths.theme_conf_file);
    let base = theme_conf_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| glib::user_config_dir().join("theme-switch"));
    let cur_map = parse_theme_conf(&theme_conf_path);
    let cur_name = cur_map.get("name").cloned().unwrap_or_default();

    let pal_dir = base.join("palettes");
    let Ok(entries) = fs::read_dir(&pal_dir) else {
        return Vec::new();
    };

    let mut themes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("conf") {
            continue;
        }
        let Some(key) = path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()) else {
            continue;
        };
        let conf = parse_theme_conf(&path);
        let name = conf.get("name").cloned().unwrap_or_else(|| key.clone());
        let mode = conf
            .get("mode")
            .cloned()
            .unwrap_or_else(|| "dark".to_string());
        let bg_main = conf.get("bg_main").map(|s| s.as_str()).unwrap_or("1c161e");
        let accent = conf
            .get("accent")
            .or_else(|| conf.get("accent1"))
            .map(|s| s.as_str())
            .unwrap_or("e975ca");
        let selection = conf
            .get("selection")
            .map(|s| s.as_str())
            .unwrap_or("bf40a1");
        let magenta = conf
            .get("magenta")
            .or_else(|| conf.get("accent2"))
            .map(|s| s.as_str())
            .unwrap_or("ffc2ff");

        let is_active = cfg.theme_preset == "auto" && !cur_name.is_empty() && name == cur_name;
        themes.push(ThemeEntry {
            key,
            name,
            mode,
            swatches: [
                parse_hex_rgb(bg_main),
                parse_hex_rgb(accent),
                parse_hex_rgb(selection),
                parse_hex_rgb(magenta),
            ],
            is_active,
        });
    }

    themes.sort_by(|a, b| a.key.to_lowercase().cmp(&b.key.to_lowercase()));
    themes
}

const WALL_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

fn wallpaper_dir() -> std::path::PathBuf {
    crate::config::expand_path(&crate::config::get().paths.wallpapers_dir)
}

fn wallpaper_state_file() -> std::path::PathBuf {
    crate::config::expand_path(&crate::config::get().paths.wallpaper_state_file)
}

/// Finds a paired vertical wallpaper in `~/Pictures/Wallpapers/vertical/` (adapted from wallpaper-carousel.py)
fn find_vertical_pair(img_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let vert_dir = wallpaper_dir().join("vertical");
    if !vert_dir.is_dir() {
        return None;
    }
    if let Some(base) = img_path.file_name() {
        let exact = vert_dir.join(base);
        if exact.is_file() {
            return Some(exact);
        }
    }
    if let Some(stem) = img_path.file_stem().and_then(|s| s.to_str()) {
        for ext in WALL_EXTS {
            let alt = vert_dir.join(format!("{}.{}", stem, ext));
            if alt.is_file() {
                return Some(alt);
            }
        }
    }
    None
}

fn fetch_wallpapers_snapshot() -> Vec<WallpaperEntry> {
    let dir = wallpaper_dir();
    let current = fs::read_to_string(wallpaper_state_file())
        .unwrap_or_default()
        .trim()
        .to_string();

    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut list = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        if !WALL_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let path_str = path.to_string_lossy().to_string();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Wallpaper")
            .to_string();
        let has_vertical_pair = find_vertical_pair(&path).is_some();
        let is_active = !current.is_empty() && current == path_str;
        list.push(WallpaperEntry {
            path: path_str,
            name,
            has_vertical_pair,
            is_active,
        });
    }
    list.sort_by(|a, b| {
        match (a.name.parse::<u64>(), b.name.parse::<u64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });
    list
}

fn decode_wallpaper_thumb(path: &str) -> Option<(glib::Bytes, bool, usize, i32, i32)> {
    use gtk4::gdk_pixbuf::{InterpType, Pixbuf};
    let pix = Pixbuf::from_file(path).ok()?;
    let pw = pix.width();
    let ph = pix.height();
    if pw <= 0 || ph <= 0 {
        return None;
    }
    let target_w = 166;
    let target_h = 94;
    let scale = (target_w as f64 / pw as f64).max(target_h as f64 / ph as f64);
    let sw = ((pw as f64 * scale).round() as i32).max(target_w);
    let sh = ((ph as f64 * scale).round() as i32).max(target_h);
    let scaled = pix.scale_simple(sw, sh, InterpType::Bilinear)?;
    let ox = ((sw - target_w) / 2).max(0);
    let oy = ((sh - target_h) / 2).max(0);
    let cropped = scaled.new_subpixbuf(ox, oy, target_w, target_h);
    Some((
        cropped.read_pixel_bytes(),
        cropped.has_alpha(),
        cropped.rowstride() as usize,
        cropped.width(),
        cropped.height(),
    ))
}

/// Applies wallpaper using the configured backend (`swww`, `awww`, `hyprpaper`, or `custom`)
/// with per-monitor orientation & paired vertical crops.
fn apply_wallpaper_native(path: &str) -> Result<String, String> {
    let p = std::path::Path::new(path);
    if !p.is_file() {
        return Err("Wallpaper file not found".to_string());
    }
    let state_path = wallpaper_state_file();
    if let Some(parent) = state_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&state_path, path);

    let vert_path = find_vertical_pair(p)
        .map(|vp| vp.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());

    let tools = crate::config::get().tools;
    let backend = tools.wallpaper_backend.as_str();

    if backend == "swww" || backend == "awww" {
        let daemon_running = Command::new("pgrep")
            .args(["-x", "swww-daemon"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || Command::new("pgrep")
                .args(["-x", "awww-daemon"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        if !daemon_running {
            let daemon_bin = if backend == "awww" {
                "awww-daemon"
            } else {
                "swww-daemon"
            };
            let _ = Command::new(daemon_bin).args(["--format", "xrgb"]).spawn();
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    let mut applied_any = false;
    if let Ok(out) = Command::new("hyprctl").args(["monitors", "-j"]).output() {
        if let Ok(arr) = serde_json::from_slice::<Vec<Value>>(&out.stdout) {
            for m in arr {
                let Some(name) = m.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let transform = m.get("transform").and_then(|v| v.as_i64()).unwrap_or(0);
                let width = m.get("width").and_then(|v| v.as_i64()).unwrap_or(1920);
                let height = m.get("height").and_then(|v| v.as_i64()).unwrap_or(1080);
                let is_vert = transform == 1 || transform == 3 || height > width;
                let pos = if is_vert { "top-left" } else { "top-right" };
                let target_img = if is_vert { vert_path.as_str() } else { path };

                match backend {
                    "hyprpaper" => {
                        let _ = Command::new("hyprctl")
                            .args(["hyprpaper", "preload", target_img])
                            .status();
                        let spec = format!("{},{}", name, target_img);
                        let _ = Command::new("hyprctl")
                            .args(["hyprpaper", "wallpaper", &spec])
                            .spawn();
                    }
                    "custom" => {
                        let cmd = tools
                            .wallpaper_custom_cmd
                            .replace("{monitor}", name)
                            .replace("{path}", target_img);
                        if !cmd.trim().is_empty() {
                            let _ = Command::new("sh").args(["-c", &cmd]).spawn();
                        }
                    }
                    _ => {
                        let bin = if backend == "awww" { "awww" } else { "swww" };
                        let _ = Command::new(bin)
                            .args([
                                "img",
                                "-o",
                                name,
                                target_img,
                                "--resize",
                                "crop",
                                "--transition-type",
                                "grow",
                                "--transition-pos",
                                pos,
                                "--transition-fps",
                                "144",
                                "--transition-step",
                                "200",
                                "--transition-bezier",
                                ".25,.1,.25,1",
                                "--transition-duration",
                                "1.4",
                            ])
                            .spawn();
                    }
                }
                applied_any = true;
            }
        }
    }

    if !applied_any {
        match backend {
            "hyprpaper" => {
                let _ = Command::new("hyprctl")
                    .args(["hyprpaper", "preload", path])
                    .status();
                let _ = Command::new("hyprctl")
                    .args(["hyprpaper", "wallpaper", &format!(",{}", path)])
                    .spawn();
            }
            "custom" => {
                let cmd = tools
                    .wallpaper_custom_cmd
                    .replace("{monitor}", "")
                    .replace("{path}", path);
                if !cmd.trim().is_empty() {
                    let _ = Command::new("sh").args(["-c", &cmd]).spawn();
                }
            }
            _ => {
                let bin = if backend == "awww" { "awww" } else { "swww" };
                let _ = Command::new(bin)
                    .args([
                        "img",
                        path,
                        "--resize",
                        "crop",
                        "--transition-type",
                        "grow",
                        "--transition-pos",
                        "top-right",
                        "--transition-fps",
                        "144",
                    ])
                    .spawn();
            }
        }
    }

    let label = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("wallpaper")
        .to_string();
    Ok(format!("Applied {}", label))
}

// ==========================================
// UI BUILDER
// ==========================================

fn parse_quoted_str(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\'' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let quote = bytes[i];
    i += 1;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            out.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return Some(out);
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    None
}

fn prettify_key_combo(raw: &str) -> String {
    let mut s = raw
        .replace("Super_L", "(Tap)")
        .replace("mouse:272", "Left Drag")
        .replace("mouse:273", "Right Drag")
        .replace("mouse_down", "Scroll Down")
        .replace("mouse_up", "Scroll Up")
        .replace("XF86AudioRaiseVolume", "Vol Up")
        .replace("XF86AudioLowerVolume", "Vol Down")
        .replace("XF86AudioMute", "Audio Mute")
        .replace("XF86AudioMicMute", "Mic Mute")
        .replace("XF86MonBrightnessUp", "Brightness Up")
        .replace("XF86MonBrightnessDown", "Brightness Down")
        .replace("XF86AudioNext", "Media Next")
        .replace("XF86AudioPrev", "Media Prev")
        .replace("XF86AudioPause", "Media Pause")
        .replace("XF86AudioPlay", "Media Play")
        .replace(" + (Tap)", " (Tap)")
        .trim()
        .to_string();
    // Uppercase single vim/letter keys cleanly after '+'
    if let Some((prefix, last)) = s.rsplit_once(" + ") {
        if last.len() == 1 && last.chars().all(|c| c.is_ascii_lowercase()) {
            s = format!("{} + {}", prefix, last.to_uppercase());
        }
    }
    s
}

fn describe_bind_action(action: &str) -> (String, String) {
    let a = action.trim();
    if let Some(rest) = a.strip_prefix("hl.dsp.exec_cmd(") {
        let cmd = parse_quoted_str(rest).unwrap_or_else(|| rest.to_string());
        if cmd.contains("lumine-capture") {
            let desc = if cmd.contains("-m") {
                "Screenshot active monitor".to_string()
            } else if cmd.contains("-f") {
                "Screenshot all monitors".to_string()
            } else if cmd.contains("ocr") {
                "Region OCR to clipboard".to_string()
            } else {
                "Screenshot region / editor".to_string()
            };
            return ("CAPTURE & SCREENSHOTS".to_string(), desc);
        }
        if cmd.contains("swayosd-client") || cmd.contains("playerctl") {
            let desc = if cmd.contains("output-volume raise") {
                "Volume up".to_string()
            } else if cmd.contains("output-volume lower") {
                "Volume down".to_string()
            } else if cmd.contains("output-volume mute") {
                "Toggle audio mute".to_string()
            } else if cmd.contains("input-volume mute") {
                "Toggle mic mute".to_string()
            } else if cmd.contains("brightness raise") {
                "Brightness up".to_string()
            } else if cmd.contains("brightness lower") {
                "Brightness down".to_string()
            } else if cmd.contains("playerctl next") {
                "Next track".to_string()
            } else if cmd.contains("playerctl previous") {
                "Previous track".to_string()
            } else if cmd.contains("playerctl play-pause") {
                "Play / pause media".to_string()
            } else {
                cmd.clone()
            };
            return ("MEDIA & HARDWARE".to_string(), desc);
        }
        let desc = if cmd.contains("lily-dashboard") {
            "Toggle Lily Dashboard".to_string()
        } else if cmd.contains("kitty") {
            "Open terminal (kitty)".to_string()
        } else if cmd.contains("dolphin") {
            "Open file manager (dolphin)".to_string()
        } else if cmd.contains("wofi") {
            "Toggle app launcher (wofi)".to_string()
        } else if cmd.contains("hyprlauncher") {
            "Open hyprlauncher".to_string()
        } else if cmd.contains("power-menu") {
            "Open power menu".to_string()
        } else if cmd.contains("wallpaper-carousel") {
            "Open wallpaper carousel".to_string()
        } else if cmd.contains("waybar") {
            "Restart Waybar".to_string()
        } else {
            format!("Run: {}", cmd)
        };
        return ("APPS & LAUNCHERS".to_string(), desc);
    }

    if a.starts_with("hl.dsp.window.close") {
        return ("WINDOW MANAGEMENT".to_string(), "Close active window".to_string());
    }
    if a.starts_with("hl.dsp.window.float") {
        return ("WINDOW MANAGEMENT".to_string(), "Toggle floating window".to_string());
    }
    if a.starts_with("hl.dsp.window.fullscreen") {
        return ("WINDOW MANAGEMENT".to_string(), "Toggle fullscreen".to_string());
    }
    if a.starts_with("hl.dsp.window.drag") {
        return ("WINDOW MANAGEMENT".to_string(), "Move window (mouse drag)".to_string());
    }
    if a.starts_with("hl.dsp.window.resize") {
        return ("WINDOW MANAGEMENT".to_string(), "Resize window (mouse drag)".to_string());
    }
    if a.starts_with("hl.dsp.layout(") {
        let mode = parse_quoted_str(a).unwrap_or_else(|| "layout".to_string());
        let desc = if mode == "togglesplit" {
            "Toggle dwindle split direction".to_string()
        } else {
            format!("Layout: {}", mode)
        };
        return ("WINDOW MANAGEMENT".to_string(), desc);
    }
    if a.starts_with("hl.dsp.focus(") {
        if let Some(dir_pos) = a.find("direction") {
            if let Some(dir) = parse_quoted_str(&a[dir_pos..]) {
                return ("WINDOW MANAGEMENT".to_string(), format!("Focus {}", dir));
            }
        }
        if a.contains("e+1") {
            return ("WORKSPACES & SCRATCHPADS".to_string(), "Next workspace".to_string());
        }
        if a.contains("e-1") {
            return ("WORKSPACES & SCRATCHPADS".to_string(), "Previous workspace".to_string());
        }
        if a.contains("workspace") {
            return ("WORKSPACES & SCRATCHPADS".to_string(), "Switch workspace".to_string());
        }
    }
    if a.starts_with("hl.dsp.window.move(") {
        if let Some(dir_pos) = a.find("direction") {
            if let Some(dir) = parse_quoted_str(&a[dir_pos..]) {
                return ("WINDOW MANAGEMENT".to_string(), format!("Move window {}", dir));
            }
        }
        if let Some(ws_pos) = a.find("workspace") {
            if let Some(ws) = parse_quoted_str(&a[ws_pos..]) {
                if let Some(sp) = ws.strip_prefix("special:") {
                    return (
                        "WORKSPACES & SCRATCHPADS".to_string(),
                        format!("Send window to '{}' scratchpad", sp),
                    );
                }
                return (
                    "WORKSPACES & SCRATCHPADS".to_string(),
                    format!("Move window to workspace {}", ws),
                );
            }
            return (
                "WORKSPACES & SCRATCHPADS".to_string(),
                "Move window to workspace".to_string(),
            );
        }
    }
    if a.starts_with("hl.dsp.workspace.toggle_special(") {
        let name = parse_quoted_str(a).unwrap_or_else(|| "special".to_string());
        return (
            "WORKSPACES & SCRATCHPADS".to_string(),
            format!("Toggle '{}' scratchpad", name),
        );
    }
    ("OTHER".to_string(), a.to_string())
}

fn eval_lua_key_expr(expr: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut out = String::new();
    for part in expr.split("..") {
        let p = part.trim();
        if let Some(lit) = parse_quoted_str(p) {
            out.push_str(&lit);
        } else if let Some(val) = vars.get(p) {
            out.push_str(val);
        } else if p == "key" || p == "i" {
            out.push_str("1..0");
        } else {
            out.push_str(p);
        }
    }
    prettify_key_combo(&out)
}

fn split_top_level_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quote.is_some() {
            cur.push(ch);
            escaped = true;
            continue;
        }
        if let Some(q) = in_quote {
            cur.push(ch);
            if ch == q {
                in_quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                in_quote = Some(ch);
                cur.push(ch);
            }
            '(' | '{' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                args.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        args.push(cur.trim().to_string());
    }
    args
}

fn fetch_keybinds_snapshot() -> Vec<KeybindEntry> {
    let path = crate::config::expand_path(&crate::config::get().paths.keybinds_file);
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };

    let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut entries = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("--") || line.starts_with('#') {
            continue;
        }
        // Track Hyprlua variables (`local mainMod = "SUPER"`) and classic Hyprland `$mainMod = SUPER`
        if let Some(rest) = line.strip_prefix("local ") {
            if let Some((lhs, rhs)) = rest.split_once('=') {
                if let Some(val) = parse_quoted_str(rhs) {
                    vars.insert(lhs.trim().to_string(), val);
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('$') {
            if let Some((lhs, rhs)) = rest.split_once('=') {
                let val = rhs.split('#').next().unwrap_or("").trim().to_string();
                vars.insert(format!("${}", lhs.trim()), val);
            }
            continue;
        }

        // 1. Hyprlua format: `hl.bind(key, action[, opts])`
        if let Some(bind_start) = line.find("hl.bind(") {
            let after = &line[bind_start + "hl.bind(".len()..];
            if let Some(end_idx) = after.rfind(')') {
                let inner = &after[..end_idx];
                let args = split_top_level_args(inner);
                if args.len() >= 2 {
                    let keys = eval_lua_key_expr(&args[0], &vars);
                    let (group, mut desc) = describe_bind_action(&args[1]);
                    if keys.contains("1..0") {
                        if desc == "Switch workspace" {
                            desc = "Switch to workspace 1–10".to_string();
                        } else if desc == "Move window to workspace" {
                            desc = "Move active window to workspace 1–10".to_string();
                        }
                    }
                    entries.push(KeybindEntry {
                        group,
                        keys,
                        desc,
                        raw: args[1].clone(),
                    });
                }
            }
            continue;
        }

        // 2. Classic hyprland.conf format: `bind[elmr] = MOD, KEY, dispatcher[, params]`
        if line.starts_with("bind") {
            if let Some((lhs, rhs)) = line.split_once('=') {
                let lhs = lhs.trim();
                if lhs == "bind" || lhs.starts_with("bind") && lhs.len() <= 8 {
                    let body = rhs.split('#').next().unwrap_or("").trim();
                    let parts: Vec<&str> = body.splitn(4, ',').map(|s| s.trim()).collect();
                    if parts.len() >= 3 {
                        let mut mods = parts[0].to_string();
                        for (k, v) in &vars {
                            mods = mods.replace(k, v);
                        }
                        let key = parts[1];
                        let combo = if mods.is_empty() {
                            prettify_key_combo(key)
                        } else {
                            prettify_key_combo(&format!("{} + {}", mods.replace(' ', " + "), key))
                        };
                        let dispatcher = parts[2];
                        let params = parts.get(3).copied().unwrap_or("");
                        let raw = if params.is_empty() {
                            dispatcher.to_string()
                        } else {
                            format!("{} {}", dispatcher, params)
                        };
                        let (group, desc) = match dispatcher {
                            "exec" => ("APPS & LAUNCHERS".to_string(), format!("Run: {}", params)),
                            "killactive" => ("WINDOW MANAGEMENT".to_string(), "Close active window".to_string()),
                            "togglefloating" => ("WINDOW MANAGEMENT".to_string(), "Toggle floating window".to_string()),
                            "fullscreen" => ("WINDOW MANAGEMENT".to_string(), "Toggle fullscreen".to_string()),
                            "movefocus" => ("WINDOW MANAGEMENT".to_string(), format!("Focus {}", params)),
                            "movewindow" => ("WINDOW MANAGEMENT".to_string(), format!("Move window {}", params)),
                            "workspace" => ("WORKSPACES & SCRATCHPADS".to_string(), format!("Workspace {}", params)),
                            "movetoworkspace" => ("WORKSPACES & SCRATCHPADS".to_string(), format!("Move to workspace {}", params)),
                            "togglespecialworkspace" => ("WORKSPACES & SCRATCHPADS".to_string(), format!("Toggle '{}' scratchpad", params)),
                            _ => ("OTHER".to_string(), raw.clone()),
                        };
                        entries.push(KeybindEntry {
                            group,
                            keys: combo,
                            desc,
                            raw,
                        });
                    }
                }
            }
        }
    }

    entries
}

pub fn build_system_ui(
    close_drawer: Rc<dyn Fn()>,
    reload_theme: Rc<dyn Fn()>,
    apply_tabs: Rc<dyn Fn()>,
) -> SystemUi {
    let (tx, rx) = mpsc::channel::<SystemMsg>();

    // --- LEFT MENU (inside main stack, 288px) ---
    let list_view = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let hdr = Label::builder()
        .label("SYSTEM")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    list_view.append(&hdr);

    let sub_lbl = Label::builder()
        .label("Select a section to configure")
        .halign(Align::Start)
        .css_classes(vec!["plan-subnote".to_string()])
        .margin_bottom(4)
        .build();
    list_view.append(&sub_lbl);

    let make_cat_btn = |icon: &str, title: &str, desc: &str| -> Button {
        let btn = Button::builder()
            .css_classes(vec!["fav-item-btn".to_string()])
            .hexpand(true)
            .build();
        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        let icon_lbl = Label::builder()
            .label(icon)
            .css_classes(vec!["power-btn-lbl".to_string()])
            .width_request(22)
            .build();
        let text_col = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(1)
            .hexpand(true)
            .build();
        let title_lbl = Label::builder()
            .label(title)
            .halign(Align::Start)
            .css_classes(vec!["plan-step".to_string()])
            .build();
        let desc_lbl = Label::builder()
            .label(desc)
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        text_col.append(&title_lbl);
        text_col.append(&desc_lbl);

        let arrow = Label::builder()
            .label(">")
            .css_classes(vec!["fav-score-badge".to_string()])
            .valign(Align::Center)
            .build();

        row.append(&icon_lbl);
        row.append(&text_col);
        row.append(&arrow);
        btn.set_child(Some(&row));
        btn
    };

    let sound_cat_btn = make_cat_btn("", "Sound", "Outputs, mics & app volumes");
    let net_cat_btn = make_cat_btn("󰤨", "Network", "Connections, Wi-Fi & VPNs");
    let theme_cat_btn = make_cat_btn("󰏘", "Themes", "Live desktop color palettes");
    let wall_cat_btn = make_cat_btn("󰸉", "Wallpapers", "Multi-monitor wallpaper picker");
    let disp_cat_btn = make_cat_btn("󰍹", "Displays", "Monitors, refresh & focus");
    let kbd_cat_btn = make_cat_btn("󰌌", "Keybinds", "Live Hyprland cheat sheet");
    let over_cat_btn = make_cat_btn("󰍛", "Overview", "Stats, capture & quick tools");
    let sett_cat_btn = make_cat_btn("󰒓", "Settings", "Hyprland, apps, paths & tabs");

    list_view.append(&sound_cat_btn);
    list_view.append(&net_cat_btn);
    list_view.append(&theme_cat_btn);
    list_view.append(&wall_cat_btn);
    list_view.append(&disp_cat_btn);
    list_view.append(&kbd_cat_btn);
    list_view.append(&over_cat_btn);
    list_view.append(&sett_cat_btn);

    // Quick summary badges in the left pane (spacer pushes it neatly to bottom)
    let spacer = Box::builder().vexpand(true).build();
    list_view.append(&spacer);

    let quick_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .css_classes(vec!["plan-check-row".to_string()])
        .build();
    let quick_hdr = Label::builder()
        .label("QUICK STATUS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    let quick_audio_lbl = Label::builder()
        .label("Audio: ...")
        .halign(Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    let quick_net_lbl = Label::builder()
        .label("Net: ...")
        .halign(Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    quick_card.append(&quick_hdr);
    quick_card.append(&quick_audio_lbl);
    quick_card.append(&quick_net_lbl);
    list_view.append(&quick_card);

    // --- RIGHT SLIDE-OUT DETAIL PANE (400px, attached to dash_box) ---
    let detail_view = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["plan-detail-pane".to_string()])
        .width_request(400)
        .vexpand(true)
        .visible(false)
        .build();

    let detail_header = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_top(12)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(6)
        .build();

    let detail_title = Label::builder()
        .label("SOUND")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["plan-h1".to_string()])
        .build();

    let detail_refresh_btn = Button::builder()
        .label("Refresh")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let detail_close_btn = Button::builder()
        .label("×")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .tooltip_text("Close System panel")
        .build();

    detail_header.append(&detail_title);
    detail_header.append(&detail_refresh_btn);
    detail_header.append(&detail_close_btn);
    detail_view.append(&detail_header);

    let detail_stack = Stack::new();
    detail_stack.set_transition_type(StackTransitionType::SlideLeftRight);
    detail_stack.set_transition_duration(180);
    detail_stack.set_hexpand(true);
    detail_stack.set_vexpand(true);
    detail_view.append(&detail_stack);

    let make_scroll_page = || -> (ScrolledWindow, Box) {
        let sc = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .vscrollbar_policy(PolicyType::Automatic)
            .hexpand(true)
            .vexpand(true)
            .build();
        let inner = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(10)
            .margin_top(6)
            .margin_bottom(14)
            .margin_start(14)
            .margin_end(14)
            .build();
        sc.set_child(Some(&inner));
        (sc, inner)
    };

    // PAGE 1: SOUND
    let (sound_scroll, sound_box) = make_scroll_page();
    detail_stack.add_named(&sound_scroll, Some("sound"));

    // PAGE 2: NETWORK
    let (net_scroll, net_box) = make_scroll_page();
    let net_status_lbl = Label::builder()
        .label("")
        .halign(Align::Start)
        .wrap(true)
        .css_classes(vec!["plan-subnote".to_string()])
        .visible(false)
        .build();
    let net_content_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .build();
    net_box.append(&net_status_lbl);
    net_box.append(&net_content_box);
    detail_stack.add_named(&net_scroll, Some("network"));

    // PAGE 3: THEMES
    let (theme_scroll, theme_box) = make_scroll_page();
    let theme_status_lbl = Label::builder()
        .label("")
        .halign(Align::Start)
        .wrap(true)
        .css_classes(vec!["plan-subnote".to_string()])
        .visible(false)
        .build();
    let theme_content_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .build();
    theme_box.append(&theme_status_lbl);
    theme_box.append(&theme_content_box);
    detail_stack.add_named(&theme_scroll, Some("themes"));

    // PAGE 4: WALLPAPERS
    let (wall_scroll, wall_box) = make_scroll_page();
    let wall_status_lbl = Label::builder()
        .label("")
        .halign(Align::Start)
        .wrap(true)
        .css_classes(vec!["plan-subnote".to_string()])
        .visible(false)
        .build();
    let wall_content_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .build();
    wall_box.append(&wall_status_lbl);
    wall_box.append(&wall_content_box);
    detail_stack.add_named(&wall_scroll, Some("wallpapers"));

    // PAGE 5: DISPLAYS
    let (disp_scroll, disp_box) = make_scroll_page();
    detail_stack.add_named(&disp_scroll, Some("displays"));

    // PAGE 6: KEYBINDS (live binds.lua parser + instant filter)
    let kbd_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(6)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(10)
        .hexpand(true)
        .vexpand(true)
        .build();
    let kbd_search = Entry::builder()
        .placeholder_text("Search keybinds, keys, or apps...")
        .css_classes(vec!["fav-search".to_string()])
        .build();
    let kbd_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();
    let kbd_list_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_bottom(8)
        .build();
    kbd_scroll.set_child(Some(&kbd_list_box));
    kbd_page.append(&kbd_search);
    kbd_page.append(&kbd_scroll);
    detail_stack.add_named(&kbd_page, Some("keybinds"));

    // PAGE 7: OVERVIEW
    let (over_scroll, over_box) = make_scroll_page();
    detail_stack.add_named(&over_scroll, Some("overview"));

    // PAGE 8: SETTINGS (Hyprland live sliders, apps/tools, paths, visible tabs)
    let (sett_scroll, sett_box) = make_scroll_page();
    detail_stack.add_named(&sett_scroll, Some("settings"));

    // Shared active section tracking
    let active_section: Rc<RefCell<String>> = Rc::new(RefCell::new("sound".to_string()));
    let cpu_state: Rc<RefCell<Option<(u64, u64)>>> = Rc::new(RefCell::new(None));
    let wifi_pw_target: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let keybinds_cache: Rc<RefCell<Vec<KeybindEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let last_overview_snap: Rc<RefCell<Option<OverviewSnapshot>>> = Rc::new(RefCell::new(None));
    let wall_thumb_cache: Rc<RefCell<std::collections::HashMap<String, gtk4::gdk::Texture>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    let wall_img_slots: Rc<RefCell<std::collections::HashMap<String, Picture>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));

    let cat_buttons = vec![
        ("sound", sound_cat_btn.clone()),
        ("network", net_cat_btn.clone()),
        ("themes", theme_cat_btn.clone()),
        ("wallpapers", wall_cat_btn.clone()),
        ("displays", disp_cat_btn.clone()),
        ("keybinds", kbd_cat_btn.clone()),
        ("overview", over_cat_btn.clone()),
        ("settings", sett_cat_btn.clone()),
    ];

    let trigger_refresh: Rc<dyn Fn(&str, bool)> = {
        let tx_ref = tx.clone();
        let cpu_ref = cpu_state.clone();
        let sett_box_ref = sett_box.clone();
        let reload_theme_ref = reload_theme.clone();
        let apply_tabs_ref = apply_tabs.clone();
        Rc::new(move |section: &str, rescan_wifi: bool| {
            let tx_bg = tx_ref.clone();
            match section {
                "settings" => {
                    render_settings_page(&sett_box_ref, &reload_theme_ref, &apply_tabs_ref);
                }
                "sound" => {
                    std::thread::spawn(move || {
                        let _ = tx_bg.send(SystemMsg::Audio(fetch_audio_snapshot()));
                    });
                }
                "network" => {
                    std::thread::spawn(move || {
                        let _ = tx_bg.send(SystemMsg::Network(fetch_network_snapshot(rescan_wifi)));
                    });
                }
                "themes" => {
                    std::thread::spawn(move || {
                        let _ = tx_bg.send(SystemMsg::Themes(fetch_themes_snapshot()));
                    });
                }
                "wallpapers" => {
                    std::thread::spawn(move || {
                        let _ = tx_bg.send(SystemMsg::Wallpapers(fetch_wallpapers_snapshot()));
                    });
                }
                "displays" => {
                    std::thread::spawn(move || {
                        let _ = tx_bg.send(SystemMsg::Displays(fetch_displays_snapshot()));
                    });
                }
                "keybinds" => {
                    std::thread::spawn(move || {
                        let _ = tx_bg.send(SystemMsg::Keybinds(fetch_keybinds_snapshot()));
                    });
                }
                "overview" => {
                    let prev = *cpu_ref.borrow();
                    let tx_ov = tx_bg;
                    std::thread::spawn(move || {
                        let mut p = prev;
                        let snap = fetch_overview_snapshot(&mut p);
                        let _ = tx_ov.send(SystemMsg::Overview(snap));
                    });
                    // Prime cpu_state on main thread too so deltas advance cleanly
                    if let Some(cur) = read_cpu_times() {
                        *cpu_ref.borrow_mut() = Some(cur);
                    }
                }
                _ => {}
            }
        })
    };

    let open_section: Rc<dyn Fn(&str)> = {
        let detail_v = detail_view.clone();
        let d_stack = detail_stack.clone();
        let d_title = detail_title.clone();
        let active_s = active_section.clone();
        let cats = cat_buttons.clone();
        let refresh = trigger_refresh.clone();
        Rc::new(move |sec: &str| {
            *active_s.borrow_mut() = sec.to_string();
            d_stack.set_visible_child_name(sec);
            d_title.set_label(match sec {
                "sound" => "SOUND",
                "network" => "NETWORK",
                "themes" => "THEME SWITCH",
                "wallpapers" => "WALLPAPERS",
                "displays" => "DISPLAYS",
                "keybinds" => "KEYBINDS",
                "overview" => "OVERVIEW & TOOLS",
                "settings" => "SETTINGS",
                _ => "SYSTEM",
            });
            for (name, btn) in &cats {
                if *name == sec {
                    btn.add_css_class("pinned");
                } else {
                    btn.remove_css_class("pinned");
                }
            }
            detail_v.set_visible(true);
            refresh(sec, false);
        })
    };

    {
        let open_s = open_section.clone();
        sound_cat_btn.connect_clicked(move |_| open_s("sound"));
    }
    {
        let open_s = open_section.clone();
        net_cat_btn.connect_clicked(move |_| open_s("network"));
    }
    {
        let open_s = open_section.clone();
        theme_cat_btn.connect_clicked(move |_| open_s("themes"));
    }
    {
        let open_s = open_section.clone();
        wall_cat_btn.connect_clicked(move |_| open_s("wallpapers"));
    }
    {
        let open_s = open_section.clone();
        disp_cat_btn.connect_clicked(move |_| open_s("displays"));
    }
    {
        let open_s = open_section.clone();
        kbd_cat_btn.connect_clicked(move |_| open_s("keybinds"));
    }
    {
        let open_s = open_section.clone();
        over_cat_btn.connect_clicked(move |_| open_s("overview"));
    }
    {
        let open_s = open_section.clone();
        sett_cat_btn.connect_clicked(move |_| open_s("settings"));
    }

    // Live search filtering for Keybinds
    {
        let kbd_cache_f = keybinds_cache.clone();
        let kbd_box_f = kbd_list_box.clone();
        kbd_search.connect_changed(move |entry| {
            let query = entry.text().to_string();
            render_keybinds_page(&kbd_box_f, &kbd_cache_f.borrow(), &query);
        });
    }

    {
        let detail_close = detail_view.clone();
        detail_close_btn.connect_clicked(move |_| {
            detail_close.set_visible(false);
        });
    }

    {
        let active_r = active_section.clone();
        let refresh_r = trigger_refresh.clone();
        detail_refresh_btn.connect_clicked(move |_| {
            let cur = active_r.borrow().clone();
            refresh_r(&cur, cur == "network");
        });
    }

    // Populate quick status summary once on initial build
    {
        let tx_init = tx.clone();
        std::thread::spawn(move || {
            let _ = tx_init.send(SystemMsg::Audio(fetch_audio_snapshot()));
            let _ = tx_init.send(SystemMsg::Network(fetch_network_snapshot(false)));
        });
    }

    // Live periodic refresh ONLY while the slide-out detail panel is visible on Overview
    {
        let detail_vis = detail_view.clone();
        let active_tick = active_section.clone();
        let refresh_tick = trigger_refresh.clone();
        glib::timeout_add_seconds_local(2, move || {
            if detail_vis.is_visible() {
                let sec = active_tick.borrow().clone();
                if sec == "overview" {
                    refresh_tick("overview", false);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // Message receiver loop
    let sound_box_r = sound_box.clone();
    let net_box_r = net_content_box.clone();
    let net_status_r = net_status_lbl.clone();
    let theme_box_r = theme_content_box.clone();
    let theme_status_r = theme_status_lbl.clone();
    let wall_box_r = wall_content_box.clone();
    let wall_status_r = wall_status_lbl.clone();
    let wall_cache_r = wall_thumb_cache.clone();
    let wall_slots_r = wall_img_slots.clone();
    let disp_box_r = disp_box.clone();
    let kbd_box_r = kbd_list_box.clone();
    let kbd_search_r = kbd_search.clone();
    let kbd_cache_r = keybinds_cache.clone();
    let over_box_r = over_box.clone();
    let last_ov_r = last_overview_snap.clone();
    let close_for_ui = close_drawer.clone();
    let reload_theme_ui = reload_theme.clone();
    let quick_audio_r = quick_audio_lbl.clone();
    let quick_net_r = quick_net_lbl.clone();
    let refresh_for_ui = trigger_refresh.clone();
    let tx_for_ui = tx.clone();
    let wifi_pw_r = wifi_pw_target.clone();
    let suspend_armed = Rc::new(Cell::new(false));

    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                SystemMsg::Audio(snap) => {
                    if let Some(def) = snap.sinks.iter().find(|s| s.is_default) {
                        let mute_tag = if def.muted { " [MUTED]" } else { "" };
                        quick_audio_r.set_label(&format!(
                            "Audio: {:.0}%{} - {}",
                            def.volume_pct, mute_tag, def.description
                        ));
                    } else {
                        quick_audio_r.set_label("Audio: No output device");
                    }
                    render_sound_page(&sound_box_r, &snap, &refresh_for_ui);
                }
                SystemMsg::Network(snap) => {
                    if let Some(first) = snap.active.first() {
                        let ip_part = if first.ip.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", first.ip)
                        };
                        quick_net_r.set_label(&format!("Net: {}{}", first.name, ip_part));
                    } else {
                        quick_net_r.set_label("Net: Disconnected");
                    }
                    render_network_page(
                        &net_box_r,
                        &net_status_r,
                        &snap,
                        &wifi_pw_r,
                        &refresh_for_ui,
                        &tx_for_ui,
                    );
                }
                SystemMsg::NetworkAction(status) => {
                    net_status_r.set_label(&status);
                    net_status_r.set_visible(true);
                    refresh_for_ui("network", false);
                }
                SystemMsg::Themes(themes) => {
                    render_themes_page(
                        &theme_box_r,
                        &theme_status_r,
                        &themes,
                        &tx_for_ui,
                        &reload_theme_ui,
                    );
                }
                SystemMsg::ThemeApplied(status) => {
                    theme_status_r.set_label(&status);
                    theme_status_r.set_visible(true);
                    refresh_for_ui("themes", false);
                }
                SystemMsg::Wallpapers(walls) => {
                    render_wallpapers_page(
                        &wall_box_r,
                        &wall_status_r,
                        &walls,
                        &wall_cache_r,
                        &wall_slots_r,
                        &tx_for_ui,
                    );
                }
                SystemMsg::WallpaperThumb {
                    path,
                    bytes,
                    has_alpha,
                    rowstride,
                    width,
                    height,
                } => {
                    let fmt = if has_alpha {
                        gtk4::gdk::MemoryFormat::R8g8b8a8
                    } else {
                        gtk4::gdk::MemoryFormat::R8g8b8
                    };
                    let mem_tex =
                        gtk4::gdk::MemoryTexture::new(width, height, fmt, &bytes, rowstride);
                    let tex: gtk4::gdk::Texture = mem_tex.upcast();
                    wall_cache_r.borrow_mut().insert(path.clone(), tex.clone());
                    if let Some(img) = wall_slots_r.borrow().get(&path) {
                        img.set_paintable(Some(&tex));
                    }
                }
                SystemMsg::WallpaperApplied(status) => {
                    wall_status_r.set_label(&status);
                    wall_status_r.set_visible(true);
                    refresh_for_ui("wallpapers", false);
                }
                SystemMsg::Displays(monitors) => {
                    render_displays_page(&disp_box_r, &monitors, &refresh_for_ui);
                }
                SystemMsg::Keybinds(binds) => {
                    *kbd_cache_r.borrow_mut() = binds;
                    let q = kbd_search_r.text().to_string();
                    render_keybinds_page(&kbd_box_r, &kbd_cache_r.borrow(), &q);
                }
                SystemMsg::Overview(snap) => {
                    *last_ov_r.borrow_mut() = Some(snap.clone());
                    render_overview_page(
                        &over_box_r,
                        &snap,
                        &suspend_armed,
                        &refresh_for_ui,
                        &close_for_ui,
                        &tx_for_ui,
                    );
                }
                SystemMsg::ColorPicked => {
                    if let Some(snap) = last_ov_r.borrow().as_ref() {
                        render_overview_page(
                            &over_box_r,
                            snap,
                            &suspend_armed,
                            &refresh_for_ui,
                            &close_for_ui,
                            &tx_for_ui,
                        );
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });

    let open_default_cb: Rc<dyn Fn()> = {
        let active_cur = active_section.clone();
        let open_s = open_section.clone();
        Rc::new(move || {
            let sec = active_cur.borrow().clone();
            open_s(&sec);
        })
    };

    SystemUi {
        list_view,
        detail_view,
        open_default_cb,
    }
}

// ==========================================
// PAGE RENDERERS
// ==========================================

fn clear_box(b: &Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

fn render_sound_page(
    container: &Box,
    snap: &AudioSnapshot,
    refresh_cb: &Rc<dyn Fn(&str, bool)>,
) {
    clear_box(container);

    // 1. MASTER OUTPUT VOLUME & MUTE
    if let Some(def_sink) = snap.sinks.iter().find(|s| s.is_default) {
        let master_card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(6)
            .css_classes(vec!["plan-check-row".to_string()])
            .build();

        let top_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();
        let title = Label::builder()
            .label("OUTPUT VOLUME")
            .halign(Align::Start)
            .hexpand(true)
            .css_classes(vec!["fav-group-header".to_string()])
            .build();
        let pct_lbl = Label::builder()
            .label(&format!("{:.0}%", def_sink.volume_pct))
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        let mute_btn = Button::builder()
            .label(if def_sink.muted { "Muted" } else { "Mute" })
            .css_classes(if def_sink.muted {
                vec!["plan-fmt-btn".to_string(), "active".to_string()]
            } else {
                vec!["plan-fmt-btn".to_string()]
            })
            .build();
        top_row.append(&title);
        top_row.append(&pct_lbl);
        top_row.append(&mute_btn);

        let slider = Scale::with_range(Orientation::Horizontal, 0.0, 150.0, 2.0);
        slider.set_value(def_sink.volume_pct.clamp(0.0, 150.0));
        slider.set_draw_value(false);
        slider.add_css_class("music-seek");

        let sink_name_s = def_sink.name.clone();
        let pct_s = pct_lbl.clone();
        slider.connect_value_changed(move |sc| {
            let val = sc.value().round() as u32;
            pct_s.set_label(&format!("{}%", val));
            let _ = Command::new("pactl")
                .args(["set-sink-volume", &sink_name_s, &format!("{}%", val)])
                .spawn();
        });

        let sink_name_m = def_sink.name.clone();
        let ref_m = refresh_cb.clone();
        mute_btn.connect_clicked(move |_| {
            let _ = Command::new("pactl")
                .args(["set-sink-mute", &sink_name_m, "toggle"])
                .status();
            ref_m("sound", false);
        });

        master_card.append(&top_row);
        master_card.append(&slider);
        container.append(&master_card);
    }

    // 2. OUTPUT DEVICES (SINKS)
    let out_hdr = Label::builder()
        .label("OUTPUT DEVICES")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(4)
        .build();
    container.append(&out_hdr);

    if snap.sinks.is_empty() {
        let empty = Label::builder()
            .label("No audio outputs found.")
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        container.append(&empty);
    } else {
        let stream_ids: Vec<u64> = snap.streams.iter().map(|s| s.index).collect();
        for sink in &snap.sinks {
            let mut classes = vec!["fav-item-btn".to_string()];
            if sink.is_default {
                classes.push("pinned".to_string());
            }
            let btn = Button::builder()
                .css_classes(classes)
                .hexpand(true)
                .build();
            let row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .build();
            let lbl = Label::builder()
                .label(&sink.description)
                .halign(Align::Start)
                .hexpand(true)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .css_classes(vec!["plan-body".to_string()])
                .build();
            let badge = Label::builder()
                .label(if sink.is_default { "ACTIVE" } else { "Select" })
                .css_classes(vec!["fav-score-badge".to_string()])
                .build();
            row.append(&lbl);
            row.append(&badge);
            btn.set_child(Some(&row));

            let s_name = sink.name.clone();
            let ids = stream_ids.clone();
            let ref_c = refresh_cb.clone();
            btn.connect_clicked(move |_| {
                let _ = Command::new("pactl")
                    .args(["set-default-sink", &s_name])
                    .status();
                for id in &ids {
                    let _ = Command::new("pactl")
                        .args(["move-sink-input", &id.to_string(), &s_name])
                        .status();
                }
                ref_c("sound", false);
            });
            container.append(&btn);
        }
    }

    // 3. INPUT DEVICES (MICROPHONES)
    let in_hdr = Label::builder()
        .label("INPUT DEVICES (MICROPHONES)")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&in_hdr);

    if let Some(def_src) = snap.sources.iter().find(|s| s.is_default) {
        let mic_card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(6)
            .css_classes(vec!["plan-check-row".to_string()])
            .build();
        let top_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();
        let lbl = Label::builder()
            .label(&def_src.description)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-body".to_string()])
            .build();
        let pct_lbl = Label::builder()
            .label(&format!("{:.0}%", def_src.volume_pct))
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        let mute_btn = Button::builder()
            .label(if def_src.muted { "Muted" } else { "Mute" })
            .css_classes(if def_src.muted {
                vec!["plan-fmt-btn".to_string(), "active".to_string()]
            } else {
                vec!["plan-fmt-btn".to_string()]
            })
            .build();
        top_row.append(&lbl);
        top_row.append(&pct_lbl);
        top_row.append(&mute_btn);

        let slider = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 2.0);
        slider.set_value(def_src.volume_pct.clamp(0.0, 100.0));
        slider.set_draw_value(false);
        slider.add_css_class("music-seek");

        let src_name_s = def_src.name.clone();
        let pct_s = pct_lbl.clone();
        slider.connect_value_changed(move |sc| {
            let val = sc.value().round() as u32;
            pct_s.set_label(&format!("{}%", val));
            let _ = Command::new("pactl")
                .args(["set-source-volume", &src_name_s, &format!("{}%", val)])
                .spawn();
        });

        let src_name_m = def_src.name.clone();
        let ref_m = refresh_cb.clone();
        mute_btn.connect_clicked(move |_| {
            let _ = Command::new("pactl")
                .args(["set-source-mute", &src_name_m, "toggle"])
                .status();
            ref_m("sound", false);
        });

        mic_card.append(&top_row);
        mic_card.append(&slider);
        container.append(&mic_card);
    }

    for src in &snap.sources {
        if src.is_default {
            continue;
        }
        let btn = Button::builder()
            .css_classes(vec!["fav-item-btn".to_string()])
            .hexpand(true)
            .build();
        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();
        let lbl = Label::builder()
            .label(&src.description)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-body".to_string()])
            .build();
        let badge = Label::builder()
            .label("Use Mic")
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        row.append(&lbl);
        row.append(&badge);
        btn.set_child(Some(&row));

        let s_name = src.name.clone();
        let ref_c = refresh_cb.clone();
        btn.connect_clicked(move |_| {
            let _ = Command::new("pactl")
                .args(["set-default-source", &s_name])
                .status();
            ref_c("sound", false);
        });
        container.append(&btn);
    }

    // 4. PER-APP AUDIO STREAMS
    let apps_hdr = Label::builder()
        .label("APP VOLUMES")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&apps_hdr);

    if snap.streams.is_empty() {
        let empty = Label::builder()
            .label("No apps currently playing audio.")
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        container.append(&empty);
    } else {
        for stream in &snap.streams {
            let card = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(4)
                .css_classes(vec!["plan-check-row".to_string()])
                .build();
            let top = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(6)
                .build();
            let label_txt = if stream.media_name.is_empty() || stream.media_name == stream.app_name {
                stream.app_name.clone()
            } else {
                format!("{} - {}", stream.app_name, stream.media_name)
            };
            let lbl = Label::builder()
                .label(&label_txt)
                .halign(Align::Start)
                .hexpand(true)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .css_classes(vec!["plan-body".to_string()])
                .build();
            let pct_lbl = Label::builder()
                .label(&format!("{:.0}%", stream.volume_pct))
                .css_classes(vec!["fav-score-badge".to_string()])
                .build();
            let mute_btn = Button::builder()
                .label(if stream.muted { "Muted" } else { "Mute" })
                .css_classes(if stream.muted {
                    vec!["plan-fmt-btn".to_string(), "active".to_string()]
                } else {
                    vec!["plan-fmt-btn".to_string()]
                })
                .build();
            top.append(&lbl);
            top.append(&pct_lbl);
            top.append(&mute_btn);

            let slider = Scale::with_range(Orientation::Horizontal, 0.0, 150.0, 2.0);
            slider.set_value(stream.volume_pct.clamp(0.0, 150.0));
            slider.set_draw_value(false);
            slider.add_css_class("music-seek");

            let idx_str = stream.index.to_string();
            let pct_s = pct_lbl.clone();
            slider.connect_value_changed(move |sc| {
                let val = sc.value().round() as u32;
                pct_s.set_label(&format!("{}%", val));
                let _ = Command::new("pactl")
                    .args(["set-sink-input-volume", &idx_str, &format!("{}%", val)])
                    .spawn();
            });

            let idx_m = stream.index.to_string();
            let ref_m = refresh_cb.clone();
            mute_btn.connect_clicked(move |_| {
                let _ = Command::new("pactl")
                    .args(["set-sink-input-mute", &idx_m, "toggle"])
                    .status();
                ref_m("sound", false);
            });

            card.append(&top);
            card.append(&slider);
            container.append(&card);
        }
    }
}

fn render_network_page(
    container: &Box,
    status_lbl: &Label,
    snap: &NetworkSnapshot,
    wifi_pw_target: &Rc<RefCell<Option<String>>>,
    refresh_cb: &Rc<dyn Fn(&str, bool)>,
    tx: &mpsc::Sender<SystemMsg>,
) {
    clear_box(container);

    // 1. ACTIVE CONNECTIONS
    let active_hdr = Label::builder()
        .label("ACTIVE CONNECTIONS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    container.append(&active_hdr);

    if snap.active.is_empty() {
        let empty = Label::builder()
            .label("No active network connections.")
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        container.append(&empty);
    } else {
        for conn in &snap.active {
            let card = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .css_classes(vec!["plan-check-row".to_string()])
                .build();
            let col = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(2)
                .hexpand(true)
                .build();
            let name_lbl = Label::builder()
                .label(&conn.name)
                .halign(Align::Start)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .css_classes(vec!["plan-step".to_string()])
                .build();
            let sub_text = if conn.ip.is_empty() {
                format!("{} ({})", conn.kind, conn.device)
            } else {
                format!("{}  •  {}", conn.device, conn.ip)
            };
            let sub = Label::builder()
                .label(&sub_text)
                .halign(Align::Start)
                .css_classes(vec!["plan-subnote".to_string()])
                .build();
            col.append(&name_lbl);
            col.append(&sub);

            let disc_btn = Button::builder()
                .label("Disconnect")
                .css_classes(vec!["plan-close-btn".to_string()])
                .valign(Align::Center)
                .build();
            let cname = conn.name.clone();
            let tx_d = tx.clone();
            let st_d = status_lbl.clone();
            disc_btn.connect_clicked(move |_| {
                st_d.set_label(&format!("Disconnecting {}...", cname));
                st_d.set_visible(true);
                let name_bg = cname.clone();
                let tx_bg = tx_d.clone();
                std::thread::spawn(move || {
                    let ok = Command::new("nmcli")
                        .args(["con", "down", &name_bg])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    let msg = if ok {
                        format!("Disconnected {}", name_bg)
                    } else {
                        format!("Failed to disconnect {}", name_bg)
                    };
                    let _ = tx_bg.send(SystemMsg::NetworkAction(msg));
                });
            });

            card.append(&col);
            card.append(&disc_btn);
            container.append(&card);
        }
    }

    // 2. WI-FI SECTION (if Wi-Fi hardware is present)
    if snap.has_wifi_device {
        let wifi_hdr_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .build();
        let wifi_hdr = Label::builder()
            .label("WI-FI")
            .halign(Align::Start)
            .hexpand(true)
            .css_classes(vec!["fav-group-header".to_string()])
            .build();
        let wifi_toggle = Button::builder()
            .label(if snap.wifi_enabled { "Wi-Fi: ON" } else { "Wi-Fi: OFF" })
            .css_classes(if snap.wifi_enabled {
                vec!["plan-fmt-btn".to_string(), "active".to_string()]
            } else {
                vec!["plan-fmt-btn".to_string()]
            })
            .build();
        let scan_btn = Button::builder()
            .label("Scan")
            .css_classes(vec!["plan-close-btn".to_string()])
            .sensitive(snap.wifi_enabled)
            .build();

        let cur_on = snap.wifi_enabled;
        let tx_t = tx.clone();
        wifi_toggle.connect_clicked(move |_| {
            let next = if cur_on { "off" } else { "on" };
            let tx_bg = tx_t.clone();
            std::thread::spawn(move || {
                let _ = Command::new("nmcli").args(["radio", "wifi", next]).status();
                let _ = tx_bg.send(SystemMsg::NetworkAction(format!(
                    "Wi-Fi turned {}",
                    next.to_uppercase()
                )));
            });
        });

        let ref_s = refresh_cb.clone();
        let st_s = status_lbl.clone();
        scan_btn.connect_clicked(move |_| {
            st_s.set_label("Scanning Wi-Fi networks...");
            st_s.set_visible(true);
            ref_s("network", true);
        });

        wifi_hdr_row.append(&wifi_hdr);
        wifi_hdr_row.append(&wifi_toggle);
        wifi_hdr_row.append(&scan_btn);
        container.append(&wifi_hdr_row);

        // Inline password prompt if user clicked a secured Wi-Fi network
        if let Some(target_ssid) = wifi_pw_target.borrow().clone() {
            let pw_card = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(6)
                .css_classes(vec!["plan-check-row".to_string()])
                .build();
            let pw_lbl = Label::builder()
                .label(&format!("Password for {}:", target_ssid))
                .halign(Align::Start)
                .css_classes(vec!["plan-step".to_string()])
                .build();
            let pw_entry = Entry::builder()
                .placeholder_text("Enter Wi-Fi passphrase...")
                .visibility(false)
                .css_classes(vec!["fav-search".to_string()])
                .build();
            let btn_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(6)
                .build();
            let conn_btn = Button::builder()
                .label("Connect")
                .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
                .hexpand(true)
                .build();
            let cancel_btn = Button::builder()
                .label("Cancel")
                .css_classes(vec!["plan-close-btn".to_string()])
                .build();
            btn_row.append(&conn_btn);
            btn_row.append(&cancel_btn);
            pw_card.append(&pw_lbl);
            pw_card.append(&pw_entry);
            pw_card.append(&btn_row);
            container.append(&pw_card);

            let do_connect = {
                let ssid_c = target_ssid.clone();
                let entry_c = pw_entry.clone();
                let pw_slot = wifi_pw_target.clone();
                let tx_c = tx.clone();
                let st_c = status_lbl.clone();
                Rc::new(move || {
                    let pass = entry_c.text().to_string();
                    *pw_slot.borrow_mut() = None;
                    st_c.set_label(&format!("Connecting to {}...", ssid_c));
                    st_c.set_visible(true);
                    let ssid_bg = ssid_c.clone();
                    let tx_bg = tx_c.clone();
                    std::thread::spawn(move || {
                        let out = Command::new("nmcli")
                            .args(["dev", "wifi", "connect", &ssid_bg, "password", &pass])
                            .output();
                        let msg = match out {
                            Ok(o) if o.status.success() => format!("Connected to {}", ssid_bg),
                            Ok(o) => {
                                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                                format!("Failed: {}", err)
                            }
                            Err(e) => format!("Failed: {}", e),
                        };
                        let _ = tx_bg.send(SystemMsg::NetworkAction(msg));
                    });
                })
            };

            let dc_btn = do_connect.clone();
            conn_btn.connect_clicked(move |_| dc_btn());
            let dc_ent = do_connect.clone();
            pw_entry.connect_activate(move |_| dc_ent());

            let pw_cancel = wifi_pw_target.clone();
            let ref_cancel = refresh_cb.clone();
            cancel_btn.connect_clicked(move |_| {
                *pw_cancel.borrow_mut() = None;
                ref_cancel("network", false);
            });
        }

        if snap.wifi_enabled {
            for net in snap.wifi_list.iter().take(12) {
                let mut classes = vec!["fav-item-btn".to_string()];
                if net.in_use {
                    classes.push("pinned".to_string());
                }
                let btn = Button::builder()
                    .css_classes(classes)
                    .hexpand(true)
                    .build();
                let row = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .spacing(8)
                    .build();
                let lock_str = if net.security.is_empty() || net.security == "--" {
                    ""
                } else {
                    " "
                };
                let lbl = Label::builder()
                    .label(&format!("{}{}", lock_str, net.ssid))
                    .halign(Align::Start)
                    .hexpand(true)
                    .ellipsize(gtk4::pango::EllipsizeMode::End)
                    .css_classes(vec!["plan-body".to_string()])
                    .build();
                let sig = Label::builder()
                    .label(&if net.in_use {
                        format!("{}% (ON)", net.signal)
                    } else {
                        format!("{}%", net.signal)
                    })
                    .css_classes(vec!["fav-score-badge".to_string()])
                    .build();
                row.append(&lbl);
                row.append(&sig);
                btn.set_child(Some(&row));

                let ssid_c = net.ssid.clone();
                let secured = !net.security.is_empty() && net.security != "--";
                let in_use = net.in_use;
                let pw_slot = wifi_pw_target.clone();
                let tx_w = tx.clone();
                let ref_w = refresh_cb.clone();
                let st_w = status_lbl.clone();

                btn.connect_clicked(move |_| {
                    if in_use {
                        return;
                    }
                    // Try connecting with existing saved profile or open network first
                    let has_saved = Command::new("nmcli")
                        .args(["con", "show", &ssid_c])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if has_saved || !secured {
                        st_w.set_label(&format!("Connecting to {}...", ssid_c));
                        st_w.set_visible(true);
                        let ssid_bg = ssid_c.clone();
                        let tx_bg = tx_w.clone();
                        std::thread::spawn(move || {
                            let ok = if has_saved {
                                Command::new("nmcli")
                                    .args(["con", "up", &ssid_bg])
                                    .status()
                                    .map(|s| s.success())
                                    .unwrap_or(false)
                            } else {
                                Command::new("nmcli")
                                    .args(["dev", "wifi", "connect", &ssid_bg])
                                    .status()
                                    .map(|s| s.success())
                                    .unwrap_or(false)
                            };
                            let msg = if ok {
                                format!("Connected to {}", ssid_bg)
                            } else {
                                format!("Failed to connect to {}", ssid_bg)
                            };
                            let _ = tx_bg.send(SystemMsg::NetworkAction(msg));
                        });
                    } else {
                        *pw_slot.borrow_mut() = Some(ssid_c.clone());
                        ref_w("network", false);
                    }
                });
                container.append(&btn);
            }
        }
    }

    // 3. SAVED PROFILES / VPNS
    if !snap.saved_inactive.is_empty() {
        let saved_hdr = Label::builder()
            .label("SAVED PROFILES & VPNS")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(6)
            .build();
        container.append(&saved_hdr);

        for conn in snap.saved_inactive.iter().take(10) {
            let btn = Button::builder()
                .css_classes(vec!["fav-item-btn".to_string()])
                .hexpand(true)
                .build();
            let row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .build();
            let lbl = Label::builder()
                .label(&conn.name)
                .halign(Align::Start)
                .hexpand(true)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .css_classes(vec!["plan-body".to_string()])
                .build();
            let badge = Label::builder()
                .label(&format!("{} ->", conn.kind))
                .css_classes(vec!["fav-score-badge".to_string()])
                .build();
            row.append(&lbl);
            row.append(&badge);
            btn.set_child(Some(&row));

            let cname = conn.name.clone();
            let tx_c = tx.clone();
            let st_c = status_lbl.clone();
            btn.connect_clicked(move |_| {
                st_c.set_label(&format!("Connecting {}...", cname));
                st_c.set_visible(true);
                let name_bg = cname.clone();
                let tx_bg = tx_c.clone();
                std::thread::spawn(move || {
                    let ok = Command::new("nmcli")
                        .args(["con", "up", &name_bg])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    let msg = if ok {
                        format!("Connected {}", name_bg)
                    } else {
                        format!("Failed to connect {}", name_bg)
                    };
                    let _ = tx_bg.send(SystemMsg::NetworkAction(msg));
                });
            });
            container.append(&btn);
        }
    }
}

fn render_themes_page(
    container: &Box,
    status_lbl: &Label,
    themes: &[ThemeEntry],
    tx: &mpsc::Sender<SystemMsg>,
    reload_theme: &Rc<dyn Fn()>,
) {
    clear_box(container);
    let cur_cfg = crate::config::get();

    // 1. External theme-switch palettes (if present)
    if !themes.is_empty() {
        let hdr = Label::builder()
            .label("EXTERNAL PALETTES")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .build();
        container.append(&hdr);
    }

    for theme in themes {
        let mut classes = vec!["fav-item-btn".to_string()];
        if theme.is_active {
            classes.push("pinned".to_string());
        }
        let btn = Button::builder()
            .css_classes(classes)
            .hexpand(true)
            .build();

        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        let swatches = theme.swatches;
        let swatch_area = DrawingArea::builder()
            .content_width(64)
            .content_height(18)
            .valign(Align::Center)
            .build();
        swatch_area.set_draw_func(move |_, cr, _, h| {
            let cy = h as f64 / 2.0;
            let r = 6.5;
            for (i, &(red, green, blue)) in swatches.iter().enumerate() {
                let cx = 8.0 + (i as f64 * 16.0);
                cr.set_source_rgb(red, green, blue);
                cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
                let _ = cr.fill_preserve();
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
                cr.set_line_width(1.0);
                let _ = cr.stroke();
            }
        });

        let col = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(1)
            .hexpand(true)
            .build();
        let name_lbl = Label::builder()
            .label(&theme.name)
            .halign(Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-body".to_string()])
            .build();
        let mode_lbl = Label::builder()
            .label(&theme.mode.to_uppercase())
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        col.append(&name_lbl);
        col.append(&mode_lbl);

        let badge = Label::builder()
            .label(if theme.is_active { "ACTIVE" } else { "Apply" })
            .css_classes(vec!["fav-score-badge".to_string()])
            .valign(Align::Center)
            .build();

        row.append(&swatch_area);
        row.append(&col);
        row.append(&badge);
        btn.set_child(Some(&row));

        let key = theme.key.clone();
        let display_name = theme.name.clone();
        let tx_c = tx.clone();
        let st_c = status_lbl.clone();
        let rel_c = reload_theme.clone();
        btn.connect_clicked(move |_| {
            crate::config::update(|c| c.theme_preset = "auto".to_string());
            st_c.set_label(&format!("Applying {}...", display_name));
            st_c.set_visible(true);
            let k = key.clone();
            let dn = display_name.clone();
            let tx_bg = tx_c.clone();
            let rel_after = rel_c.clone();
            std::thread::spawn(move || {
                let bin = if std::path::Path::new("/usr/local/bin/theme-switch").exists() {
                    "/usr/local/bin/theme-switch"
                } else {
                    "theme-switch"
                };
                let ok = Command::new(bin)
                    .arg(&k)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let msg = if ok {
                    format!("Applied {}", dn)
                } else {
                    format!("Failed to apply {}", dn)
                };
                let _ = tx_bg.send(SystemMsg::ThemeApplied(msg));
            });
            glib::timeout_add_local_once(Duration::from_millis(200), move || rel_after());
        });

        container.append(&btn);
    }

    // 2. Built-in fallback presets (works on any machine without external theme scripts)
    let bi_hdr = Label::builder()
        .label("BUILT-IN PRESETS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(if themes.is_empty() { 0 } else { 8 })
        .build();
    container.append(&bi_hdr);

    for preset in crate::config::BUILTIN_THEMES {
        let is_active = cur_cfg.theme_preset == preset.key;
        let mut classes = vec!["fav-item-btn".to_string()];
        if is_active {
            classes.push("pinned".to_string());
        }
        let btn = Button::builder()
            .css_classes(classes)
            .hexpand(true)
            .build();

        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        let swatches = [
            parse_hex_rgb(preset.bg_main),
            parse_hex_rgb(preset.accent),
            parse_hex_rgb(preset.selection),
            parse_hex_rgb(preset.magenta),
        ];
        let swatch_area = DrawingArea::builder()
            .content_width(64)
            .content_height(18)
            .valign(Align::Center)
            .build();
        swatch_area.set_draw_func(move |_, cr, _, h| {
            let cy = h as f64 / 2.0;
            let r = 6.5;
            for (i, &(red, green, blue)) in swatches.iter().enumerate() {
                let cx = 8.0 + (i as f64 * 16.0);
                cr.set_source_rgb(red, green, blue);
                cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
                let _ = cr.fill_preserve();
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
                cr.set_line_width(1.0);
                let _ = cr.stroke();
            }
        });

        let col = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(1)
            .hexpand(true)
            .build();
        let name_lbl = Label::builder()
            .label(preset.name)
            .halign(Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-body".to_string()])
            .build();
        let mode_lbl = Label::builder()
            .label(&format!("{} • BUILT-IN", preset.mode.to_uppercase()))
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        col.append(&name_lbl);
        col.append(&mode_lbl);

        let badge = Label::builder()
            .label(if is_active { "ACTIVE" } else { "Use" })
            .css_classes(vec!["fav-score-badge".to_string()])
            .valign(Align::Center)
            .build();

        row.append(&swatch_area);
        row.append(&col);
        row.append(&badge);
        btn.set_child(Some(&row));

        let key = preset.key.to_string();
        let name = preset.name.to_string();
        let tx_c = tx.clone();
        let rel_c = reload_theme.clone();
        btn.connect_clicked(move |_| {
            crate::config::update(|c| c.theme_preset = key.clone());
            rel_c();
            let _ = tx_c.send(SystemMsg::ThemeApplied(format!("Using built-in {}", name)));
        });

        container.append(&btn);
    }
}

fn render_wallpapers_page(
    container: &Box,
    status_lbl: &Label,
    walls: &[WallpaperEntry],
    thumb_cache: &Rc<RefCell<std::collections::HashMap<String, gtk4::gdk::Texture>>>,
    img_slots: &Rc<RefCell<std::collections::HashMap<String, Picture>>>,
    tx: &mpsc::Sender<SystemMsg>,
) {
    clear_box(container);
    img_slots.borrow_mut().clear();

    let top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    let hdr = Label::builder()
        .label(&format!("WALLPAPERS ({})", walls.len()))
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    let rand_btn = Button::builder()
        .label("Random")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .sensitive(!walls.is_empty())
        .build();
    top_row.append(&hdr);
    top_row.append(&rand_btn);
    container.append(&top_row);

    if walls.is_empty() {
        let empty = Label::builder()
            .label("No wallpapers found in ~/Pictures/Wallpapers")
            .halign(Align::Start)
            .wrap(true)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        container.append(&empty);
        return;
    }

    // Random wallpaper button
    {
        let all_paths: Vec<String> = walls.iter().map(|w| w.path.clone()).collect();
        let tx_r = tx.clone();
        let st_r = status_lbl.clone();
        rand_btn.connect_clicked(move |_| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as usize)
                .unwrap_or(0);
            let chosen = all_paths[nanos % all_paths.len()].clone();
            st_r.set_label("Applying random wallpaper...");
            st_r.set_visible(true);
            let tx_bg = tx_r.clone();
            std::thread::spawn(move || {
                let msg = match apply_wallpaper_native(&chosen) {
                    Ok(m) => m,
                    Err(e) => e,
                };
                let _ = tx_bg.send(SystemMsg::WallpaperApplied(msg));
            });
        });
    }

    let grid = FlowBox::builder()
        .selection_mode(SelectionMode::None)
        .homogeneous(true)
        .max_children_per_line(2)
        .min_children_per_line(2)
        .column_spacing(8)
        .row_spacing(8)
        .build();

    let mut to_decode: Vec<String> = Vec::new();

    for wall in walls {
        let mut classes = vec!["wall-tile-btn".to_string()];
        if wall.is_active {
            classes.push("active".to_string());
        }
        let btn = Button::builder()
            .css_classes(classes)
            .tooltip_text(&if wall.has_vertical_pair {
                format!("{} (paired vertical crop available)", wall.name)
            } else {
                wall.name.clone()
            })
            .build();

        let card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .build();

        let frame = Box::builder()
            .width_request(166)
            .height_request(94)
            .overflow(Overflow::Hidden)
            .css_classes(vec!["media-art-box".to_string()])
            .build();

        let img = Picture::builder()
            .can_shrink(true)
            .width_request(166)
            .height_request(94)
            .hexpand(true)
            .vexpand(true)
            .build();

        if let Some(tex) = thumb_cache.borrow().get(&wall.path) {
            img.set_paintable(Some(tex));
        } else {
            img_slots.borrow_mut().insert(wall.path.clone(), img.clone());
            to_decode.push(wall.path.clone());
        }
        frame.append(&img);

        let meta_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();
        let name_lbl = Label::builder()
            .label(&wall.name)
            .halign(Align::Start)
            .hexpand(true)
            .width_chars(1)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        meta_row.append(&name_lbl);

        if wall.is_active {
            let act = Label::builder()
                .label("ON")
                .css_classes(vec!["fav-score-badge".to_string()])
                .build();
            meta_row.append(&act);
        } else if wall.has_vertical_pair {
            let vtag = Label::builder()
                .label("V")
                .css_classes(vec!["fav-score-badge".to_string()])
                .build();
            meta_row.append(&vtag);
        }

        card.append(&frame);
        card.append(&meta_row);
        btn.set_child(Some(&card));

        let path_c = wall.path.clone();
        let name_c = wall.name.clone();
        let tx_c = tx.clone();
        let st_c = status_lbl.clone();
        btn.connect_clicked(move |_| {
            st_c.set_label(&format!("Applying {}...", name_c));
            st_c.set_visible(true);
            let p = path_c.clone();
            let tx_bg = tx_c.clone();
            std::thread::spawn(move || {
                let msg = match apply_wallpaper_native(&p) {
                    Ok(m) => m,
                    Err(e) => e,
                };
                let _ = tx_bg.send(SystemMsg::WallpaperApplied(msg));
            });
        });

        grid.insert(&btn, -1);
    }

    container.append(&grid);

    // Decode uncached thumbnails sequentially on one background worker thread (pure RAM, zero SSD cache!)
    if !to_decode.is_empty() {
        let tx_th = tx.clone();
        std::thread::spawn(move || {
            for p in to_decode {
                if let Some((bytes, has_alpha, rowstride, width, height)) =
                    decode_wallpaper_thumb(&p)
                {
                    let _ = tx_th.send(SystemMsg::WallpaperThumb {
                        path: p,
                        bytes,
                        has_alpha,
                        rowstride,
                        width,
                        height,
                    });
                }
            }
        });
    }
}

fn render_displays_page(
    container: &Box,
    monitors: &[MonitorInfo],
    refresh_cb: &Rc<dyn Fn(&str, bool)>,
) {
    clear_box(container);

    let hdr = Label::builder()
        .label("CONNECTED MONITORS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    container.append(&hdr);

    if monitors.is_empty() {
        let empty = Label::builder()
            .label("Could not query Hyprland monitors.")
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        container.append(&empty);
    } else {
        for m in monitors {
            let card = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .css_classes(vec!["plan-check-row".to_string()])
                .build();
            let col = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(2)
                .hexpand(true)
                .build();
            let title_str = if m.focused {
                format!("{}  (Focused)", m.name)
            } else {
                m.name.clone()
            };
            let title = Label::builder()
                .label(&title_str)
                .halign(Align::Start)
                .css_classes(vec!["plan-step".to_string()])
                .build();
            let spec = Label::builder()
                .label(&format!(
                    "{}x{} @ {:.0}Hz  •  Scale {:.2}  •  WS {}",
                    m.width, m.height, m.refresh, m.scale, m.workspace
                ))
                .halign(Align::Start)
                .css_classes(vec!["plan-subnote".to_string()])
                .build();
            col.append(&title);
            col.append(&spec);

            let focus_btn = Button::builder()
                .label("Focus")
                .css_classes(if m.focused {
                    vec!["plan-fmt-btn".to_string(), "active".to_string()]
                } else {
                    vec!["plan-fmt-btn".to_string()]
                })
                .valign(Align::Center)
                .build();
            let mname = m.name.clone();
            let ref_c = refresh_cb.clone();
            focus_btn.connect_clicked(move |_| {
                crate::config::hypr_focus_monitor(&mname);
                ref_c("displays", false);
            });

            card.append(&col);
            card.append(&focus_btn);
            container.append(&card);
        }
    }

    let actions_hdr = Label::builder()
        .label("HYPRLAND ACTIONS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&actions_hdr);

    let reload_hypr_btn = Button::builder()
        .label("Reload Hyprland Config")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .build();
    let ref_h = refresh_cb.clone();
    reload_hypr_btn.connect_clicked(move |_| {
        let _ = Command::new("hyprctl").arg("reload").spawn();
        ref_h("displays", false);
    });
    container.append(&reload_hypr_btn);
}

fn render_keybinds_page(container: &Box, binds: &[KeybindEntry], query: &str) {
    clear_box(container);
    let q = query.trim().to_lowercase();

    let filtered: Vec<&KeybindEntry> = binds
        .iter()
        .filter(|b| {
            if q.is_empty() {
                return true;
            }
            b.keys.to_lowercase().contains(&q)
                || b.desc.to_lowercase().contains(&q)
                || b.group.to_lowercase().contains(&q)
                || b.raw.to_lowercase().contains(&q)
        })
        .collect();

    if filtered.is_empty() {
        let empty = Label::builder()
            .label(if binds.is_empty() {
                "Could not read ~/.config/hypr/binds.lua"
            } else {
                "No matching keybinds."
            })
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        container.append(&empty);
        return;
    }

    let group_order = [
        "APPS & LAUNCHERS",
        "CAPTURE & SCREENSHOTS",
        "WINDOW MANAGEMENT",
        "WORKSPACES & SCRATCHPADS",
        "MEDIA & HARDWARE",
        "OTHER",
    ];

    for group_name in group_order {
        let items: Vec<&&KeybindEntry> = filtered
            .iter()
            .filter(|b| b.group == group_name)
            .collect();
        if items.is_empty() {
            continue;
        }

        let hdr = Label::builder()
            .label(group_name)
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(4)
            .build();
        container.append(&hdr);

        let card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(6)
            .css_classes(vec!["plan-check-row".to_string()])
            .build();

        for b in items {
            let row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .tooltip_text(&b.raw)
                .build();

            let key_pill = Label::builder()
                .label(&b.keys)
                .halign(Align::Start)
                .valign(Align::Center)
                .css_classes(vec!["kbd-pill".to_string()])
                .build();

            let desc_lbl = Label::builder()
                .label(&b.desc)
                .halign(Align::Start)
                .valign(Align::Center)
                .hexpand(true)
                .wrap(true)
                .xalign(0.0)
                .css_classes(vec!["plan-body".to_string()])
                .build();

            row.append(&key_pill);
            row.append(&desc_lbl);
            card.append(&row);
        }

        container.append(&card);
    }
}

fn render_overview_page(
    container: &Box,
    snap: &OverviewSnapshot,
    suspend_armed: &Rc<Cell<bool>>,
    refresh_cb: &Rc<dyn Fn(&str, bool)>,
    close_drawer: &Rc<dyn Fn()>,
    tx: &mpsc::Sender<SystemMsg>,
) {
    clear_box(container);

    // 1. LIVE METRICS CARD
    let sys_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .css_classes(vec!["plan-check-row".to_string()])
        .build();

    let cpu_row = Box::builder().orientation(Orientation::Horizontal).build();
    let cpu_k = Label::builder()
        .label("CPU Usage")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["plan-step".to_string()])
        .build();
    let cpu_v = Label::builder()
        .label(&format!("{:.0}%", snap.cpu_pct))
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    cpu_row.append(&cpu_k);
    cpu_row.append(&cpu_v);
    let cpu_bar = ProgressBar::builder()
        .fraction((snap.cpu_pct / 100.0).clamp(0.0, 1.0))
        .css_classes(vec!["drive-progress".to_string()])
        .build();

    let mem_row = Box::builder().orientation(Orientation::Horizontal).build();
    let mem_k = Label::builder()
        .label("Memory")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["plan-step".to_string()])
        .build();
    let mem_v = Label::builder()
        .label(&format!("{:.1} / {:.1} GiB", snap.mem_used_gb, snap.mem_total_gb))
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    mem_row.append(&mem_k);
    mem_row.append(&mem_v);
    let mem_bar = ProgressBar::builder()
        .fraction(snap.mem_frac)
        .css_classes(vec!["drive-progress".to_string()])
        .build();

    sys_card.append(&cpu_row);
    sys_card.append(&cpu_bar);
    sys_card.append(&mem_row);
    sys_card.append(&mem_bar);

    if !snap.gpu_name.is_empty() {
        let gpu_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .margin_top(2)
            .build();
        let gpu_k = Label::builder()
            .label(&snap.gpu_name)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-step".to_string()])
            .build();
        let gpu_v_str = format!(
            "{}%  •  {}°C",
            snap.gpu_util.unwrap_or(0),
            snap.gpu_temp.unwrap_or(0)
        );
        let gpu_v = Label::builder()
            .label(&gpu_v_str)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        gpu_row.append(&gpu_k);
        gpu_row.append(&gpu_v);
        sys_card.append(&gpu_row);

        if let (Some(used), Some(total)) = (snap.gpu_vram_used, snap.gpu_vram_total) {
            if total > 0 {
                let vram_lbl = Label::builder()
                    .label(&format!("VRAM: {} / {} MiB", used, total))
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build();
                let vram_bar = ProgressBar::builder()
                    .fraction((used as f64 / total as f64).clamp(0.0, 1.0))
                    .css_classes(vec!["drive-progress".to_string()])
                    .build();
                sys_card.append(&vram_lbl);
                sys_card.append(&vram_bar);
            }
        }
    }

    if !snap.uptime.is_empty() {
        let up_lbl = Label::builder()
            .label(&format!("Uptime: {}", snap.uptime))
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .margin_top(2)
            .build();
        sys_card.append(&up_lbl);
    }

    container.append(&sys_card);

    // 2. CAPTURE & COLOR PICKER
    let cap_hdr = Label::builder()
        .label("CAPTURE & COLOR PICKER")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&cap_hdr);

    let cap_row1 = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();

    let cap_region_btn = Button::builder()
        .label("󰩭 Region")
        .css_classes(vec!["primary-btn".to_string()])
        .tooltip_text("Capture region / editor (lumine-capture)")
        .build();
    {
        let close_c = close_drawer.clone();
        cap_region_btn.connect_clicked(move |_| {
            launch_capture(CaptureMode::RegionEditor, &close_c);
        });
    }

    let cap_mon_btn = Button::builder()
        .label("󰍹 Monitor")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Capture active monitor (lumine-capture -m)")
        .build();
    {
        let close_c = close_drawer.clone();
        cap_mon_btn.connect_clicked(move |_| {
            launch_capture(CaptureMode::Monitor, &close_c);
        });
    }

    let cap_full_btn = Button::builder()
        .label("󰹑 Full")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Capture all monitors (lumine-capture -f)")
        .build();
    {
        let close_c = close_drawer.clone();
        cap_full_btn.connect_clicked(move |_| {
            launch_capture(CaptureMode::Full, &close_c);
        });
    }

    cap_row1.append(&cap_region_btn);
    cap_row1.append(&cap_mon_btn);
    cap_row1.append(&cap_full_btn);
    container.append(&cap_row1);

    let cap_row2 = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();

    let cap_ocr_btn = Button::builder()
        .label("󰗊 Region OCR")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Drag region to OCR text straight to clipboard")
        .build();
    {
        let close_c = close_drawer.clone();
        cap_ocr_btn.connect_clicked(move |_| {
            launch_capture(CaptureMode::RegionOcr, &close_c);
        });
    }

    let pick_color_btn = Button::builder()
        .label("󰈊 Pick Color")
        .css_classes(vec!["primary-btn".to_string()])
        .tooltip_text("Pick any screen pixel and copy #hex to clipboard")
        .build();
    {
        let close_c = close_drawer.clone();
        let tx_c = tx.clone();
        pick_color_btn.connect_clicked(move |_| {
            launch_color_picker_inner(&close_c, Some(tx_c.clone()));
        });
    }

    cap_row2.append(&cap_ocr_btn);
    cap_row2.append(&pick_color_btn);
    container.append(&cap_row2);

    if let Ok(guard) = LAST_PICKED_COLOR.lock() {
        if let Some((r, g, b)) = *guard {
            let hex = format!("#{:02x}{:02x}{:02x}", r, g, b);
            let rgb = format!("rgb({}, {}, {})", r, g, b);
            let swatch_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .css_classes(vec!["plan-check-row".to_string()])
                .build();

            let swatch = DrawingArea::builder()
                .content_width(28)
                .content_height(28)
                .valign(Align::Center)
                .build();
            let (rf, gf, bf) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
            swatch.set_draw_func(move |_, cr, w, h| {
                let rad = 7.0;
                let wf = w as f64;
                let hf = h as f64;
                cr.new_sub_path();
                cr.arc(wf - rad, rad, rad, -std::f64::consts::FRAC_PI_2, 0.0);
                cr.arc(wf - rad, hf - rad, rad, 0.0, std::f64::consts::FRAC_PI_2);
                cr.arc(rad, hf - rad, rad, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
                cr.arc(rad, rad, rad, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
                cr.close_path();
                cr.set_source_rgb(rf, gf, bf);
                let _ = cr.fill();
            });

            let hex_btn = Button::builder()
                .label(&hex)
                .css_classes(vec!["plan-fmt-btn".to_string()])
                .tooltip_text("Click to copy hex")
                .hexpand(true)
                .build();
            let hex_copy = hex.clone();
            hex_btn.connect_clicked(move |_| {
                let _ = Command::new("wl-copy").args(["--", &hex_copy]).spawn();
            });

            let rgb_btn = Button::builder()
                .label(&rgb)
                .css_classes(vec!["plan-fmt-btn".to_string()])
                .tooltip_text("Click to copy RGB")
                .hexpand(true)
                .build();
            let rgb_copy = rgb.clone();
            rgb_btn.connect_clicked(move |_| {
                let _ = Command::new("wl-copy").args(["--", &rgb_copy]).spawn();
            });

            swatch_row.append(&swatch);
            swatch_row.append(&hex_btn);
            swatch_row.append(&rgb_btn);
            container.append(&swatch_row);
        }
    }

    // 3. QUICK CONTROLS
    let ctrl_hdr = Label::builder()
        .label("QUICK CONTROLS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&ctrl_hdr);

    let row1 = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .homogeneous(true)
        .build();

    let dnd_on = snap.dnd_enabled.unwrap_or(false);
    let dnd_btn = Button::builder()
        .label(if dnd_on { "DND: ON" } else { "DND: OFF" })
        .css_classes(if dnd_on {
            vec!["plan-fmt-btn".to_string(), "active".to_string()]
        } else {
            vec!["plan-fmt-btn".to_string()]
        })
        .build();
    let ref_dnd = refresh_cb.clone();
    dnd_btn.connect_clicked(move |_| {
        match crate::config::get().tools.notification_daemon.as_str() {
            "swaync" => {
                let _ = Command::new("swaync-client").arg("-d").status();
            }
            "dunst" => {
                let _ = Command::new("dunstctl").args(["set-paused", "toggle"]).status();
            }
            "mako" => {
                let _ = Command::new("makoctl").args(["mode", "-t", "do-not-disturb"]).status();
            }
            _ => {}
        }
        ref_dnd("overview", false);
    });

    let notif_btn = Button::builder()
        .label("Notifications")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    notif_btn.connect_clicked(move |_| {
        match crate::config::get().tools.notification_daemon.as_str() {
            "swaync" => {
                let _ = Command::new("swaync-client").arg("-t").spawn();
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

    row1.append(&dnd_btn);
    row1.append(&notif_btn);
    container.append(&row1);

    let row2 = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .homogeneous(true)
        .build();

    let is_susp_armed = suspend_armed.get();
    let suspend_btn = Button::builder()
        .label(if is_susp_armed { "Sure? Suspend" } else { "󰤄 Suspend" })
        .css_classes(if is_susp_armed {
            vec!["plan-fmt-btn".to_string(), "active".to_string()]
        } else {
            vec!["plan-fmt-btn".to_string()]
        })
        .build();
    let susp_arm = suspend_armed.clone();
    let ref_susp = refresh_cb.clone();
    suspend_btn.connect_clicked(move |btn| {
        if susp_arm.get() {
            susp_arm.set(false);
            let _ = Command::new("systemctl").arg("suspend").spawn();
            return;
        }
        susp_arm.set(true);
        btn.set_label("Sure? Suspend");
        btn.add_css_class("active");
        let arm_r = susp_arm.clone();
        let ref_r = ref_susp.clone();
        glib::timeout_add_local_once(Duration::from_secs(3), move || {
            if arm_r.get() {
                arm_r.set(false);
                ref_r("overview", false);
            }
        });
    });

    let hypr_reload_btn = Button::builder()
        .label("󰑓 Reload Hyprland")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    hypr_reload_btn.connect_clicked(move |_| {
        let _ = Command::new("hyprctl").arg("reload").spawn();
    });

    row2.append(&suspend_btn);
    row2.append(&hypr_reload_btn);
    container.append(&row2);
}

// ==========================================
// SETTINGS PAGE (HYPRLAND, APPS, PATHS, TABS)
// ==========================================

fn render_settings_page(
    container: &Box,
    reload_theme: &Rc<dyn Fn()>,
    apply_tabs: &Rc<dyn Fn()>,
) {
    clear_box(container);
    let cfg = crate::config::get();

    // Debounced disk flush so dragging sliders NEVER spams SSD writes
    let flush_gen = Rc::new(Cell::new(0u64));
    let schedule_flush: Rc<dyn Fn()> = {
        let fg = flush_gen.clone();
        Rc::new(move || {
            let next = fg.get().wrapping_add(1);
            fg.set(next);
            let fg_chk = fg.clone();
            glib::timeout_add_local_once(Duration::from_millis(450), move || {
                if fg_chk.get() == next {
                    crate::config::flush_to_disk();
                }
            });
        })
    };

    // Helper: create a titled slider card row
    let make_slider_row = |title: &str,
                           min: f64,
                           max: f64,
                           step: f64,
                           initial: f64,
                           val_text: String|
     -> (Box, Scale, Label) {
        let wrap = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(3)
            .build();
        let hdr = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();
        let title_lbl = Label::builder()
            .label(title)
            .halign(Align::Start)
            .hexpand(true)
            .css_classes(vec!["plan-body".to_string()])
            .build();
        let val_lbl = Label::builder()
            .label(&val_text)
            .halign(Align::End)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        hdr.append(&title_lbl);
        hdr.append(&val_lbl);

        let scale = Scale::with_range(Orientation::Horizontal, min, max, step);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_value(initial.clamp(min, max));
        scale.set_css_classes(&["music-seek"]);

        wrap.append(&hdr);
        wrap.append(&scale);
        (wrap, scale, val_lbl)
    };

    // ------------------------------------------
    // 1. CONFIG FILE BAR
    // ------------------------------------------
    let cfg_path_str = crate::config::config_path().to_string_lossy().to_string();
    let cfg_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["plan-check-row".to_string()])
        .build();
    let cfg_path_lbl = Label::builder()
        .label(&cfg_path_str)
        .halign(Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(vec!["plan-subnote".to_string()])
        .tooltip_text(&cfg_path_str)
        .build();
    let cfg_btn_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .build();
    let open_cfg_btn = Button::builder()
        .label("Open config.json")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    {
        let p = cfg_path_str.clone();
        open_cfg_btn.connect_clicked(move |_| {
            let _ = Command::new("xdg-open").arg(&p).spawn();
        });
    }
    let reload_cfg_btn = Button::builder()
        .label("Reload File")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        reload_cfg_btn.connect_clicked(move |_| {
            let fresh = crate::config::reload_from_disk();
            crate::config::apply_hyprland_settings(&fresh.hyprland);
            rel_t();
            app_t();
            render_settings_page(&c_box, &rel_t, &app_t);
        });
    }
    cfg_btn_row.append(&open_cfg_btn);
    cfg_btn_row.append(&reload_cfg_btn);
    cfg_card.append(&cfg_path_lbl);
    cfg_card.append(&cfg_btn_row);
    container.append(&cfg_card);

    // ------------------------------------------
    // 2. HYPRLAND & APPEARANCE (LIVE)
    // ------------------------------------------
    let hypr_hdr = Label::builder()
        .label("HYPRLAND & APPEARANCE")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&hypr_hdr);

    let hypr_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .css_classes(vec!["plan-check-row".to_string()])
        .build();

    // 2a. Mathematical Corner Rounding slider (0% -> 100% = current Hyprland & Dashboard max)
    let init_r_pct = (cfg.hyprland.rounding_scale * 100.0).round();
    let max_hypr_r = cfg.hyprland.hypr_max_rounding.max(1);
    let init_hypr_px = crate::config::hypr_rounding_px(&cfg.hyprland);
    let init_dash_px = (16.0 * cfg.hyprland.rounding_scale).round() as i32;
    let (round_row, round_scale, round_val_lbl) = make_slider_row(
        "Corner Rounding",
        0.0,
        100.0,
        1.0,
        init_r_pct,
        format!("{}%  (H {}px · D {}px)", init_r_pct as i32, init_hypr_px, init_dash_px),
    );
    {
        let lbl = round_val_lbl.clone();
        let rel_t = reload_theme.clone();
        let flush = schedule_flush.clone();
        round_scale.connect_value_changed(move |sc| {
            let pct = sc.value().round().clamp(0.0, 100.0);
            let t = pct / 100.0;
            let updated = crate::config::update_in_memory(|c| {
                c.hyprland.rounding_scale = t;
            });
            let hypr_px = crate::config::hypr_rounding_px(&updated.hyprland);
            let dash_px = (16.0 * t).round() as i32;
            lbl.set_label(&format!("{}%  (H {}px · D {}px)", pct as i32, hypr_px, dash_px));
            crate::config::hypr_set_keyword("decoration:rounding", &hypr_px.to_string());
            rel_t();
            flush();
        });
    }
    hypr_card.append(&round_row);

    // 2b. Dashboard Glass Opacity
    let init_glass_pct = (cfg.hyprland.glass_opacity * 100.0).round();
    let (glass_row, glass_scale, glass_val_lbl) = make_slider_row(
        "Dashboard Glass Opacity",
        35.0,
        100.0,
        1.0,
        init_glass_pct,
        format!("{}%", init_glass_pct as i32),
    );
    {
        let lbl = glass_val_lbl.clone();
        let rel_t = reload_theme.clone();
        let flush = schedule_flush.clone();
        glass_scale.connect_value_changed(move |sc| {
            let pct = sc.value().round().clamp(35.0, 100.0);
            lbl.set_label(&format!("{}%", pct as i32));
            crate::config::update_in_memory(|c| {
                c.hyprland.glass_opacity = pct / 100.0;
            });
            rel_t();
            flush();
        });
    }
    hypr_card.append(&glass_row);

    // Query live Hyprland options as defaults when unset in config
    let live_gaps_in = cfg
        .hyprland
        .gaps_in
        .or_else(|| crate::config::hypr_get_option_i32("general:gaps_in"))
        .unwrap_or(8);
    let live_gaps_out = cfg
        .hyprland
        .gaps_out
        .or_else(|| crate::config::hypr_get_option_i32("general:gaps_out"))
        .unwrap_or(10);
    let live_border = cfg
        .hyprland
        .border_size
        .or_else(|| crate::config::hypr_get_option_i32("general:border_size"))
        .unwrap_or(2);
    let live_blur_on = cfg
        .hyprland
        .blur_enabled
        .or_else(|| crate::config::hypr_get_option_bool("decoration:blur:enabled"))
        .unwrap_or(true);
    let live_blur_size = cfg
        .hyprland
        .blur_size
        .or_else(|| crate::config::hypr_get_option_i32("decoration:blur:size"))
        .unwrap_or(6);
    let live_blur_passes = cfg
        .hyprland
        .blur_passes
        .or_else(|| crate::config::hypr_get_option_i32("decoration:blur:passes"))
        .unwrap_or(2);
    let live_active_op = cfg
        .hyprland
        .active_opacity
        .or_else(|| crate::config::hypr_get_option_f64("decoration:active_opacity"))
        .unwrap_or(0.95);
    let live_inactive_op = cfg
        .hyprland
        .inactive_opacity
        .or_else(|| crate::config::hypr_get_option_f64("decoration:inactive_opacity"))
        .unwrap_or(0.80);

    // 2c. Gaps In / Gaps Out / Border Size
    let (gi_row, gi_scale, gi_lbl) = make_slider_row(
        "Gaps In",
        0.0,
        32.0,
        1.0,
        live_gaps_in as f64,
        format!("{}px", live_gaps_in),
    );
    {
        let lbl = gi_lbl.clone();
        let flush = schedule_flush.clone();
        gi_scale.connect_value_changed(move |sc| {
            let v = sc.value().round() as i32;
            lbl.set_label(&format!("{}px", v));
            crate::config::update_in_memory(|c| c.hyprland.gaps_in = Some(v));
            crate::config::hypr_set_keyword("general:gaps_in", &v.to_string());
            flush();
        });
    }
    hypr_card.append(&gi_row);

    let (go_row, go_scale, go_lbl) = make_slider_row(
        "Gaps Out",
        0.0,
        48.0,
        1.0,
        live_gaps_out as f64,
        format!("{}px", live_gaps_out),
    );
    {
        let lbl = go_lbl.clone();
        let flush = schedule_flush.clone();
        go_scale.connect_value_changed(move |sc| {
            let v = sc.value().round() as i32;
            lbl.set_label(&format!("{}px", v));
            crate::config::update_in_memory(|c| c.hyprland.gaps_out = Some(v));
            crate::config::hypr_set_keyword("general:gaps_out", &v.to_string());
            flush();
        });
    }
    hypr_card.append(&go_row);

    let (bs_row, bs_scale, bs_lbl) = make_slider_row(
        "Border Size",
        0.0,
        6.0,
        1.0,
        live_border as f64,
        format!("{}px", live_border),
    );
    {
        let lbl = bs_lbl.clone();
        let flush = schedule_flush.clone();
        bs_scale.connect_value_changed(move |sc| {
            let v = sc.value().round() as i32;
            lbl.set_label(&format!("{}px", v));
            crate::config::update_in_memory(|c| c.hyprland.border_size = Some(v));
            crate::config::hypr_set_keyword("general:border_size", &v.to_string());
            flush();
        });
    }
    hypr_card.append(&bs_row);

    // 2d. Blur toggle + Size + Passes
    let blur_hdr_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();
    let blur_title = Label::builder()
        .label("Hyprland Blur")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["plan-body".to_string()])
        .build();
    let blur_state = Rc::new(Cell::new(live_blur_on));
    let blur_btn = Button::builder()
        .label(if live_blur_on { "ON" } else { "OFF" })
        .css_classes(if live_blur_on {
            vec!["plan-fmt-btn".to_string(), "active".to_string()]
        } else {
            vec!["plan-fmt-btn".to_string()]
        })
        .build();
    {
        let st = blur_state.clone();
        blur_btn.connect_clicked(move |b| {
            let next = !st.get();
            st.set(next);
            b.set_label(if next { "ON" } else { "OFF" });
            if next {
                b.add_css_class("active");
            } else {
                b.remove_css_class("active");
            }
            crate::config::update(|c| c.hyprland.blur_enabled = Some(next));
            crate::config::hypr_set_keyword(
                "decoration:blur:enabled",
                if next { "true" } else { "false" },
            );
        });
    }
    blur_hdr_row.append(&blur_title);
    blur_hdr_row.append(&blur_btn);
    hypr_card.append(&blur_hdr_row);

    let (bsize_row, bsize_scale, bsize_lbl) = make_slider_row(
        "Blur Size",
        1.0,
        20.0,
        1.0,
        live_blur_size as f64,
        format!("{}", live_blur_size),
    );
    {
        let lbl = bsize_lbl.clone();
        let flush = schedule_flush.clone();
        bsize_scale.connect_value_changed(move |sc| {
            let v = sc.value().round() as i32;
            lbl.set_label(&format!("{}", v));
            crate::config::update_in_memory(|c| c.hyprland.blur_size = Some(v));
            crate::config::hypr_set_keyword("decoration:blur:size", &v.to_string());
            flush();
        });
    }
    hypr_card.append(&bsize_row);

    let (bpass_row, bpass_scale, bpass_lbl) = make_slider_row(
        "Blur Passes",
        1.0,
        4.0,
        1.0,
        live_blur_passes as f64,
        format!("{}", live_blur_passes),
    );
    {
        let lbl = bpass_lbl.clone();
        let flush = schedule_flush.clone();
        bpass_scale.connect_value_changed(move |sc| {
            let v = sc.value().round() as i32;
            lbl.set_label(&format!("{}", v));
            crate::config::update_in_memory(|c| c.hyprland.blur_passes = Some(v));
            crate::config::hypr_set_keyword("decoration:blur:passes", &v.to_string());
            flush();
        });
    }
    hypr_card.append(&bpass_row);

    // 2e. Window Opacity (Active / Inactive)
    let (aop_row, aop_scale, aop_lbl) = make_slider_row(
        "Active Window Opacity",
        50.0,
        100.0,
        1.0,
        (live_active_op * 100.0).round(),
        format!("{}%", (live_active_op * 100.0).round() as i32),
    );
    {
        let lbl = aop_lbl.clone();
        let flush = schedule_flush.clone();
        aop_scale.connect_value_changed(move |sc| {
            let pct = sc.value().round().clamp(50.0, 100.0);
            let f = pct / 100.0;
            lbl.set_label(&format!("{}%", pct as i32));
            crate::config::update_in_memory(|c| c.hyprland.active_opacity = Some(f));
            crate::config::hypr_set_keyword("decoration:active_opacity", &format!("{:.2}", f));
            flush();
        });
    }
    hypr_card.append(&aop_row);

    let (iop_row, iop_scale, iop_lbl) = make_slider_row(
        "Inactive Window Opacity",
        30.0,
        100.0,
        1.0,
        (live_inactive_op * 100.0).round(),
        format!("{}%", (live_inactive_op * 100.0).round() as i32),
    );
    {
        let lbl = iop_lbl.clone();
        let flush = schedule_flush.clone();
        iop_scale.connect_value_changed(move |sc| {
            let pct = sc.value().round().clamp(30.0, 100.0);
            let f = pct / 100.0;
            lbl.set_label(&format!("{}%", pct as i32));
            crate::config::update_in_memory(|c| c.hyprland.inactive_opacity = Some(f));
            crate::config::hypr_set_keyword("decoration:inactive_opacity", &format!("{:.2}", f));
            flush();
        });
    }
    hypr_card.append(&iop_row);

    // 2f. Apply-on-startup + Recalibrate Max + Reset Hyprland
    let hypr_foot = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .margin_top(2)
        .build();
    let startup_on = Rc::new(Cell::new(cfg.hyprland.apply_on_startup));
    let startup_btn = Button::builder()
        .label(if cfg.hyprland.apply_on_startup {
            "Startup: ON"
        } else {
            "Startup: OFF"
        })
        .css_classes(if cfg.hyprland.apply_on_startup {
            vec!["plan-fmt-btn".to_string(), "active".to_string()]
        } else {
            vec!["plan-fmt-btn".to_string()]
        })
        .tooltip_text("Re-apply saved Hyprland tweaks automatically when dashboard starts")
        .build();
    {
        let st = startup_on.clone();
        startup_btn.connect_clicked(move |b| {
            let next = !st.get();
            st.set(next);
            b.set_label(if next { "Startup: ON" } else { "Startup: OFF" });
            if next {
                b.add_css_class("active");
            } else {
                b.remove_css_class("active");
            }
            crate::config::update(|c| c.hyprland.apply_on_startup = next);
        });
    }

    let recal_btn = Button::builder()
        .label(&format!("Max: {}px", max_hypr_r))
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Re-read Hyprland's default rounding (after hyprctl reload) as the 100% max")
        .build();
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        recal_btn.connect_clicked(move |_| {
            let _ = Command::new("hyprctl").arg("reload").status();
            let detected = crate::config::hypr_get_option_i32("decoration:rounding")
                .filter(|&r| r > 0)
                .unwrap_or(20);
            crate::config::update(|c| {
                c.hyprland.hypr_max_rounding = detected;
                c.hyprland.rounding_scale = 1.0;
            });
            rel_t();
            render_settings_page(&c_box, &rel_t, &app_t);
        });
    }

    let reset_hypr_btn = Button::builder()
        .label("Reset Hypr")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Run hyprctl reload and clear saved Hyprland overrides")
        .build();
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        reset_hypr_btn.connect_clicked(move |_| {
            let _ = Command::new("hyprctl").arg("reload").status();
            let detected = crate::config::hypr_get_option_i32("decoration:rounding")
                .filter(|&r| r > 0)
                .unwrap_or(20);
            crate::config::update(|c| {
                c.hyprland.rounding_scale = 1.0;
                c.hyprland.hypr_max_rounding = detected;
                c.hyprland.glass_opacity = 0.65;
                c.hyprland.gaps_in = None;
                c.hyprland.gaps_out = None;
                c.hyprland.border_size = None;
                c.hyprland.blur_enabled = None;
                c.hyprland.blur_size = None;
                c.hyprland.blur_passes = None;
                c.hyprland.active_opacity = None;
                c.hyprland.inactive_opacity = None;
            });
            rel_t();
            render_settings_page(&c_box, &rel_t, &app_t);
        });
    }

    hypr_foot.append(&startup_btn);
    hypr_foot.append(&recal_btn);
    hypr_foot.append(&reset_hypr_btn);
    hypr_card.append(&hypr_foot);
    container.append(&hypr_card);

    // ------------------------------------------
    // 3. APPS & TOOLS
    // ------------------------------------------
    let apps_hdr = Label::builder()
        .label("APPS & BACKENDS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&apps_hdr);

    let apps_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .css_classes(vec!["plan-check-row".to_string()])
        .build();

    // Helper for segmented choice rows
    let make_choice_group = |parent: &Box,
                             label_txt: &str,
                             options: &[(&str, &str)],
                             current: &str,
                             on_pick: Rc<dyn Fn(String)>| {
        let lbl = Label::builder()
            .label(label_txt)
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        parent.append(&lbl);

        let flow = FlowBox::builder()
            .selection_mode(SelectionMode::None)
            .column_spacing(5)
            .row_spacing(5)
            .max_children_per_line(4)
            .build();

        for &(val, title) in options {
            let active = current == val;
            let btn = Button::builder()
                .label(title)
                .css_classes(if active {
                    vec!["plan-fmt-btn".to_string(), "active".to_string()]
                } else {
                    vec!["plan-fmt-btn".to_string()]
                })
                .build();
            let v_str = val.to_string();
            let cb = on_pick.clone();
            btn.connect_clicked(move |_| cb(v_str.clone()));
            flow.insert(&btn, -1);
        }
        parent.append(&flow);
    };

    // 3a. Screenshot Tool
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        make_choice_group(
            &apps_card,
            "Screenshot Tool",
            &[
                ("lumine-capture", "lumine-capture"),
                ("hyprshot", "hyprshot"),
                ("grimblast", "grimblast"),
                ("grim-slurp", "grim+slurp"),
                ("custom", "Custom"),
            ],
            &cfg.tools.screenshot_tool,
            Rc::new(move |val| {
                crate::config::update(|c| c.tools.screenshot_tool = val);
                render_settings_page(&c_box, &rel_t, &app_t);
            }),
        );
    }

    if cfg.tools.screenshot_tool == "custom" {
        let reg_e = Entry::builder()
            .text(&cfg.tools.screenshot_region_cmd)
            .placeholder_text("Region command...")
            .css_classes(vec!["fav-search".to_string()])
            .build();
        let mon_e = Entry::builder()
            .text(&cfg.tools.screenshot_monitor_cmd)
            .placeholder_text("Monitor command...")
            .css_classes(vec!["fav-search".to_string()])
            .build();
        let full_e = Entry::builder()
            .text(&cfg.tools.screenshot_full_cmd)
            .placeholder_text("Full screen command...")
            .css_classes(vec!["fav-search".to_string()])
            .build();
        let ocr_e = Entry::builder()
            .text(&cfg.tools.screenshot_ocr_cmd)
            .placeholder_text("OCR command...")
            .css_classes(vec!["fav-search".to_string()])
            .build();
        let save_sc = Button::builder()
            .label("Save Custom Screenshot Commands")
            .css_classes(vec!["primary-btn".to_string()])
            .build();
        let (r_c, m_c, f_c, o_c) = (reg_e.clone(), mon_e.clone(), full_e.clone(), ocr_e.clone());
        save_sc.connect_clicked(move |b| {
            crate::config::update(|c| {
                c.tools.screenshot_region_cmd = r_c.text().to_string();
                c.tools.screenshot_monitor_cmd = m_c.text().to_string();
                c.tools.screenshot_full_cmd = f_c.text().to_string();
                c.tools.screenshot_ocr_cmd = o_c.text().to_string();
            });
            b.set_label("Saved!");
            let b_r = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                b_r.set_label("Save Custom Screenshot Commands");
            });
        });
        apps_card.append(&reg_e);
        apps_card.append(&mon_e);
        apps_card.append(&full_e);
        apps_card.append(&ocr_e);
        apps_card.append(&save_sc);
    }

    // 3b. Color Picker
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        make_choice_group(
            &apps_card,
            "Color Picker",
            &[
                ("builtin", "Built-in (slurp+grim)"),
                ("hyprpicker", "hyprpicker"),
                ("custom", "Custom"),
            ],
            &cfg.tools.color_picker_tool,
            Rc::new(move |val| {
                crate::config::update(|c| c.tools.color_picker_tool = val);
                render_settings_page(&c_box, &rel_t, &app_t);
            }),
        );
    }
    if cfg.tools.color_picker_tool == "custom" {
        let cp_e = Entry::builder()
            .text(&cfg.tools.color_picker_cmd)
            .placeholder_text("Custom color picker command...")
            .css_classes(vec!["fav-search".to_string()])
            .build();
        let cp_save = Button::builder()
            .label("Save Picker Command")
            .css_classes(vec!["primary-btn".to_string()])
            .build();
        let cp_c = cp_e.clone();
        cp_save.connect_clicked(move |b| {
            crate::config::update(|c| c.tools.color_picker_cmd = cp_c.text().to_string());
            b.set_label("Saved!");
            let b_r = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                b_r.set_label("Save Picker Command");
            });
        });
        apps_card.append(&cp_e);
        apps_card.append(&cp_save);
    }

    // 3c. Wallpaper Backend
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        make_choice_group(
            &apps_card,
            "Wallpaper Backend",
            &[
                ("swww", "swww"),
                ("awww", "awww"),
                ("hyprpaper", "hyprpaper"),
                ("custom", "Custom"),
            ],
            &cfg.tools.wallpaper_backend,
            Rc::new(move |val| {
                crate::config::update(|c| c.tools.wallpaper_backend = val);
                render_settings_page(&c_box, &rel_t, &app_t);
            }),
        );
    }
    if cfg.tools.wallpaper_backend == "custom" {
        let wp_e = Entry::builder()
            .text(&cfg.tools.wallpaper_custom_cmd)
            .placeholder_text("Use {monitor} and {path} placeholders...")
            .css_classes(vec!["fav-search".to_string()])
            .build();
        let wp_save = Button::builder()
            .label("Save Wallpaper Command")
            .css_classes(vec!["primary-btn".to_string()])
            .build();
        let wp_c = wp_e.clone();
        wp_save.connect_clicked(move |b| {
            crate::config::update(|c| c.tools.wallpaper_custom_cmd = wp_c.text().to_string());
            b.set_label("Saved!");
            let b_r = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                b_r.set_label("Save Wallpaper Command");
            });
        });
        apps_card.append(&wp_e);
        apps_card.append(&wp_save);
    }

    // 3d. Notification Daemon
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        make_choice_group(
            &apps_card,
            "Notification Daemon",
            &[
                ("swaync", "swaync"),
                ("dunst", "dunst"),
                ("mako", "mako"),
                ("none", "Disabled"),
            ],
            &cfg.tools.notification_daemon,
            Rc::new(move |val| {
                crate::config::update(|c| c.tools.notification_daemon = val);
                render_settings_page(&c_box, &rel_t, &app_t);
            }),
        );
    }

    // 3e. Clock Click Action
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        make_choice_group(
            &apps_card,
            "Sidebar Clock Click",
            &[("calendar", "Open Calendar"), ("custom", "Custom Command")],
            &cfg.tools.clock_click_mode,
            Rc::new(move |val| {
                crate::config::update(|c| c.tools.clock_click_mode = val);
                render_settings_page(&c_box, &rel_t, &app_t);
            }),
        );
    }
    if cfg.tools.clock_click_mode == "custom" {
        let clk_e = Entry::builder()
            .text(&cfg.tools.clock_click_cmd)
            .placeholder_text("Shell command when clicking clock...")
            .css_classes(vec!["fav-search".to_string()])
            .build();
        let clk_save = Button::builder()
            .label("Save Clock Command")
            .css_classes(vec!["primary-btn".to_string()])
            .build();
        let clk_c = clk_e.clone();
        clk_save.connect_clicked(move |b| {
            crate::config::update(|c| c.tools.clock_click_cmd = clk_c.text().to_string());
            b.set_label("Saved!");
            let b_r = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                b_r.set_label("Save Clock Command");
            });
        });
        apps_card.append(&clk_e);
        apps_card.append(&clk_save);
    }

    // 3f. Hyprland Dispatch Syntax
    {
        let c_box = container.clone();
        let rel_t = reload_theme.clone();
        let app_t = apply_tabs.clone();
        make_choice_group(
            &apps_card,
            "Hyprland Dispatcher Syntax",
            &[
                ("auto", "Auto-detect"),
                ("hyprlua", "Hyprlua (0.57+)"),
                ("classic", "Classic"),
            ],
            &cfg.tools.hypr_dispatch_mode,
            Rc::new(move |val| {
                crate::config::update(|c| c.tools.hypr_dispatch_mode = val);
                render_settings_page(&c_box, &rel_t, &app_t);
            }),
        );
    }

    container.append(&apps_card);

    // ------------------------------------------
    // 4. VISIBLE SIDEBAR TABS (LIVE)
    // ------------------------------------------
    let tabs_hdr = Label::builder()
        .label("VISIBLE SIDEBAR TABS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&tabs_hdr);

    let tabs_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["plan-check-row".to_string()])
        .build();

    let tabs_flow = FlowBox::builder()
        .selection_mode(SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(3)
        .max_children_per_line(3)
        .column_spacing(6)
        .row_spacing(6)
        .build();

    let tab_defs: &[(&str, &str, bool)] = &[
        ("favorites", "Favorites", cfg.visible_tabs.favorites),
        ("clipboard", "Clipboard", cfg.visible_tabs.clipboard),
        ("plans", "Plans", cfg.visible_tabs.plans),
        ("calendar", "Calendar", cfg.visible_tabs.calendar),
        ("timers", "Timers", cfg.visible_tabs.timers),
        ("music", "Music", cfg.visible_tabs.music),
        ("drive", "Drive", cfg.visible_tabs.drive),
        ("passwords", "Passwords", cfg.visible_tabs.passwords),
        ("locker", "Locker", cfg.visible_tabs.locker),
    ];

    for &(key, label, initial) in tab_defs {
        let on = Rc::new(Cell::new(initial));
        let btn = Button::builder()
            .label(label)
            .css_classes(if initial {
                vec!["plan-fmt-btn".to_string(), "active".to_string()]
            } else {
                vec!["plan-fmt-btn".to_string()]
            })
            .build();
        let on_c = on.clone();
        let app_t = apply_tabs.clone();
        btn.connect_clicked(move |b| {
            let next = !on_c.get();
            on_c.set(next);
            if next {
                b.add_css_class("active");
            } else {
                b.remove_css_class("active");
            }
            crate::config::update(|c| match key {
                "favorites" => c.visible_tabs.favorites = next,
                "clipboard" => c.visible_tabs.clipboard = next,
                "plans" => c.visible_tabs.plans = next,
                "calendar" => c.visible_tabs.calendar = next,
                "timers" => c.visible_tabs.timers = next,
                "music" => c.visible_tabs.music = next,
                "drive" => c.visible_tabs.drive = next,
                "passwords" => c.visible_tabs.passwords = next,
                "locker" => c.visible_tabs.locker = next,
                _ => {}
            });
            app_t();
        });
        tabs_flow.insert(&btn, -1);
    }
    tabs_card.append(&tabs_flow);
    container.append(&tabs_card);

    // ------------------------------------------
    // 5. PATHS & DEFAULTS
    // ------------------------------------------
    let paths_hdr = Label::builder()
        .label("PATHS & DEFAULTS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(6)
        .build();
    container.append(&paths_hdr);

    let paths_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["plan-check-row".to_string()])
        .build();

    let add_path_field = |parent: &Box, label: &str, value: &str, placeholder: &str| -> Entry {
        let lbl = Label::builder()
            .label(label)
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        let e = Entry::builder()
            .text(value)
            .placeholder_text(placeholder)
            .css_classes(vec!["fav-search".to_string()])
            .build();
        parent.append(&lbl);
        parent.append(&e);
        e
    };

    let wp_dir_e = add_path_field(
        &paths_card,
        "Wallpapers Directory",
        &cfg.paths.wallpapers_dir,
        "~/Pictures/Wallpapers",
    );
    let plans_dir_e = add_path_field(
        &paths_card,
        "Plans Directory",
        &cfg.paths.plans_dir,
        "~/Documents/plans",
    );
    let kbd_file_e = add_path_field(
        &paths_card,
        "Hyprland Keybinds File",
        &cfg.paths.keybinds_file,
        "~/.config/hypr/binds.lua or hyprland.conf",
    );
    let theme_file_e = add_path_field(
        &paths_card,
        "External Theme Config File",
        &cfg.paths.theme_conf_file,
        "~/.config/theme-switch/current.conf",
    );
    let rclone_e = add_path_field(
        &paths_card,
        "Default rclone Remote",
        &cfg.paths.rclone_default_remote,
        "gdrive:/",
    );
    let repo_e = add_path_field(
        &paths_card,
        "Source Repo (optional, for Reload rebuild)",
        &cfg.paths.source_repo_dir,
        "Leave blank for prebuilt binary restart",
    );

    let save_paths_btn = Button::builder()
        .label("Save Paths")
        .css_classes(vec!["primary-btn".to_string()])
        .margin_top(4)
        .build();
    {
        let (w_e, p_e, k_e, t_e, rc_e, rp_e) = (
            wp_dir_e.clone(),
            plans_dir_e.clone(),
            kbd_file_e.clone(),
            theme_file_e.clone(),
            rclone_e.clone(),
            repo_e.clone(),
        );
        let rel_t = reload_theme.clone();
        save_paths_btn.connect_clicked(move |b| {
            crate::config::update(|c| {
                c.paths.wallpapers_dir = w_e.text().trim().to_string();
                c.paths.plans_dir = p_e.text().trim().to_string();
                c.paths.keybinds_file = k_e.text().trim().to_string();
                c.paths.theme_conf_file = t_e.text().trim().to_string();
                c.paths.rclone_default_remote = rc_e.text().trim().to_string();
                c.paths.source_repo_dir = rp_e.text().trim().to_string();
            });
            rel_t();
            b.set_label("Saved!");
            let b_r = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                b_r.set_label("Save Paths");
            });
        });
    }
    paths_card.append(&save_paths_btn);
    container.append(&paths_card);
}
