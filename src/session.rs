//! Hyprland window session saver and restorer.
//!
//! Saves and restores mapped windows (workspace, geometry, float/fullscreen flags,
//! and `/proc/<pid>/cmdline`). Controlled by the sidebar's Load / Save / Auto-save
//! buttons; auto-save defaults to off and ignores window-title changes when enabled.

use gtk4::glib;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static AUTO_SAVE_ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Default)]
struct SessionPrefs {
    #[serde(default)]
    auto_save: bool,
}

#[derive(Deserialize)]
struct HyprWorkspace {
    id: i32,
}

#[derive(Deserialize)]
struct HyprClient {
    mapped: bool,
    at: [i32; 2],
    size: [i32; 2],
    workspace: HyprWorkspace,
    floating: bool,
    #[serde(default)]
    fullscreen: i32,
    class: String,
    #[serde(default)]
    title: String,
    pid: i32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedWindow {
    pub workspace_id: i32,
    pub floating: bool,
    pub fullscreen: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub class: String,
    #[serde(default)]
    pub title: String,
    pub command: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct SessionData {
    pub windows: Vec<SavedWindow>,
}

fn config_dir() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path
}

fn current_session_path() -> PathBuf {
    config_dir().join("current_session.json")
}

fn previous_session_path() -> PathBuf {
    config_dir().join("previous_session.json")
}

fn prefs_path() -> PathBuf {
    config_dir().join("session_prefs.json")
}

fn load_prefs() -> SessionPrefs {
    if let Ok(data) = fs::read_to_string(prefs_path()) {
        if let Ok(p) = serde_json::from_str::<SessionPrefs>(&data) {
            return p;
        }
    }
    SessionPrefs::default()
}

pub fn is_auto_save_enabled() -> bool {
    AUTO_SAVE_ENABLED.load(Ordering::Relaxed)
}

pub fn set_auto_save_enabled(enabled: bool) {
    AUTO_SAVE_ENABLED.store(enabled, Ordering::Relaxed);
    let prefs = SessionPrefs { auto_save: enabled };
    if let Ok(json) = serde_json::to_string_pretty(&prefs) {
        let _ = fs::write(prefs_path(), json);
    }
}

/// Captures the current Hyprland windows and writes them as the saved session immediately.
pub fn save_session_now() -> usize {
    let Some(session) = capture_hyprland_clients() else {
        return 0;
    };
    if session.windows.is_empty() {
        return 0;
    }
    let count = session.windows.len();
    if let Ok(json) = serde_json::to_string_pretty(&session) {
        let _ = fs::write(current_session_path(), &json);
        let _ = fs::write(previous_session_path(), &json);
    }
    count
}

/// Title-independent layout signature so browser tabs / song titles / editor filenames
/// changing never trigger unnecessary SSD writes.
fn layout_signature(session: &SessionData) -> Vec<(i32, bool, bool, [i32; 2], [i32; 2], String, String)> {
    let mut sig: Vec<_> = session
        .windows
        .iter()
        .map(|w| {
            (
                w.workspace_id,
                w.floating,
                w.fullscreen,
                w.at,
                w.size,
                w.class.clone(),
                w.command.clone(),
            )
        })
        .collect();
    sig.sort();
    sig
}

pub fn init_session_tracker() {
    let prefs = load_prefs();
    AUTO_SAVE_ENABLED.store(prefs.auto_save, Ordering::Relaxed);

    // Only rotate current_session.json -> previous_session.json when auto-save is enabled
    // and this looks like a fresh boot (>30s since last write), so manual saves and
    // "Reload Dashboard" never overwrite your saved session.
    if prefs.auto_save {
        let curr = current_session_path();
        let prev = previous_session_path();
        let should_rotate = if let Ok(meta) = fs::metadata(&curr) {
            if let Ok(modified) = meta.modified() {
                modified.elapsed().unwrap_or(Duration::from_secs(0)) > Duration::from_secs(30)
            } else {
                true
            }
        } else {
            false
        };

        if should_rotate {
            if let Ok(data) = fs::read_to_string(&curr) {
                if let Ok(parsed) = serde_json::from_str::<SessionData>(&data) {
                    if !parsed.windows.is_empty() {
                        let _ = fs::copy(&curr, &prev);
                    }
                }
            }
        }
    }

    std::thread::spawn(move || {
        let mut last_sig: Option<Vec<(i32, bool, bool, [i32; 2], [i32; 2], String, String)>> = None;
        loop {
            std::thread::sleep(Duration::from_secs(30));
            if !AUTO_SAVE_ENABLED.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(session) = capture_hyprland_clients() {
                if !session.windows.is_empty() {
                    let sig = layout_signature(&session);
                    if last_sig.as_ref() != Some(&sig) {
                        if let Ok(json) = serde_json::to_string_pretty(&session) {
                            if fs::write(current_session_path(), &json).is_ok() {
                                last_sig = Some(sig);
                            }
                        }
                    }
                }
            }
        }
    });
}

fn capture_hyprland_clients() -> Option<SessionData> {
    let output = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let clients: Vec<HyprClient> = serde_json::from_slice(&output.stdout).ok()?;
    let mut windows = Vec::new();

    for c in clients {
        if !c.mapped
            || c.workspace.id <= 0
            || c.class.is_empty()
            || c.class == "com.lily.dashboard"
        {
            continue;
        }

        let command = resolve_launch_command(c.pid, &c.class);
        if command.is_empty() {
            continue;
        }

        windows.push(SavedWindow {
            workspace_id: c.workspace.id,
            floating: c.floating,
            fullscreen: c.fullscreen > 0,
            at: c.at,
            size: c.size,
            class: c.class,
            title: c.title,
            command,
        });
    }

    Some(SessionData { windows })
}

/// Reads `/proc/<pid>/cmdline` and returns the argument vector
fn read_proc_cmdline(pid: i32) -> Vec<String> {
    if pid <= 0 {
        return Vec::new();
    }
    let path = format!("/proc/{}/cmdline", pid);
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect()
}

/// Reads direct child PIDs of `pid` from `/proc/<pid>/task/<pid>/children`
fn get_child_pids(pid: i32) -> Vec<i32> {
    let path = format!("/proc/{}/task/{}/children", pid, pid);
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .split_whitespace()
        .filter_map(|s| s.parse::<i32>().ok())
        .collect()
}

fn is_shell_binary(bin_name: &str) -> bool {
    matches!(
        bin_name,
        "bash" | "zsh" | "fish" | "sh" | "dash" | "nu" | "-bash" | "-zsh" | "-fish"
    )
}

/// Walks the process tree inside a terminal emulator to find if a TUI/CLI program
/// (like `btop`, `cava`, `lava`, `nvim`, `yazi`, etc.) is running inside it!
/// Only allow known, safe interactive TUI/visualizer apps to be auto-restored inside terminals.
/// Commands like `dd`, `rm`, `sudo`, `pacman`, or compilers will NEVER be auto-executed!
fn is_safe_tui_app(cmd_line: &str) -> bool {
    let first_word = cmd_line.split_whitespace().next().unwrap_or("");
    let bin_name = std::path::Path::new(first_word)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first_word);

    matches!(
        bin_name,
        "btop"
            | "htop"
            | "nvtop"
            | "top"
            | "cava"
            | "lava"
            | "lavat"
            | "pipes.sh"
            | "cbonsai"
            | "tty-clock"
            | "peaclock"
            | "asciiquarium"
            | "cmatrix"
            | "yazi"
            | "ranger"
            | "lf"
            | "nnn"
            | "nvim"
            | "vim"
            | "helix"
            | "hx"
            | "micro"
            | "nano"
            | "lazygit"
            | "tmux"
            | "zellij"
            | "pulsemixer"
            | "alsamixer"
            | "wiremix"
            | "bluetuith"
            | "impala"
            | "ncmpcpp"
            | "cmus"
            | "spotify_player"
    )
}

fn find_foreground_terminal_command(term_pid: i32) -> Option<String> {
    // 1. Check if the terminal itself was invoked with `-e` (e.g. `kitty -e btop`)
    let term_args = read_proc_cmdline(term_pid);
    if term_args.len() > 1 {
        for (i, arg) in term_args.iter().enumerate().skip(1) {
            if arg == "-e" || arg == "--" {
                if i + 1 < term_args.len() {
                    let sub_cmd = term_args[i + 1..].join(" ");
                    if is_safe_tui_app(&sub_cmd) {
                        return Some(sub_cmd);
                    }
                }
            }
        }
    }

    // 2. Otherwise, inspect live child processes of the terminal
    for child_pid in get_child_pids(term_pid) {
        let child_args = read_proc_cmdline(child_pid);
        if let Some(first) = child_args.first() {
            let bin = std::path::Path::new(first)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| first.clone());

            if is_shell_binary(&bin) {
                for grand_pid in get_child_pids(child_pid) {
                    let grand_args = read_proc_cmdline(grand_pid);
                    let sub_cmd = grand_args.join(" ");
                    if is_safe_tui_app(&sub_cmd) {
                        return Some(sub_cmd);
                    }
                }
            } else {
                let sub_cmd = child_args.join(" ");
                if is_safe_tui_app(&sub_cmd) {
                    return Some(sub_cmd);
                }
            }
        }
    }

    None
}

fn resolve_launch_command(pid: i32, class: &str) -> String {
    let class_lower = class.to_lowercase();

    // 1. Special handling for terminal emulators: check what program is running INSIDE them!
    let terminals = ["kitty", "alacritty", "foot", "wezterm", "ghostty"];
    for term in terminals {
        if class_lower == term || class_lower.ends_with(&format!(".{}", term)) {
            if let Some(inner_cmd) = find_foreground_terminal_command(pid) {
                return format!("{} -e {}", term, inner_cmd);
            }
            return term.to_string();
        }
    }

    // 2. Standard desktop / Electron applications
    let known_map = [
        ("code-oss", "code-oss"),
        ("code", "code"),
        ("vscodium", "codium"),
        ("firefox", "firefox"),
        ("librewolf", "librewolf"),
        ("zen", "zen-browser"),
        ("chromium", "chromium"),
        ("brave-browser", "brave"),
        ("discord", "discord"),
        ("vesktop", "vesktop"),
        ("spotify", "spotify"),
        ("steam", "steam"),
        ("obs", "obs"),
        ("vlc", "vlc"),
        ("mpv", "mpv"),
        ("thunar", "thunar"),
        ("nautilus", "nautilus"),
        ("dolphin", "dolphin"),
        ("obsidian", "obsidian"),
    ];

    for (cls, cmd) in known_map {
        if class_lower == cls || class_lower.ends_with(&format!(".{}", cls)) {
            return cmd.to_string();
        }
    }

    // 3. Fallback to `/proc/<pid>/cmdline`
    let parts = read_proc_cmdline(pid);
    if let Some(first) = parts.first() {
        let is_electron_sub = parts.iter().any(|arg| arg.starts_with("--type="));
        if !is_electron_sub {
            return first.clone();
        }
    }

    class_lower
}

pub fn restore_previous_session() -> usize {
    let prev = previous_session_path();
    let curr = current_session_path();

    // Try previous_session.json first; if it doesn't exist or has 0 windows, fall back to current_session.json
    let mut session_opt: Option<SessionData> = None;

    if prev.exists() {
        if let Ok(data) = fs::read_to_string(&prev) {
            if let Ok(parsed) = serde_json::from_str::<SessionData>(&data) {
                if !parsed.windows.is_empty() {
                    session_opt = Some(parsed);
                }
            }
        }
    }

    if session_opt.is_none() && curr.exists() {
        if let Ok(data) = fs::read_to_string(&curr) {
            if let Ok(parsed) = serde_json::from_str::<SessionData>(&data) {
                if !parsed.windows.is_empty() {
                    session_opt = Some(parsed);
                }
            }
        }
    }

    let Some(session) = session_opt else {
        return 0;
    };

    // Snapshot currently open windows right now so we can skip any window that is ALREADY open!
    let currently_open = capture_hyprland_clients()
        .map(|s| s.windows)
        .unwrap_or_default();

    // Track which currently-open windows we've matched so if you had 2 empty terminals saved
    // and only 1 is currently open, it skips 1 and opens the 2nd!
    let mut matched_open_indices: HashSet<usize> = HashSet::new();
    let mut seen_single_instance: HashSet<(i32, String)> = HashSet::new();
    let mut restored_count = 0;

    for win in session.windows {
        // COMPARATOR: Check if an identical window (same workspace + same command/class) is already open!
        let already_open_idx = currently_open.iter().enumerate().find_map(|(idx, open_w)| {
            if matched_open_indices.contains(&idx) {
                return None;
            }
            let same_ws = open_w.workspace_id == win.workspace_id;
            let same_cmd = open_w.command == win.command;
            let same_class_and_title = open_w.class.eq_ignore_ascii_case(&win.class)
                && !win.title.is_empty()
                && open_w.title == win.title;

            if same_ws && (same_cmd || same_class_and_title) {
                Some(idx)
            } else {
                None
            }
        });

        if let Some(idx) = already_open_idx {
            // This exact window is already running on this workspace (e.g. from your startup script)—skip it!
            matched_open_indices.insert(idx);
            continue;
        }

        let is_browser_or_ide = matches!(
            win.command.as_str(),
            "firefox"
                | "zen-browser"
                | "brave"
                | "chromium"
                | "discord"
                | "vesktop"
                | "spotify"
                | "steam"
        );

        if is_browser_or_ide {
            let key = (win.workspace_id, win.command.clone());
            if !seen_single_instance.insert(key) {
                continue;
            }
        }

        let mut rules = vec![format!("workspace {} silent", win.workspace_id)];
        if win.floating {
            rules.push("float".to_string());
            rules.push(format!("move {} {}", win.at[0], win.at[1]));
            rules.push(format!("size {} {}", win.size[0], win.size[1]));
        }
        if win.fullscreen {
            rules.push("fullscreen".to_string());
        }

        crate::config::hypr_exec_with_rules(&rules.join("; "), &win.command);

        restored_count += 1;
    }

    restored_count
}