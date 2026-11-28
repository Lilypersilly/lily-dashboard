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
pub struct PowerRowConfig {
    /// Number of buttons in the sidebar footer power row (1, 2, or 3).
    pub count: usize,
    /// Action for each slot: "shutdown", "reboot", "logout", "suspend", "lock", or "menu"
    pub slots: [String; 3],
    pub menu_shutdown: bool,
    pub menu_reboot: bool,
    pub menu_logout: bool,
    pub menu_suspend: bool,
    pub menu_lock: bool,
    /// Optional custom screen-lock command (empty = auto-detect hyprlock / swaylock / loginctl)
    pub lock_cmd: String,
}

impl Default for PowerRowConfig {
    fn default() -> Self {
        Self {
            count: 2,
            slots: [
                "shutdown".to_string(),
                "reboot".to_string(),
                "menu".to_string(),
            ],
            menu_shutdown: true,
            menu_reboot: true,
            menu_logout: true,
            menu_suspend: true,
            menu_lock: true,
            lock_cmd: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ChromeToggles {
    pub show_top_bar: bool,
    pub show_tray: bool,
    pub show_status_strip: bool,
    pub show_session_row: bool,
    pub show_power_row: bool,
    pub show_weather: bool,
    pub show_clock: bool,
    pub show_reload_btn: bool,
}

impl Default for ChromeToggles {
    fn default() -> Self {
        Self {
            show_top_bar: true,
            show_tray: true,
            show_status_strip: true,
            show_session_row: true,
            show_power_row: true,
            show_weather: true,
            show_clock: true,
            show_reload_btn: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct LayoutConfig {
    /// "left" or "right"
    pub side: String,
    /// Edge trigger vertical alignment: "top", "center", or "bottom"
    pub trigger_align: String,
    /// Edge trigger size: "small" (72px), "medium" (100px), or "large" (140px)
    pub trigger_size: String,
    /// true = open on edge hover; false = click-only
    pub trigger_hover_open: bool,
    /// Empty = follow active monitor; non-empty = pin to specific connector (e.g. "DP-3")
    pub pinned_monitor: String,
    /// Default tab opened on startup ("run", "favs", "music", etc.)
    pub default_tab: String,
    /// Ordered sidebar entries: tab IDs ("run", "favs", ...), "---" for 1px separators,
    /// and "folder:<Name>" .. "/folder" blocks for collapsible sidebar folders.
    /// Any tab not in this list is unloaded (hidden & background watchers paused).
    pub sidebar_items: Vec<String>,
    /// Folder names currently collapsed in the sidebar rail.
    pub collapsed_folders: Vec<String>,
    /// Main dashboard panel width in px (340..=560, default 400)
    pub panel_width: i32,
    /// Slide animation duration in ms (0 = instant snap)
    pub anim_duration_ms: u64,
    pub chrome: ChromeToggles,
}

#[allow(dead_code)]
pub const ALL_TAB_IDS: &[(&str, &str)] = &[
    ("run", "Run"),
    ("favs", "Favorites"),
    ("clipboard", "Clipboard"),
    ("plans", "Plans"),
    ("calendar", "Calendar"),
    ("timers", "Timers"),
    ("scicalc", "SciCalc"),
    ("music", "Music"),
    ("drive", "Drive"),
    ("usb", "USB Drives"),
    ("passwords", "Passwords"),
    ("locker", "Locker"),
    ("tasks", "Tasks"),
    ("services", "Services"),
    ("system", "System"),
];

pub const DEFAULT_SIDEBAR_ITEMS: &[&str] = &[
    "run",
    "favs",
    "clipboard",
    "---",
    "plans",
    "calendar",
    "timers",
    "scicalc",
    "---",
    "music",
    "---",
    "drive",
    "usb",
    "passwords",
    "locker",
    "---",
    "tasks",
    "services",
    "system",
];

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            side: "left".to_string(),
            trigger_align: "center".to_string(),
            trigger_size: "medium".to_string(),
            trigger_hover_open: false,
            pinned_monitor: String::new(),
            default_tab: "run".to_string(),
            sidebar_items: DEFAULT_SIDEBAR_ITEMS.iter().map(|s| s.to_string()).collect(),
            collapsed_folders: Vec::new(),
            panel_width: 400,
            anim_duration_ms: 350,
            chrome: ChromeToggles::default(),
        }
    }
}

/// Returns true if `item` in `layout.sidebar_items` is an actual module tab ID
/// (as opposed to `"---"`, `"folder:<Name>"`, or `"/folder"`).
pub fn is_sidebar_tab_id(item: &str) -> bool {
    item != "---" && item != "/folder" && !item.starts_with("folder:")
}

/// Keeps `visible_tabs` booleans in sync with `layout.sidebar_items` so watchers
/// (Favorites inotify, Clipboard wl-paste) automatically pause when a tab is unloaded.
pub fn sync_visible_tabs_from_sidebar(cfg: &mut AppConfig) {
    // Always ensure "system" stays loaded so user can never lock themselves out of Settings
    if !cfg.layout.sidebar_items.iter().any(|x| x == "system") {
        cfg.layout.sidebar_items.push("system".to_string());
    }
    let has = |id: &str| cfg.layout.sidebar_items.iter().any(|x| x == id);
    cfg.visible_tabs.favorites = has("favs");
    cfg.visible_tabs.clipboard = has("clipboard");
    cfg.visible_tabs.plans = has("plans");
    cfg.visible_tabs.calendar = has("calendar");
    cfg.visible_tabs.timers = has("timers");
    cfg.visible_tabs.music = has("music");
    cfg.visible_tabs.drive = has("drive");
    cfg.visible_tabs.passwords = has("passwords");
    cfg.visible_tabs.locker = has("locker");
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct VimBindsConfig {
    /// Master enable/disable toggle for modal Vim keyboard navigation
    pub enabled: bool,
    /// Start tabs in Normal mode instead of auto-focusing search bars (`Run`, `Services`, etc.)
    pub start_in_normal: bool,
    /// Show live `NRM` / `INS` mode pill in the top bar when Vim mode is enabled
    pub show_mode_badge: bool,
    pub down: String,
    pub up: String,
    pub left: String,
    pub right: String,
    pub pane_left: String,
    pub pane_right: String,
    pub next_tab: String,
    pub prev_tab: String,
    pub top: String,
    pub bottom: String,
    pub search: String,
    pub insert: String,
    pub context_menu: String,
    pub close: String,
}

impl Default for VimBindsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            start_in_normal: false,
            show_mode_badge: true,
            down: "j".to_string(),
            up: "k".to_string(),
            left: "h".to_string(),
            right: "l".to_string(),
            pane_left: "H".to_string(),
            pane_right: "L".to_string(),
            next_tab: "J".to_string(),
            prev_tab: "K".to_string(),
            top: "gg".to_string(),
            bottom: "G".to_string(),
            search: "/".to_string(),
            insert: "i".to_string(),
            context_menu: "m".to_string(),
            close: "q".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct BindingsConfig {
    /// Press Escape to close the dashboard drawer when open
    pub esc_to_close: bool,
    /// Press Ctrl+1..9 or Alt+1..9 to jump directly to loaded sidebar tabs
    pub tab_jump_enabled: bool,
    /// Modifier for number tab jumping: "ctrl" or "alt"
    pub tab_jump_mod: String,
    /// Optional modal Vim keyboard navigation & customizable binds
    pub vim: VimBindsConfig,
}

impl Default for BindingsConfig {
    fn default() -> Self {
        Self {
            esc_to_close: true,
            tab_jump_enabled: true,
            tab_jump_mod: "ctrl".to_string(),
            vim: VimBindsConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ThemeTargetsConfig {
    /// Hyprland (`marshmellow.lua` + `lily-colors.conf` + reload)
    pub hyprland: bool,
    /// GTK 3/4 `gtk.css`, `settings.ini`, `gsettings`, and XDG desktop portal color-scheme
    pub gtk: bool,
    /// Qt5 (`qt5ct`), Qt6 (`qt6ct`), `kdeglobals`, `LilyDynamic.colors`, and Dolphin
    pub qt_dolphin: bool,
    /// Atomic `~/.local/share/icons/Lily-Papirus-{Dark,Light}` folder icon color overlay
    pub papirus_folders: bool,
    /// Kitty, Alacritty, Foot, and Ghostty (only written when `~/.config/<term>` exists)
    pub terminals: bool,
    /// Yazi terminal file manager (`~/.config/yazi/lily-theme.toml`)
    pub yazi: bool,
    /// Wofi, Rofi (`lily-theme.rasi`), and Fuzzel (`lily-theme.ini`)
    pub launchers: bool,
    /// SwayNC, SwayOSD, Mako (`lily-theme`), and Dunst (`dunstrc.d/99-lily-theme.conf`)
    pub notifications: bool,
    /// Desktop/TUI apps: Vesktop (Vencord) and btop
    pub apps: bool,
    /// Optional user script `~/.config/lily-dashboard/theme-hook.sh` with `LILY_*` env vars
    pub hook_script: bool,
}

impl Default for ThemeTargetsConfig {
    fn default() -> Self {
        Self {
            hyprland: true,
            gtk: true,
            qt_dolphin: true,
            papirus_folders: true,
            terminals: true,
            yazi: true,
            launchers: true,
            notifications: true,
            apps: true,
            hook_script: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AppConfig {
    /// "auto" (read `paths.theme_conf_file`, fall back to marshmellow-dark), a built-in preset key, or "custom"
    pub theme_preset: String,
    /// Optional imported custom palette hex colors (used when `theme_preset == "custom"`)
    pub custom_palette: Option<HashMap<String, String>>,
    /// Button visual style: "tonal", "outlined", "pill", "solid", "neon", or "ghost"
    pub button_style: String,
    /// Primary UI font family (default: bundled "Josefin Sans")
    pub ui_font: String,
    /// Global UI font size multiplier in [0.75, 1.35] (1.0 = 100%)
    pub font_scale: f64,
    /// Monospace / Nerd Font icon family (default: "MesloLGL Nerd Font")
    pub mono_font: String,
    pub power: PowerRowConfig,
    pub layout: LayoutConfig,
    pub visible_tabs: VisibleTabs,
    pub tools: ToolsConfig,
    pub paths: PathsConfig,
    pub hyprland: HyprlandConfig,
    pub bindings: BindingsConfig,
    pub theme_targets: ThemeTargetsConfig,
    /// Automatically extract & apply theme from selected wallpaper: "off", "dash", or "global"
    pub wallpaper_theme_sync: String,
    /// Mode used when auto-extracting theme from wallpaper: "dark" or "light"
    pub wallpaper_theme_mode: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme_preset: "auto".to_string(),
            custom_palette: None,
            button_style: "tonal".to_string(),
            ui_font: "Josefin Sans".to_string(),
            font_scale: 1.0,
            mono_font: "MesloLGL Nerd Font".to_string(),
            power: PowerRowConfig::default(),
            layout: LayoutConfig::default(),
            visible_tabs: VisibleTabs::default(),
            tools: ToolsConfig::default(),
            paths: PathsConfig::default(),
            hyprland: HyprlandConfig::default(),
            bindings: BindingsConfig::default(),
            theme_targets: ThemeTargetsConfig::default(),
            wallpaper_theme_sync: "off".to_string(),
            wallpaper_theme_mode: "dark".to_string(),
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

pub fn theme_hook_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("theme-hook.sh");
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
            parsed.power.count = parsed.power.count.clamp(1, 3);
            parsed.layout.panel_width = parsed.layout.panel_width.clamp(340, 560);
            if parsed.layout.sidebar_items.is_empty() {
                // Migrate legacy visible_tabs into sidebar_items cleanly
                let v = &parsed.visible_tabs;
                let keep = |id: &str| match id {
                    "favs" => v.favorites,
                    "clipboard" => v.clipboard,
                    "plans" => v.plans,
                    "calendar" => v.calendar,
                    "timers" => v.timers,
                    "music" => v.music,
                    "drive" => v.drive,
                    "passwords" => v.passwords,
                    "locker" => v.locker,
                    _ => true,
                };
                parsed.layout.sidebar_items = DEFAULT_SIDEBAR_ITEMS
                    .iter()
                    .filter(|&&item| item == "---" || keep(item))
                    .map(|s| s.to_string())
                    .collect();
            }
            sync_visible_tabs_from_sidebar(&mut parsed);
            if !parsed.font_scale.is_finite() || parsed.font_scale <= 0.0 {
                parsed.font_scale = 1.0;
            }
            parsed.font_scale = parsed.font_scale.clamp(0.75, 1.35);
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
    BuiltinTheme {
        key: "gruvbox-light",
        name: "Gruvbox Light",
        mode: "light",
        bg_main: "fbf1c7",
        bg_pill: "ebdbb2",
        bg_recessed: "f2e5bc",
        bg_contrast: "ffffff",
        fg_main: "3c3836",
        fg_muted: "7c6f64",
        accent: "d65d0e",
        selection: "d79921",
        magenta: "b16286",
    },
    BuiltinTheme {
        key: "catppuccin-macchiato",
        name: "Catppuccin Macchiato",
        mode: "dark",
        bg_main: "24273a",
        bg_pill: "363a4f",
        bg_recessed: "1e2030",
        bg_contrast: "181926",
        fg_main: "cad3f5",
        fg_muted: "a5adcb",
        accent: "c6a0f6",
        selection: "8aadf4",
        magenta: "f5bde6",
    },
    BuiltinTheme {
        key: "catppuccin-latte",
        name: "Catppuccin Latte",
        mode: "light",
        bg_main: "eff1f5",
        bg_pill: "e6e9ef",
        bg_recessed: "dce0e8",
        bg_contrast: "ffffff",
        fg_main: "4c4f69",
        fg_muted: "6c6f85",
        accent: "8839ef",
        selection: "7287fd",
        magenta: "ea76cb",
    },
    BuiltinTheme {
        key: "rose-pine",
        name: "Rosé Pine",
        mode: "dark",
        bg_main: "191724",
        bg_pill: "26233a",
        bg_recessed: "1f1d2e",
        bg_contrast: "13111c",
        fg_main: "e0def4",
        fg_muted: "908caa",
        accent: "ebbcba",
        selection: "c4a7e7",
        magenta: "eb6f92",
    },
    BuiltinTheme {
        key: "rose-pine-dawn",
        name: "Rosé Pine Dawn",
        mode: "light",
        bg_main: "faf4ed",
        bg_pill: "f2e9e1",
        bg_recessed: "fffaf3",
        bg_contrast: "ffffff",
        fg_main: "575279",
        fg_muted: "797593",
        accent: "d7827e",
        selection: "907aa9",
        magenta: "b4637a",
    },
    BuiltinTheme {
        key: "tokyo-night",
        name: "Tokyo Night",
        mode: "dark",
        bg_main: "1a1b26",
        bg_pill: "24283b",
        bg_recessed: "16161e",
        bg_contrast: "101118",
        fg_main: "c0caf5",
        fg_muted: "565f89",
        accent: "7aa2f7",
        selection: "3d59a1",
        magenta: "bb9af7",
    },
    BuiltinTheme {
        key: "dracula",
        name: "Dracula",
        mode: "dark",
        bg_main: "282a36",
        bg_pill: "44475a",
        bg_recessed: "21222c",
        bg_contrast: "191a21",
        fg_main: "f8f8f2",
        fg_muted: "6272a4",
        accent: "bd93f9",
        selection: "6272a4",
        magenta: "ff79c6",
    },
    BuiltinTheme {
        key: "everforest-dark",
        name: "Everforest Dark",
        mode: "dark",
        bg_main: "2d353b",
        bg_pill: "3d484d",
        bg_recessed: "232a2e",
        bg_contrast: "1e2326",
        fg_main: "d3c6aa",
        fg_muted: "859289",
        accent: "a7c080",
        selection: "543a48",
        magenta: "d699b6",
    },
    BuiltinTheme {
        key: "kanagawa",
        name: "Kanagawa Wave",
        mode: "dark",
        bg_main: "1f1f28",
        bg_pill: "2a2a37",
        bg_recessed: "16161d",
        bg_contrast: "121218",
        fg_main: "dcd7ba",
        fg_muted: "727169",
        accent: "7e9cd8",
        selection: "2d4f67",
        magenta: "957fb8",
    },
    BuiltinTheme {
        key: "one-dark",
        name: "One Dark",
        mode: "dark",
        bg_main: "282c34",
        bg_pill: "353b45",
        bg_recessed: "21252b",
        bg_contrast: "181a1f",
        fg_main: "abb2bf",
        fg_muted: "5c6370",
        accent: "61afef",
        selection: "3e4451",
        magenta: "c678dd",
    },
    BuiltinTheme {
        key: "solarized-dark",
        name: "Solarized Dark",
        mode: "dark",
        bg_main: "002b36",
        bg_pill: "073642",
        bg_recessed: "00212b",
        bg_contrast: "001a22",
        fg_main: "eee8d5",
        fg_muted: "839496",
        accent: "2aa198",
        selection: "268bd2",
        magenta: "d33682",
    },
    BuiltinTheme {
        key: "oled-pitch",
        name: "OLED Pitch",
        mode: "dark",
        bg_main: "000000",
        bg_pill: "121218",
        bg_recessed: "08080c",
        bg_contrast: "000000",
        fg_main: "f2f4f8",
        fg_muted: "787c99",
        accent: "7dcfff",
        selection: "3d59a1",
        magenta: "bb9af7",
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

    if cfg.theme_preset == "custom" {
        if let Some(ref custom) = cfg.custom_palette {
            for (k, v) in custom {
                map.insert(k.clone(), v.trim_start_matches('#').to_string());
            }
            return map;
        }
    }

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
/// Runs `hyprctl reload` on a background thread and re-applies any saved live Hyprland
/// overrides (blur, rounding, gaps, opacity) right after so reloading bindings or themes
/// never resets your dashboard Hyprland tweaks.
pub fn hypr_reload_preserving_overrides() {
    let hypr = get().hyprland;
    std::thread::spawn(move || {
        let _ = Command::new("hyprctl").arg("reload").status();
        std::thread::sleep(std::time::Duration::from_millis(120));
        apply_hyprland_settings(&hypr);
    });
}

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

/// Executes a modular power action ("shutdown", "reboot", "logout", "suspend", "lock").
pub fn run_power_action(action: &str) {
    match action {
        "shutdown" => {
            let _ = Command::new("systemctl").arg("poweroff").spawn();
        }
        "reboot" => {
            let _ = Command::new("systemctl").arg("reboot").spawn();
        }
        "suspend" => {
            let _ = Command::new("systemctl").arg("suspend").spawn();
        }
        "logout" => {
            if is_hyprlua() {
                let _ = Command::new("hyprctl")
                    .args(["dispatch", "hl.dsp.exit()"])
                    .spawn();
            } else {
                let _ = Command::new("hyprctl").args(["dispatch", "exit"]).spawn();
            }
        }
        "lock" => {
            let custom = get().power.lock_cmd;
            if !custom.trim().is_empty() {
                let _ = Command::new("sh").args(["-c", custom.trim()]).spawn();
            } else if bin_exists("hyprlock") {
                let _ = Command::new("hyprlock").spawn();
            } else if bin_exists("swaylock") {
                let _ = Command::new("swaylock").spawn();
            } else {
                let _ = Command::new("loginctl").arg("lock-session").spawn();
            }
        }
        _ => {}
    }
}

// ==========================================
// BUNDLED JOSEFIN SANS (SIL OFL-1.1) & FONT DISCOVERY
// ==========================================

const JOSEFIN_REGULAR: &[u8] = include_bytes!("../assets/fonts/JosefinSans-Regular.ttf");
const JOSEFIN_BOLD: &[u8] = include_bytes!("../assets/fonts/JosefinSans-Bold.ttf");
const JOSEFIN_ITALIC: &[u8] = include_bytes!("../assets/fonts/JosefinSans-Italic.ttf");
const JOSEFIN_BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/JosefinSans-BoldItalic.ttf");
const JOSEFIN_OFL: &[u8] = include_bytes!("../assets/fonts/OFL.txt");

/// Ensures the bundled Josefin Sans font family is installed in
/// `~/.local/share/fonts/lily-dashboard/` if not already available on the system.
/// No-ops immediately on subsequent boots (zero extra SSD writes).
pub fn ensure_bundled_fonts() {
    let dest_dir = glib::user_data_dir().join("fonts").join("lily-dashboard");
    let marker = dest_dir.join("JosefinSans-Regular.ttf");
    if marker.is_file() {
        return;
    }

    // If Josefin Sans is already installed elsewhere on the system, skip writing
    if let Ok(out) = Command::new("fc-list").args([":", "family"]).output() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout
            .lines()
            .any(|l| l.split(',').any(|p| p.trim().eq_ignore_ascii_case("Josefin Sans")))
        {
            return;
        }
    }

    if fs::create_dir_all(&dest_dir).is_ok() {
        let _ = fs::write(dest_dir.join("JosefinSans-Regular.ttf"), JOSEFIN_REGULAR);
        let _ = fs::write(dest_dir.join("JosefinSans-Bold.ttf"), JOSEFIN_BOLD);
        let _ = fs::write(dest_dir.join("JosefinSans-Italic.ttf"), JOSEFIN_ITALIC);
        let _ = fs::write(dest_dir.join("JosefinSans-BoldItalic.ttf"), JOSEFIN_BOLD_ITALIC);
        let _ = fs::write(dest_dir.join("OFL.txt"), JOSEFIN_OFL);
        let _ = Command::new("fc-cache").args(["-f"]).arg(&dest_dir).status();
    }
}

/// Returns a sorted, deduplicated list of installed font families via `fc-list`.
pub fn list_installed_fonts() -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut fonts = Vec::new();
    let mut add = |name: &str| {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            fonts.push(trimmed.to_string());
        }
    };

    add("Josefin Sans");
    if let Ok(out) = Command::new("fc-list").args([":", "family"]).output() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Some(primary) = line.split(',').next() {
                add(primary);
            }
        }
    }

    fonts.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    fonts
}

/// Lists connected Hyprland monitor connector names (e.g. `["DP-3", "DVI-D-1"]`).
pub fn list_connected_monitors() -> Vec<String> {
    let mut out_names = Vec::new();
    if let Ok(output) = Command::new("hyprctl").args(["monitors", "-j"]).output() {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
            if let Some(arr) = json.as_array() {
                for m in arr {
                    if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                        out_names.push(name.to_string());
                    }
                }
            }
        }
    }
    out_names
}

// ==========================================
// ONE-CLICK THEME SHARING (EXPORT / IMPORT JSON)
// ==========================================

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SharedTheme {
    pub palette: HashMap<String, String>,
    pub button_style: String,
    pub ui_font: String,
    pub font_scale: f64,
    pub rounding_scale: f64,
    pub glass_opacity: f64,
}

pub fn export_theme_json() -> String {
    let cfg = get();
    let palette = resolve_theme_colors(&cfg);
    let shared = SharedTheme {
        palette,
        button_style: cfg.button_style,
        ui_font: cfg.ui_font,
        font_scale: (cfg.font_scale * 100.0).round() / 100.0,
        rounding_scale: (cfg.hyprland.rounding_scale * 100.0).round() / 100.0,
        glass_opacity: (cfg.hyprland.glass_opacity * 100.0).round() / 100.0,
    };
    serde_json::to_string(&shared).unwrap_or_else(|_| "{}".to_string())
}

pub fn import_theme_json(raw: &str) -> Result<(), String> {
    let parsed: SharedTheme =
        serde_json::from_str(raw.trim()).map_err(|e| format!("Invalid theme JSON: {}", e))?;
    let updated = update(|c| {
        if !parsed.palette.is_empty() {
            c.theme_preset = "custom".to_string();
            c.custom_palette = Some(parsed.palette.clone());
        }
        if !parsed.button_style.trim().is_empty() {
            c.button_style = parsed.button_style.trim().to_string();
        }
        if !parsed.ui_font.trim().is_empty() {
            c.ui_font = parsed.ui_font.trim().to_string();
        }
        if parsed.font_scale.is_finite() && parsed.font_scale > 0.0 {
            c.font_scale = parsed.font_scale.clamp(0.75, 1.35);
        }
        if parsed.rounding_scale.is_finite() {
            c.hyprland.rounding_scale = parsed.rounding_scale.clamp(0.0, 1.0);
        }
        if parsed.glass_opacity.is_finite() {
            c.hyprland.glass_opacity = parsed.glass_opacity.clamp(0.35, 1.0);
        }
    });
    let r_px = hypr_rounding_px(&updated.hyprland);
    hypr_set_keyword("decoration:rounding", &r_px.to_string());
    Ok(())
}

// ==========================================
// CUSTOM CSS STYLE THEMES (`~/.config/lily-dashboard/themes/css/`)
// ==========================================

const BUILTIN_CSS_THEMES: &[(&str, &str)] = &[
    ("tonal.css", include_str!("../assets/css/tonal.css")),
    ("outlined.css", include_str!("../assets/css/outlined.css")),
    ("pill.css", include_str!("../assets/css/pill.css")),
    ("solid.css", include_str!("../assets/css/solid.css")),
    ("neon.css", include_str!("../assets/css/neon.css")),
    ("ghost.css", include_str!("../assets/css/ghost.css")),
];

pub fn css_themes_dir() -> PathBuf {
    let dir = glib::user_config_dir()
        .join("lily-dashboard")
        .join("themes")
        .join("css");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Installs the 6 stock `.css` style themes into `~/.config/lily-dashboard/themes/css/`
/// if they don't already exist (never overwrites user edits).
pub fn ensure_builtin_css_themes() {
    let dir = css_themes_dir();
    for &(fname, content) in BUILTIN_CSS_THEMES {
        let dest = dir.join(fname);
        if !dest.exists() {
            let _ = fs::write(&dest, content);
        }
    }
}

/// Returns `(key, display_name)` for every `.css` style file in `~/.config/lily-dashboard/themes/css/`.
pub fn list_css_themes() -> Vec<(String, String)> {
    ensure_builtin_css_themes();
    let dir = css_themes_dir();
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("css") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let mut display = stem
                .split(|c| c == '-' || c == '_')
                .filter(|w| !w.is_empty())
                .map(|w| {
                    let mut ch = w.chars();
                    match ch.next() {
                        Some(f) => format!("{}{}", f.to_uppercase(), ch.as_str()),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if let Ok(raw) = fs::read_to_string(&p) {
                for line in raw.lines().take(5) {
                    if let Some(rest) = line.trim().strip_prefix("/*") {
                        let inner = rest.trim_end_matches("*/").trim();
                        if let Some(n) = inner.strip_prefix("@name") {
                            let n = n.trim();
                            if !n.is_empty() {
                                display = n.to_string();
                                break;
                            }
                        }
                    }
                }
            }
            out.push((stem.to_string(), display));
        }
    }
    // Keep stock order first, then custom user themes alphabetically
    let stock_order = ["tonal", "outlined", "pill", "solid", "neon", "ghost"];
    out.sort_by(|a, b| {
        let ia = stock_order.iter().position(|&s| s == a.0).unwrap_or(99);
        let ib = stock_order.iter().position(|&s| s == b.0).unwrap_or(99);
        ia.cmp(&ib)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    out
}

pub fn load_css_theme_source(key: &str) -> String {
    let clean = key.trim().trim_end_matches(".css");
    let path = css_themes_dir().join(format!("{}.css", clean));
    if let Ok(raw) = fs::read_to_string(&path) {
        return raw;
    }
    for &(fname, content) in BUILTIN_CSS_THEMES {
        if fname.strip_suffix(".css") == Some(clean) {
            return content.to_string();
        }
    }
    String::new()
}
