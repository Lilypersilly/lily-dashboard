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

/// Applies `theme` globally across Hyprland, GTK3/4, Qt6/Dolphin (dynamic KDE color scheme),
/// Papirus folders, Kitty, btop, SwayNC, SwayOSD, Wofi, Starship, and Lily Dashboard.
pub fn apply_global(d: &ThemeDefinition) -> String {
    let home = glib::home_dir();
    let cfg_dir = glib::user_config_dir();
    let data_dir = glib::user_data_dir();
    let is_light = d.mode.eq_ignore_ascii_case("light");
    let folder_color = d.resolved_folder_color();
    let icon_theme = apply_papirus_folder_color(is_light, &folder_color);
    let gtk_dark = if is_light { "0" } else { "1" };
    let gtk_theme = if is_light { "adw-gtk3" } else { "adw-gtk3-dark" };
    let g_scheme = if is_light { "prefer-light" } else { "prefer-dark" };

    // 0. Write current.conf (keeps Lily Dashboard "auto" and external tools synced)
    let ts_dir = cfg_dir.join("theme-switch");
    let _ = fs::create_dir_all(&ts_dir);
    let mut cur_conf = format!(
        "name = {}\nmode = {}\nfolder_color = {}\nbg_main = {}\nbg_pill = {}\nbg_recessed = {}\nbg_contrast = {}\nfg_main = {}\nfg_muted = {}\nfg_on_contrast = {}\naccent = {}\nselection = {}\nmagenta = {}\nstarship_bg = {}\nstarship_fg = {}\n",
        d.name, d.mode, folder_color, d.bg_main, d.bg_pill, d.bg_recessed, d.bg_contrast,
        d.fg_main, d.fg_muted, d.fg_on_contrast, d.accent, d.selection, d.magenta,
        d.starship_bg, d.starship_fg
    );
    for (i, c) in d.terminal.iter().enumerate().take(16) {
        cur_conf.push_str(&format!("color{} = {}\n", i, c));
    }
    let _ = fs::write(ts_dir.join("current.conf"), &cur_conf);

    crate::config::update(|c| {
        c.theme_preset = "auto".to_string();
        c.custom_palette = None;
    });

    // 1. Hyprland marshmellow.lua
    let hypr_lua = format!(
        "-- Dynamic Color Palette for Hyprland (Lily Dashboard)\nreturn {{\n    bg_light       = \"rgba({}ee)\",\n    bg_button      = \"rgba({}ee)\",\n    bg_inactive    = \"rgba({}aa)\",\n    bg_dark        = \"rgba({}ee)\",\n    accent         = \"rgba({}ee)\",\n    selection      = \"rgba({}ee)\",\n    magenta        = \"rgba({}ee)\",\n    text_dark      = \"rgba({}ff)\",\n    text_muted     = \"rgba({}aa)\",\n    shadow         = 0xee{},\n}}\n",
        d.bg_main, d.bg_pill, d.bg_recessed, d.bg_contrast, d.accent, d.selection, d.magenta,
        d.fg_main, d.fg_muted, d.bg_contrast
    );
    let hypr_dir = cfg_dir.join("hypr");
    if hypr_dir.is_dir() {
        let _ = fs::write(hypr_dir.join("marshmellow.lua"), hypr_lua);
    }

    // 2. Wofi CSS
    let wofi_dir = cfg_dir.join("wofi");
    if wofi_dir.is_dir() {
        let wofi_css = format!(
            "window {{ margin: 0px; border: 3px solid #{}; background-color: {}; border-radius: 14px; font-family: \"Josefin Sans\", sans-serif; font-size: 15px; font-weight: 600; }}\n#outer-box {{ margin: 12px; border: none; background-color: transparent; }}\n#input {{ margin: 4px 4px 12px 4px; padding: 10px 14px; border: 2px solid #{}; border-radius: 10px; color: #{}; background-color: #{}; outline: none; }}\n#input:focus {{ border: 2px solid #{}; }}\n#inner-box {{ margin: 4px; border: none; background-color: transparent; }}\n#scroll {{ margin: 0px; border: none; }}\n#text {{ margin: 4px 8px; border: none; color: #{}; }}\n#img {{ margin-right: 8px; }}\n#entry {{ padding: 8px 12px; margin: 3px 4px; border-radius: 10px; border: none; background-color: transparent; }}\n#entry:selected {{ background-color: #{}; border: 1px solid #{}; outline: none; }}\n#entry:selected #text {{ color: #{}; font-weight: bold; }}\n",
            d.accent, rgba_css(&d.bg_main, 0.95), d.accent, d.fg_on_contrast, d.bg_contrast,
            d.magenta, d.fg_main, d.selection, d.accent, d.fg_on_contrast
        );
        let _ = fs::write(wofi_dir.join("style.css"), wofi_css);
    }

    // 3. SwayNC & SwayOSD
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

    // 4. Kitty
    let kitty_dir = cfg_dir.join("kitty");
    if kitty_dir.is_dir() {
        let mut kitty = format!(
            "background #{}\nforeground #{}\ncursor #{}\ncursor_text_color #{}\nselection_background #{}\nselection_foreground #{}\n",
            d.bg_main, d.fg_main, d.accent, d.bg_main, d.selection, d.fg_main
        );
        for (i, c) in d.terminal.iter().enumerate().take(16) {
            kitty.push_str(&format!("color{} #{}\n", i, c));
        }
        let _ = fs::write(kitty_dir.join("active-theme.conf"), kitty);
    }

    // 5. GTK3 & GTK4 CSS + settings.ini + gsettings + GtkSourceView
    let sel_fg = &d.fg_on_contrast;
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

    // 7. Qt6 & Dolphin (DYNAMIC LilyDynamic.colors — fixes the pink bandaid!)
    let dolphin_was_open = Command::new("pgrep")
        .args(["-x", "dolphin"])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if dolphin_was_open {
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

    let qt_qss_dir = cfg_dir.join("qt6ct").join("qss");
    let _ = fs::create_dir_all(&qt_qss_dir);
    let qss = format!(
        "QWidget {{ color: #{}; }}\nQAbstractItemView, QListView, QTreeView, QAbstractScrollArea, QAbstractScrollArea > QWidget {{ background-color: #{}; color: #{}; }}\nQMenu {{ background-color: {}; color: #{}; border: 2px solid #{}; border-radius: 14px; padding: 6px; }}\nQMenu::item {{ padding: 6px 24px 6px 12px; border-radius: 8px; margin: 2px 4px; background-color: transparent; }}\nQMenu::item:selected {{ background-color: #{}; color: #{}; }}\nQMenu::item:disabled {{ color: #{}; }}\nQMenu::separator {{ height: 1px; background: #{}; margin: 4px 10px; }}\n",
        d.fg_main, d.bg_recessed, d.fg_main, rgba_css(&d.bg_main, 0.95), d.fg_main,
        d.accent, d.selection, sel_fg, d.fg_muted, d.accent
    );
    let _ = fs::write(qt_qss_dir.join("marshmellow-menus.qss"), qss);

    let qt_col_dir = cfg_dir.join("qt6ct").join("colors");
    let _ = fs::create_dir_all(&qt_col_dir);
    let qt_pal_path = qt_col_dir.join("marshmellow.conf");
    let act = format!(
        "#ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}, #ff{}",
        d.fg_main, d.bg_pill, d.bg_main, d.bg_pill, d.bg_recessed, d.bg_main, d.fg_main,
        d.fg_main, d.fg_main, d.bg_recessed, d.bg_main, d.bg_contrast, d.selection,
        sel_fg, d.magenta, d.accent, d.bg_main, d.bg_main, d.bg_contrast, sel_fg,
        d.fg_muted, d.accent
    );
    let _ = fs::write(
        &qt_pal_path,
        format!("[ColorScheme]\nactive_colors={0}\ndisabled_colors={0}\ninactive_colors={0}\n", act),
    );

    let qt_conf_p = cfg_dir.join("qt6ct").join("qt6ct.conf");
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

    // Dynamic KDE ColorScheme (replaces hardcoded MarshmellowDark.colors!)
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

    // 8. btop dynamic theme
    let btop_dir = cfg_dir.join("btop").join("themes");
    if btop_dir.parent().map(|p| p.is_dir()).unwrap_or(false) {
        let _ = fs::create_dir_all(&btop_dir);
        let p_cyan = d.terminal.get(6).map(|s| s.as_str()).unwrap_or("89dceb");
        let p_red = d.terminal.get(1).map(|s| s.as_str()).unwrap_or("f38ba8");
        let btop_theme = format!(
            "theme[main_bg]=\"#{0}\"\ntheme[main_fg]=\"#{1}\"\ntheme[title]=\"#{2}\"\ntheme[hi_fg]=\"#{3}\"\ntheme[selected_bg]=\"#{4}\"\ntheme[selected_fg]=\"#{5}\"\ntheme[inactive_fg]=\"#{6}\"\ntheme[graph_text]=\"#{6}\"\ntheme[meter_bg]=\"#{7}\"\ntheme[proc_misc]=\"#{3}\"\ntheme[cpu_box]=\"#{2}\"\ntheme[mem_box]=\"#{2}\"\ntheme[net_box]=\"#{2}\"\ntheme[proc_box]=\"#{2}\"\ntheme[div_line]=\"#{2}\"\ntheme[temp_start]=\"#{3}\"\ntheme[temp_mid]=\"#{8}\"\ntheme[temp_end]=\"#{9}\"\ntheme[cpu_start]=\"#{3}\"\ntheme[cpu_mid]=\"#{2}\"\ntheme[cpu_end]=\"#{8}\"\ntheme[free_start]=\"#{3}\"\ntheme[free_mid]=\"#{2}\"\ntheme[free_end]=\"#{8}\"\ntheme[cached_start]=\"#{3}\"\ntheme[cached_mid]=\"#{2}\"\ntheme[cached_end]=\"#{8}\"\ntheme[available_start]=\"#{3}\"\ntheme[available_mid]=\"#{2}\"\ntheme[available_end]=\"#{8}\"\ntheme[used_start]=\"#{3}\"\ntheme[used_mid]=\"#{2}\"\ntheme[used_end]=\"#{8}\"\ntheme[download_start]=\"#{3}\"\ntheme[download_mid]=\"#{2}\"\ntheme[download_end]=\"#{8}\"\ntheme[upload_start]=\"#{3}\"\ntheme[upload_mid]=\"#{2}\"\ntheme[upload_end]=\"#{8}\"\ntheme[process_start]=\"#{3}\"\ntheme[process_mid]=\"#{2}\"\ntheme[process_end]=\"#{8}\"\n",
            d.bg_main, d.fg_main, d.accent, p_cyan, d.selection, sel_fg, d.fg_muted, d.bg_recessed,
            d.magenta, p_red
        );
        let _ = fs::write(btop_dir.join("marshmellow.theme"), btop_theme);
    }

    // 8b. Vesktop (Vencord) live theme CSS (supports Discord Visual Refresh + Dark/Darker/Midnight/Light)
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

    // 9. Live desktop reloads
    if dolphin_was_open {
        let _ = Command::new("dolphin").spawn();
    }
    crate::config::hypr_reload_preserving_overrides();
    let _ = Command::new("pkill").args(["-USR1", "-x", "kitty"]).spawn();
    let _ = Command::new("swaync-client").arg("-rs").spawn();
    let _ = Command::new("sh")
        .args(["-c", "pkill -x swayosd-server 2>/dev/null; sleep 0.1; command -v swayosd-server >/dev/null && swayosd-server &"])
        .spawn();

    format!("Applied '{}' globally (folders: {})!", d.name, folder_color)
}
