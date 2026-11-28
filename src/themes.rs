//! Native Rust system-wide theme engine for Lily Dashboard.
//!
//! Replaces the external Python `theme-switch.py` script with a built-in engine that:
//! - Derives a full desktop palette (surfaces, muted text, contrast foregrounds, 16-color
//!   terminal palette, KDE/Dolphin color scheme, GTK3/4 CSS, Hyprland, Kitty, btop, SwayNC,
//!   SwayOSD, Wofi, Starship) from just 6 core colors.
//! - Stores editable user themes as JSON in `~/.config/lily-dashboard/themes/*.json` so any
//!   derived color can also be fine-tuned by hand.
//! - Dynamically generates `LilyDynamic.colors` + `kdeglobals` (fixing Dolphin's hardcoded
//!   pink selection bug) and applies matching `papirus-folders` colors without sudo.

use gtk4::glib;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ThemeDefinition {
    pub key: String,
    pub name: String,
    /// "dark" or "light"
    pub mode: String,
    /// "auto" (match closest Papirus color to `accent`) or explicit color like "orange", "pink", "violet"
    pub folder_color: String,

    // --- 6 CORE COLORS (edited in the visual Theme Editor) ---
    pub bg_main: String,
    pub bg_pill: String,
    pub fg_main: String,
    pub accent: String,
    pub selection: String,
    pub magenta: String,

    // --- AUTO-DERIVED COLORS (fine-tunable in .json) ---
    pub bg_recessed: String,
    pub bg_contrast: String,
    pub fg_muted: String,
    pub fg_on_contrast: String,
    pub starship_bg: String,
    pub starship_fg: String,
    /// Terminal ANSI 0..15 hex colors (without '#')
    pub terminal: Vec<String>,

    /// Not serialized: whether this theme comes from `~/.config/lily-dashboard/themes/<key>.json`
    #[serde(skip)]
    pub json_path: Option<PathBuf>,
}

impl Default for ThemeDefinition {
    fn default() -> Self {
        Self::from_six_colors(
            "custom",
            "Custom Theme",
            "dark",
            "auto",
            "1c161e",
            "2a202d",
            "fee4ff",
            "e975ca",
            "bf40a1",
            "ffc2ff",
        )
    }
}

pub fn clean_hex(raw: &str, fallback: &str) -> String {
    let s: String = raw
        .trim()
        .trim_start_matches('#')
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(6)
        .collect();
    if s.len() == 6 {
        s.to_lowercase()
    } else {
        fallback.to_lowercase()
    }
}

pub fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let c = clean_hex(hex, "1c161e");
    let r = u8::from_str_radix(&c[0..2], 16).unwrap_or(28);
    let g = u8::from_str_radix(&c[2..4], 16).unwrap_or(22);
    let b = u8::from_str_radix(&c[4..6], 16).unwrap_or(30);
    (r, g, b)
}

fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("{:02x}{:02x}{:02x}", r, g, b)
}

fn blend_hex(a: &str, b: &str, t: f64) -> String {
    let (ar, ag, ab) = hex_to_rgb(a);
    let (br, bg, bb) = hex_to_rgb(b);
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f64) * (1.0 - t) + (y as f64) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    rgb_to_hex(mix(ar, br), mix(ag, bg), mix(ab, bb))
}

fn relative_luminance(hex: &str) -> f64 {
    let (r, g, b) = hex_to_rgb(hex);
    0.2126 * (r as f64 / 255.0) + 0.7152 * (g as f64 / 255.0) + 0.0722 * (b as f64 / 255.0)
}

fn rgb_csv(hex: &str) -> String {
    let (r, g, b) = hex_to_rgb(hex);
    format!("{},{},{}", r, g, b)
}

fn rgba_css(hex: &str, alpha: f64) -> String {
    let (r, g, b) = hex_to_rgb(hex);
    format!("rgba({}, {}, {}, {:.2})", r, g, b, alpha)
}

/// All 25 Papirus folder colors with representative RGB swatches for auto-matching.
pub const PAPIRUS_COLORS: &[(&str, (u8, u8, u8))] = &[
    ("adwaita", (147, 196, 232)),
    ("black", (66, 66, 66)),
    ("blue", (82, 148, 226)),
    ("bluegrey", (96, 125, 139)),
    ("breeze", (61, 174, 233)),
    ("brown", (141, 110, 99)),
    ("carmine", (163, 0, 52)),
    ("cyan", (0, 188, 212)),
    ("darkcyan", (0, 131, 143)),
    ("deeporange", (255, 87, 34)),
    ("green", (102, 187, 106)),
    ("grey", (142, 142, 142)),
    ("indigo", (92, 107, 192)),
    ("magenta", (202, 113, 223)),
    ("nordic", (129, 161, 193)),
    ("orange", (238, 146, 58)),
    ("palebrown", (188, 164, 140)),
    ("paleorange", (238, 172, 108)),
    ("pink", (233, 117, 202)),
    ("red", (229, 57, 53)),
    ("teal", (38, 166, 154)),
    ("violet", (149, 117, 205)),
    ("white", (228, 228, 228)),
    ("yaru", (233, 84, 32)),
    ("yellow", (245, 194, 17)),
];

pub fn closest_papirus_color(accent_hex: &str) -> &'static str {
    let (r, g, b) = hex_to_rgb(accent_hex);
    let mut best = "pink";
    let mut best_dist = i64::MAX;
    for &(name, (pr, pg, pb)) in PAPIRUS_COLORS {
        let dr = r as i64 - pr as i64;
        let dg = g as i64 - pg as i64;
        let db = b as i64 - pb as i64;
        // Weighted perceptual RGB distance
        let dist = dr * dr * 2 + dg * dg * 4 + db * db * 3;
        if dist < best_dist {
            best_dist = dist;
            best = name;
        }
    }
    best
}

impl ThemeDefinition {
    /// Derives a complete theme (all surfaces, muted text, contrast text, and 16 ANSI colors)
    /// from just 6 core colors.
    #[allow(clippy::too_many_arguments)]
    pub fn from_six_colors(
        key: &str,
        name: &str,
        mode: &str,
        folder_color: &str,
        bg_main: &str,
        bg_pill: &str,
        fg_main: &str,
        accent: &str,
        selection: &str,
        magenta: &str,
    ) -> Self {
        let is_light = mode.eq_ignore_ascii_case("light");
        let bg_main = clean_hex(bg_main, "1c161e");
        let bg_pill = clean_hex(bg_pill, "2a202d");
        let fg_main = clean_hex(fg_main, "fee4ff");
        let accent = clean_hex(accent, "e975ca");
        let selection = clean_hex(selection, "bf40a1");
        let magenta = clean_hex(magenta, "ffc2ff");

        let bg_recessed = if is_light {
            blend_hex(&bg_main, &fg_main, 0.06)
        } else {
            blend_hex(&bg_main, "000000", 0.28)
        };
        let bg_contrast = if is_light {
            blend_hex(&bg_main, "ffffff", 0.65)
        } else {
            blend_hex(&bg_main, "000000", 0.48)
        };
        let fg_muted = blend_hex(&fg_main, &bg_main, 0.42);
        let fg_on_contrast = if relative_luminance(&selection) > 0.52 {
            "18141c".to_string()
        } else {
            "f8f4fa".to_string()
        };

        let starship_bg = bg_pill.clone();
        let starship_fg = accent.clone();

        // Derive harmonious 16-color ANSI palette
        let red = blend_hex("f38ba8", &accent, 0.18);
        let green = blend_hex("a6e3a1", &accent, 0.12);
        let yellow = blend_hex("f9e2af", &accent, 0.15);
        let blue = blend_hex("89b4fa", &accent, 0.15);
        let cyan = blend_hex("89dceb", &accent, 0.15);
        let bright_red = blend_hex(&red, "ffffff", 0.18);
        let bright_green = blend_hex(&green, "ffffff", 0.18);
        let bright_yellow = blend_hex(&yellow, "ffffff", 0.18);
        let bright_blue = blend_hex(&blue, "ffffff", 0.18);
        let bright_cyan = blend_hex(&cyan, "ffffff", 0.18);
        let bright_mag = blend_hex(&magenta, "ffffff", 0.15);

        let terminal = vec![
            bg_contrast.clone(), // 0 black
            red,                 // 1 red
            green,               // 2 green
            yellow,              // 3 yellow
            blue,                // 4 blue
            accent.clone(),      // 5 magenta
            cyan,                // 6 cyan
            fg_muted.clone(),    // 7 white
            bg_pill.clone(),     // 8 bright black
            bright_red,          // 9 bright red
            bright_green,        // 10 bright green
            bright_yellow,       // 11 bright yellow
            bright_blue,         // 12 bright blue
            bright_mag,          // 13 bright magenta
            bright_cyan,         // 14 bright cyan
            fg_main.clone(),     // 15 bright white
        ];

        Self {
            key: key.to_string(),
            name: name.to_string(),
            mode: if is_light { "light" } else { "dark" }.to_string(),
            folder_color: if folder_color.trim().is_empty() {
                "auto".to_string()
            } else {
                folder_color.trim().to_string()
            },
            bg_main,
            bg_pill,
            fg_main,
            accent,
            selection,
            magenta,
            bg_recessed,
            bg_contrast,
            fg_muted,
            fg_on_contrast,
            starship_bg,
            starship_fg,
            terminal,
            json_path: None,
        }
    }

    pub fn resolved_folder_color(&self) -> String {
        if self.folder_color.is_empty() || self.folder_color.eq_ignore_ascii_case("auto") {
            closest_papirus_color(&self.accent).to_string()
        } else {
            self.folder_color.clone()
        }
    }

    pub fn to_palette_map(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("bg_main".into(), self.bg_main.clone());
        m.insert("bg_pill".into(), self.bg_pill.clone());
        m.insert("bg_recessed".into(), self.bg_recessed.clone());
        m.insert("bg_contrast".into(), self.bg_contrast.clone());
        m.insert("fg_main".into(), self.fg_main.clone());
        m.insert("fg_muted".into(), self.fg_muted.clone());
        m.insert("fg_on_contrast".into(), self.fg_on_contrast.clone());
        m.insert("accent".into(), self.accent.clone());
        m.insert("selection".into(), self.selection.clone());
        m.insert("magenta".into(), self.magenta.clone());
        m
    }
}

pub fn user_themes_dir() -> PathBuf {
    let dir = glib::user_config_dir().join("lily-dashboard").join("themes");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn sanitize_key(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = s.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "custom-theme".to_string()
    } else {
        trimmed
    }
}

pub fn save_user_theme(theme: &ThemeDefinition) -> Result<PathBuf, String> {
    let path = user_themes_dir().join(format!("{}.json", sanitize_key(&theme.key)));
    let json = serde_json::to_string_pretty(theme).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| format!("Failed to save theme: {}", e))?;
    Ok(path)
}

fn parse_legacy_conf(path: &Path) -> Option<ThemeDefinition> {
    let content = fs::read_to_string(path).ok()?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim().to_string();
            let val = v.trim().trim_start_matches('#').to_string();
            map.insert(key, val);
        }
    }
    let key = path.file_stem()?.to_string_lossy().to_string();
    let name = map.get("name").cloned().unwrap_or_else(|| key.clone());
    let mode = map.get("mode").cloned().unwrap_or_else(|| "dark".to_string());
    let folder = map
        .get("folder_color")
        .cloned()
        .unwrap_or_else(|| "auto".to_string());

    let mut t = ThemeDefinition::from_six_colors(
        &key,
        &name,
        &mode,
        &folder,
        map.get("bg_main").map(|s| s.as_str()).unwrap_or("1c161e"),
        map.get("bg_pill").map(|s| s.as_str()).unwrap_or("2a202d"),
        map.get("fg_main").map(|s| s.as_str()).unwrap_or("fee4ff"),
        map.get("accent").map(|s| s.as_str()).unwrap_or("e975ca"),
        map.get("selection").map(|s| s.as_str()).unwrap_or("bf40a1"),
        map.get("magenta").map(|s| s.as_str()).unwrap_or("ffc2ff"),
    );

    if let Some(v) = map.get("bg_recessed") {
        t.bg_recessed = clean_hex(v, &t.bg_recessed);
    }
    if let Some(v) = map.get("bg_contrast") {
        t.bg_contrast = clean_hex(v, &t.bg_contrast);
    }
    if let Some(v) = map.get("fg_muted") {
        t.fg_muted = clean_hex(v, &t.fg_muted);
    }
    if let Some(v) = map.get("fg_on_contrast") {
        t.fg_on_contrast = clean_hex(v, &t.fg_on_contrast);
    }
    if let Some(v) = map.get("starship_bg") {
        t.starship_bg = clean_hex(v, &t.starship_bg);
    }
    if let Some(v) = map.get("starship_fg") {
        t.starship_fg = clean_hex(v, &t.starship_fg);
    }
    for i in 0..16 {
        if let Some(v) = map.get(&format!("color{}", i)) {
            if i < t.terminal.len() {
                t.terminal[i] = clean_hex(v, &t.terminal[i]);
            }
        }
    }
    Some(t)
}

/// Returns a unified, deduplicated list of all themes:
/// 1. User `.json` themes in `~/.config/lily-dashboard/themes/`
/// 2. Imported legacy `~/.config/theme-switch/palettes/*.conf`
/// 3. Built-in presets from `config::BUILTIN_THEMES`
pub fn list_all_themes() -> Vec<ThemeDefinition> {
    let mut by_key: HashMap<String, ThemeDefinition> = HashMap::new();

    // 3. Built-in presets (lowest priority so user/conf overrides win)
    for b in crate::config::BUILTIN_THEMES {
        let mut t = ThemeDefinition::from_six_colors(
            b.key,
            b.name,
            b.mode,
            "auto",
            b.bg_main,
            b.bg_pill,
            b.fg_main,
            b.accent,
            b.selection,
            b.magenta,
        );
        t.bg_recessed = b.bg_recessed.to_string();
        t.bg_contrast = b.bg_contrast.to_string();
        t.fg_muted = b.fg_muted.to_string();
        by_key.insert(t.key.clone(), t);
    }

    // 2. Legacy theme-switch palettes
    let legacy_dir = glib::user_config_dir().join("theme-switch").join("palettes");
    if let Ok(rd) = fs::read_dir(&legacy_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("conf") {
                if let Some(t) = parse_legacy_conf(&p) {
                    by_key.insert(t.key.clone(), t);
                }
            }
        }
    }

    // 1. User JSON themes (highest priority)
    let udir = user_themes_dir();
    if let Ok(rd) = fs::read_dir(&udir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(raw) = fs::read_to_string(&p) {
                    if let Ok(mut t) = serde_json::from_str::<ThemeDefinition>(&raw) {
                        if t.key.is_empty() {
                            t.key = p
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| "custom".to_string());
                        }
                        if t.name.is_empty() {
                            t.name = t.key.clone();
                        }
                        t.json_path = Some(p);
                        by_key.insert(t.key.clone(), t);
                    }
                }
            }
        }
    }

    let mut list: Vec<ThemeDefinition> = by_key.into_values().collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

/// Applies `theme` to Lily Dashboard ONLY (leaves Hyprland/Dolphin/GTK untouched).
pub fn apply_dash_only(theme: &ThemeDefinition) {
    let pal = theme.to_palette_map();
    crate::config::update(|c| {
        c.theme_preset = "custom".to_string();
        c.custom_palette = Some(pal);
    });
}

fn replace_line_prefix(text: &str, prefix: &str, new_line: &str) -> String {
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.trim_start().starts_with(prefix) {
                found = true;
                new_line.to_string()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(new_line.to_string());
    }
    let mut res = out.join("\n");
    res.push('\n');
    res
}

fn build_kde_color_scheme(d: &ThemeDefinition, icon_theme: &str) -> String {
    let bg_main = rgb_csv(&d.bg_main);
    let bg_pill = rgb_csv(&d.bg_pill);
    let bg_rec = rgb_csv(&d.bg_recessed);
    let bg_con = rgb_csv(&d.bg_contrast);
    let fg_main = rgb_csv(&d.fg_main);
    let fg_muted = rgb_csv(&d.fg_muted);
    let accent = rgb_csv(&d.accent);
    let sel = rgb_csv(&d.selection);
    let sel_fg = rgb_csv(&d.fg_on_contrast);
    let mag = rgb_csv(&d.magenta);
    let neg = "243,139,168";
    let neu = "249,226,175";
    let pos = "166,227,161";

    format!(
        r#"[ColorEffects:Disabled]
Color={bg_rec}
ColorAmount=0
ColorEffect=0
ContrastAmount=0.65
ContrastEffect=1
IntensityAmount=0.1
IntensityEffect=2

[ColorEffects:Inactive]
ChangeSelectionColor=true
Color={bg_pill}
ColorAmount=0.025
ColorEffect=2
ContrastAmount=0.1
ContrastEffect=2
Enable=false
IntensityAmount=0
IntensityEffect=0

[Colors:Button]
BackgroundAlternate={bg_pill}
BackgroundNormal={bg_pill}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={accent}
ForegroundInactive={fg_muted}
ForegroundLink={mag}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={fg_main}
ForegroundPositive={pos}
ForegroundVisited={accent}

[Colors:Complementary]
BackgroundAlternate={bg_rec}
BackgroundNormal={bg_main}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={accent}
ForegroundInactive={fg_muted}
ForegroundLink={accent}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={fg_main}
ForegroundPositive={pos}
ForegroundVisited={mag}

[Colors:Header]
BackgroundAlternate={bg_rec}
BackgroundNormal={bg_pill}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={accent}
ForegroundInactive={fg_muted}
ForegroundLink={mag}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={fg_main}
ForegroundPositive={pos}
ForegroundVisited={accent}

[Colors:Header][Inactive]
BackgroundAlternate={bg_rec}
BackgroundNormal={bg_rec}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={accent}
ForegroundInactive={fg_muted}
ForegroundLink={mag}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={fg_muted}
ForegroundPositive={pos}
ForegroundVisited={accent}

[Colors:Selection]
BackgroundAlternate={sel}
BackgroundNormal={sel}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={sel_fg}
ForegroundInactive={sel_fg}
ForegroundLink={sel_fg}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={sel_fg}
ForegroundPositive={pos}
ForegroundVisited={sel_fg}

[Colors:Tooltip]
BackgroundAlternate={bg_con}
BackgroundNormal={bg_main}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={accent}
ForegroundInactive={fg_muted}
ForegroundLink={mag}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={fg_main}
ForegroundPositive={pos}
ForegroundVisited={accent}

[Colors:View]
BackgroundAlternate={bg_main}
BackgroundNormal={bg_rec}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={accent}
ForegroundInactive={fg_muted}
ForegroundLink={mag}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={fg_main}
ForegroundPositive={pos}
ForegroundVisited={accent}

[Colors:Window]
BackgroundAlternate={bg_pill}
BackgroundNormal={bg_main}
DecorationFocus={accent}
DecorationHover={accent}
ForegroundActive={accent}
ForegroundInactive={fg_muted}
ForegroundLink={mag}
ForegroundNegative={neg}
ForegroundNeutral={neu}
ForegroundNormal={fg_main}
ForegroundPositive={pos}
ForegroundVisited={accent}

[General]
ColorScheme=LilyDynamic
Name=LilyDynamic
shadeSortColumn=true

[WM]
activeBackground={bg_pill}
activeBlend={fg_main}
activeForeground={fg_main}
inactiveBackground={bg_rec}
inactiveBlend={fg_muted}
inactiveForeground={fg_muted}

[Icons]
Theme={icon_theme}

[KDE]
widgetStyle=Breeze
"#
    )
}

/// Builds a lightweight, symlink-only user icon theme overlay in
/// `~/.local/share/icons/Lily-Papirus-{Dark,Light}` pointing folder icons to the chosen
/// Papirus color variant in `/usr/share/icons/Papirus`. Requires zero root/sudo and <1 MB.
///
/// Uses a hidden staging directory + atomic rename so running GTK4 apps (whose
/// `GtkIconTheme` mmaps `icon-theme.cache`) never observe a half-written directory or
/// truncated cache file.
fn apply_papirus_folder_color(is_light: bool, folder_color: &str) -> String {
    let base_theme = if is_light { "Papirus-Light" } else { "Papirus-Dark" };
    let sys_papirus = PathBuf::from("/usr/share/icons/Papirus");
    if !sys_papirus.is_dir() {
        return base_theme.to_string();
    }

    let overlay_name = if is_light {
        "Lily-Papirus-Light"
    } else {
        "Lily-Papirus-Dark"
    };
    let icons_root = glib::user_data_dir().join("icons");
    let _ = fs::create_dir_all(&icons_root);
    let overlay_dir = icons_root.join(overlay_name);
    let marker_path = overlay_dir.join(".lily-color");

    // Fast path: already built for this exact folder color — avoid touching icons_root at all
    if overlay_dir.is_dir()
        && fs::read_to_string(&marker_path)
            .map(|s| s.trim() == folder_color)
            .unwrap_or(false)
    {
        return overlay_name.to_string();
    }

    let staging_dir = icons_root.join(format!(".{}.staging", overlay_name));
    let old_dir = icons_root.join(format!(".{}.old", overlay_name));
    let _ = fs::remove_dir_all(&staging_dir);
    let _ = fs::remove_dir_all(&old_dir);
    let _ = fs::create_dir_all(&staging_dir);

    let inherits = if is_light {
        "Papirus-Light,Papirus,breeze,hicolor"
    } else {
        "Papirus-Dark,Papirus,breeze-dark,hicolor"
    };

    let sizes: &[(&str, u32)] = &[
        ("16x16", 16),
        ("22x22", 22),
        ("24x24", 24),
        ("32x32", 32),
        ("48x48", 48),
        ("64x64", 64),
    ];

    let mut dir_list = Vec::new();
    let mut sections = String::new();
    for &(sz, px) in sizes {
        dir_list.push(format!("{}/places", sz));
        sections.push_str(&format!(
            "\n[{sz}/places]\nSize={px}\nContext=Places\nType=Fixed\n"
        ));
    }
    let index_theme = format!(
        "[Icon Theme]\nName={overlay_name}\nComment=Dynamic Papirus folder color overlay (Lily Dashboard)\nInherits={inherits}\nDirectories={}\n{sections}",
        dir_list.join(",")
    );
    let _ = fs::write(staging_dir.join("index.theme"), index_theme);

    let folder_exact = format!("folder-{}.svg", folder_color);
    let folder_prefix = format!("folder-{}-", folder_color);
    let user_prefix = format!("user-{}-", folder_color);

    let mut linked_any = false;
    for &(sz, _) in sizes {
        let src_places = sys_papirus.join(sz).join("places");
        if !src_places.is_dir() {
            continue;
        }
        let dst_places = staging_dir.join(sz).join("places");
        let _ = fs::create_dir_all(&dst_places);

        if let Ok(rd) = fs::read_dir(&src_places) {
            for entry in rd.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                let target = entry.path();
                if fname == folder_exact {
                    for alias in [
                        "folder.svg",
                        "inode-directory.svg",
                        "gtk-directory.svg",
                        "stock_folder.svg",
                    ] {
                        let _ = std::os::unix::fs::symlink(&target, dst_places.join(alias));
                    }
                    linked_any = true;
                } else if let Some(rest) = fname.strip_prefix(&folder_prefix) {
                    let _ = std::os::unix::fs::symlink(
                        &target,
                        dst_places.join(format!("folder-{}", rest)),
                    );
                    linked_any = true;
                } else if let Some(rest) = fname.strip_prefix(&user_prefix) {
                    let _ = std::os::unix::fs::symlink(
                        &target,
                        dst_places.join(format!("user-{}", rest)),
                    );
                    linked_any = true;
                }
            }
        }

        // HiDPI @2x symlink
        let hi_path = staging_dir.join(format!("{}@2x", sz));
        let _ = std::os::unix::fs::symlink(sz, &hi_path);
    }

    if !linked_any {
        let _ = fs::remove_dir_all(&staging_dir);
        return base_theme.to_string();
    }

    let _ = fs::write(staging_dir.join(".lily-color"), folder_color);

    if crate::config::bin_exists("gtk-update-icon-cache") {
        let _ = Command::new("gtk-update-icon-cache")
            .args(["-f", "-t"])
            .arg(&staging_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // Atomic directory swap (new inode for icon-theme.cache so live mmaps never break)
    if overlay_dir.exists() {
        let _ = fs::rename(&overlay_dir, &old_dir);
    }
    if fs::rename(&staging_dir, &overlay_dir).is_err() {
        let _ = fs::rename(&old_dir, &overlay_dir);
        let _ = fs::remove_dir_all(&staging_dir);
    } else {
        let _ = fs::remove_dir_all(&old_dir);
    }

    overlay_name.to_string()
}

/// Applies `theme` globally across enabled `ThemeTargetsConfig` categories (only touching
/// apps whose `~/.config/<app>` directory already exists on disk, so zero clutter is created).
pub fn apply_global(d: &ThemeDefinition) -> String {
    let home = glib::home_dir();
    let cfg_dir = glib::user_config_dir();
    let data_dir = glib::user_data_dir();
    let targets = crate::config::get().theme_targets;
    let is_light = d.mode.eq_ignore_ascii_case("light");
    let folder_color = d.resolved_folder_color();
    let icon_theme = if targets.papirus_folders {
        apply_papirus_folder_color(is_light, &folder_color)
    } else if is_light {
        "Papirus-Light".to_string()
    } else {
        "Papirus-Dark".to_string()
    };
    let gtk_dark = if is_light { "0" } else { "1" };
    let gtk_theme = if is_light { "adw-gtk3" } else { "adw-gtk3-dark" };
    let g_scheme = if is_light { "prefer-light" } else { "prefer-dark" };
    let sel_fg = &d.fg_on_contrast;

    let default_term = ThemeDefinition::default().terminal;
    let tc = |idx: usize| -> &str {
        d.terminal
            .get(idx)
            .map(|s| s.as_str())
            .unwrap_or_else(|| default_term.get(idx).map(|s| s.as_str()).unwrap_or("cdd6f4"))
    };

    let proc_running = |name: &str| -> bool {
        Command::new("pgrep")
            .args(["-x", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    // 0. Write current.conf (keeps Lily Dashboard "auto" and external tools synced)
    let ts_dir = cfg_dir.join("theme-switch");
    let _ = fs::create_dir_all(&ts_dir);
    let mut cur_conf = format!(
        "name = {}\nmode = {}\nfolder_color = {}\nbg_main = {}\nbg_pill = {}\nbg_recessed = {}\nbg_contrast = {}\nfg_main = {}\nfg_muted = {}\nfg_on_contrast = {}\naccent = {}\nselection = {}\nmagenta = {}\nstarship_bg = {}\nstarship_fg = {}\n",
        d.name, d.mode, folder_color, d.bg_main, d.bg_pill, d.bg_recessed, d.bg_contrast,
        d.fg_main, d.fg_muted, d.fg_on_contrast, d.accent, d.selection, d.magenta,
        d.starship_bg, d.starship_fg
    );
    for i in 0..16 {
        cur_conf.push_str(&format!("color{} = {}\n", i, tc(i)));
    }
    let _ = fs::write(ts_dir.join("current.conf"), &cur_conf);

    crate::config::update(|c| {
        c.theme_preset = "auto".to_string();
        c.custom_palette = None;
    });

    // 1. Hyprland (`marshmellow.lua` for Hyprlua + `lily-colors.conf` for classic hyprland.conf)
    if targets.hyprland {
        let hypr_dir = cfg_dir.join("hypr");
        if hypr_dir.is_dir() {
            let hypr_lua = format!(
                "-- Dynamic Color Palette for Hyprland (Lily Dashboard)\nreturn {{\n    bg_light       = \"rgba({}ee)\",\n    bg_button      = \"rgba({}ee)\",\n    bg_inactive    = \"rgba({}aa)\",\n    bg_dark        = \"rgba({}ee)\",\n    accent         = \"rgba({}ee)\",\n    selection      = \"rgba({}ee)\",\n    magenta        = \"rgba({}ee)\",\n    text_dark      = \"rgba({}ff)\",\n    text_muted     = \"rgba({}aa)\",\n    shadow         = 0xee{},\n}}\n",
                d.bg_main, d.bg_pill, d.bg_recessed, d.bg_contrast, d.accent, d.selection, d.magenta,
                d.fg_main, d.fg_muted, d.bg_contrast
            );
            let _ = fs::write(hypr_dir.join("marshmellow.lua"), hypr_lua);

            let hypr_conf = format!(
                "# Dynamic Color Palette for Hyprland (Managed by Lily Dashboard)\n# Usage in hyprland.conf: source = ~/.config/hypr/lily-colors.conf\n$bg_main = rgba({bg_main}ee)\n$bg_pill = rgba({bg_pill}ee)\n$bg_recessed = rgba({bg_recessed}aa)\n$bg_contrast = rgba({bg_contrast}ee)\n$fg_main = rgba({fg_main}ff)\n$fg_muted = rgba({fg_muted}aa)\n$fg_on_contrast = rgba({sel_fg}ff)\n$accent = rgba({accent}ee)\n$selection = rgba({selection}ee)\n$magenta = rgba({magenta}ee)\n$shadow = rgba({bg_contrast}ee)\n\n$bg_main_hex = {bg_main}\n$bg_pill_hex = {bg_pill}\n$fg_main_hex = {fg_main}\n$accent_hex = {accent}\n$selection_hex = {selection}\n$magenta_hex = {magenta}\n",
                bg_main = d.bg_main,
                bg_pill = d.bg_pill,
                bg_recessed = d.bg_recessed,
                bg_contrast = d.bg_contrast,
                fg_main = d.fg_main,
                fg_muted = d.fg_muted,
                sel_fg = sel_fg,
                accent = d.accent,
                selection = d.selection,
                magenta = d.magenta,
            );
            let _ = fs::write(hypr_dir.join("lily-colors.conf"), hypr_conf);
        }
    }

    // 2. App Launchers (Wofi, Rofi, Fuzzel)
    if targets.launchers {
        let wofi_dir = cfg_dir.join("wofi");
        if wofi_dir.is_dir() {
            let wofi_css = format!(
                "window {{ margin: 0px; border: 3px solid #{}; background-color: {}; border-radius: 14px; font-family: \"Josefin Sans\", sans-serif; font-size: 15px; font-weight: 600; }}\n#outer-box {{ margin: 12px; border: none; background-color: transparent; }}\n#input {{ margin: 4px 4px 12px 4px; padding: 10px 14px; border: 2px solid #{}; border-radius: 10px; color: #{}; background-color: #{}; outline: none; }}\n#input:focus {{ border: 2px solid #{}; }}\n#inner-box {{ margin: 4px; border: none; background-color: transparent; }}\n#scroll {{ margin: 0px; border: none; }}\n#text {{ margin: 4px 8px; border: none; color: #{}; }}\n#img {{ margin-right: 8px; }}\n#entry {{ padding: 8px 12px; margin: 3px 4px; border-radius: 10px; border: none; background-color: transparent; }}\n#entry:selected {{ background-color: #{}; border: 1px solid #{}; outline: none; }}\n#entry:selected #text {{ color: #{}; font-weight: bold; }}\n",
                d.accent, rgba_css(&d.bg_main, 0.95), d.accent, d.fg_on_contrast, d.bg_contrast,
                d.magenta, d.fg_main, d.selection, d.accent, d.fg_on_contrast
            );
            let _ = fs::write(wofi_dir.join("style.css"), wofi_css);
        }

        let rofi_dir = cfg_dir.join("rofi");
        if rofi_dir.is_dir() {
            let rofi_rasi = format!(
                "/* Managed by Lily Dashboard — add `@import \"lily-theme.rasi\"` to ~/.config/rofi/config.rasi */\n* {{\n    bg-main:        #{bg_main};\n    bg-pill:        #{bg_pill};\n    bg-recessed:    #{bg_recessed};\n    bg-contrast:    #{bg_contrast};\n    fg-main:        #{fg_main};\n    fg-muted:       #{fg_muted};\n    fg-on-contrast: #{sel_fg};\n    accent:         #{accent};\n    selection:      #{selection};\n    magenta:        #{magenta};\n\n    background-color:           @bg-main;\n    text-color:                 @fg-main;\n    border-color:               @accent;\n    selected-normal-background: @selection;\n    selected-normal-foreground: @fg-on-contrast;\n    alternate-normal-background: @bg-recessed;\n    normal-background:          @bg-main;\n    normal-foreground:          @fg-main;\n}}\n",
                bg_main = d.bg_main,
                bg_pill = d.bg_pill,
                bg_recessed = d.bg_recessed,
                bg_contrast = d.bg_contrast,
                fg_main = d.fg_main,
                fg_muted = d.fg_muted,
                sel_fg = sel_fg,
                accent = d.accent,
                selection = d.selection,
                magenta = d.magenta,
            );
            let _ = fs::write(rofi_dir.join("lily-theme.rasi"), rofi_rasi);
        }

        let fuzzel_dir = cfg_dir.join("fuzzel");
        if fuzzel_dir.is_dir() {
            let fuzzel_ini = format!(
                "# Managed by Lily Dashboard — add `include=~/.config/fuzzel/lily-theme.ini` to ~/.config/fuzzel/fuzzel.ini\n[colors]\nbackground={bg_main}ee\ntext={fg_main}ff\nprompt={accent}ff\nplaceholder={fg_muted}ff\ninput={fg_main}ff\nmatch={magenta}ff\nselection={selection}ff\nselection-text={sel_fg}ff\nselection-match={accent}ff\ncounter={fg_muted}ff\nborder={accent}ff\n",
                bg_main = d.bg_main,
                fg_main = d.fg_main,
                accent = d.accent,
                fg_muted = d.fg_muted,
                magenta = d.magenta,
                selection = d.selection,
                sel_fg = sel_fg,
            );
            let _ = fs::write(fuzzel_dir.join("lily-theme.ini"), fuzzel_ini);
        }
    }

    // 3. Notification Daemons & OSD (SwayNC, SwayOSD, Mako, Dunst)
    if targets.notifications {
        let swaync_p = cfg_dir.join("swaync").join("style.css");
        if let Ok(mut snc) = fs::read_to_string(&swaync_p) {
            snc = replace_line_prefix(
                &snc,
                "@define-color bg-main",
                &format!("@define-color bg-main {};", rgba_css(&d.bg_main, 0.88)),
            );
            snc = replace_line_prefix(
                &snc,
                "@define-color accent",
                &format!("@define-color accent #{};", d.accent),
            );
            snc = replace_line_prefix(
                &snc,
                "@define-color text-dark",
                &format!("@define-color text-dark #{};", d.fg_main),
            );
            snc = replace_line_prefix(
                &snc,
                "@define-color text-muted",
                &format!("@define-color text-muted #{};", d.fg_muted),
            );
            let _ = fs::write(&swaync_p, snc);
        }

        let swayosd_p = cfg_dir.join("swayosd").join("style.css");
        if swayosd_p.parent().map(|p| p.is_dir()).unwrap_or(false) {
            let osd_css = format!(
                "* {{ min-height: 0; margin: 0; padding: 0; }}\nwindow {{ padding: 0px 14px; background: {}; border: 2px solid #{}; border-radius: 999px; }}\n#osd {{ margin: 0px 4px; }}\nimage {{ color: #{}; min-width: 14px; min-height: 14px; margin-right: 8px; }}\nprogressbar trough {{ min-width: 120px; min-height: 3px; border-radius: 999px; background: {}; border: none; }}\nprogressbar progress {{ min-height: 3px; border-radius: 999px; background: #{}; border: none; }}\n",
                rgba_css(&d.bg_main, 0.88), d.accent, d.magenta, rgba_css(&d.fg_muted, 0.35), d.accent
            );
            let _ = fs::write(&swayosd_p, osd_css);
        }

        let mako_dir = cfg_dir.join("mako");
        if mako_dir.is_dir() {
            let mako_cfg = format!(
                "# Managed by Lily Dashboard — add `include=~/.config/mako/lily-theme` to ~/.config/mako/config\nbackground-color=#{bg_main}ee\ntext-color=#{fg_main}\nborder-color=#{accent}\nprogress-color=over #{selection}88\n\n[urgency=low]\nborder-color=#{fg_muted}\n\n[urgency=high]\nborder-color=#{magenta}\n",
                bg_main = d.bg_main,
                fg_main = d.fg_main,
                accent = d.accent,
                selection = d.selection,
                fg_muted = d.fg_muted,
                magenta = d.magenta,
            );
            let _ = fs::write(mako_dir.join("lily-theme"), mako_cfg);
        }

        let dunst_dir = cfg_dir.join("dunst");
        if dunst_dir.is_dir() {
            let dropin_dir = dunst_dir.join("dunstrc.d");
            let _ = fs::create_dir_all(&dropin_dir);
            let dunst_cfg = format!(
                "# Managed by Lily Dashboard (auto-loaded by Dunst dunstrc.d drop-in support)\n[global]\n    frame_color = \"#{accent}\"\n    separator_color = frame\n    highlight = \"#{accent}\"\n\n[urgency_low]\n    background = \"#{bg_recessed}ee\"\n    foreground = \"#{fg_muted}\"\n    frame_color = \"#{bg_pill}\"\n\n[urgency_normal]\n    background = \"#{bg_main}ee\"\n    foreground = \"#{fg_main}\"\n    frame_color = \"#{accent}\"\n\n[urgency_critical]\n    background = \"#{bg_main}f5\"\n    foreground = \"#{fg_main}\"\n    frame_color = \"#{magenta}\"\n",
                accent = d.accent,
                bg_recessed = d.bg_recessed,
                fg_muted = d.fg_muted,
                bg_pill = d.bg_pill,
                bg_main = d.bg_main,
                fg_main = d.fg_main,
                magenta = d.magenta,
            );
            let _ = fs::write(dropin_dir.join("99-lily-theme.conf"), dunst_cfg);
        }
    }

    // 4. Terminals (Kitty, Alacritty, Foot, Ghostty)
    if targets.terminals {
        let kitty_dir = cfg_dir.join("kitty");
        if kitty_dir.is_dir() {
            let mut kitty = format!(
                "background #{}\nforeground #{}\ncursor #{}\ncursor_text_color #{}\nselection_background #{}\nselection_foreground #{}\n",
                d.bg_main, d.fg_main, d.accent, d.bg_main, d.selection, d.fg_main
            );
            for i in 0..16 {
                kitty.push_str(&format!("color{} #{}\n", i, tc(i)));
            }
            let _ = fs::write(kitty_dir.join("active-theme.conf"), kitty);
        }

        let alacritty_dir = cfg_dir.join("alacritty");
        if alacritty_dir.is_dir() {
            let alacritty_toml = format!(
                "# Managed by Lily Dashboard\n# Add to alacritty.toml: general.import = [\"~/.config/alacritty/lily-theme.toml\"]\n[colors.primary]\nbackground = \"#{bg_main}\"\nforeground = \"#{fg_main}\"\n\n[colors.cursor]\ntext = \"#{bg_main}\"\ncursor = \"#{accent}\"\n\n[colors.selection]\ntext = \"#{sel_fg}\"\nbackground = \"#{selection}\"\n\n[colors.normal]\nblack   = \"#{c0}\"\nred     = \"#{c1}\"\ngreen   = \"#{c2}\"\nyellow  = \"#{c3}\"\nblue    = \"#{c4}\"\nmagenta = \"#{c5}\"\ncyan    = \"#{c6}\"\nwhite   = \"#{c7}\"\n\n[colors.bright]\nblack   = \"#{c8}\"\nred     = \"#{c9}\"\ngreen   = \"#{c10}\"\nyellow  = \"#{c11}\"\nblue    = \"#{c12}\"\nmagenta = \"#{c13}\"\ncyan    = \"#{c14}\"\nwhite   = \"#{c15}\"\n",
                bg_main = d.bg_main,
                fg_main = d.fg_main,
                accent = d.accent,
                sel_fg = sel_fg,
                selection = d.selection,
                c0 = tc(0), c1 = tc(1), c2 = tc(2), c3 = tc(3),
                c4 = tc(4), c5 = tc(5), c6 = tc(6), c7 = tc(7),
                c8 = tc(8), c9 = tc(9), c10 = tc(10), c11 = tc(11),
                c12 = tc(12), c13 = tc(13), c14 = tc(14), c15 = tc(15),
            );
            let _ = fs::write(alacritty_dir.join("lily-theme.toml"), alacritty_toml);
        }

        let foot_dir = cfg_dir.join("foot");
        if foot_dir.is_dir() {
            let mut foot_ini = format!(
                "# Managed by Lily Dashboard\n# Add to foot.ini: include=~/.config/foot/lily-theme.ini\n[colors]\nbackground={bg_main}\nforeground={fg_main}\nselection-background={selection}\nselection-foreground={sel_fg}\n",
                bg_main = d.bg_main,
                fg_main = d.fg_main,
                selection = d.selection,
                sel_fg = sel_fg,
            );
            for i in 0..8 {
                foot_ini.push_str(&format!("regular{}={}\n", i, tc(i)));
            }
            for i in 0..8 {
                foot_ini.push_str(&format!("bright{}={}\n", i, tc(i + 8)));
            }
            let _ = fs::write(foot_dir.join("lily-theme.ini"), foot_ini);
        }

        let ghostty_dir = cfg_dir.join("ghostty");
        if ghostty_dir.is_dir() {
            let g_themes = ghostty_dir.join("themes");
            let _ = fs::create_dir_all(&g_themes);
            let mut ghostty = format!(
                "# Managed by Lily Dashboard — set `theme = lily-dynamic` in ~/.config/ghostty/config\nbackground = #{bg_main}\nforeground = #{fg_main}\ncursor-color = #{accent}\ncursor-text = #{bg_main}\nselection-background = #{selection}\nselection-foreground = #{sel_fg}\n",
                bg_main = d.bg_main,
                fg_main = d.fg_main,
                accent = d.accent,
                selection = d.selection,
                sel_fg = sel_fg,
            );
            for i in 0..16 {
                ghostty.push_str(&format!("palette = {}=#{}\n", i, tc(i)));
            }
            let _ = fs::write(g_themes.join("lily-dynamic"), ghostty);
        }
    }

    // 4b. Yazi Terminal File Manager
    if targets.yazi {
        let yazi_dir = cfg_dir.join("yazi");
        if yazi_dir.is_dir() {
            let yazi_toml = format!(
                "# Managed by Lily Dashboard ({name})\n[mgr]\ncwd = {{ fg = \"#{accent}\" }}\nhovered = {{ fg = \"#{sel_fg}\", bg = \"#{selection}\", bold = true }}\npreview_hovered = {{ underline = true }}\nfind_keyword = {{ fg = \"#{magenta}\", bold = true }}\nfind_position = {{ fg = \"#{accent}\" }}\nmarker_copied = {{ fg = \"#{c2}\", bg = \"#{c2}\" }}\nmarker_cut = {{ fg = \"#{c1}\", bg = \"#{c1}\" }}\nmarker_selected = {{ fg = \"#{accent}\", bg = \"#{accent}\" }}\nborder_style = {{ fg = \"#{bg_pill}\" }}\n\n[mode]\nnormal_main = {{ fg = \"#{sel_fg}\", bg = \"#{accent}\", bold = true }}\nnormal_alt = {{ fg = \"#{fg_main}\", bg = \"#{bg_pill}\" }}\nselect_main = {{ fg = \"#{sel_fg}\", bg = \"#{magenta}\", bold = true }}\nselect_alt = {{ fg = \"#{fg_main}\", bg = \"#{bg_pill}\" }}\nunset_main = {{ fg = \"#{sel_fg}\", bg = \"#{c1}\", bold = true }}\nunset_alt = {{ fg = \"#{fg_main}\", bg = \"#{bg_pill}\" }}\n\n[status]\nsep_left = {{ open = \"\", close = \"\" }}\nsep_right = {{ open = \"\", close = \"\" }}\noverall = {{ fg = \"#{fg_main}\", bg = \"#{bg_recessed}\" }}\nprogress_label = {{ fg = \"#{fg_main}\", bold = true }}\nprogress_normal = {{ fg = \"#{accent}\", bg = \"#{bg_pill}\" }}\nprogress_error = {{ fg = \"#{c1}\", bg = \"#{bg_pill}\" }}\nperm_type = {{ fg = \"#{accent}\" }}\nperm_read = {{ fg = \"#{c3}\" }}\nperm_write = {{ fg = \"#{c1}\" }}\nperm_exec = {{ fg = \"#{c2}\" }}\nperm_sep = {{ fg = \"#{fg_muted}\" }}\n\n[input]\nborder = {{ fg = \"#{accent}\" }}\ntitle = {{ fg = \"#{accent}\" }}\nvalue = {{ fg = \"#{fg_main}\" }}\nselected = {{ bg = \"#{selection}\", fg = \"#{sel_fg}\" }}\n\n[pick]\nborder = {{ fg = \"#{accent}\" }}\nactive = {{ fg = \"#{magenta}\", bold = true }}\ninactive = {{ fg = \"#{fg_main}\" }}\n\n[confirm]\nborder = {{ fg = \"#{accent}\" }}\ntitle = {{ fg = \"#{accent}\" }}\n",
                name = d.name,
                accent = d.accent,
                sel_fg = sel_fg,
                selection = d.selection,
                magenta = d.magenta,
                bg_pill = d.bg_pill,
                bg_recessed = d.bg_recessed,
                fg_main = d.fg_main,
                fg_muted = d.fg_muted,
                c1 = tc(1),
                c2 = tc(2),
                c3 = tc(3),
            );
            let _ = fs::write(yazi_dir.join("lily-theme.toml"), &yazi_toml);
            let main_yazi_theme = yazi_dir.join("theme.toml");
            let can_write_main = if !main_yazi_theme.exists() {
                true
            } else {
                fs::read_to_string(&main_yazi_theme)
                    .map(|s| s.starts_with("# Managed by Lily Dashboard"))
                    .unwrap_or(false)
            };
            if can_write_main {
                let _ = fs::write(main_yazi_theme, &yazi_toml);
            }
        }
    }

    // 5. GTK3 & GTK4 CSS + settings.ini + gsettings + XDG portal
    if targets.gtk {
        let gtk_css = format!(
            r#"@define-color window_bg_color #{bg_main};
@define-color window_fg_color #{fg_main};
@define-color view_bg_color #{bg_main};
@define-color view_fg_color #{fg_main};
@define-color headerbar_bg_color #{bg_pill};
@define-color headerbar_fg_color #{fg_main};
@define-color headerbar_border_color #{accent};
@define-color headerbar_backdrop_color #{bg_recessed};
@define-color sidebar_bg_color #{bg_recessed};
@define-color sidebar_fg_color #{fg_main};
@define-color sidebar_backdrop_color #{bg_recessed};
@define-color sidebar_shade_color {shade};
@define-color card_bg_color #{bg_pill};
@define-color card_fg_color #{fg_main};
@define-color popover_bg_color #{bg_main};
@define-color popover_fg_color #{fg_main};
@define-color dialog_bg_color #{bg_main};
@define-color dialog_fg_color #{fg_main};
@define-color accent_color #{accent};
@define-color accent_bg_color #{accent};
@define-color accent_fg_color #{sel_fg};

@define-color theme_bg_color #{bg_main};
@define-color theme_fg_color #{fg_main};
@define-color theme_base_color #{bg_main};
@define-color theme_text_color #{fg_main};
@define-color theme_selected_bg_color #{selection};
@define-color theme_selected_fg_color #{sel_fg};

filechooser, filechooser .view, filechooser treeview.view {{ background-color: #{bg_main}; color: #{fg_main}; }}
filechooser placessidebar, filechooser placessidebar list, filechooser placessidebar row {{ background-color: #{bg_recessed}; color: #{fg_main}; }}
filechooser placessidebar row:selected, filechooser treeview.view:selected {{ background-color: #{selection}; color: #{sel_fg}; }}
filechooser .path-bar button {{ background-color: #{bg_pill}; color: #{fg_main}; }}
filechooser .path-bar button:checked {{ background-color: #{selection}; color: #{sel_fg}; }}
tooltip, tooltip.background {{ background-color: #{bg_pill}; color: #{fg_main}; border: 1px solid #{accent}; border-radius: 8px; }}
tooltip label {{ color: #{fg_main}; }}
textview {{ padding: 16px; }}
"#,
            bg_main = d.bg_main,
            fg_main = d.fg_main,
            bg_pill = d.bg_pill,
            accent = d.accent,
            bg_recessed = d.bg_recessed,
            shade = rgba_css(&d.bg_contrast, 0.08),
            sel_fg = sel_fg,
            selection = d.selection,
        );

        for gdir in ["gtk-3.0", "gtk-4.0"] {
            let dir = cfg_dir.join(gdir);
            let _ = fs::create_dir_all(&dir);
            let _ = fs::write(dir.join("gtk.css"), &gtk_css);
            let s_ini = dir.join("settings.ini");
            if let Ok(mut ini) = fs::read_to_string(&s_ini) {
                ini = replace_line_prefix(&ini, "gtk-theme-name=", &format!("gtk-theme-name={}", gtk_theme));
                ini = replace_line_prefix(&ini, "gtk-icon-theme-name=", &format!("gtk-icon-theme-name={}", icon_theme));
                ini = replace_line_prefix(
                    &ini,
                    "gtk-application-prefer-dark-theme=",
                    &format!("gtk-application-prefer-dark-theme={}", gtk_dark),
                );
                let _ = fs::write(&s_ini, ini);
            } else {
                let ini = format!(
                    "[Settings]\ngtk-theme-name={}\ngtk-icon-theme-name={}\ngtk-application-prefer-dark-theme={}\n",
                    gtk_theme, icon_theme, gtk_dark
                );
                let _ = fs::write(&s_ini, ini);
            }
        }

        let _ = Command::new("gsettings")
            .args(["set", "org.gnome.desktop.interface", "color-scheme", g_scheme])
            .status();
        let _ = Command::new("gsettings")
            .args(["set", "org.gnome.desktop.interface", "gtk-theme", gtk_theme])
            .status();
        let _ = Command::new("gsettings")
            .args(["set", "org.gnome.desktop.interface", "icon-theme", &icon_theme])
            .status();
        let portal_scheme = if is_light { "'prefer-light'" } else { "'prefer-dark'" };
        let _ = Command::new("dconf")
            .args(["write", "/org/x/apps/portal/color-scheme", portal_scheme])
            .status();
    }

    // 6. Qt5 (`qt5ct`), Qt6 (`qt6ct`), KDE ColorScheme & Dolphin
    let dolphin_was_open = if targets.qt_dolphin {
        let open = proc_running("dolphin");
        if open {
            let _ = Command::new("pkill").args(["-x", "dolphin"]).status();
            std::thread::sleep(std::time::Duration::from_millis(120));
        }

        let cache_dir = home.join(".cache");
        if let Ok(rd) = fs::read_dir(&cache_dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("kdeglobals")
                    || name.starts_with("kioexec")
                    || name == "icon-cache.kcache"
                {
                    let p = entry.path();
                    if p.is_dir() {
                        let _ = fs::remove_dir_all(&p);
                    } else {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }

        let qss = format!(
            "QWidget {{ color: #{}; }}\nQAbstractItemView, QListView, QTreeView, QAbstractScrollArea, QAbstractScrollArea > QWidget {{ background-color: #{}; color: #{}; }}\nQMenu {{ background-color: {}; color: #{}; border: 2px solid #{}; border-radius: 14px; padding: 6px; }}\nQMenu::item {{ padding: 6px 24px 6px 12px; border-radius: 8px; margin: 2px 4px; background-color: transparent; }}\nQMenu::item:selected {{ background-color: #{}; color: #{}; }}\nQMenu::item:disabled {{ color: #{}; }}\nQMenu::separator {{ height: 1px; background: #{}; margin: 4px 10px; }}\n",
            d.fg_main, d.bg_recessed, d.fg_main, rgba_css(&d.bg_main, 0.95), d.fg_main,
            d.accent, d.selection, sel_fg, d.fg_muted, d.accent
        );
        let act = format!(
            "#ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}",
            d.fg_main, d.bg_pill, d.bg_main, d.bg_pill, d.bg_recessed, d.bg_main, d.fg_main,
            d.fg_main, d.fg_main, d.bg_recessed, d.bg_main, d.bg_contrast, d.selection,
            sel_fg, d.magenta, d.accent, d.bg_main, d.bg_main, d.bg_contrast, sel_fg,
            d.fg_muted, d.accent
        );
        let qt_scheme_body = format!(
            "[ColorScheme]\nactive_colors={0}\ndisabled_colors={0}\ninactive_colors={0}\n",
            act
        );

        for (qt_dir_name, qt_conf_name) in [("qt6ct", "qt6ct.conf"), ("qt5ct", "qt5ct.conf")] {
            let qt_base = cfg_dir.join(qt_dir_name);
            if qt_base.is_dir() {
                let qt_qss_dir = qt_base.join("qss");
                let _ = fs::create_dir_all(&qt_qss_dir);
                let _ = fs::write(qt_qss_dir.join("marshmellow-menus.qss"), &qss);

                let qt_col_dir = qt_base.join("colors");
                let _ = fs::create_dir_all(&qt_col_dir);
                let qt_pal_path = qt_col_dir.join("marshmellow.conf");
                let _ = fs::write(&qt_pal_path, &qt_scheme_body);

                let qt_conf_p = qt_base.join(qt_conf_name);
                if let Ok(mut qt) = fs::read_to_string(&qt_conf_p) {
                    qt = replace_line_prefix(&qt, "icon_theme=", &format!("icon_theme={}", icon_theme));
                    qt = replace_line_prefix(
                        &qt,
                        "color_scheme_path=",
                        &format!("color_scheme_path={}", qt_pal_path.display()),
                    );
                    qt = replace_line_prefix(&qt, "custom_palette=", "custom_palette=true");
                    let _ = fs::write(&qt_conf_p, qt);
                }
            }
        }

        // Dynamic KDE ColorScheme (`LilyDynamic.colors` + `kdeglobals`)
        let kde_scheme = build_kde_color_scheme(d, &icon_theme);
        let cs_dir = data_dir.join("color-schemes");
        let _ = fs::create_dir_all(&cs_dir);
        let _ = fs::write(cs_dir.join("LilyDynamic.colors"), &kde_scheme);
        let _ = fs::write(cfg_dir.join("kdeglobals"), &kde_scheme);

        for drc in [
            cfg_dir.join("dolphinrc"),
            home.join(".local/state/dolphinstaterc"),
        ] {
            if let Ok(rc) = fs::read_to_string(&drc) {
                let updated = if rc.contains("ColorScheme=") {
                    replace_line_prefix(&rc, "ColorScheme=", "ColorScheme=LilyDynamic")
                } else if rc.contains("[UiSettings]") {
                    rc.replace("[UiSettings]", "[UiSettings]\nColorScheme=LilyDynamic")
                } else {
                    format!("{}\n[UiSettings]\nColorScheme=LilyDynamic\n", rc)
                };
                let _ = fs::write(&drc, updated);
            }
        }
        open
    } else {
        false
    };

    // 7. Desktop & TUI Apps (btop + Vesktop/Vencord)
    if targets.apps {
        let btop_dir = cfg_dir.join("btop").join("themes");
        if btop_dir.parent().map(|p| p.is_dir()).unwrap_or(false) {
            let _ = fs::create_dir_all(&btop_dir);
            let p_cyan = tc(6);
            let p_red = tc(1);
            let btop_theme = format!(
                "theme[main_bg]=\"#{0}\"\ntheme[main_fg]=\"#{1}\"\ntheme[title]=\"#{2}\"\ntheme[hi_fg]=\"#{3}\"\ntheme[selected_bg]=\"#{4}\"\ntheme[selected_fg]=\"#{5}\"\ntheme[inactive_fg]=\"#{6}\"\ntheme[graph_text]=\"#{6}\"\ntheme[meter_bg]=\"#{7}\"\ntheme[proc_misc]=\"#{3}\"\ntheme[cpu_box]=\"#{2}\"\ntheme[mem_box]=\"#{2}\"\ntheme[net_box]=\"#{2}\"\ntheme[proc_box]=\"#{2}\"\ntheme[div_line]=\"#{2}\"\ntheme[temp_start]=\"#{3}\"\ntheme[temp_mid]=\"#{8}\"\ntheme[temp_end]=\"#{9}\"\ntheme[cpu_start]=\"#{3}\"\ntheme[cpu_mid]=\"#{2}\"\ntheme[cpu_end]=\"#{8}\"\ntheme[free_start]=\"#{3}\"\ntheme[free_mid]=\"#{2}\"\ntheme[free_end]=\"#{8}\"\ntheme[cached_start]=\"#{3}\"\ntheme[cached_mid]=\"#{2}\"\ntheme[cached_end]=\"#{8}\"\ntheme[available_start]=\"#{3}\"\ntheme[available_mid]=\"#{2}\"\ntheme[available_end]=\"#{8}\"\ntheme[used_start]=\"#{3}\"\ntheme[used_mid]=\"#{2}\"\ntheme[used_end]=\"#{8}\"\ntheme[download_start]=\"#{3}\"\ntheme[download_mid]=\"#{2}\"\ntheme[download_end]=\"#{8}\"\ntheme[upload_start]=\"#{3}\"\ntheme[upload_mid]=\"#{2}\"\ntheme[upload_end]=\"#{8}\"\ntheme[process_start]=\"#{3}\"\ntheme[process_mid]=\"#{2}\"\ntheme[process_end]=\"#{8}\"\n",
                d.bg_main, d.fg_main, d.accent, p_cyan, d.selection, sel_fg, d.fg_muted, d.bg_recessed,
                d.magenta, p_red
            );
            let _ = fs::write(btop_dir.join("marshmellow.theme"), btop_theme);
        }

        let vesktop_themes_dir = cfg_dir.join("vesktop").join("themes");
        if vesktop_themes_dir.is_dir() {
            let fg_sub = blend_hex(&d.fg_main, &d.fg_muted, 0.35);
            let vesktop_css = format!(
                r#"/**
 * @name Marshmellow
 * @author lilypersilly
 * @description Dynamic Lily Dashboard Theme ({name})
 * @version 1.3.0
 */

:root,
.theme-light,
.theme-dark,
.theme-darker,
.theme-midnight,
.visual-refresh {{
    /* Fonts */
    --font-primary: "Josefin Sans", "Noto Sans CJK JP", sans-serif !important;
    --font-display: "Josefin Sans", "Noto Sans CJK JP", sans-serif !important;
    --font-code: "MesloLGL Nerd Font", monospace !important;

    /* Main Backgrounds (Legacy + Visual Refresh) */
    --background-primary: #{bg_main} !important;
    --bg-base-primary: #{bg_main} !important;
    --background-base-low: #{bg_main} !important;
    --background-base-lower: #{bg_recessed} !important;
    --background-base-lowest: #{bg_pill} !important;
    --background-surface-high: #{bg_pill} !important;
    --background-surface-higher: #{bg_pill} !important;
    --background-surface-highest: #{bg_recessed} !important;
    --bg-overlay-chat: #{bg_main} !important;
    --bg-overlay-1: #{bg_main} !important;
    --bg-overlay-2: #{bg_recessed} !important;
    --bg-overlay-3: #{bg_pill} !important;
    --chat-background: #{bg_main} !important;
    --chat-background-default: #{bg_main} !important;
    --home-background: #{bg_main} !important;
    --card-primary-bg: #{bg_pill} !important;
    --card-secondary-bg: #{bg_recessed} !important;

    /* Channel List & Member Sidebar */
    --background-secondary: #{bg_recessed} !important;
    --bg-base-secondary: #{bg_recessed} !important;
    --bg-surface-raised: #{bg_recessed} !important;

    /* Server Bar & Bottom-Left User Panel */
    --background-secondary-alt: #{bg_pill} !important;
    --background-tertiary: #{bg_pill} !important;
    --bg-base-tertiary: #{bg_pill} !important;

    /* Popups, Modals & Context Menus */
    --background-floating: #{bg_main} !important;
    --bg-surface-overlay: #{bg_main} !important;
    --modal-background: #{bg_main} !important;
    --modal-footer-background: #{bg_recessed} !important;
    --background-accent: #{selection} !important;

    /* Hover, Modifiers & Borders */
    --background-modifier-hover: {acc_22} !important;
    --background-modifier-active: {acc_35} !important;
    --background-modifier-selected: #{selection} !important;
    --background-modifier-accent: {acc_35} !important;
    --bg-mod-faint: {acc_08} !important;
    --bg-mod-subtle: {acc_12} !important;
    --bg-mod-normal: {acc_22} !important;
    --bg-mod-strong: {acc_35} !important;
    --background-mod-subtle: {acc_12} !important;
    --background-mod-normal: {acc_22} !important;
    --background-mod-strong: {acc_35} !important;
    --border-subtle: {acc_22} !important;
    --border-normal: {acc_35} !important;
    --border-strong: #{accent} !important;

    /* Message Input Box */
    --channeltextarea-background: #{bg_pill} !important;
    --input-background: #{bg_pill} !important;
    --input-border: {acc_35} !important;
    --input-placeholder-text: #{fg_muted} !important;

    /* Text & Icons (Legacy + Visual Refresh) */
    --text-normal: #{fg_main} !important;
    --text-default: #{fg_main} !important;
    --text-strong: #{fg_main} !important;
    --text-subtle: #{fg_sub} !important;
    --text-primary: #{fg_main} !important;
    --text-secondary: #{fg_sub} !important;
    --text-tertiary: #{fg_muted} !important;
    --text-muted: #{fg_muted} !important;
    --text-link: #{magenta} !important;
    --text-feedback-info: #{fg_main} !important;
    --header-primary: #{fg_main} !important;
    --header-secondary: #{fg_sub} !important;
    --header-muted: #{fg_muted} !important;
    --channels-default: #{fg_sub} !important;
    --channel-icon: #{fg_muted} !important;
    --icon-primary: #{fg_main} !important;
    --icon-secondary: #{fg_sub} !important;
    --icon-muted: #{fg_muted} !important;
    --icon-strong: #{fg_main} !important;
    --interactive-normal: #{fg_sub} !important;
    --interactive-hover: #{accent} !important;
    --interactive-active: #{fg_main} !important;
    --interactive-muted: #{fg_muted} !important;
    --interactive-text-default: #{fg_sub} !important;
    --interactive-text-hover: #{accent} !important;
    --interactive-text-active: #{fg_main} !important;
    --button-secondary-text: #{fg_main} !important;
    --button-outline-primary-text: #{fg_main} !important;

    /* Accents & Mentions */
    --brand-experiment: #{accent} !important;
    --brand-experiment-560: #{selection} !important;
    --brand-500: #{accent} !important;
    --brand-560: #{selection} !important;
    --control-primary-background-default: #{accent} !important;
    --control-primary-text-default: #{sel_fg} !important;
    --mention-foreground: #{accent} !important;
    --mention-background: {acc_25} !important;
    --background-mentioned: {acc_18} !important;
    --background-mentioned-hover: {acc_28} !important;
    --info-warning-foreground: #{accent} !important;

    /* Scrollbars */
    --scrollbar-thin-thumb: #{accent} !important;
    --scrollbar-auto-thumb: #{accent} !important;
    --scrollbar-auto-track: #{bg_recessed} !important;
}}

/* Force Main Chat, Friends, Active Now, Top Bar, Form Footer, Sidebar, and Member List */
[class*="chat_"],
[class*="chatContent_"],
[class*="messagesWrapper_"],
[class*="scroller_"],
[class*="form_"],
[class*="tabBody_"],
[class*="peopleColumn_"],
[class*="nowPlayingColumn_"],
[class*="applicationStore_"],
[class*="shop_"] {{
    background: #{bg_main} !important;
    background-color: #{bg_main} !important;
}}

[class*="bar_"][class*="base_"],
[class*="title_"][class*="bar_"],
[class*="themed_"][class*="container_"],
section[class*="title_"] {{
    background: #{bg_recessed} !important;
    background-color: #{bg_recessed} !important;
}}

[class*="form_"]::before,
[class*="form_"]::after {{
    display: none !important;
}}

[class*="sidebar_"],
[class*="privateChannels_"],
[class*="members_"],
[class*="membersWrap_"],
nav[class*="guilds_"] {{
    background-color: #{bg_recessed} !important;
}}

/* Eliminate Outer Corners on the Message Input Box */
[class*="channelTextArea_"] {{
    background: transparent !important;
    background-color: transparent !important;
    border: none !important;
    box-shadow: none !important;
}}

[class*="channelTextArea_"] [class*="scrollableContainer_"] {{
    background: #{bg_pill} !important;
    background-color: #{bg_pill} !important;
    border: 2px solid #{accent} !important;
    border-radius: 16px !important;
    box-shadow: none !important;
}}

/* Full 4-Sided Rounded Border on the Bottom-Left User Panel */
[class*="panels_"] {{
    background-color: #{bg_pill} !important;
    border: 2px solid #{accent} !important;
    border-radius: 16px !important;
    box-shadow: none !important;
}}

[class*="panels_"] > div,
[class*="panels_"] [class*="container_"] {{
    background: transparent !important;
    background-color: transparent !important;
}}

/* Ensure crisp foreground text in Light & Dark modes regardless of Discord's internal class */
[class*="peopleListItem_"] [class*="username_"],
[class*="userInfo_"] [class*="username_"],
[class*="panels_"] [class*="title_"],
[class*="panels_"] [class*="panelTitleContainer_"] *,
[class*="topPill_"] [class*="item_"] {{
    color: #{fg_main} !important;
}}

[class*="peopleListItem_"] [class*="subtext_"],
[class*="panels_"] [class*="subtext_"],
[class*="searchBar_"] button,
[class*="searchBarComponent_"] {{
    color: #{fg_sub} !important;
}}
"#,
                name = d.name,
                bg_main = d.bg_main,
                bg_recessed = d.bg_recessed,
                bg_pill = d.bg_pill,
                fg_main = d.fg_main,
                fg_sub = fg_sub,
                fg_muted = d.fg_muted,
                accent = d.accent,
                selection = d.selection,
                sel_fg = sel_fg,
                magenta = d.magenta,
                acc_08 = rgba_css(&d.accent, 0.08),
                acc_12 = rgba_css(&d.accent, 0.12),
                acc_18 = rgba_css(&d.accent, 0.18),
                acc_22 = rgba_css(&d.accent, 0.22),
                acc_25 = rgba_css(&d.accent, 0.25),
                acc_28 = rgba_css(&d.accent, 0.28),
                acc_35 = rgba_css(&d.accent, 0.35),
            );
            let lower_path = vesktop_themes_dir.join("marshmellow.theme.css");
            let upper_path = vesktop_themes_dir.join("Marshmellow.theme.css");
            if lower_path.exists() {
                let _ = fs::write(&lower_path, &vesktop_css);
            }
            if upper_path.exists() || !lower_path.exists() {
                let _ = fs::write(&upper_path, &vesktop_css);
            }
        }
    }

    // 8. Live desktop reloads (only for enabled targets & running daemons)
    if targets.qt_dolphin && dolphin_was_open {
        let _ = Command::new("dolphin").spawn();
    }
    if targets.hyprland {
        crate::config::hypr_reload_preserving_overrides();
    }
    if targets.terminals && proc_running("kitty") {
        let _ = Command::new("pkill").args(["-USR1", "-x", "kitty"]).spawn();
    }
    if targets.notifications {
        if proc_running("swaync") && crate::config::bin_exists("swaync-client") {
            let _ = Command::new("swaync-client").arg("-rs").spawn();
        }
        if proc_running("swayosd-server") || cfg_dir.join("swayosd").is_dir() {
            let _ = Command::new("sh")
                .args([
                    "-c",
                    "pkill -x swayosd-server 2>/dev/null; sleep 0.1; command -v swayosd-server >/dev/null && swayosd-server &",
                ])
                .spawn();
        }
        if proc_running("mako") && crate::config::bin_exists("makoctl") {
            let _ = Command::new("makoctl").arg("reload").spawn();
        }
        if proc_running("dunst") && crate::config::bin_exists("dunstctl") {
            let _ = Command::new("dunstctl").arg("reload").spawn();
        }
    }

    // 9. Optional user hook script (`~/.config/lily-dashboard/theme-hook.sh`)
    if targets.hook_script {
        let hook_path = crate::config::theme_hook_path();
        if hook_path.is_file() {
            let mut cmd = Command::new("sh");
            cmd.arg(&hook_path)
                .env("LILY_THEME_KEY", &d.key)
                .env("LILY_THEME_NAME", &d.name)
                .env("LILY_MODE", &d.mode)
                .env("LILY_FOLDER_COLOR", &folder_color)
                .env("LILY_ICON_THEME", &icon_theme)
                .env("LILY_BG_MAIN", &d.bg_main)
                .env("LILY_BG_PILL", &d.bg_pill)
                .env("LILY_BG_RECESSED", &d.bg_recessed)
                .env("LILY_BG_CONTRAST", &d.bg_contrast)
                .env("LILY_FG_MAIN", &d.fg_main)
                .env("LILY_FG_MUTED", &d.fg_muted)
                .env("LILY_FG_ON_CONTRAST", &d.fg_on_contrast)
                .env("LILY_ACCENT", &d.accent)
                .env("LILY_SELECTION", &d.selection)
                .env("LILY_MAGENTA", &d.magenta)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for i in 0..16 {
                cmd.env(format!("LILY_COLOR{}", i), tc(i));
            }
            let _ = cmd.spawn();
        }
    }

    if targets.papirus_folders {
        format!("Applied '{}' globally (folders: {})!", d.name, folder_color)
    } else {
        format!("Applied '{}' globally!", d.name)
    }
}

/// Pure-Rust wallpaper palette extractor (zero `pywal`/`matugen` dependency).
/// Samples pixels across `rgb_bytes`, builds a saturation-weighted 24-bin hue histogram,
/// picks primary & secondary harmonious accents, and derives a full 6-color `ThemeDefinition`.
pub fn extract_theme_from_pixels(
    display_name: &str,
    mode: &str,
    rgb_bytes: &[u8],
    width: i32,
    height: i32,
    rowstride: usize,
    channels: usize,
) -> ThemeDefinition {
    let is_light = mode.eq_ignore_ascii_case("light");
    let key = sanitize_key(display_name);
    if width <= 0 || height <= 0 || channels < 3 || rgb_bytes.is_empty() {
        return ThemeDefinition::from_six_colors(
            &key,
            display_name,
            mode,
            "auto",
            if is_light { "f5eef8" } else { "1c161e" },
            if is_light { "eadcf0" } else { "2a202d" },
            if is_light { "2a2030" } else { "fee4ff" },
            "e975ca",
            "bf40a1",
            "ffc2ff",
        );
    }

    let rgb_to_hsv = |r: u8, g: u8, b: u8| -> (f64, f64, f64) {
        let rf = r as f64 / 255.0;
        let gf = g as f64 / 255.0;
        let bf = b as f64 / 255.0;
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let d = max - min;
        let h = if d < 1e-6 {
            0.0
        } else if (max - rf).abs() < 1e-6 {
            60.0 * (((gf - bf) / d).rem_euclid(6.0))
        } else if (max - gf).abs() < 1e-6 {
            60.0 * (((bf - rf) / d) + 2.0)
        } else {
            60.0 * (((rf - gf) / d) + 4.0)
        };
        let s = if max < 1e-6 { 0.0 } else { d / max };
        (h, s, max)
    };

    let hsv_to_hex = |h_deg: f64, s: f64, v: f64| -> String {
        let h = h_deg.rem_euclid(360.0);
        let s = s.clamp(0.0, 1.0);
        let v = v.clamp(0.0, 1.0);
        let c = v * s;
        let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
        let m = v - c;
        let (r1, g1, b1) = if h < 60.0 {
            (c, x, 0.0)
        } else if h < 120.0 {
            (x, c, 0.0)
        } else if h < 180.0 {
            (0.0, c, x)
        } else if h < 240.0 {
            (0.0, x, c)
        } else if h < 300.0 {
            (x, 0.0, c)
        } else {
            (c, 0.0, x)
        };
        rgb_to_hex(
            ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
            ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
            ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        )
    };

    const BINS: usize = 24;
    let mut bin_weight = [0.0f64; BINS];
    let mut bin_hue_sum = [0.0f64; BINS];
    let mut bin_sat_sum = [0.0f64; BINS];
    let mut total_sat = 0.0f64;
    let mut sample_count = 0usize;

    let step_x = (width / 44).max(1);
    let step_y = (height / 44).max(1);
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let off = (y as usize) * rowstride + (x as usize) * channels;
            if off + 2 < rgb_bytes.len() {
                let (h, s, v) = rgb_to_hsv(rgb_bytes[off], rgb_bytes[off + 1], rgb_bytes[off + 2]);
                total_sat += s;
                sample_count += 1;
                if s > 0.08 && v > 0.08 && v < 0.98 {
                    let b_idx = ((h / 360.0) * BINS as f64).floor() as usize % BINS;
                    let w = s.powf(1.35) * (0.35 + 0.65 * v);
                    bin_weight[b_idx] += w;
                    bin_hue_sum[b_idx] += h * w;
                    bin_sat_sum[b_idx] += s * w;
                }
            }
            x += step_x;
        }
        y += step_y;
    }

    let avg_sat = if sample_count > 0 {
        total_sat / sample_count as f64
    } else {
        0.0
    };

    // Find primary hue peak
    let mut best_bin = 0usize;
    let mut best_w = 0.0f64;
    for i in 0..BINS {
        if bin_weight[i] > best_w {
            best_w = bin_weight[i];
            best_bin = i;
        }
    }

    let is_monochrome = best_w < 0.25 || avg_sat < 0.06;
    let primary_hue = if best_w > 0.0 {
        bin_hue_sum[best_bin] / best_w
    } else {
        265.0
    };
    let primary_sat = if is_monochrome {
        0.14
    } else {
        (bin_sat_sum[best_bin] / best_w).clamp(0.45, 0.82)
    };

    // Find secondary hue peak at least 35 degrees away (or +32 deg analogous harmonic shift)
    let mut sec_bin = None;
    let mut sec_w = 0.0f64;
    for i in 0..BINS {
        let h_i = if bin_weight[i] > 0.0 {
            bin_hue_sum[i] / bin_weight[i]
        } else {
            (i as f64 + 0.5) * (360.0 / BINS as f64)
        };
        let diff = (h_i - primary_hue).abs();
        let ang_dist = diff.min(360.0 - diff);
        if ang_dist >= 35.0 && bin_weight[i] > sec_w {
            sec_w = bin_weight[i];
            sec_bin = Some((h_i, (bin_sat_sum[i] / bin_weight[i]).clamp(0.40, 0.80)));
        }
    }
    let (sec_hue, sec_sat) = match sec_bin {
        Some((sh, ss)) if sec_w >= best_w * 0.18 && !is_monochrome => (sh, ss),
        _ => (
            (primary_hue + 32.0).rem_euclid(360.0),
            if is_monochrome { 0.12 } else { (primary_sat * 0.88).clamp(0.38, 0.78) },
        ),
    };

    let (bg_main, bg_pill, fg_main, accent, selection, magenta) = if is_light {
        let bg = hsv_to_hex(primary_hue, if is_monochrome { 0.03 } else { 0.09 }, 0.96);
        let pill = hsv_to_hex(primary_hue, if is_monochrome { 0.05 } else { 0.15 }, 0.90);
        let fg = hsv_to_hex(primary_hue, if is_monochrome { 0.08 } else { 0.32 }, 0.18);
        let acc = hsv_to_hex(primary_hue, primary_sat.clamp(0.55, 0.85), 0.72);
        let sel = hsv_to_hex(primary_hue, primary_sat.clamp(0.50, 0.80), 0.82);
        let mag = hsv_to_hex(sec_hue, sec_sat.clamp(0.50, 0.82), 0.68);
        (bg, pill, fg, acc, sel, mag)
    } else {
        let bg = hsv_to_hex(primary_hue, if is_monochrome { 0.05 } else { 0.28 }, 0.12);
        let pill = hsv_to_hex(primary_hue, if is_monochrome { 0.07 } else { 0.26 }, 0.18);
        let fg = hsv_to_hex(primary_hue, if is_monochrome { 0.03 } else { 0.10 }, 0.95);
        let acc = hsv_to_hex(primary_hue, primary_sat, 0.88);
        let sel = hsv_to_hex(primary_hue, (primary_sat * 1.05).clamp(0.20, 0.82), 0.66);
        let mag = hsv_to_hex(sec_hue, sec_sat, 0.92);
        (bg, pill, fg, acc, sel, mag)
    };

    ThemeDefinition::from_six_colors(
        &key,
        display_name,
        mode,
        "auto",
        &bg_main,
        &bg_pill,
        &fg_main,
        &accent,
        &selection,
        &magenta,
    )
}
