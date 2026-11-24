//! Centralized configuration (`~/.config/lily-dashboard/config.json`), built-in
//! fallback color palettes, and Hyprland (Hyprlua + classic) IPC helpers.
//!
//! On first run, `AppConfig` auto-detects installed tools (`lumine-capture`, `swww`,
//! `swaync`, `binds.lua`, etc.) and queries Hyprland's current `decoration:rounding`
//! as the baseline maximum so the corner-rounding slider maps `1.0` to the user's
//! existing desktop geometry and `0.0` to completely square corners.

use gtk4::glib;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct VisibleTabs {
    pub favorites: bool,
    pub clipboard: bool,
    pub plans: bool,
    pub calendar: bool,
    pub timers: bool,
    pub music: bool,
    pub drive: bool,
    pub passwords: bool,
    pub locker: bool,
}

impl Default for VisibleTabs {
    fn default() -> Self {
        Self {
            favorites: true,
            clipboard: true,
            plans: true,
            calendar: true,
            timers: true,
            music: true,
            drive: true,
            passwords: true,
            locker: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ToolsConfig {
    /// "lumine-capture", "hyprshot", "grimblast", "grim-slurp", or "custom"
    pub screenshot_tool: String,
    pub screenshot_region_cmd: String,
    pub screenshot_monitor_cmd: String,
    pub screenshot_full_cmd: String,
    pub screenshot_ocr_cmd: String,
    /// "builtin" (slurp+grim), "hyprpicker", or "custom"
    pub color_picker_tool: String,
    pub color_picker_cmd: String,
    /// "swww", "awww", "hyprpaper", or "custom"
    pub wallpaper_backend: String,
    /// For "custom" backend: `{path}` and `{monitor}` placeholders are substituted
    pub wallpaper_custom_cmd: String,
    /// "swaync", "dunst", "mako", or "none"
    pub notification_daemon: String,
    /// "calendar" (open built-in Calendar tab) or "custom"
    pub clock_click_mode: String,
    pub clock_click_cmd: String,
    /// "auto", "hyprlua", or "classic"
    pub hypr_dispatch_mode: String,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        let screenshot_tool = if bin_exists("lumine-capture") {
            "lumine-capture"
        } else if bin_exists("hyprshot") {
            "hyprshot"
        } else if bin_exists("grimblast") {
            "grimblast"
        } else {
            "grim-slurp"
        }
        .to_string();

        let color_picker_tool = if bin_exists("slurp") && bin_exists("grim") {
            "builtin"
        } else if bin_exists("hyprpicker") {
            "hyprpicker"
        } else {
            "builtin"
        }
        .to_string();

        let wallpaper_backend = if bin_exists("swww") {
            "swww"
        } else if bin_exists("awww") {
            "awww"
        } else if bin_exists("hyprpaper") {
            "hyprpaper"
        } else {
            "swww"
        }
        .to_string();

        let notification_daemon = if bin_exists("swaync-client") {
            "swaync"
        } else if bin_exists("dunstctl") {
            "dunst"
        } else if bin_exists("makoctl") {
            "mako"
        } else {
            "none"
        }
        .to_string();

        let nixie_path = glib::home_dir().join(".local/bin/divergence-popup");
        let (clock_click_mode, clock_click_cmd) = if nixie_path.is_file() {
            (
                "custom".to_string(),
                "if pgrep -f 'divergence-popup' >/dev/null; then pkill -f 'divergence-popup'; else ~/.local/bin/divergence-popup & fi".to_string(),
            )
        } else {
            ("calendar".to_string(), String::new())
        };

        Self {
            screenshot_tool,
            screenshot_region_cmd: "lumine-capture".to_string(),
            screenshot_monitor_cmd: "lumine-capture -m".to_string(),
            screenshot_full_cmd: "lumine-capture -f".to_string(),
            screenshot_ocr_cmd: "lumine-capture -r ocr -t c".to_string(),
            color_picker_tool,
            color_picker_cmd: "hyprpicker -a".to_string(),
            wallpaper_backend,
            wallpaper_custom_cmd: "swww img -o '{monitor}' '{path}'".to_string(),
            notification_daemon,
            clock_click_mode,
            clock_click_cmd,
            hypr_dispatch_mode: "auto".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct PathsConfig {
    pub wallpapers_dir: String,
    pub wallpaper_state_file: String,
    pub plans_dir: String,
    pub keybinds_file: String,
    pub theme_conf_file: String,
    pub rclone_default_remote: String,
    /// Optional repo path for self-rebuilding via Reload Dashboard. Empty = restart current binary.
    pub source_repo_dir: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        let home = glib::home_dir();
        let plans_dir = if home.join("documents/plans").exists() {
            "~/documents/plans".to_string()
        } else {
            "~/Documents/plans".to_string()
        };

        let cfg = glib::user_config_dir();
        let keybinds_file = if cfg.join("hypr/binds.lua").is_file() {
            "~/.config/hypr/binds.lua".to_string()
        } else if cfg.join("hypr/keybinds.conf").is_file() {
            "~/.config/hypr/keybinds.conf".to_string()
        } else {
            "~/.config/hypr/hyprland.conf".to_string()
        };

        // Detect repo dir only if running from a local cargo build checkout
        let source_repo_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| {
                // e.g. <repo>/target/release/lily-dashboard -> <repo>
                let repo = exe.parent()?.parent()?.parent()?;
                if repo.join("Cargo.toml").is_file() {
                    Some(repo.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        Self {
            wallpapers_dir: "~/Pictures/Wallpapers".to_string(),
            wallpaper_state_file: "~/.config/hypr/.current_wallpaper".to_string(),
            plans_dir,
            keybinds_file,
            theme_conf_file: "~/.config/theme-switch/current.conf".to_string(),
            rclone_default_remote: "gdrive:/".to_string(),
            source_repo_dir,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct HyprlandConfig {
    /// Normalized corner rounding scale in [0.0, 1.0].
    /// 1.0 matches both Hyprland's current max rounding and the dashboard's max radii (16px outer / 10px controls).
    /// 0.0 sets both Hyprland and every dashboard corner cleanly to 0px.
    pub rounding_scale: f64,
    /// Baseline maximum Hyprland window rounding (captured from `decoration:rounding` at 100%).
    pub hypr_max_rounding: i32,
    /// Dashboard frosted-glass background opacity (0.30..=1.00).
    pub glass_opacity: f64,
    /// Re-apply saved Hyprland live tweaks automatically when the dashboard launches.
    pub apply_on_startup: bool,
    pub gaps_in: Option<i32>,
    pub gaps_out: Option<i32>,
    pub border_size: Option<i32>,
    pub blur_enabled: Option<bool>,
    pub blur_size: Option<i32>,
    pub blur_passes: Option<i32>,
    pub active_opacity: Option<f64>,
    pub inactive_opacity: Option<f64>,
}

impl Default for HyprlandConfig {
    fn default() -> Self {
        let detected_rounding = hypr_get_option_i32("decoration:rounding")
            .filter(|&r| r > 0)
            .unwrap_or(20);
        Self {
            rounding_scale: 1.0,
            hypr_max_rounding: detected_rounding,
            glass_opacity: 0.65,
            apply_on_startup: false,
            gaps_in: None,
            gaps_out: None,
            border_size: None,
            blur_enabled: None,
            blur_size: None,
            blur_passes: None,
            active_opacity: None,
            inactive_opacity: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AppConfig {
    /// "auto" (read `paths.theme_conf_file`, fall back to marshmellow-dark) or a built-in preset key
    pub theme_preset: String,
    pub visible_tabs: VisibleTabs,
    pub tools: ToolsConfig,
    pub paths: PathsConfig,
    pub hyprland: HyprlandConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme_preset: "auto".to_string(),
            visible_tabs: VisibleTabs::default(),
            tools: ToolsConfig::default(),
            paths: PathsConfig::default(),
            hyprland: HyprlandConfig::default(),
        }
    }
}

static CONFIG_STATE: OnceLock<Mutex<AppConfig>> = OnceLock::new();

pub fn config_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("config.json");
    path
}

pub fn expand_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        glib::home_dir().join(rest)
    } else if trimmed == "~" {
        glib::home_dir()
    } else {
        PathBuf::from(trimmed)
    }
}

pub fn bin_exists(name: &str) -> bool {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let p = Path::new(dir).join(name);
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

fn load_from_disk() -> AppConfig {
    let path = config_path();
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(mut parsed) = serde_json::from_str::<AppConfig>(&data) {
            parsed.hyprland.rounding_scale = parsed.hyprland.rounding_scale.clamp(0.0, 1.0);
            parsed.hyprland.glass_opacity = parsed.hyprland.glass_opacity.clamp(0.30, 1.0);
            if parsed.hyprland.hypr_max_rounding <= 0 {
                parsed.hyprland.hypr_max_rounding = 20;
            }
            return parsed;
        }
    }
    let fresh = AppConfig::default();
    save_to_disk(&fresh);
    fresh
}

fn save_to_disk(cfg: &AppConfig) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_path(), json);
    }
}

pub fn get() -> AppConfig {
    let lock = CONFIG_STATE.get_or_init(|| Mutex::new(load_from_disk()));
    lock.lock().map(|g| g.clone()).unwrap_or_default()
}

pub fn update<F: FnOnce(&mut AppConfig)>(f: F) -> AppConfig {
    let snapshot = update_in_memory(f);
    save_to_disk(&snapshot);
    snapshot
}

pub fn update_in_memory<F: FnOnce(&mut AppConfig)>(f: F) -> AppConfig {
    let lock = CONFIG_STATE.get_or_init(|| Mutex::new(load_from_disk()));
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard);
    guard.hyprland.rounding_scale = guard.hyprland.rounding_scale.clamp(0.0, 1.0);
    guard.hyprland.glass_opacity = guard.hyprland.glass_opacity.clamp(0.30, 1.0);
    guard.clone()
}

pub fn flush_to_disk() {
    let snapshot = get();
    save_to_disk(&snapshot);
}

pub fn reload_from_disk() -> AppConfig {
    let fresh = load_from_disk();
    let lock = CONFIG_STATE.get_or_init(|| Mutex::new(fresh.clone()));
    if let Ok(mut guard) = lock.lock() {
        *guard = fresh.clone();
    }
    fresh
}

// ==========================================
// BUILT-IN FALLBACK THEME PRESETS
// ==========================================

#[derive(Clone, Copy)]
pub struct BuiltinTheme {
    pub key: &'static str,
    pub name: &'static str,
    pub mode: &'static str,
    pub bg_main: &'static str,
    pub bg_pill: &'static str,
    pub bg_recessed: &'static str,
    pub bg_contrast: &'static str,
    pub fg_main: &'static str,
    pub fg_muted: &'static str,
    pub accent: &'static str,
    pub selection: &'static str,
    pub magenta: &'static str,
}

pub const BUILTIN_THEMES: &[BuiltinTheme] = &[
    BuiltinTheme {
        key: "marshmellow-dark",
        name: "Marshmellow (Dark)",
        mode: "dark",
        bg_main: "1c161e",
        bg_pill: "2a202d",
        bg_recessed: "141016",
        bg_contrast: "110d13",
        fg_main: "fee4ff",
        fg_muted: "a884a6",
        accent: "e975ca",
        selection: "bf40a1",
        magenta: "ffc2ff",
    },
    BuiltinTheme {
        key: "marshmellow-light",
        name: "Marshmellow (Light)",
        mode: "light",
        bg_main: "fee4ff",
        bg_pill: "f8d4ff",
        bg_recessed: "f3c6fb",
        bg_contrast: "ffffff",
        fg_main: "332936",
        fg_muted: "7a5e80",
        accent: "d45cb4",
        selection: "ef96d9",
        magenta: "bf40a1",
    },
    BuiltinTheme {
        key: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        mode: "dark",
        bg_main: "1e1e2e",
        bg_pill: "313244",
        bg_recessed: "181825",
        bg_contrast: "11111b",
        fg_main: "cdd6f4",
        fg_muted: "a6adc8",
        accent: "cba6f7",
        selection: "89b4fa",
        magenta: "f5c2e7",
    },
    BuiltinTheme {
        key: "nord",
        name: "Nord",
        mode: "dark",
        bg_main: "2e3440",
        bg_pill: "3b4252",
        bg_recessed: "272c36",
        bg_contrast: "222630",
        fg_main: "eceff4",
        fg_muted: "9aa7bd",
        accent: "88c0d0",
        selection: "5e81ac",
        magenta: "8fbcbb",
    },
    BuiltinTheme {
        key: "gruvbox-dark",
        name: "Gruvbox Dark",
        mode: "dark",
        bg_main: "282828",
        bg_pill: "3c3836",
        bg_recessed: "1d2021",
        bg_contrast: "17191a",
        fg_main: "ebdbb2",
        fg_muted: "a89984",
        accent: "fe8019",
        selection: "d65d0e",
        magenta: "fabd2f",
    },
];

pub fn resolve_theme_colors(cfg: &AppConfig) -> HashMap<String, String> {
    let mut map = HashMap::new();

    let apply_builtin = |map: &mut HashMap<String, String>, t: &BuiltinTheme| {
        map.insert("bg_main".into(), t.bg_main.into());
        map.insert("bg_pill".into(), t.bg_pill.into());
        map.insert("bg_recessed".into(), t.bg_recessed.into());
        map.insert("bg_contrast".into(), t.bg_contrast.into());
        map.insert("fg_main".into(), t.fg_main.into());
        map.insert("fg_muted".into(), t.fg_muted.into());
        map.insert("accent".into(), t.accent.into());
        map.insert("selection".into(), t.selection.into());
        map.insert("magenta".into(), t.magenta.into());
    };

    // Start with Marshmellow Dark as fallback defaults
    apply_builtin(&mut map, &BUILTIN_THEMES[0]);

    if cfg.theme_preset != "auto" {
        if let Some(preset) = BUILTIN_THEMES.iter().find(|t| t.key == cfg.theme_preset) {
            apply_builtin(&mut map, preset);
            return map;
        }
    }

    // "auto": read external theme file if present (e.g. ~/.config/theme-switch/current.conf)
    let path = expand_path(&cfg.paths.theme_conf_file);
    if let Ok(content) = fs::read_to_string(&path) {
        for line in content.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }

    map
}

// ==========================================
// HYPRLAND IPC & AUTO-DETECT DISPATCHERS
// ==========================================

pub fn is_hyprlua() -> bool {
    let mode = get().tools.hypr_dispatch_mode;
    match mode.as_str() {
        "hyprlua" => true,
        "classic" => false,
        _ => glib::user_config_dir().join("hypr/hyprland.lua").is_file(),
    }
}

pub fn hypr_focus_workspace(ws: &str) {
    if is_hyprlua() {
        let lua = if ws.parse::<i32>().is_ok() {
            format!("hl.dsp.focus({{ workspace = {} }})", ws)
        } else {
            format!("hl.dsp.focus({{ workspace = \"{}\" }})", ws)
        };
        let _ = Command::new("hyprctl").args(["dispatch", &lua]).spawn();
    } else {
        let _ = Command::new("hyprctl")
            .args(["dispatch", "workspace", ws])
            .spawn();
    }
}

pub fn hypr_focus_window(address: &str) {
    if is_hyprlua() {
        let lua = format!("hl.dsp.focus({{ window = \"address:{}\" }})", address);
        let _ = Command::new("hyprctl").args(["dispatch", &lua]).spawn();
    } else {
        let target = format!("address:{}", address);
        let _ = Command::new("hyprctl")
            .args(["dispatch", "focuswindow", &target])
            .spawn();
    }
}

pub fn hypr_close_window(address: &str) {
    if is_hyprlua() {
        let lua = format!(
            "hl.dsp.window.close({{ window = \"address:{}\" }})",
            address
        );
        let _ = Command::new("hyprctl").args(["dispatch", &lua]).spawn();
    } else {
        let target = format!("address:{}", address);
        let _ = Command::new("hyprctl")
            .args(["dispatch", "closewindow", &target])
            .spawn();
    }
}

pub fn hypr_focus_monitor(name: &str) {
    if is_hyprlua() {
        let lua = format!("hl.dsp.focus({{ monitor = \"{}\" }})", name);
        let _ = Command::new("hyprctl").args(["dispatch", &lua]).spawn();
    } else {
        let _ = Command::new("hyprctl")
            .args(["dispatch", "focusmonitor", name])
            .spawn();
    }
}

pub fn hypr_exec_with_rules(rules: &str, cmd: &str) {
    if is_hyprlua() {
        let lua = format!(
            "hl.dsp.exec_cmd(\"[{}] {}\")",
            rules,
            cmd.replace('"', "\\\"")
        );
        let _ = Command::new("hyprctl").args(["dispatch", &lua]).spawn();
    } else {
        let arg = format!("[{}] {}", rules, cmd);
        let _ = Command::new("hyprctl")
            .args(["dispatch", "exec", &arg])
            .spawn();
    }
}

fn lua_nested_config_expr(key: &str, raw_val: &str) -> String {
    let parts: Vec<&str> = key.split(':').filter(|s| !s.is_empty()).collect();
    let mut expr = raw_val.to_string();
    for part in parts.iter().rev() {
        expr = format!("{{ {} = {} }}", part, expr);
    }
    format!("hl.config({})", expr)
}

pub fn hypr_set_keyword(key: &str, val: &str) {
    if is_hyprlua() {
        let lua = lua_nested_config_expr(key, val);
        let _ = Command::new("hyprctl").args(["eval", &lua]).spawn();
    } else {
        let _ = Command::new("hyprctl").args(["keyword", key, val]).spawn();
    }
}

pub fn hypr_get_option_i32(key: &str) -> Option<i32> {
    let out = Command::new("hyprctl")
        .args(["getoption", key, "-j"])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("int").and_then(|x| x.as_i64()).map(|n| n as i32)
}

pub fn hypr_get_option_f64(key: &str) -> Option<f64> {
    let out = Command::new("hyprctl")
        .args(["getoption", key, "-j"])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("float").and_then(|x| x.as_f64())
}

pub fn hypr_get_option_bool(key: &str) -> Option<bool> {
    let out = Command::new("hyprctl")
        .args(["getoption", key, "-j"])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("int")
        .and_then(|x| x.as_i64())
        .map(|n| n != 0)
        .or_else(|| v.get("bool").and_then(|x| x.as_bool()))
}

/// Computes Hyprland's pixel rounding for a normalized scale `t` in `[0.0, 1.0]`,
/// proportional to `hypr_max_rounding`.
pub fn hypr_rounding_px(cfg: &HyprlandConfig) -> i32 {
    ((cfg.hypr_max_rounding.max(0) as f64) * cfg.rounding_scale.clamp(0.0, 1.0)).round() as i32
}

/// Applies all saved Hyprland tweaks live in one IPC call (supports both Hyprlua and classic Hyprland).
pub fn apply_hyprland_settings(h: &HyprlandConfig) {
    let mut pairs: Vec<(&str, String)> = Vec::new();
    let r_px = hypr_rounding_px(h);
    pairs.push(("decoration:rounding", r_px.to_string()));
    if let Some(v) = h.gaps_in {
        pairs.push(("general:gaps_in", v.to_string()));
    }
    if let Some(v) = h.gaps_out {
        pairs.push(("general:gaps_out", v.to_string()));
    }
    if let Some(v) = h.border_size {
        pairs.push(("general:border_size", v.to_string()));
    }
    if let Some(v) = h.blur_enabled {
        pairs.push((
            "decoration:blur:enabled",
            if v { "true" } else { "false" }.to_string(),
        ));
    }
    if let Some(v) = h.blur_size {
        pairs.push(("decoration:blur:size", v.to_string()));
    }
    if let Some(v) = h.blur_passes {
        pairs.push(("decoration:blur:passes", v.to_string()));
    }
    if let Some(v) = h.active_opacity {
        pairs.push(("decoration:active_opacity", format!("{:.2}", v)));
    }
    if let Some(v) = h.inactive_opacity {
        pairs.push(("decoration:inactive_opacity", format!("{:.2}", v)));
    }

    if pairs.is_empty() {
        return;
    }

    if is_hyprlua() {
        let lua_stmts: Vec<String> = pairs
            .iter()
            .map(|(k, v)| lua_nested_config_expr(k, v))
            .collect();
        let script = lua_stmts.join("; ");
        let _ = Command::new("hyprctl").args(["eval", &script]).spawn();
    } else {
        let batch: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("keyword {} {}", k, v))
            .collect();
        let _ = Command::new("hyprctl")
            .args(["--batch", &batch.join(" ; ")])
            .spawn();
    }
}
