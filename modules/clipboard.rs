// @dash-id: clipboard
// @dash-title: Clipboard
// @dash-lifecycle: stateless
// @dash-hooks: init, shutdown, visibility
//! Clipboard history tab backed by background `wl-paste --watch` listeners.
//!
//! Tracks up to 200 text entries and 20 PNG images (15 MB cap) entirely in RAM so
//! normal copying never touches the SSD. Only explicitly pinned entries are saved
//! to `~/.config/lily-dashboard/clipboard_pins.json`.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Image, Label, Orientation, Overflow, PolicyType, Popover,
    ScrolledWindow, SearchEntry,
};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

const MAX_TEXT_HISTORY: usize = 200;
const MAX_IMAGE_HISTORY: usize = 20;
const MAX_IMAGE_BYTES: usize = 15 * 1024 * 1024; // 15 MB cap per image

#[derive(Clone)]
enum ClipPayload {
    Text(String),
    ImagePng(Vec<u8>),
}

#[derive(Clone)]
struct ClipItem {
    id: u64,
    payload: ClipPayload,
    pinned: bool,
}

impl ClipItem {
    fn preview_text(&self) -> String {
        match &self.payload {
            ClipPayload::Text(s) => s.replace('\n', " ↵ ").trim().to_string(),
            ClipPayload::ImagePng(bytes) => {
                let kb = bytes.len() / 1024;
                if kb >= 1024 {
                    format!("󰋩 Image ({:.1} MB PNG)", bytes.len() as f64 / (1024.0 * 1024.0))
                } else {
                    format!("󰋩 Image ({} KB PNG)", kb.max(1))
                }
            }
        }
    }

    fn matches_query(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        match &self.payload {
            ClipPayload::Text(s) => s.to_lowercase().contains(query_lower),
            ClipPayload::ImagePng(_) => "image png screenshot".contains(query_lower),
        }
    }

    fn same_payload(&self, other: &ClipPayload) -> bool {
        match (&self.payload, other) {
            (ClipPayload::Text(a), ClipPayload::Text(b)) => a == b,
            (ClipPayload::ImagePng(a), ClipPayload::ImagePng(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
enum SavedPin {
    Text(String),
    ImagePngB64(String),
}

#[derive(Serialize, Deserialize, Default)]
struct SavedPinsFile {
    #[serde(default)]
    pins: Vec<SavedPin>,
}

fn pins_file_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("clipboard_pins.json");
    path
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(((input.len() + 2) / 3) * 4);
    let mut i = 0;
    while i < input.len() {
        let b0 = input[i] as u32;
        let b1 = if i + 1 < input.len() { input[i + 1] as u32 } else { 0 };
        let b2 = if i + 2 < input.len() { input[i + 2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if i + 1 < input.len() {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < input.len() {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn load_saved_pins() -> Vec<ClipPayload> {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    path.push("clipboard_pins.json");
    let Ok(data) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<SavedPinsFile>(&data) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pin in parsed.pins {
        match pin {
            SavedPin::Text(s) if !s.trim().is_empty() => out.push(ClipPayload::Text(s)),
            SavedPin::ImagePngB64(b64) => {
                if let Some(bytes) = decode_base64(&b64) {
                    if !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES {
                        out.push(ClipPayload::ImagePng(bytes));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn save_pinned_items(items: &VecDeque<ClipItem>) {
    let pins: Vec<SavedPin> = items
        .iter()
        .filter(|it| it.pinned)
        .map(|it| match &it.payload {
            ClipPayload::Text(s) => SavedPin::Text(s.clone()),
            ClipPayload::ImagePng(b) => SavedPin::ImagePngB64(encode_base64(b)),
        })
        .collect();

    let file = SavedPinsFile { pins };
    if let Ok(json) = serde_json::to_string_pretty(&file) {
        let _ = fs::write(pins_file_path(), json);
    }
}

struct ClipboardStore {
    next_id: u64,
    items: VecDeque<ClipItem>,
}

impl ClipboardStore {
    fn new() -> Self {
        let mut st = Self {
            next_id: 1,
            items: VecDeque::new(),
        };
        for payload in load_saved_pins() {
            let id = st.next_id;
            st.next_id = st.next_id.wrapping_add(1);
            st.items.push_back(ClipItem {
                id,
                payload,
                pinned: true,
            });
        }
        st
    }

    fn push_payload(&mut self, payload: ClipPayload) -> bool {
        match &payload {
            ClipPayload::Text(t) if t.trim().is_empty() => return false,
            ClipPayload::ImagePng(b) if b.is_empty() || b.len() > MAX_IMAGE_BYTES => return false,
            _ => {}
        }

        // If identical item already exists, preserve its pinned status and bump to front
        let mut was_pinned = false;
        if let Some(pos) = self.items.iter().position(|it| it.same_payload(&payload)) {
            if pos == 0 {
                return false; // Already the most recent entry
            }
            if let Some(existing) = self.items.remove(pos) {
                was_pinned = existing.pinned;
            }
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.items.push_front(ClipItem {
            id,
            payload,
            pinned: was_pinned,
        });

        // Evict oldest unpinned entries beyond RAM caps
        let mut unpinned_text = 0usize;
        let mut unpinned_img = 0usize;
        self.items.retain(|it| {
            if it.pinned {
                return true;
            }
            match it.payload {
                ClipPayload::Text(_) => {
                    unpinned_text += 1;
                    unpinned_text <= MAX_TEXT_HISTORY
                }
                ClipPayload::ImagePng(_) => {
                    unpinned_img += 1;
                    unpinned_img <= MAX_IMAGE_HISTORY
                }
            }
        });

        true
    }
}

static STORE: OnceLock<Mutex<ClipboardStore>> = OnceLock::new();
static WATCHER_CHILDREN: OnceLock<Mutex<Vec<Child>>> = OnceLock::new();
static STORE_VERSION: AtomicU64 = AtomicU64::new(0);
static WATCHER_RUNNING: AtomicBool = AtomicBool::new(false);

fn store() -> &'static Mutex<ClipboardStore> {
    STORE.get_or_init(|| Mutex::new(ClipboardStore::new()))
}

fn bump_version() {
    STORE_VERSION.fetch_add(1, Ordering::Relaxed);
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let bytes = input.trim().as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;

    for &b in bytes {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn spawn_wl_paste_watcher(mime_type: &'static str, is_image: bool) {
    std::thread::spawn(move || {
        let mut cmd = Command::new("wl-paste");
        if !is_image {
            cmd.arg("--no-newline");
        }
        cmd.args([
            "--type",
            mime_type,
            "--watch",
            "sh",
            "-c",
            "base64 -w 0 && printf '\\n'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

        let Ok(mut child) = cmd.spawn() else {
            return;
        };

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return;
        };

        if let Ok(mut list) = WATCHER_CHILDREN.get_or_init(|| Mutex::new(Vec::new())).lock() {
            list.push(child);
        }

        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some(raw_bytes) = decode_base64(trimmed) else {
                continue;
            };
            if raw_bytes.is_empty() {
                continue;
            }
            let payload = if is_image {
                ClipPayload::ImagePng(raw_bytes)
            } else {
                match String::from_utf8(raw_bytes) {
                    Ok(s) => ClipPayload::Text(s),
                    Err(e) => String::from_utf8_lossy(e.as_bytes()).to_string().into(),
                }
            };
            if let Ok(mut st) = store().lock() {
                if st.push_payload(payload) {
                    bump_version();
                }
            }
        }
    });
}

impl From<String> for ClipPayload {
    fn from(s: String) -> Self {
        ClipPayload::Text(s)
    }
}

/// Starts pure-RAM Wayland clipboard watchers (`wl-paste --watch`) on startup if the
/// Clipboard tab is enabled in `config.json`. Never writes anything to SSD.
pub fn init_clipboard_watcher() {
    set_watcher_enabled(crate::config::get().visible_tabs.clipboard);
}

/// Live-starts or kills the background `wl-paste` watchers when toggling Clipboard in Settings.
pub fn set_watcher_enabled(enabled: bool) {
    if enabled {
        if !WATCHER_RUNNING.swap(true, Ordering::SeqCst) {
            spawn_wl_paste_watcher("text/plain;charset=utf-8", false);
            spawn_wl_paste_watcher("image/png", true);
        }
    } else if WATCHER_RUNNING.swap(false, Ordering::SeqCst) {
        shutdown_watchers();
    }
}

/// Cleans up background `wl-paste` watcher children on dashboard exit/reload.
pub fn shutdown_watchers() {
    WATCHER_RUNNING.store(false, Ordering::SeqCst);
    if let Some(lock) = WATCHER_CHILDREN.get() {
        if let Ok(mut children) = lock.lock() {
            for child in children.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            children.clear();
        }
    }
}

fn copy_payload_to_clipboard(payload: ClipPayload) {
    std::thread::spawn(move || {
        let mut cmd = Command::new("wl-copy");
        let bytes = match &payload {
            ClipPayload::Text(s) => s.as_bytes(),
            ClipPayload::ImagePng(b) => {
                cmd.args(["--type", "image/png"]);
                b.as_slice()
            }
        };
        if let Ok(mut child) = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(bytes);
            }
            let _ = child.wait();
        }
    });
}

pub fn build_clipboard_ui(popover_guard: Rc<Cell<usize>>) -> Box {
    init_clipboard_watcher();

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

    let header_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let title_lbl = Label::builder()
        .label("CLIPBOARD (RAM)")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let clear_btn = Button::builder()
        .label("Clear")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Clear unpinned clipboard history from RAM")
        .build();

    header_row.append(&title_lbl);
    header_row.append(&clear_btn);
    root.append(&header_row);

    if !crate::config::bin_exists("wl-paste") {
        let warn_card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(3)
            .css_classes(vec!["plan-check-row".to_string()])
            .build();
        warn_card.append(
            &Label::builder()
                .label("⚠ Missing dependency: wl-clipboard (wl-paste)")
                .halign(Align::Start)
                .css_classes(vec!["plan-step".to_string()])
                .build(),
        );
        warn_card.append(
            &Label::builder()
                .label("Install via your package manager (e.g. sudo pacman -S wl-clipboard).")
                .halign(Align::Start)
                .wrap(true)
                .css_classes(vec!["plan-subnote".to_string()])
                .build(),
        );
        root.append(&warn_card);
    }

    let search_entry = SearchEntry::builder()
        .placeholder_text("Search clipboard...")
        .css_classes(vec!["fav-search".to_string()])
        .build();
    root.append(&search_entry);

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let list_container = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    scrolled.set_child(Some(&list_container));
    crate::module_api::attach_viewport_culler(&scrolled, &list_container);
    root.append(&scrolled);

    let query_rc = Rc::new(RefCell::new(String::new()));
    let last_seen_ver = Rc::new(Cell::new(STORE_VERSION.load(Ordering::Relaxed)));

    populate_clipboard_list(&list_container, &query_rc, &popover_guard);

    // Search filter
    {
        let list_s = list_container.clone();
        let q_s = query_rc.clone();
        let g_s = popover_guard.clone();
        search_entry.connect_search_changed(move |entry| {
            *q_s.borrow_mut() = entry.text().trim().to_lowercase();
            populate_clipboard_list(&list_s, &q_s, &g_s);
        });
    }

    // Clear unpinned items with two-step "Sure?" confirmation
    {
        let clear_armed = Rc::new(Cell::new(false));
        let list_c = list_container.clone();
        let q_c = query_rc.clone();
        let g_c = popover_guard.clone();
        clear_btn.connect_clicked(move |btn| {
            if clear_armed.get() {
                clear_armed.set(false);
                btn.set_label("Clear");
                btn.remove_css_class("active");
                if let Ok(mut st) = store().lock() {
                    st.items.retain(|it| it.pinned);
                    bump_version();
                }
                populate_clipboard_list(&list_c, &q_c, &g_c);
                return;
            }
            clear_armed.set(true);
            btn.set_label("Sure?");
            btn.add_css_class("active");
            let arm_r = clear_armed.clone();
            let btn_r = btn.clone();
            glib::timeout_add_local_once(Duration::from_secs(3), move || {
                if arm_r.get() {
                    arm_r.set(false);
                    btn_r.set_label("Clear");
                    btn_r.remove_css_class("active");
                }
            });
        });
    }

    // Auto-refresh UI while this tab widget is alive; exits cleanly when tab unloads
    {
        let list_weak = list_container.downgrade();
        let q_poll = query_rc.clone();
        let g_poll = popover_guard.clone();
        let ver_poll = last_seen_ver.clone();
        glib::timeout_add_local(Duration::from_millis(250), move || {
            let Some(list_box) = list_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let cur_ver = STORE_VERSION.load(Ordering::Relaxed);
            if cur_ver != ver_poll.get() {
                ver_poll.set(cur_ver);
                populate_clipboard_list(&list_box, &q_poll, &g_poll);
            }
            glib::ControlFlow::Continue
        });
    }

    root
}

fn populate_clipboard_list(
    container: &Box,
    query_rc: &Rc<RefCell<String>>,
    popover_guard: &Rc<Cell<usize>>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let query = query_rc.borrow().clone();
    let all_items: Vec<ClipItem> = store()
        .lock()
        .map(|st| st.items.iter().cloned().collect())
        .unwrap_or_default();

    let pinned_items: Vec<ClipItem> = all_items
        .iter()
        .filter(|it| it.pinned && it.matches_query(&query))
        .cloned()
        .collect();

    let history_items: Vec<ClipItem> = all_items
        .iter()
        .filter(|it| !it.pinned && it.matches_query(&query))
        .take(80)
        .cloned()
        .collect();

    let mut row_idx = 0usize;

    if !pinned_items.is_empty() {
        let pin_hdr = Label::builder()
            .label("PINNED")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(2)
            .build();
        container.append(&pin_hdr);

        for item in &pinned_items {
            let row = build_clip_row(item, container, query_rc, popover_guard);
            if row_idx >= 18 {
                row.add_css_class("lily-settled");
                row.add_css_class("lily-offscreen");
            }
            row_idx += 1;
            container.append(&row);
        }
    }

    if !history_items.is_empty() {
        let hist_hdr = Label::builder()
            .label("HISTORY")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(4)
            .build();
        container.append(&hist_hdr);

        for item in &history_items {
            let row = build_clip_row(item, container, query_rc, popover_guard);
            if row_idx >= 18 {
                row.add_css_class("lily-settled");
                row.add_css_class("lily-offscreen");
            }
            row_idx += 1;
            container.append(&row);
        }
    }

    if let Some(sc) = container
        .ancestor(ScrolledWindow::static_type())
        .and_then(|w| w.downcast::<ScrolledWindow>().ok())
    {
        crate::module_api::refresh_viewport_culler(&sc, container);
    }

    if pinned_items.is_empty() && history_items.is_empty() {
        let empty = Label::builder()
            .label(if query.is_empty() {
                "Clipboard is empty.\nCopy text or screenshots to collect them in RAM!"
            } else {
                "No matching clipboard items."
            })
            .justify(gtk4::Justification::Center)
            .wrap(true)
            .css_classes(vec!["fav-score-badge".to_string()])
            .margin_top(20)
            .build();
        container.append(&empty);
    }
}

fn png_bytes_to_thumb(png_bytes: &[u8]) -> Option<gtk4::gdk::Texture> {
    use gtk4::gdk_pixbuf::{InterpType, PixbufLoader};
    let loader = PixbufLoader::new();
    loader.write(png_bytes).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    let w = pixbuf.width();
    let h = pixbuf.height();
    if w <= 0 || h <= 0 {
        return None;
    }
    let side = w.min(h);
    let square = pixbuf.new_subpixbuf((w - side) / 2, (h - side) / 2, side, side);
    let scaled = square.scale_simple(36, 36, InterpType::Bilinear)?;
    Some(gtk4::gdk::Texture::for_pixbuf(&scaled))
}

fn build_clip_row(
    item: &ClipItem,
    container: &Box,
    query_rc: &Rc<RefCell<String>>,
    popover_guard: &Rc<Cell<usize>>,
) -> Box {
    let row_box = Box::builder().orientation(Orientation::Horizontal).build();

    let mut classes = vec!["fav-item-btn".to_string()];
    if item.pinned {
        classes.push("pinned".to_string());
    }

    let preview = item.preview_text();
    let tooltip = match &item.payload {
        ClipPayload::Text(full) => {
            let truncated: String = full.chars().take(400).collect();
            format!("Left-click to copy | Right-click to Pin or Delete\n\n{}", truncated)
        }
        ClipPayload::ImagePng(_) => {
            format!("Left-click to copy image | Right-click to Pin or Delete\n{}", preview)
        }
    };

    let btn = Button::builder()
        .css_classes(classes)
        .hexpand(true)
        .tooltip_text(&tooltip)
        .build();

    let row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();

    if let ClipPayload::ImagePng(ref png_bytes) = item.payload {
        if let Some(tex) = png_bytes_to_thumb(png_bytes) {
            let thumb_frame = Box::builder()
                .css_classes(vec!["music-art-frame".to_string()])
                .width_request(36)
                .height_request(36)
                .valign(Align::Center)
                .overflow(Overflow::Hidden)
                .build();
            let img = Image::builder()
                .paintable(&tex)
                .pixel_size(36)
                .width_request(36)
                .height_request(36)
                .build();
            thumb_frame.append(&img);
            row.append(&thumb_frame);
        }
    }

    let lbl = Label::builder()
        .label(&preview)
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-body".to_string()])
        .build();

    let orig_badge = if item.pinned {
        "PIN"
    } else if matches!(item.payload, ClipPayload::ImagePng(_)) {
        "IMG"
    } else {
        "Copy"
    };

    let badge = Label::builder()
        .label(orig_badge)
        .css_classes(vec!["fav-score-badge".to_string()])
        .valign(Align::Center)
        .build();

    row.append(&lbl);
    row.append(&badge);
    btn.set_child(Some(&row));

    // Left-click copies immediately
    let payload_click = item.payload.clone();
    let badge_click = badge.clone();
    btn.connect_clicked(move |_| {
        copy_payload_to_clipboard(payload_click.clone());
        badge_click.set_label("Copied!");
        let b_reset = badge_click.clone();
        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
            b_reset.set_label(orig_badge);
        });
    });

    // Right-click lazy context menu: Copy | Pin / Unpin | Delete
    let right_click = GestureClick::builder().button(3).build();
    let row_weak = row_box.downgrade();
    let item_id = item.id;
    let is_pinned = item.pinned;
    let payload_ctx = item.payload.clone();
    let badge_ctx = badge.clone();
    let list_ctx = container.clone();
    let q_ctx = query_rc.clone();
    let g_ctx = popover_guard.clone();

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
            let pw = p.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(pop) = pw.upgrade() {
                    pop.unparent();
                }
            });
        });

        let gm = g_ctx.clone();
        popover.connect_map(move |_| gm.set(gm.get() + 1));
        let gu = g_ctx.clone();
        popover.connect_unmap(move |_| {
            let g = gu.clone();
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                g.set(g.get().saturating_sub(1));
            });
        });

        let menu_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .build();

        let copy_btn = Button::builder()
            .label("Copy")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();
        let pin_btn = Button::builder()
            .label(if is_pinned { "Unpin" } else { "Pin to Top" })
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();
        let del_btn = Button::builder()
            .label("Delete")
            .css_classes(vec!["plan-delete-menu-btn".to_string()])
            .build();

        menu_box.append(&copy_btn);
        menu_box.append(&pin_btn);
        menu_box.append(&del_btn);
        popover.set_child(Some(&menu_box));

        let pl_c = payload_ctx.clone();
        let bc = badge_ctx.clone();
        let pc = popover.downgrade();
        copy_btn.connect_clicked(move |_| {
            if let Some(p) = pc.upgrade() {
                p.popdown();
            }
            copy_payload_to_clipboard(pl_c.clone());
            bc.set_label("Copied!");
            let br = bc.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                br.set_label(orig_badge);
            });
        });

        let lc = list_ctx.clone();
        let qc = q_ctx.clone();
        let gc = g_ctx.clone();
        let pp = popover.downgrade();
        pin_btn.connect_clicked(move |_| {
            if let Some(p) = pp.upgrade() {
                p.popdown();
            }
            if let Ok(mut st) = store().lock() {
                if let Some(entry) = st.items.iter_mut().find(|it| it.id == item_id) {
                    entry.pinned = !entry.pinned;
                    save_pinned_items(&st.items);
                    bump_version();
                }
            }
            populate_clipboard_list(&lc, &qc, &gc);
        });

        let lc = list_ctx.clone();
        let qc = q_ctx.clone();
        let gc = g_ctx.clone();
        let pd = popover.downgrade();
        del_btn.connect_clicked(move |_| {
            if let Some(p) = pd.upgrade() {
                p.popdown();
            }
            if let Ok(mut st) = store().lock() {
                let was_pinned = st
                    .items
                    .iter()
                    .find(|it| it.id == item_id)
                    .map(|it| it.pinned)
                    .unwrap_or(false);
                st.items.retain(|it| it.id != item_id);
                if was_pinned {
                    save_pinned_items(&st.items);
                }
                bump_version();
            }
            populate_clipboard_list(&lc, &qc, &gc);
        });

        popover.popup();
    });
    btn.add_controller(right_click);

    row_box.append(&btn);
    row_box
}

pub fn init() {
    init_clipboard_watcher();
}

pub fn shutdown() {
    shutdown_watchers();
}

pub fn set_enabled(enabled: bool) {
    set_watcher_enabled(enabled);
}

pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    crate::module_api::ModuleHandle::simple(build_clipboard_ui(ctx.popover_guard.clone()))
}
