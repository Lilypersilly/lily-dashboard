//! Per-style UI sound pack playback and `.sounds` manifest manager.
//!
//! Resolves event bindings from the active style's `<stylename>.sounds` file
//! (or an explicitly unlinked sound style), caches resolved audio file paths in RAM
//! so hover/click playback does zero disk I/O, and plays clips non-blockingly via
//! `pw-play` -> `paplay` -> `ffplay` in a background worker thread.

use crate::config::{self, AppConfig};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SoundEvent {
    DashOpen,
    DashClose,
    ButtonHover,
    ButtonClick,
    TabSwitch,
    PaneOpen,
    PaneClose,
    DestructiveClick,
    VimMode,
}

pub const ALL_EVENTS: &[(SoundEvent, &str, &str)] = &[
    (SoundEvent::DashOpen, "dash_open", "Dashboard Open"),
    (SoundEvent::DashClose, "dash_close", "Dashboard Close"),
    (SoundEvent::ButtonHover, "button_hover", "Button Hover"),
    (SoundEvent::ButtonClick, "button_click", "Button Click"),
    (SoundEvent::TabSwitch, "tab_switch", "Tab Switch"),
    (SoundEvent::PaneOpen, "pane_open", "Pane Open"),
    (SoundEvent::PaneClose, "pane_close", "Pane Close"),
    (SoundEvent::DestructiveClick, "destructive_click", "Destructive Click"),
    (SoundEvent::VimMode, "vim_mode", "Vim Mode"),
];

impl SoundEvent {
    pub fn key(self) -> &'static str {
        ALL_EVENTS
            .iter()
            .find(|(ev, _, _)| *ev == self)
            .map(|(_, k, _)| *k)
            .unwrap_or("button_click")
    }

    pub fn from_key(raw: &str) -> Option<Self> {
        let norm = raw.trim().to_ascii_lowercase().replace('-', "_");
        ALL_EVENTS
            .iter()
            .find(|(_, k, _)| *k == norm)
            .map(|(ev, _, _)| *ev)
    }

    fn debounce_ms(self) -> u64 {
        match self {
            SoundEvent::ButtonHover => 35,
            SoundEvent::ButtonClick => 20,
            _ => 50,
        }
    }
}

#[derive(Clone, Debug)]
struct ResolvedBinding {
    paths: Vec<PathBuf>,
    gain: f64,
}

#[derive(Default)]
struct CachedSoundPack {
    style_key: Option<String>,
    bindings: HashMap<SoundEvent, ResolvedBinding>,
}

static ACTIVE_PACK: OnceLock<Mutex<CachedSoundPack>> = OnceLock::new();
static DEBOUNCE_STATE: OnceLock<Mutex<HashMap<SoundEvent, Instant>>> = OnceLock::new();
static VARIATION_COUNTER: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_PLAYERS: AtomicUsize = AtomicUsize::new(0);

const MAX_CONCURRENT_PLAYERS: usize = 6;

pub fn active_sound_style_key(cfg: &AppConfig) -> Option<String> {
    if cfg.sound_unlinked {
        let trimmed = cfg.sound_style.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else {
        let trimmed = cfg.button_style.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

/// Parses a style's `.sounds` file into raw `event -> value` strings (preserving any `@ gain`
/// or comma-separated variation list for UI inspection).
pub fn parse_sounds_file(style_key: &str) -> HashMap<SoundEvent, String> {
    let mut map = HashMap::new();
    let path = config::style_sounds_path(style_key);
    let Ok(raw) = fs::read_to_string(&path) else {
        return map;
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let Some(ev) = SoundEvent::from_key(k) else {
            continue;
        };
        // Strip optional inline `# comment` only if preceded by whitespace
        let val_no_comment = if let Some(idx) = v.find(" #") {
            &v[..idx]
        } else {
            v
        };
        let val = val_no_comment.trim();
        if !val.is_empty() && !val.eq_ignore_ascii_case("none") {
            map.insert(ev, val.to_string());
        }
    }
    map
}

fn split_value_and_gain(raw: &str) -> (&str, f64) {
    if let Some((left, right)) = raw.rsplit_once('@') {
        if let Ok(g) = right.trim().parse::<f64>() {
            if g.is_finite() {
                return (left.trim(), g.clamp(0.0, 2.0));
            }
        }
    }
    (raw.trim(), 1.0)
}

fn resolve_binding_paths(style_dir: &Path, raw_list: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for part in raw_list.split(',') {
        let clean = part
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if clean.is_empty() || clean.eq_ignore_ascii_case("none") {
            continue;
        }
        let candidate = if Path::new(clean).is_absolute() {
            Some(PathBuf::from(clean))
        } else {
            config::resolve_case_insensitive(style_dir, clean)
        };
        let Some(resolved) = candidate else {
            continue;
        };
        if resolved.is_dir() {
            if let Ok(rd) = fs::read_dir(&resolved) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_file() && config::is_supported_audio_file(&p) {
                        out.push(p);
                    }
                }
            }
        } else if resolved.is_file() {
            out.push(resolved);
        }
    }
    out
}

pub fn reload_active_pack() {
    let cfg = config::get();
    let active_key = active_sound_style_key(&cfg);
    let mut new_bindings = HashMap::new();

    if let Some(ref key) = active_key {
        let style_dir = config::style_dir(key);
        let raw_map = parse_sounds_file(key);
        for (ev, raw_val) in raw_map {
            let (list_part, gain) = split_value_and_gain(&raw_val);
            let paths = resolve_binding_paths(&style_dir, list_part);
            if !paths.is_empty() {
                new_bindings.insert(ev, ResolvedBinding { paths, gain });
            }
        }
    }

    let lock = ACTIVE_PACK.get_or_init(|| Mutex::new(CachedSoundPack::default()));
    if let Ok(mut guard) = lock.lock() {
        guard.style_key = active_key;
        guard.bindings = new_bindings;
    }
}

pub fn save_sound_binding(
    style_key: &str,
    event: SoundEvent,
    rel_path: Option<&str>,
) -> Result<(), String> {
    let sounds_path = config::style_sounds_path(style_key);
    if let Some(parent) = sounds_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let existing = fs::read_to_string(&sounds_path)
        .unwrap_or_else(|_| config::STARTER_SOUNDS_TEMPLATE.to_string());

    let target_key = event.key();
    let new_clean = rel_path.map(|s| s.trim()).filter(|s| !s.is_empty());

    let mut lines_out: Vec<String> = Vec::new();
    let mut replaced = false;

    for line in existing.lines() {
        let trimmed = line.trim();
        let uncommented = trimmed.strip_prefix('#').map(|s| s.trim()).unwrap_or(trimmed);
        let is_matching_key = uncommented
            .split_once('=')
            .and_then(|(k, _)| SoundEvent::from_key(k))
            .map(|ev| ev == event)
            .unwrap_or(false);

        if is_matching_key && !trimmed.starts_with("# Syntax:") {
            if !replaced {
                replaced = true;
                if let Some(val) = new_clean {
                    lines_out.push(format!("{} = {}", target_key, val));
                } else {
                    lines_out.push(format!("# {} = ", target_key));
                }
            }
            continue;
        }
        lines_out.push(line.to_string());
    }

    if !replaced {
        if let Some(val) = new_clean {
            lines_out.push(format!("{} = {}", target_key, val));
        }
    }

    let mut final_str = lines_out.join("\n");
    if !final_str.ends_with('\n') {
        final_str.push('\n');
    }

    fs::write(&sounds_path, final_str)
        .map_err(|e| format!("Failed writing {}: {}", sounds_path.display(), e))?;
    reload_active_pack();
    Ok(())
}

fn spawn_audio_file(path: PathBuf, volume: f64) {
    let prev = ACTIVE_PLAYERS.fetch_add(1, Ordering::AcqRel);
    if prev >= MAX_CONCURRENT_PLAYERS {
        ACTIVE_PLAYERS.fetch_sub(1, Ordering::AcqRel);
        return;
    }

    let vol = volume.clamp(0.0, 1.0);
    std::thread::spawn(move || {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                ACTIVE_PLAYERS.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _guard = Guard;

        let path_str = path.to_string_lossy().to_string();
        let pw_vol = format!("{:.3}", vol);
        let pw_ok = Command::new("pw-play")
            .args([
                "--media-role=Notification",
                "--latency",
                "20ms",
                "--volume",
                &pw_vol,
                &path_str,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if pw_ok {
            return;
        }

        let pa_vol = ((vol * 65536.0).round() as i64).clamp(0, 65536).to_string();
        let pa_ok = Command::new("paplay")
            .args([
                "--client-name=lily-dashboard",
                &format!("--volume={}", pa_vol),
                &path_str,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if pa_ok {
            return;
        }

        let ff_vol = ((vol * 100.0).round() as i64).clamp(0, 100).to_string();
        let _ = Command::new("ffplay")
            .args(["-nodisp", "-autoexit", "-loglevel", "quiet", "-volume", &ff_vol, &path_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

pub fn play(event: SoundEvent) {
    let cfg = config::get();
    if !cfg.sounds_enabled || cfg.sound_volume <= 0.005 {
        return;
    }

    let lock = ACTIVE_PACK.get_or_init(|| Mutex::new(CachedSoundPack::default()));
    let picked: Option<(PathBuf, f64)> = {
        let Ok(guard) = lock.lock() else {
            return;
        };
        let binding = guard.bindings.get(&event).or_else(|| {
            if matches!(event, SoundEvent::DestructiveClick | SoundEvent::TabSwitch) {
                guard.bindings.get(&SoundEvent::ButtonClick)
            } else {
                None
            }
        });
        let Some(b) = binding else {
            return;
        };
        if b.paths.is_empty() {
            return;
        }
        let idx = if b.paths.len() == 1 {
            0
        } else {
            VARIATION_COUNTER.fetch_add(1, Ordering::Relaxed) % b.paths.len()
        };
        Some((b.paths[idx].clone(), b.gain))
    };

    let Some((path, gain)) = picked else {
        return;
    };

    let deb_lock = DEBOUNCE_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut deb) = deb_lock.lock() {
        let now = Instant::now();
        let min_gap = Duration::from_millis(event.debounce_ms());
        if let Some(last) = deb.get(&event) {
            if now.duration_since(*last) < min_gap {
                return;
            }
        }
        deb.insert(event, now);
    }

    let final_vol = (cfg.sound_volume.clamp(0.0, 1.0) * gain.clamp(0.0, 2.0)).clamp(0.0, 1.0);
    if final_vol <= 0.005 {
        return;
    }
    spawn_audio_file(path, final_vol);
}

pub fn play_by_name(name: &str) {
    if let Some(ev) = SoundEvent::from_key(name) {
        play(ev);
    }
}

/// Immediately previews an audio file (ignoring debounce) using current volume (or 50% if muted).
pub fn preview_sound_file(path: &Path) {
    if !path.is_file() {
        return;
    }
    let cfg = config::get();
    let vol = if cfg.sound_volume <= 0.01 {
        0.5
    } else {
        cfg.sound_volume.clamp(0.0, 1.0)
    };
    spawn_audio_file(path.to_path_buf(), vol);
}

/// Helper to test whichever sound best represents the active sound pack from Settings/Styling.
pub fn preview_active_pack() {
    let lock = ACTIVE_PACK.get_or_init(|| Mutex::new(CachedSoundPack::default()));
    let path_opt = lock.lock().ok().and_then(|guard| {
        for (ev, _, _) in ALL_EVENTS {
            if let Some(b) = guard.bindings.get(ev) {
                if let Some(p) = b.paths.first() {
                    return Some(p.clone());
                }
            }
        }
        None
    });
    if let Some(p) = path_opt {
        preview_sound_file(&p);
    }
}

/// Formats a raw `.sounds` value for compact button labels (strips path prefix and gain).
pub fn short_binding_label(raw: &str) -> String {
    let (list_part, _gain) = split_value_and_gain(raw);
    let first = list_part.split(',').next().unwrap_or("").trim();
    if first.is_empty() {
        return "None".to_string();
    }
    let fname = Path::new(first)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| first.to_string());
    if list_part.contains(',') {
        format!("{} (+)", fname)
    } else {
        fname
    }
}
