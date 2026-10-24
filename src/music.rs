use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Entry, GestureClick, Label, Orientation, PolicyType, Popover, Scale,
    ScrolledWindow, SearchEntry, Stack,
};
use serde::Deserialize;
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MPV_SOCKET: &str = "/tmp/lily-ymusic-mpv.sock";

#[derive(Clone, Debug)]
pub struct PlaylistItem {
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TrackItem {
    pub id: String,
    pub title: String,
    pub uploader: String,
    pub duration_str: String,
}

enum WorkerMsg {
    PlaylistsLoaded(Result<(String, Vec<PlaylistItem>), String>),
    PlaylistTracksLoaded(Result<(String, Vec<TrackItem>), String>),
}

struct PlayerState {
    mpv_child: Option<Child>,
    is_paused: bool,
    current_track: Option<TrackItem>,
    queue: Vec<TrackItem>,
    queue_idx: usize,
    mpris: Option<MediaControls>,
}

impl PlayerState {
    fn new() -> Self {
        let config = PlatformConfig {
            dbus_name: "lily_ymusic",
            display_name: "Lily YouTube Music",
            hwnd: None,
        };
        let mpris = MediaControls::new(config).ok();

        Self {
            mpv_child: None,
            is_paused: false,
            current_track: None,
            queue: Vec::new(),
            queue_idx: 0,
            mpris,
        }
    }

    fn play_track(&mut self, track: TrackItem, cookie_arg: Option<&str>) {
        self.stop_mpv();

        let _ = std::fs::remove_file(MPV_SOCKET);
        let url = format!("https://www.youtube.com/watch?v={}", track.id);

        let mut cmd = Command::new("mpv");
        cmd.args([
            "--no-video",
            "--ytdl-format=bestaudio/best",
            &format!("--input-ipc-server={}", MPV_SOCKET),
        ]);

        if let Some(browser_spec) = cookie_arg {
            if !browser_spec.is_empty() {
                cmd.arg(format!(
                    "--ytdl-raw-options=cookies-from-browser={}",
                    browser_spec
                ));
            }
        }

        cmd.arg(&url);

        if let Ok(child) = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            self.mpv_child = Some(child);
            self.is_paused = false;
            self.current_track = Some(track.clone());

            if let Some(ref mut mpris) = self.mpris {
                let _ = mpris.set_metadata(MediaMetadata {
                    title: Some(&track.title),
                    artist: Some(&track.uploader),
                    album: Some("YouTube Music"),
                    ..Default::default()
                });
                let _ = mpris.set_playback(MediaPlayback::Playing { progress: None });
            }
        }
    }

    /// Queries `(time_pos_secs, duration_secs)` from the running mpv IPC socket
    fn query_progress(&self) -> Option<(f64, f64)> {
        if self.mpv_child.is_none() {
            return None;
        }
        let pos = query_mpv_f64("time-pos")?;
        let dur = query_mpv_f64("duration")?;
        if dur > 0.0 {
            Some((pos.max(0.0), dur))
        } else {
            None
        }
    }

    /// Seeks to an exact position in seconds via mpv IPC
    fn seek_to(&self, seconds: f64) {
        if self.mpv_child.is_none() {
            return;
        }
        if let Ok(mut stream) = UnixStream::connect(MPV_SOCKET) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(30)));
            let cmd = format!("{{\"command\": [\"seek\", {:.2}, \"absolute\"]}}\n", seconds.max(0.0));
            let _ = stream.write_all(cmd.as_bytes());
        }
    }

    fn play_next_in_queue(
        &mut self,
        track: TrackItem,
        cookie_arg: Option<&str>,
    ) -> Option<TrackItem> {
        if self.queue.is_empty() || self.mpv_child.is_none() {
            self.queue = vec![track.clone()];
            self.queue_idx = 0;
            self.play_track(track.clone(), cookie_arg);
            return Some(track);
        }
        let insert_pos = (self.queue_idx + 1).min(self.queue.len());
        self.queue.insert(insert_pos, track);
        None
    }

    fn add_to_queue(&mut self, track: TrackItem, cookie_arg: Option<&str>) -> Option<TrackItem> {
        if self.queue.is_empty() || self.mpv_child.is_none() {
            self.queue = vec![track.clone()];
            self.queue_idx = 0;
            self.play_track(track.clone(), cookie_arg);
            return Some(track);
        }
        self.queue.push(track);
        None
    }

    fn check_autoplay_next(&mut self, cookie_arg: Option<&str>) -> Option<TrackItem> {
        let finished = if let Some(ref mut child) = self.mpv_child {
            matches!(child.try_wait(), Ok(Some(_)))
        } else {
            false
        };

        if finished {
            self.mpv_child = None;
            return self.play_next(cookie_arg);
        }
        None
    }

    fn toggle_pause(&mut self) -> bool {
        if self.mpv_child.is_none() {
            return false;
        }
        self.is_paused = !self.is_paused;
        let cmd = if self.is_paused {
            b"{\"command\": [\"set_property\", \"pause\", true]}\n".as_slice()
        } else {
            b"{\"command\": [\"set_property\", \"pause\", false]}\n".as_slice()
        };

        if let Ok(mut stream) = UnixStream::connect(MPV_SOCKET) {
            let _ = stream.write_all(cmd);
        }

        if let Some(ref mut mpris) = self.mpris {
            let state = if self.is_paused {
                MediaPlayback::Paused { progress: None }
            } else {
                MediaPlayback::Playing { progress: None }
            };
            let _ = mpris.set_playback(state);
        }

        self.is_paused
    }

    fn play_next(&mut self, cookie_arg: Option<&str>) -> Option<TrackItem> {
        if self.queue.is_empty() {
            return None;
        }
        if self.queue_idx + 1 < self.queue.len() {
            self.queue_idx += 1;
            let next = self.queue[self.queue_idx].clone();
            self.play_track(next.clone(), cookie_arg);
            return Some(next);
        }
        None
    }

    fn play_prev(&mut self, cookie_arg: Option<&str>) -> Option<TrackItem> {
        if self.queue.is_empty() {
            return None;
        }
        if self.queue_idx > 0 {
            self.queue_idx -= 1;
            let prev = self.queue[self.queue_idx].clone();
            self.play_track(prev.clone(), cookie_arg);
            return Some(prev);
        }
        None
    }

    fn stop_mpv(&mut self) {
        if let Some(mut child) = self.mpv_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(MPV_SOCKET);
    }
}

fn query_mpv_f64(prop: &str) -> Option<f64> {
    let mut stream = UnixStream::connect(MPV_SOCKET).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(25)));

    let cmd = format!("{{\"command\": [\"get_property\", \"{}\"]}}\n", prop);
    stream.write_all(cmd.as_bytes()).ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;

    let val: serde_json::Value = serde_json::from_str(&line).ok()?;
    val.get("data")?.as_f64()
}

fn query_mpv_string(prop: &str) -> Option<String> {
    let mut stream = UnixStream::connect(MPV_SOCKET).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(25)));

    let cmd = format!("{{\"command\": [\"get_property\", \"{}\"]}}\n", prop);
    stream.write_all(cmd.as_bytes()).ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;

    let val: serde_json::Value = serde_json::from_str(&line).ok()?;
    val.get("data")?.as_str().map(|s| s.trim().to_string())
}

fn format_mm_ss(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn shuffle_tracks(tracks: &mut [TrackItem]) {
    let len = tracks.len();
    if len <= 1 {
        return;
    }
    let mut rand_bytes = vec![0u8; len * 4];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut rand_bytes);
    }
    for i in (1..len).rev() {
        let b = i * 4;
        let rnd = u32::from_le_bytes([
            rand_bytes[b],
            rand_bytes[b + 1],
            rand_bytes[b + 2],
            rand_bytes[b + 3],
        ]) as usize;
        let j = rnd % (i + 1);
        tracks.swap(i, j);
    }
}

fn get_browser_candidates() -> Vec<String> {
    let home = glib::home_dir();
    let mut list = Vec::new();

    if home.join(".mozilla/firefox").exists() {
        list.push("firefox".to_string());
    }
    if home.join(".zen").exists() {
        list.push(format!("firefox:{}", home.join(".zen").display()));
    }
    if home.join(".var/app/app.zen_browser.zen/.zen").exists() {
        list.push(format!(
            "firefox:{}",
            home.join(".var/app/app.zen_browser.zen/.zen").display()
        ));
    }
    if home.join(".librewolf").exists() {
        list.push(format!("firefox:{}", home.join(".librewolf").display()));
    }
    if home.join(".config/BraveSoftware").exists() {
        list.push("brave".to_string());
    }
    if home.join(".config/chromium").exists() {
        list.push("chromium".to_string());
    }

    if list.is_empty() {
        list.push("firefox".to_string());
    }
    list
}

fn fetch_user_playlists() -> Result<(String, Vec<PlaylistItem>), String> {
    let candidates = get_browser_candidates();
    let mut last_err = String::from("Session expired. Log into YouTube Music in your browser.");

    for browser_spec in &candidates {
        let output = Command::new("yt-dlp")
            .args([
                "--flat-playlist",
                "--dump-json",
                "--no-warnings",
                "--cookies-from-browser",
                browser_spec,
                "https://www.youtube.com/feed/playlists",
            ])
            .output()
            .map_err(|e| format!("Failed to run yt-dlp: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut playlists = vec![
            PlaylistItem {
                title: "Liked Music (YTM)".to_string(),
                url: "https://music.youtube.com/playlist?list=LM".to_string(),
            },
            PlaylistItem {
                title: "Liked Videos / Songs".to_string(),
                url: "https://www.youtube.com/playlist?list=LL".to_string(),
            },
        ];

        let mut seen_urls = std::collections::HashSet::new();
        seen_urls.insert("LM".to_string());
        seen_urls.insert("LL".to_string());

        for line in stdout.lines() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                let title = val
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let id = val
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let url = val
                    .get("url")
                    .or_else(|| val.get("webpage_url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if title.is_empty() || (id.is_empty() && url.is_empty()) {
                    continue;
                }

                let key = if !id.is_empty() { id.clone() } else { url.clone() };
                if !seen_urls.insert(key) {
                    continue;
                }

                let full_url = if !url.is_empty() && url.starts_with("http") {
                    url
                } else {
                    format!("https://www.youtube.com/playlist?list={}", id)
                };

                playlists.push(PlaylistItem {
                    title,
                    url: full_url,
                });
            }
        }

        if output.status.success() || playlists.len() > 2 {
            return Ok((browser_spec.clone(), playlists));
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Some(first) = stderr.lines().next() {
                last_err = first.to_string();
            }
        }
    }

    Err(last_err)
}

fn fetch_ytm_tracks(
    browser_spec: Option<&str>,
    query_or_url: &str,
    is_search: bool,
    search_limit: u32,
) -> Result<Vec<TrackItem>, String> {
    let target = if is_search {
        format!("ytsearch{}:{}", search_limit.max(1), query_or_url)
    } else {
        query_or_url.to_string()
    };

    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--flat-playlist", "--dump-json", "--no-warnings"]);

    if let Some(spec) = browser_spec {
        if !spec.is_empty() && !is_search {
            cmd.args(["--cookies-from-browser", spec]);
        }
    }
    cmd.arg(&target);

    let output = cmd
        .output()
        .map_err(|e| format!("yt-dlp error: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tracks = Vec::new();

    for line in stdout.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            let id = val
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = val
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Track")
                .to_string();
            if id.is_empty()
                || title == "[Deleted video]"
                || title == "[Private video]"
                || id.starts_with("PL")
            {
                continue;
            }
            let uploader = val
                .get("uploader")
                .or_else(|| val.get("channel"))
                .and_then(|v| v.as_str())
                .unwrap_or("YouTube Music")
                .to_string();

            let dur_secs = val
                .get("duration")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            let duration_str = if dur_secs > 0 {
                format!("{}:{:02}", dur_secs / 60, dur_secs % 60)
            } else {
                String::new()
            };

            tracks.push(TrackItem {
                id,
                title,
                uploader,
                duration_str,
            });
        }
    }

    if tracks.is_empty() && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("Failed to load playlist");
        return Err(first_line.to_string());
    }

    Ok(tracks)
}

pub fn build_music_ui() -> Box {
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();

    let view_stack = Stack::new();
    view_stack.set_hhomogeneous(false);
    view_stack.set_hexpand(true);
    view_stack.set_vexpand(true);

    let music_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let search_entry = SearchEntry::builder()
        .placeholder_text("Search songs...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let nav_stack = Stack::new();
    nav_stack.set_hhomogeneous(false);
    nav_stack.set_hexpand(true);
    nav_stack.set_vexpand(true);

    // --- PAGE A: PLAYLISTS LIST SCREEN ---
    let playlists_screen = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .hexpand(true)
        .vexpand(true)
        .build();

    let pl_header_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let pl_header_lbl = Label::builder()
        .label("YOUR PLAYLISTS")
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let reload_pl_btn = Button::builder()
        .label("Reload")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    pl_header_row.append(&pl_header_lbl);
    pl_header_row.append(&reload_pl_btn);

    let pl_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let playlists_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    pl_scroll.set_child(Some(&playlists_box));
    playlists_screen.append(&pl_header_row);
    playlists_screen.append(&pl_scroll);

    // --- PAGE B: INSIDE A PLAYLIST / SEARCH RESULTS SCREEN ---
    let tracks_screen = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .hexpand(true)
        .vexpand(true)
        .build();

    let tr_top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let back_to_pl_btn = Button::builder()
        .label("<- Playlists")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let shuffle_btn = Button::builder()
        .label("Shuffle Play")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .hexpand(true)
        .build();

    tr_top_row.append(&back_to_pl_btn);
    tr_top_row.append(&shuffle_btn);

    let active_pl_title_lbl = Label::builder()
        .label("PLAYLIST")
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-group-header".to_string()])
        .tooltip_text("Double-click to change max search results count")
        .build();

    let limit_edit_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .visible(false)
        .build();

    let limit_lbl = Label::builder()
        .label("Max:")
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    let limit_entry = Entry::builder()
        .text("25")
        .width_chars(5)
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();

    let limit_save_btn = Button::builder()
        .label("Save")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let limit_cancel_btn = Button::builder()
        .label("x")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .build();

    limit_edit_row.append(&limit_lbl);
    limit_edit_row.append(&limit_entry);
    limit_edit_row.append(&limit_save_btn);
    limit_edit_row.append(&limit_cancel_btn);

    let tr_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let track_list_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    tr_scroll.set_child(Some(&track_list_box));
    tracks_screen.append(&tr_top_row);
    tracks_screen.append(&active_pl_title_lbl);
    tracks_screen.append(&limit_edit_row);
    tracks_screen.append(&tr_scroll);

    // --- PAGE C: ACTIVE QUEUE VIEW SCREEN ---
    let queue_screen = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .hexpand(true)
        .vexpand(true)
        .build();

    let q_top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let back_from_q_btn = Button::builder()
        .label("<- Back")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let q_title_lbl = Label::builder()
        .label("ACTIVE QUEUE")
        .halign(Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let clear_q_btn = Button::builder()
        .label("Clear")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .tooltip_text("Clear upcoming tracks in queue")
        .build();

    q_top_row.append(&back_from_q_btn);
    q_top_row.append(&q_title_lbl);
    q_top_row.append(&clear_q_btn);

    let q_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let queue_list_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    q_scroll.set_child(Some(&queue_list_box));
    queue_screen.append(&q_top_row);
    queue_screen.append(&q_scroll);

    nav_stack.add_named(&playlists_screen, Some("playlists"));
    nav_stack.add_named(&tracks_screen, Some("tracks"));
    nav_stack.add_named(&queue_screen, Some("queue"));

    // --- BOTTOM NOW PLAYING DOCK (With Draggable Progress Seek Bar) ---
    let player_dock = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .css_classes(vec!["plan-composer-dock".to_string()])
        .build();

    let np_header_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let now_playing_lbl = Label::builder()
        .label("Not Playing")
        .halign(Align::Start)
        .valign(Align::Center)
        .css_classes(vec!["plan-step".to_string()])
        .build();

    // Mini viewport that clips long titles and lets us smoothly scroll them back & forth!
    let title_marquee_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::External) // Allows scrolling while hiding the scrollbar!
        .vscrollbar_policy(PolicyType::Never)
        .hexpand(true)
        .valign(Align::Center)
        .child(&now_playing_lbl)
        .build();

    let queue_view_btn = Button::builder()
        .label("Queue (0)")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("View and manage your active song queue")
        .build();

    np_header_row.append(&title_marquee_scroll);
    np_header_row.append(&queue_view_btn);

    // Smooth back-and-forth marquee animation for long song titles
    {
        let scroll_anim = title_marquee_scroll.clone();
        let dir = Rc::new(Cell::new(1.0_f64)); // 1.0 = scrolling right, -1.0 = scrolling left
        let pause_ticks = Rc::new(Cell::new(30_u32)); // Pause ~1.5s at each end
        let last_seen_title = Rc::new(RefCell::new(String::new()));
        let lbl_watch = now_playing_lbl.clone();

        glib::timeout_add_local(Duration::from_millis(50), move || {
            let adj = scroll_anim.hadjustment();
            let cur_title = lbl_watch.text().to_string();

            // Whenever the song changes, reset marquee to the start and pause for 1.5s
            if *last_seen_title.borrow() != cur_title {
                *last_seen_title.borrow_mut() = cur_title;
                adj.set_value(0.0);
                dir.set(1.0);
                pause_ticks.set(30);
                return glib::ControlFlow::Continue;
            }

            let max_scroll = (adj.upper() - adj.page_size()).max(0.0);
            if max_scroll <= 2.0 {
                // Title fits completely; no scrolling needed
                if adj.value() != 0.0 {
                    adj.set_value(0.0);
                }
                return glib::ControlFlow::Continue;
            }

            if pause_ticks.get() > 0 {
                pause_ticks.set(pause_ticks.get() - 1);
                return glib::ControlFlow::Continue;
            }

            let next_val = adj.value() + (dir.get() * 1.4);
            if next_val >= max_scroll {
                adj.set_value(max_scroll);
                dir.set(-1.0);
                pause_ticks.set(30); // Pause 1.5s at the right end
            } else if next_val <= 0.0 {
                adj.set_value(0.0);
                dir.set(1.0);
                pause_ticks.set(35); // Pause ~1.75s at the left start
            } else {
                adj.set_value(next_val);
            }

            glib::ControlFlow::Continue
        });
    }

    let meta_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let now_artist_lbl = Label::builder()
        .label("Right-click any song")
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    let time_lbl = Label::builder()
        .label("0:00 / 0:00")
        .halign(Align::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    meta_row.append(&now_artist_lbl);
    meta_row.append(&time_lbl);

    // Draggable Progress / Seek Bar
    let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    seek_scale.set_draw_value(false);
    seek_scale.set_hexpand(true);
    seek_scale.set_css_classes(&["music-seek"]);

    let controls_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let prev_btn = Button::builder()
        .label("|<")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .build();

    let play_pause_btn = Button::builder()
        .label("Play / Pause")
        .css_classes(vec!["plan-close-btn".to_string()])
        .hexpand(true)
        .build();

    let next_btn = Button::builder()
        .label(">|")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .build();

    controls_row.append(&prev_btn);
    controls_row.append(&play_pause_btn);
    controls_row.append(&next_btn);

    player_dock.append(&np_header_row);
    player_dock.append(&meta_row);
    player_dock.append(&seek_scale);
    player_dock.append(&controls_row);

    music_page.append(&search_entry);
    music_page.append(&nav_stack);
    music_page.append(&player_dock);

    // --- FALLBACK ERROR VIEW ---
    let error_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .valign(Align::Center)
        .margin_start(14)
        .margin_end(14)
        .build();

    let err_title = Label::builder()
        .label("LOGIN REQUIRED")
        .css_classes(vec!["plan-h2".to_string()])
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .max_width_chars(22)
        .build();

    let err_open_btn = Button::builder()
        .css_classes(vec!["fav-item-btn".to_string()])
        .build();

    let err_msg_lbl = Label::builder()
        .label("Session expired.\nLog into YouTube Music\nin your browser.\n\n[ Click to Open ]")
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .max_width_chars(24)
        .css_classes(vec!["plan-body".to_string()])
        .build();
    err_open_btn.set_child(Some(&err_msg_lbl));

    err_open_btn.connect_clicked(|_| {
        let _ = Command::new("xdg-open")
            .arg("https://music.youtube.com")
            .spawn();
    });

    let err_detail_lbl = Label::builder()
        .label("")
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .max_width_chars(26)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    let err_refresh_btn = Button::builder()
        .label("Refresh Session")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    error_page.append(&err_title);
    error_page.append(&err_open_btn);
    error_page.append(&err_detail_lbl);
    error_page.append(&err_refresh_btn);

    view_stack.add_named(&music_page, Some("music"));
    view_stack.add_named(&error_page, Some("error"));
    root.append(&view_stack);

    // State
    let player = Rc::new(RefCell::new(PlayerState::new()));
    title_marquee_scroll.set_tooltip_text(Some("Left-click to copy: song | album | artist"));

    let title_click = GestureClick::builder().button(1).build();
    let player_copy = player.clone();
    let artist_lbl_feedback = now_artist_lbl.clone();
    title_click.connect_pressed(move |_, _, _, _| {
        let current_opt = player_copy.borrow().current_track.clone();
        let Some(track) = current_opt else {
            return;
        };

        let song = track.title.trim().to_string();

        // Query the real Album tag from mpv's loaded YouTube Music metadata (fallback to "Single")
        let album = query_mpv_string("metadata/by-key/album")
            .or_else(|| query_mpv_string("metadata/by-key/Album"))
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "Single".to_string());

        // Prefer mpv's clean artist tag, or strip " - Topic" from the uploader name
        let raw_artist = query_mpv_string("metadata/by-key/artist")
            .or_else(|| query_mpv_string("metadata/by-key/Artist"))
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| track.uploader.clone());

        let artist = raw_artist
            .strip_suffix(" - Topic")
            .unwrap_or(&raw_artist)
            .trim()
            .to_string();

        let formatted = format!("{} | {} | {}", song, album, artist);

        if let Some(display) = gtk4::gdk::Display::default() {
            display.clipboard().set_text(&formatted);
        }

        let prev_text = artist_lbl_feedback.text().to_string();
        artist_lbl_feedback.set_label("Copied: song | album | artist!");
        let lbl_reset = artist_lbl_feedback.clone();
        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
            lbl_reset.set_label(&prev_text);
        });
    });
    title_marquee_scroll.add_controller(title_click);
    let active_browser = Rc::new(RefCell::new(String::new()));
    let current_playlist_tracks: Rc<RefCell<Vec<TrackItem>>> = Rc::new(RefCell::new(Vec::new()));
    let prev_nav_page = Rc::new(RefCell::new("playlists".to_string()));
    let search_limit = Rc::new(Cell::new(25u32));
    let last_search_query = Rc::new(RefCell::new(String::new()));
    let last_user_seek = Rc::new(Cell::new(Instant::now() - Duration::from_secs(10)));
    let (tx, rx) = mpsc::channel::<WorkerMsg>();
    let (mpris_tx, mpris_rx) = mpsc::channel::<MediaControlEvent>();

    if let Some(ref mut mpris) = player.borrow_mut().mpris {
        let _ = mpris.attach(move |event| {
            let _ = mpris_tx.send(event);
        });
    }

    // Wire up Draggable Seek Bar (`connect_change_value` ONLY fires on user mouse/key interaction!)
    let player_seek = player.clone();
    let last_seek_stamp = last_user_seek.clone();
    let time_lbl_seek = time_lbl.clone();
    seek_scale.connect_change_value(move |scale, _, new_val| {
        let max_dur = scale.adjustment().upper();
        let target = new_val.clamp(0.0, max_dur);
        time_lbl_seek.set_label(&format!("{} / {}", format_mm_ss(target), format_mm_ss(max_dur)));

        // Throttle IPC seek commands to once every 80ms while dragging smoothly
        if last_seek_stamp.get().elapsed() >= Duration::from_millis(80) {
            last_seek_stamp.set(Instant::now());
            player_seek.borrow().seek_to(target);
        }
        glib::Propagation::Proceed
    });

    // Helper closure to refresh the visible Queue UI & "Queue (N)" badge
    let refresh_queue_ui: Rc<dyn Fn()> = {
        let q_box = queue_list_box.clone();
        let q_btn = queue_view_btn.clone();
        let player_q = player.clone();
        let br_q = active_browser.clone();
        let np_t_q = now_playing_lbl.clone();
        let np_a_q = now_artist_lbl.clone();
        let pp_q = play_pause_btn.clone();

        let refresh_rc: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let refresh_weak = refresh_rc.clone();

        let closure: Rc<dyn Fn()> = Rc::new(move || {
            render_queue_view(
                &q_box,
                &q_btn,
                &player_q,
                &br_q,
                &np_t_q,
                &np_a_q,
                &pp_q,
                &refresh_weak,
            );
        });
        *refresh_rc.borrow_mut() = Some(closure.clone());
        closure
    };

    // Toggle Queue screen when clicking "Queue (N)"
    let nav_q_btn = nav_stack.clone();
    let prev_nav_q = prev_nav_page.clone();
    let refresh_on_open_q = refresh_queue_ui.clone();
    queue_view_btn.connect_clicked(move |_| {
        let cur = nav_q_btn
            .visible_child_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "playlists".to_string());
        if cur == "queue" {
            let target = prev_nav_q.borrow().clone();
            nav_q_btn.set_visible_child_name(&target);
        } else {
            *prev_nav_q.borrow_mut() = cur;
            refresh_on_open_q();
            nav_q_btn.set_visible_child_name("queue");
        }
    });

    let nav_from_q = nav_stack.clone();
    let prev_from_q = prev_nav_page.clone();
    back_from_q_btn.connect_clicked(move |_| {
        let target = prev_from_q.borrow().clone();
        nav_from_q.set_visible_child_name(&target);
    });

    let player_clear = player.clone();
    let refresh_clear = refresh_queue_ui.clone();
    clear_q_btn.connect_clicked(move |_| {
        {
            let mut p = player_clear.borrow_mut();
            if let Some(cur) = p.current_track.clone() {
                if p.mpv_child.is_some() {
                    p.queue = vec![cur];
                    p.queue_idx = 0;
                } else {
                    p.queue.clear();
                    p.queue_idx = 0;
                }
            } else {
                p.queue.clear();
                p.queue_idx = 0;
            }
        }
        refresh_clear();
    });

    // Double-Click on `active_pl_title_lbl` -> Open inline Max Results editor
    let dbl_click_hdr = GestureClick::builder().button(1).build();
    let hdr_lbl_hide = active_pl_title_lbl.clone();
    let edit_row_show = limit_edit_row.clone();
    let limit_entry_init = limit_entry.clone();
    let limit_cell_init = search_limit.clone();
    dbl_click_hdr.connect_pressed(move |_, n_press, _, _| {
        if n_press == 2 {
            limit_entry_init.set_text(&limit_cell_init.get().to_string());
            hdr_lbl_hide.set_visible(false);
            edit_row_show.set_visible(true);
            limit_entry_init.grab_focus();
        }
    });
    active_pl_title_lbl.add_controller(dbl_click_hdr);

    let hdr_lbl_cancel = active_pl_title_lbl.clone();
    let edit_row_cancel = limit_edit_row.clone();
    limit_cancel_btn.connect_clicked(move |_| {
        edit_row_cancel.set_visible(false);
        hdr_lbl_cancel.set_visible(true);
    });

    let hdr_lbl_save = active_pl_title_lbl.clone();
    let edit_row_save = limit_edit_row.clone();
    let entry_save = limit_entry.clone();
    let limit_cell_save = search_limit.clone();
    let last_q_save = last_search_query.clone();
    let tx_limit = tx.clone();
    let br_limit = active_browser.clone();

    let commit_limit = Rc::new(move || {
        if let Ok(parsed) = entry_save.text().trim().parse::<u32>() {
            let clamped = parsed.clamp(1, 500);
            limit_cell_save.set(clamped);

            let q = last_q_save.borrow().clone();
            if !q.is_empty() {
                hdr_lbl_save.set_label(&format!(
                    "SEARCHING '{}' ({})...",
                    q.to_uppercase(),
                    clamped
                ));
                let tx_w = tx_limit.clone();
                let br = br_limit.borrow().clone();
                std::thread::spawn(move || {
                    let spec = if br.is_empty() {
                        None
                    } else {
                        Some(br.as_str())
                    };
                    let res = fetch_ytm_tracks(spec, &q, true, clamped)
                        .map(|t| (format!("SEARCH: {}", q), t));
                    let _ = tx_w.send(WorkerMsg::PlaylistTracksLoaded(res));
                });
            }
        }
        edit_row_save.set_visible(false);
        hdr_lbl_save.set_visible(true);
    });

    let commit_on_btn = commit_limit.clone();
    limit_save_btn.connect_clicked(move |_| commit_on_btn());

    let commit_on_enter = commit_limit.clone();
    limit_entry.connect_activate(move |_| commit_on_enter());

    let load_playlists_action = {
        let tx_pl = tx.clone();
        let pl_lbl = pl_header_lbl.clone();
        let v_stack = view_stack.clone();
        let n_stack = nav_stack.clone();
        Rc::new(move || {
            v_stack.set_visible_child_name("music");
            n_stack.set_visible_child_name("playlists");
            pl_lbl.set_label("LOADING PLAYLISTS...");
            let tx_w = tx_pl.clone();
            std::thread::spawn(move || {
                let res = fetch_user_playlists();
                let _ = tx_w.send(WorkerMsg::PlaylistsLoaded(res));
            });
        })
    };

    load_playlists_action();

    let retry_pl = load_playlists_action.clone();
    err_refresh_btn.connect_clicked(move |_| retry_pl());

    let reload_pl = load_playlists_action.clone();
    reload_pl_btn.connect_clicked(move |_| reload_pl());

    let nav_back = nav_stack.clone();
    back_to_pl_btn.connect_clicked(move |_| {
        nav_back.set_visible_child_name("playlists");
    });

    let tracks_for_shuffle = current_playlist_tracks.clone();
    let player_shuffle = player.clone();
    let br_shuffle = active_browser.clone();
    let np_t_shuf = now_playing_lbl.clone();
    let np_a_shuf = now_artist_lbl.clone();
    let pp_shuf = play_pause_btn.clone();
    let refresh_q_shuf = refresh_queue_ui.clone();
    shuffle_btn.connect_clicked(move |_| {
        let mut list = tracks_for_shuffle.borrow().clone();
        if list.is_empty() {
            return;
        }
        shuffle_tracks(&mut list);
        let first = list[0].clone();
        np_t_shuf.set_label(&first.title);
        np_a_shuf.set_label(&first.uploader);
        pp_shuf.set_label("Pause");

        let br = br_shuffle.borrow().clone();
        let spec = if br.is_empty() {
            None
        } else {
            Some(br.as_str())
        };

        {
            let mut p = player_shuffle.borrow_mut();
            p.queue = list;
            p.queue_idx = 0;
            p.play_track(first, spec);
        }
        refresh_q_shuf();
    });

    let tx_search = tx.clone();
    let br_search = active_browser.clone();
    let nav_search = nav_stack.clone();
    let title_search = active_pl_title_lbl.clone();
    let limit_cell_search = search_limit.clone();
    let last_q_search = last_search_query.clone();
    search_entry.connect_activate(move |entry| {
        let q = entry.text().trim().to_string();
        if q.is_empty() {
            return;
        }
        *last_q_search.borrow_mut() = q.clone();
        let max_n = limit_cell_search.get();
        nav_search.set_visible_child_name("tracks");
        title_search.set_label(&format!("SEARCHING '{}' ({})...", q.to_uppercase(), max_n));
        let tx_w = tx_search.clone();
        let br = br_search.borrow().clone();
        std::thread::spawn(move || {
            let spec = if br.is_empty() {
                None
            } else {
                Some(br.as_str())
            };
            let res =
                fetch_ytm_tracks(spec, &q, true, max_n).map(|t| (format!("SEARCH: {}", q), t));
            let _ = tx_w.send(WorkerMsg::PlaylistTracksLoaded(res));
        });
    });

    let player_pp = player.clone();
    let pp_btn_clone = play_pause_btn.clone();
    play_pause_btn.connect_clicked(move |_| {
        let paused = player_pp.borrow_mut().toggle_pause();
        pp_btn_clone.set_label(if paused { "Resume" } else { "Pause" });
    });

    let player_next = player.clone();
    let br_next = active_browser.clone();
    let np_title_next = now_playing_lbl.clone();
    let np_artist_next = now_artist_lbl.clone();
    let pp_next = play_pause_btn.clone();
    let refresh_q_next = refresh_queue_ui.clone();
    next_btn.connect_clicked(move |_| {
        let br = br_next.borrow().clone();
        let spec = if br.is_empty() {
            None
        } else {
            Some(br.as_str())
        };
        let next_opt = player_next.borrow_mut().play_next(spec);
        if let Some(track) = next_opt {
            np_title_next.set_label(&track.title);
            np_artist_next.set_label(&track.uploader);
            pp_next.set_label("Pause");
            refresh_q_next();
        }
    });

    let player_prev = player.clone();
    let br_prev = active_browser.clone();
    let np_title_prev = now_playing_lbl.clone();
    let np_artist_prev = now_artist_lbl.clone();
    let pp_prev = play_pause_btn.clone();
    let refresh_q_prev = refresh_queue_ui.clone();
    prev_btn.connect_clicked(move |_| {
        let br = br_prev.borrow().clone();
        let spec = if br.is_empty() {
            None
        } else {
            Some(br.as_str())
        };
        let prev_opt = player_prev.borrow_mut().play_prev(spec);
        if let Some(track) = prev_opt {
            np_title_prev.set_label(&track.title);
            np_artist_prev.set_label(&track.uploader);
            pp_prev.set_label("Pause");
            refresh_q_prev();
        }
    });

    // Main loop poller (Autoplay + Progress Bar + Worker Messages + MPRIS)
    let v_stack_poll = view_stack.clone();
    let n_stack_poll = nav_stack.clone();
    let pl_box_poll = playlists_box.clone();
    let pl_lbl_poll = pl_header_lbl.clone();
    let tr_box_poll = track_list_box.clone();
    let tr_lbl_poll = active_pl_title_lbl.clone();
    let err_detail_poll = err_detail_lbl.clone();
    let browser_poll = active_browser.clone();
    let cur_tracks_poll = current_playlist_tracks.clone();
    let player_poll = player.clone();
    let np_title_poll = now_playing_lbl.clone();
    let np_artist_poll = now_artist_lbl.clone();
    let pp_poll = play_pause_btn.clone();
    let seek_poll = seek_scale.clone();
    let time_lbl_poll = time_lbl.clone();
    let last_seek_poll = last_user_seek.clone();
    let tx_for_pl_click = tx.clone();
    let refresh_q_poll = refresh_queue_ui.clone();

    glib::timeout_add_local(Duration::from_millis(200), move || {
        // 1. Check autoplay if current song finished
        {
            let br = browser_poll.borrow().clone();
            let spec = if br.is_empty() {
                None
            } else {
                Some(br.as_str())
            };
            let next_opt = player_poll.borrow_mut().check_autoplay_next(spec);
            if let Some(next_track) = next_opt {
                np_title_poll.set_label(&next_track.title);
                np_artist_poll.set_label(&next_track.uploader);
                pp_poll.set_label("Pause");
                seek_poll.set_value(0.0);
                refresh_q_poll();
            }
        }

        // 2. Update Progress Slider & Timestamp (unless user dragged it in the last 500ms)
        if last_seek_poll.get().elapsed() >= Duration::from_millis(500) {
            if let Some((pos, dur)) = player_poll.borrow().query_progress() {
                seek_poll.set_range(0.0, dur.max(1.0));
                seek_poll.set_value(pos);
                time_lbl_poll.set_label(&format!("{} / {}", format_mm_ss(pos), format_mm_ss(dur)));
            }
        }

        // 3. Worker messages
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::PlaylistsLoaded(Ok((browser, playlists))) => {
                    *browser_poll.borrow_mut() = browser;
                    v_stack_poll.set_visible_child_name("music");
                    n_stack_poll.set_visible_child_name("playlists");
                    pl_lbl_poll.set_label(&format!("YOUR PLAYLISTS ({})", playlists.len()));

                    render_playlists_list(
                        &pl_box_poll,
                        playlists,
                        &n_stack_poll,
                        &tr_lbl_poll,
                        &tr_box_poll,
                        &browser_poll,
                        &tx_for_pl_click,
                    );
                }
                WorkerMsg::PlaylistsLoaded(Err(e)) => {
                    err_detail_poll.set_label(&e);
                    v_stack_poll.set_visible_child_name("error");
                }
                WorkerMsg::PlaylistTracksLoaded(Ok((pl_title, tracks))) => {
                    let is_search_result = pl_title.starts_with("SEARCH:");
                    v_stack_poll.set_visible_child_name("music");
                    n_stack_poll.set_visible_child_name("tracks");
                    tr_lbl_poll
                        .set_label(&format!("{} ({})", pl_title.to_uppercase(), tracks.len()));
                    *cur_tracks_poll.borrow_mut() = tracks.clone();

                    render_track_list(
                        &tr_box_poll,
                        tracks,
                        is_search_result,
                        &player_poll,
                        &browser_poll,
                        &np_title_poll,
                        &np_artist_poll,
                        &pp_poll,
                        &refresh_q_poll,
                    );
                }
                WorkerMsg::PlaylistTracksLoaded(Err(e)) => {
                    tr_lbl_poll.set_label(&format!("ERROR: {}", e));
                }
            }
        }

        // 4. Waybar / MPRIS events
        while let Ok(event) = mpris_rx.try_recv() {
            let br = browser_poll.borrow().clone();
            let spec = if br.is_empty() {
                None
            } else {
                Some(br.as_str())
            };
            match event {
                MediaControlEvent::Toggle | MediaControlEvent::Pause | MediaControlEvent::Play => {
                    let paused = player_poll.borrow_mut().toggle_pause();
                    pp_poll.set_label(if paused { "Resume" } else { "Pause" });
                }
                MediaControlEvent::Next => {
                    let next_opt = player_poll.borrow_mut().play_next(spec);
                    if let Some(track) = next_opt {
                        np_title_poll.set_label(&track.title);
                        np_artist_poll.set_label(&track.uploader);
                        pp_poll.set_label("Pause");
                        refresh_q_poll();
                    }
                }
                MediaControlEvent::Previous => {
                    let prev_opt = player_poll.borrow_mut().play_prev(spec);
                    if let Some(track) = prev_opt {
                        np_title_poll.set_label(&track.title);
                        np_artist_poll.set_label(&track.uploader);
                        pp_poll.set_label("Pause");
                        refresh_q_poll();
                    }
                }
                _ => {}
            }
        }

        glib::ControlFlow::Continue
    });

    root
}

fn render_playlists_list(
    container: &Box,
    playlists: Vec<PlaylistItem>,
    nav_stack: &Stack,
    tr_header_lbl: &Label,
    tr_box: &Box,
    browser_rc: &Rc<RefCell<String>>,
    tx: &mpsc::Sender<WorkerMsg>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    for pl in playlists {
        let btn = Button::builder()
            .css_classes(vec!["fav-item-btn".to_string()])
            .hexpand(true)
            .build();

        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();

        let title_lbl = Label::builder()
            .label(&pl.title)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();

        let arrow = Label::builder()
            .label("->")
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();

        row.append(&title_lbl);
        row.append(&arrow);
        btn.set_child(Some(&row));

        let nav_click = nav_stack.clone();
        let hdr_click = tr_header_lbl.clone();
        let tr_box_click = tr_box.clone();
        let br_click = browser_rc.clone();
        let tx_click = tx.clone();
        let pl_title = pl.title.clone();
        let pl_url = pl.url.clone();

        btn.connect_clicked(move |_| {
            while let Some(child) = tr_box_click.first_child() {
                tr_box_click.remove(&child);
            }
            nav_click.set_visible_child_name("tracks");
            hdr_click.set_label(&format!("LOADING {}...", pl_title.to_uppercase()));

            let br = br_click.borrow().clone();
            let tx_w = tx_click.clone();
            let title_owned = pl_title.clone();
            let url_owned = pl_url.clone();

            std::thread::spawn(move || {
                let spec = if br.is_empty() {
                    None
                } else {
                    Some(br.as_str())
                };
                let res = fetch_ytm_tracks(spec, &url_owned, false, 25).map(|t| (title_owned, t));
                let _ = tx_w.send(WorkerMsg::PlaylistTracksLoaded(res));
            });
        });

        container.append(&btn);
    }
}

fn render_track_list(
    container: &Box,
    tracks: Vec<TrackItem>,
    is_search_result: bool,
    player: &Rc<RefCell<PlayerState>>,
    browser_rc: &Rc<RefCell<String>>,
    np_title: &Label,
    np_artist: &Label,
    pp_btn: &Button,
    refresh_queue_ui: &Rc<dyn Fn()>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    for (idx, track) in tracks.iter().enumerate() {
        let row_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .build();

        let btn = Button::builder()
            .css_classes(vec!["fav-item-btn".to_string()])
            .hexpand(true)
            .tooltip_text("Left-click to play | Right-click for Play Next / Add to Queue")
            .build();

        let col = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(2)
            .build();

        let top_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();

        let title_lbl = Label::builder()
            .label(&track.title)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();

        let dur_lbl = Label::builder()
            .label(&track.duration_str)
            .css_classes(vec!["fav-score-badge".to_string()])
            .halign(Align::End)
            .build();

        top_row.append(&title_lbl);
        top_row.append(&dur_lbl);

        let artist_lbl = Label::builder()
            .label(&track.uploader)
            .halign(Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();

        col.append(&top_row);
        col.append(&artist_lbl);
        btn.set_child(Some(&col));

        let track_clone = track.clone();
        let all_tracks = tracks.clone();
        let player_click = player.clone();
        let br_click = browser_rc.clone();
        let title_ui = np_title.clone();
        let artist_ui = np_artist.clone();
        let pp_ui = pp_btn.clone();
        let refresh_q_click = refresh_queue_ui.clone();

        btn.connect_clicked(move |_| {
            title_ui.set_label(&track_clone.title);
            artist_ui.set_label(&track_clone.uploader);
            pp_ui.set_label("Pause");

            let br = br_click.borrow().clone();
            let spec = if br.is_empty() {
                None
            } else {
                Some(br.as_str())
            };

            {
                let mut p = player_click.borrow_mut();
                if is_search_result {
                    if p.queue.is_empty() {
                        p.queue = vec![track_clone.clone()];
                        p.queue_idx = 0;
                    } else {
                        let insert_pos = (p.queue_idx + 1).min(p.queue.len());
                        p.queue.insert(insert_pos, track_clone.clone());
                        p.queue_idx = insert_pos;
                    }
                } else {
                    p.queue = all_tracks.clone();
                    p.queue_idx = idx;
                }
                p.play_track(track_clone.clone(), spec);
            }
            refresh_q_click();
        });

        let popover = Popover::builder()
            .css_classes(vec!["fav-popover".to_string()])
            .autohide(true)
            .build();
        popover.set_parent(&row_box);

        let menu_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .build();

        let play_next_btn = Button::builder()
            .label("Play Next")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let add_queue_btn = Button::builder()
            .label("Add to Queue")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        menu_box.append(&play_next_btn);
        menu_box.append(&add_queue_btn);
        popover.set_child(Some(&menu_box));

        let right_click = GestureClick::builder().button(3).build();
        let pop_open = popover.clone();
        right_click.connect_pressed(move |_, _, _, _| {
            pop_open.popup();
        });
        btn.add_controller(right_click);

        let track_pn = track.clone();
        let player_pn = player.clone();
        let br_pn = browser_rc.clone();
        let title_pn = np_title.clone();
        let artist_pn = np_artist.clone();
        let pp_pn = pp_btn.clone();
        let dur_pn = dur_lbl.clone();
        let orig_dur_pn = track.duration_str.clone();
        let pop_pn = popover.clone();
        let refresh_q_pn = refresh_queue_ui.clone();

        play_next_btn.connect_clicked(move |_| {
            pop_pn.popdown();
            let br = br_pn.borrow().clone();
            let spec = if br.is_empty() {
                None
            } else {
                Some(br.as_str())
            };

            let started = player_pn
                .borrow_mut()
                .play_next_in_queue(track_pn.clone(), spec);
            if let Some(started_track) = started {
                title_pn.set_label(&started_track.title);
                artist_pn.set_label(&started_track.uploader);
                pp_pn.set_label("Pause");
            }
            refresh_q_pn();

            dur_pn.set_label("Next!");
            let dur_reset = dur_pn.clone();
            let orig = orig_dur_pn.clone();
            glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                dur_reset.set_label(&orig);
            });
        });

        let track_aq = track.clone();
        let player_aq = player.clone();
        let br_aq = browser_rc.clone();
        let title_aq = np_title.clone();
        let artist_aq = np_artist.clone();
        let pp_aq = pp_btn.clone();
        let dur_aq = dur_lbl.clone();
        let orig_dur_aq = track.duration_str.clone();
        let pop_aq = popover.clone();
        let refresh_q_aq = refresh_queue_ui.clone();

        add_queue_btn.connect_clicked(move |_| {
            pop_aq.popdown();
            let br = br_aq.borrow().clone();
            let spec = if br.is_empty() {
                None
            } else {
                Some(br.as_str())
            };

            let started = player_aq.borrow_mut().add_to_queue(track_aq.clone(), spec);
            if let Some(started_track) = started {
                title_aq.set_label(&started_track.title);
                artist_aq.set_label(&started_track.uploader);
                pp_aq.set_label("Pause");
            }
            refresh_q_aq();

            dur_aq.set_label("Queued!");
            let dur_reset = dur_aq.clone();
            let orig = orig_dur_aq.clone();
            glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                dur_reset.set_label(&orig);
            });
        });

        row_box.append(&btn);
        container.append(&row_box);
    }
}

fn render_queue_view(
    container: &Box,
    queue_btn: &Button,
    player: &Rc<RefCell<PlayerState>>,
    browser_rc: &Rc<RefCell<String>>,
    np_title: &Label,
    np_artist: &Label,
    pp_btn: &Button,
    refresh_self: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let (queue_items, current_idx) = {
        let p = player.borrow();
        (p.queue.clone(), p.queue_idx)
    };

    queue_btn.set_label(&format!("Queue ({})", queue_items.len()));

    if queue_items.is_empty() {
        let empty = Label::builder()
            .label("Queue is empty.\nRight-click any song in\nSearch or Playlists to add!")
            .justify(gtk4::Justification::Center)
            .css_classes(vec!["fav-score-badge".to_string()])
            .margin_top(20)
            .build();
        container.append(&empty);
        return;
    }

    for (idx, track) in queue_items.into_iter().enumerate() {
        let is_current = idx == current_idx;

        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();

        let mut btn_classes = vec!["fav-item-btn".to_string()];
        if is_current {
            btn_classes.push("pinned".to_string());
        }

        let btn = Button::builder()
            .css_classes(btn_classes)
            .hexpand(true)
            .build();

        let col = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(2)
            .build();

        let top_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();

        let prefix = if is_current { "[NOW] " } else { "" };
        let title_lbl = Label::builder()
            .label(&format!("{}{}", prefix, track.title))
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();

        let pos_lbl = Label::builder()
            .label(&format!("#{}", idx + 1))
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();

        top_row.append(&title_lbl);
        top_row.append(&pos_lbl);

        let artist_lbl = Label::builder()
            .label(&track.uploader)
            .halign(Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();

        col.append(&top_row);
        col.append(&artist_lbl);
        btn.set_child(Some(&col));

        let player_jump = player.clone();
        let br_jump = browser_rc.clone();
        let title_jump = np_title.clone();
        let artist_jump = np_artist.clone();
        let pp_jump = pp_btn.clone();
        let track_jump = track.clone();
        let refresh_jump = refresh_self.clone();
        btn.connect_clicked(move |_| {
            let br = br_jump.borrow().clone();
            let spec = if br.is_empty() {
                None
            } else {
                Some(br.as_str())
            };
            {
                let mut p = player_jump.borrow_mut();
                p.queue_idx = idx;
                p.play_track(track_jump.clone(), spec);
            }
            title_jump.set_label(&track_jump.title);
            artist_jump.set_label(&track_jump.uploader);
            pp_jump.set_label("Pause");
            if let Some(ref cb) = *refresh_jump.borrow() {
                cb();
            }
        });

        let del_btn = Button::builder()
            .label("x")
            .valign(Align::Center)
            .css_classes(vec!["plan-line-del-btn".to_string()])
            .tooltip_text("Remove from queue")
            .build();

        let player_del = player.clone();
        let refresh_del = refresh_self.clone();
        del_btn.connect_clicked(move |_| {
            {
                let mut p = player_del.borrow_mut();
                if idx < p.queue.len() {
                    p.queue.remove(idx);
                    if idx < p.queue_idx && p.queue_idx > 0 {
                        p.queue_idx -= 1;
                    }
                }
            }
            if let Some(ref cb) = *refresh_del.borrow() {
                cb();
            }
        });

        row.append(&btn);
        row.append(&del_btn);
        container.append(&row);
    }
}