//! YouTube Music player tab backed by `yt-dlp` and a local `mpv` IPC socket.
//!
//! Handles playlists, search, play-next / queue management, seek and volume controls,
//! MPRIS media-key integration via `souvlaki`, and a bounded in-memory cover-art cache.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Entry, GestureClick, Image, Label, Orientation, Overflow, PolicyType,
    Popover, Scale, ScrolledWindow, SearchEntry, Stack,
};
use serde::{Deserialize, Serialize};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MPV_SOCKET: &str = "/tmp/lily-ymusic-mpv.sock";
const APP_LOG_FILE: &str = "/tmp/lily-ymusic.log";
const THUMB_RAM_DIR: &str = "/tmp/lily-ymusic-thumbs";

fn log_msg(msg: &str) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(APP_LOG_FILE) {
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

fn now_unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

const YTM_BRIDGE_PY: &str = r###"
import json
import os
import sys
from urllib.parse import parse_qs, urlparse
from ytmusicapi import YTMusic

def pick_thumb(thumbs, vid=""):
    if isinstance(thumbs, list) and thumbs:
        for t in thumbs:
            if isinstance(t, dict) and t.get("url"):
                w = t.get("width") or 0
                if 100 <= w <= 544:
                    return t["url"]
        last = thumbs[-1]
        if isinstance(last, dict) and last.get("url"):
            return last["url"]
    if vid and len(vid) == 11:
        return f"https://i.ytimg.com/vi/{vid}/mqdefault.jpg"
    return ""

def extract_artists(item, fallback="YouTube Music"):
    artists = item.get("artists")
    if isinstance(artists, list) and artists:
        names = [a.get("name", "").strip() for a in artists if isinstance(a, dict) and a.get("name")]
        if names:
            return ", ".join(names)
    author = item.get("author")
    if isinstance(author, str) and author.strip():
        return author.strip()
    if isinstance(author, dict) and author.get("name"):
        return author["name"].strip()
    if isinstance(author, list) and author:
        names = [a.get("name", "").strip() for a in author if isinstance(a, dict) and a.get("name")]
        if names:
            return ", ".join(names)
    return fallback

def extract_duration(item):
    for key in ("duration", "length"):
        val = item.get(key)
        if isinstance(val, str) and val.strip():
            return val.strip()
    secs = item.get("duration_seconds")
    if isinstance(secs, (int, float)) and secs > 0:
        s = int(secs)
        return f"{s // 60}:{s % 60:02d}"
    return ""

def make_track_dict(t, fallback_artists="YouTube Music", fallback_thumbs=None):
    vid = (t.get("videoId") or "").strip()
    if not vid:
        return None
    title = (t.get("title") or "Unknown Track").strip()
    if title in ("[Deleted video]", "[Private video]"):
        return None
    uploader = extract_artists(t, fallback=fallback_artists)
    thumb_url = pick_thumb(t.get("thumbnails") or fallback_thumbs, vid)
    return {
        "id": vid,
        "title": title,
        "uploader": uploader,
        "duration_str": extract_duration(t),
        "thumb_url": thumb_url,
        "kind": "Song",
        "list_url": "",
    }

def extract_list_id(url):
    if "list=" in url:
        qs = parse_qs(urlparse(url).query)
        vals = qs.get("list", [])
        if vals:
            return vals[0]
    if "/browse/" in url:
        return url.rstrip("/").split("/browse/")[-1]
    return url.strip()

def get_yt(auth_path, require_auth=False):
    if auth_path and os.path.exists(auth_path):
        return YTMusic(auth_path)
    if require_auth:
        raise RuntimeError("No ytm-player auth.json found. Run 'ytm setup' in your terminal.")
    return YTMusic()

def cmd_playlists(auth_path):
    yt = get_yt(auth_path, require_auth=True)
    items = []
    seen = set()

    lm_thumb = ""
    try:
        liked = yt.get_liked_songs(limit=1)
        lm_tracks = liked.get("tracks") or []
        if lm_tracks:
            lm_thumb = pick_thumb(lm_tracks[0].get("thumbnails"), lm_tracks[0].get("videoId", ""))
    except Exception:
        pass

    items.append({
        "title": "Liked Music (YTM)",
        "url": "https://music.youtube.com/playlist?list=LM",
        "thumb_url": lm_thumb,
        "is_album": False,
    })
    seen.add("LM")

    try:
        ll = yt.get_playlist("LL", limit=1)
        ll_tracks = ll.get("tracks") or []
        if ll_tracks:
            items.append({
                "title": "Liked Videos / Songs",
                "url": "https://www.youtube.com/playlist?list=LL",
                "thumb_url": pick_thumb(ll_tracks[0].get("thumbnails"), ll_tracks[0].get("videoId", "")),
                "is_album": False,
            })
            seen.add("LL")
    except Exception:
        pass

    pls = yt.get_library_playlists(limit=None) or []
    for pl in pls:
        pid = (pl.get("playlistId") or "").strip()
        title = (pl.get("title") or "").strip()
        if not pid or not title or pid in seen:
            continue
        seen.add(pid)
        items.append({
            "title": title,
            "url": f"https://music.youtube.com/playlist?list={pid}",
            "thumb_url": pick_thumb(pl.get("thumbnails")),
            "is_album": pid.startswith("OLAK5uy_"),
        })

    for it in items:
        print(json.dumps(it), flush=True)

def cmd_tracks(auth_path, url, limit_arg):
    yt = get_yt(auth_path, require_auth=False)
    item_id = extract_list_id(url)
    limit = int(limit_arg) if limit_arg and int(limit_arg) > 0 else None

    if item_id == "LM":
        yt = get_yt(auth_path, require_auth=True)
        data = yt.get_liked_songs(limit=limit or 5000)
        for t in (data.get("tracks") or []):
            out = make_track_dict(t)
            if out:
                print(json.dumps(out), flush=True)
        return

    if item_id.startswith("MPREb_"):
        data = yt.get_album(item_id)
        album_artist = extract_artists(data)
        album_thumbs = data.get("thumbnails") or []
        for t in (data.get("tracks") or []):
            out = make_track_dict(t, fallback_artists=album_artist, fallback_thumbs=album_thumbs)
            if out:
                print(json.dumps(out), flush=True)
        return

    if item_id.startswith("OLAK5uy_"):
        try:
            data = yt.get_playlist(item_id, limit=limit)
        except Exception:
            bid = yt.get_album_browse_id(item_id)
            data = yt.get_album(bid) if bid else {}
        fallback_artist = extract_artists(data)
        fallback_thumbs = data.get("thumbnails") or []
        for t in (data.get("tracks") or []):
            out = make_track_dict(t, fallback_artists=fallback_artist, fallback_thumbs=fallback_thumbs)
            if out:
                print(json.dumps(out), flush=True)
        return

    pid = item_id[2:] if item_id.startswith("VL") else item_id
    data = yt.get_playlist(pid, limit=limit)
    for t in (data.get("tracks") or []):
        out = make_track_dict(t)
        if out:
            print(json.dumps(out), flush=True)

def cmd_search(auth_path, query, limit_arg):
    yt = get_yt(auth_path, require_auth=False)
    limit = max(1, int(limit_arg)) if limit_arg else 25

    songs = yt.search(query, filter="songs", limit=limit) or []
    for s in songs[:limit]:
        out = make_track_dict(s)
        if out:
            print(json.dumps(out), flush=True)

    albums = yt.search(query, filter="albums", limit=4) or []
    for a in albums[:4]:
        bid = (a.get("browseId") or "").strip()
        title = (a.get("title") or "").strip()
        if not bid or not title:
            continue
        out = {
            "id": bid,
            "title": title,
            "uploader": extract_artists(a),
            "duration_str": "ALBUM ->",
            "thumb_url": pick_thumb(a.get("thumbnails")),
            "kind": "Album",
            "list_url": f"https://music.youtube.com/browse/{bid}",
        }
        print(json.dumps(out), flush=True)

    playlists = yt.search(query, filter="playlists", limit=4) or []
    for p in playlists[:4]:
        bid = (p.get("browseId") or p.get("playlistId") or "").strip()
        pid = bid[2:] if bid.startswith("VL") else bid
        title = (p.get("title") or "").strip()
        if not pid or not title:
            continue
        out = {
            "id": pid,
            "title": title,
            "uploader": extract_artists(p),
            "duration_str": "PLAYLIST ->",
            "thumb_url": pick_thumb(p.get("thumbnails")),
            "kind": "Playlist",
            "list_url": f"https://music.youtube.com/playlist?list={pid}",
        }
        print(json.dumps(out), flush=True)

def resolve_with_ytm_player(vid, wait_for_available):
    try:
        from ytm_player.services.stream import StreamResolver
        resolver = StreamResolver()
        info = resolver.resolve_sync(vid) if wait_for_available else resolver._resolve_sync(vid)
        if info is not None and info.url:
            return info.url, float(info.available_at or 0.0)
    except Exception:
        pass
    return None

def cmd_resolve(auth_path, vid, title="", artist="", wait_flag="1"):
    wait_bool = (wait_flag == "1")
    res = resolve_with_ytm_player(vid, wait_bool)

    # Fallback if the playlist videoId was made Private/unavailable (e.g. HFXQATcL7fg)
    if res is None and title:
        yt = get_yt(auth_path, require_auth=False)
        q = f"{title} {artist}".strip()
        candidates = []
        try:
            wp = yt.get_watch_playlist(videoId=vid, limit=1)
            for t in (wp.get("tracks") or []):
                alt_id = (t.get("videoId") or "").strip()
                if alt_id and alt_id != vid:
                    candidates.append(alt_id)
        except Exception:
            pass

        for flt in ("songs", "videos"):
            try:
                for r in (yt.search(q, filter=flt, limit=3) or []):
                    alt_id = (r.get("videoId") or "").strip()
                    if alt_id and alt_id != vid and alt_id not in candidates:
                        candidates.append(alt_id)
            except Exception:
                pass

        for alt_id in candidates[:4]:
            res = resolve_with_ytm_player(alt_id, wait_bool)
            if res is not None:
                break

    if res is None:
        sys.stderr.write(f"Stream unavailable for {vid} ({title})\n")
        sys.exit(1)

    url, avail_at = res
    print(json.dumps({"url": url, "available_at": avail_at}), flush=True)

def main():
    mode = sys.argv[1]
    auth_path = sys.argv[2] if len(sys.argv) > 2 else ""
    if mode == "playlists":
        cmd_playlists(auth_path)
    elif mode == "tracks":
        cmd_tracks(auth_path, sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else "0")
    elif mode == "search":
        cmd_search(auth_path, sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else "25")
    elif mode == "resolve":
        vid = sys.argv[3]
        title = sys.argv[4] if len(sys.argv) > 4 else ""
        artist = sys.argv[5] if len(sys.argv) > 5 else ""
        wait_flag = sys.argv[6] if len(sys.argv) > 6 else "1"
        cmd_resolve(auth_path, vid, title, artist, wait_flag)

if __name__ == "__main__":
    main()
"###;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Song,
    Album,
    Playlist,
}

impl Default for MediaKind {
    fn default() -> Self {
        Self::Song
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub thumb_url: String,
    #[serde(default)]
    pub is_album: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackItem {
    pub id: String,
    pub title: String,
    pub uploader: String,
    pub duration_str: String,
    #[serde(default)]
    pub thumb_url: String,
    #[serde(default)]
    pub kind: MediaKind,
    #[serde(default)]
    pub list_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct MusicStatsData {
    #[serde(default)]
    song_plays: HashMap<String, (u32, TrackItem)>,
    #[serde(default)]
    playlist_plays: HashMap<String, (u32, PlaylistItem)>,
}

impl MusicStatsData {
    fn path() -> std::path::PathBuf {
        let mut p = glib::user_config_dir();
        p.push("lily-dashboard");
        let _ = std::fs::create_dir_all(&p);
        p.push("music_stats.json");
        p
    }

    fn load() -> Self {
        if let Ok(data) = std::fs::read_to_string(Self::path()) {
            if let Ok(parsed) = serde_json::from_str::<Self>(&data) {
                return parsed;
            }
        }
        Self::default()
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), json);
        }
    }

    fn record_song(&mut self, mut track: TrackItem) {
        track.kind = MediaKind::Song;
        let entry = self.song_plays.entry(track.id.clone()).or_insert((0, track.clone()));
        entry.0 += 1;
        entry.1 = track;
        self.save();
    }

    fn record_playlist(&mut self, pl: PlaylistItem) {
        let entry = self.playlist_plays.entry(pl.url.clone()).or_insert((0, pl.clone()));
        entry.0 += 1;
        entry.1 = pl;
        self.save();
    }

    fn top_playlists(&self, limit: usize) -> Vec<(u32, PlaylistItem)> {
        let mut v: Vec<(u32, PlaylistItem)> = self.playlist_plays.values().cloned().collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v.into_iter().take(limit).collect()
    }

    fn top_songs(&self, limit: usize) -> Vec<(u32, TrackItem)> {
        let mut v: Vec<(u32, TrackItem)> = self.song_plays.values().cloned().collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v.into_iter().take(limit).collect()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum QueueSource {
    Manual,
    PlaylistSequential(u64),
    PlaylistShuffled(u64),
}

enum WorkerMsg {
    PlaylistsLoaded(Result<(String, Vec<PlaylistItem>), String>),
    TrackStreamStart { gen_id: u64, title: String },
    TrackStreamItem { gen_id: u64, track: TrackItem },
    TrackStreamDone { gen_id: u64 },
    TrackStreamError { gen_id: u64, error: String },
    StreamResolved {
        play_gen: u64,
        track: TrackItem,
        result: Result<String, String>,
    },
}

static YTM_PYTHON_BIN: OnceLock<Option<String>> = OnceLock::new();
static STREAM_URL_CACHE: OnceLock<Mutex<HashMap<String, (String, f64, Instant)>>> = OnceLock::new();
static ACTIVE_DOWNLOADS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static URL_TO_PIXEL_HASH: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn augmented_path_env() -> String {
    let home = glib::home_dir();
    let cur_path = std::env::var("PATH").unwrap_or_default();
    format!(
        "{}:{}:{}:{}",
        cur_path,
        home.join(".deno/bin").display(),
        home.join(".local/bin").display(),
        home.join(".cargo/bin").display()
    )
}

fn find_ytmusicapi_python() -> Option<String> {
    YTM_PYTHON_BIN
        .get_or_init(|| {
            let home = glib::home_dir();
            let path_env = augmented_path_env();
            let mut candidates: Vec<String> = Vec::new();

            for bin_name in ["ytm", "ytm-player"] {
                if let Ok(out) = Command::new("which").env("PATH", &path_env).arg(bin_name).output() {
                    if out.status.success() {
                        let bin_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if let Ok(file) = std::fs::File::open(&bin_path) {
                            let mut first_line = String::new();
                            let _ = BufReader::new(file).read_line(&mut first_line);
                            if let Some(shebang) = first_line.strip_prefix("#!") {
                                let interp = shebang.trim().to_string();
                                if !interp.is_empty() && !interp.contains("env ") {
                                    candidates.push(interp);
                                }
                            }
                        }
                    }
                }
            }

            let extra_paths = [
                home.join(".local/share/pipx/venvs/ytm-player/bin/python"),
                home.join(".local/share/pipx/venvs/ytm-player/bin/python3"),
                home.join(".local/share/uv/tools/ytm-player/bin/python"),
                home.join(".local/share/uv/tools/ytm-player/bin/python3"),
            ];
            for p in &extra_paths {
                if p.exists() {
                    candidates.push(p.display().to_string());
                }
            }
            candidates.push("python3".to_string());
            candidates.push("python".to_string());

            for cand in candidates {
                if let Ok(status) = Command::new(&cand)
                    .env("PATH", &path_env)
                    .args(["-c", "import ytmusicapi"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    if status.success() {
                        log_msg(&format!("Found ytmusicapi python: {}", cand));
                        return Some(cand);
                    }
                }
            }
            log_msg("ERROR: No python with ytmusicapi found!");
            None
        })
        .clone()
}

fn find_ytm_auth_file() -> Option<String> {
    let cfg = glib::user_config_dir();
    let candidates = [
        cfg.join("lily-dashboard/auth.json"),
        cfg.join("ytm-player/auth.json"),
        cfg.join("ytm-player/oauth.json"),
    ];
    for p in candidates {
        if p.exists() {
            return Some(p.display().to_string());
        }
    }
    None
}

fn get_stream_cache() -> &'static Mutex<HashMap<String, (String, f64, Instant)>> {
    STREAM_URL_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_cached_stream_entry(video_id: &str) -> Option<(String, f64)> {
    let cache = get_stream_cache().lock().ok()?;
    if let Some((url, avail_at, ts)) = cache.get(video_id) {
        if ts.elapsed() < Duration::from_secs(17000) {
            return Some((url.clone(), *avail_at));
        }
    }
    None
}

fn invalidate_cached_stream(video_id: &str) {
    if let Ok(mut cache) = get_stream_cache().lock() {
        cache.remove(video_id);
    }
}

fn resolve_track_stream(track: &TrackItem, wait_for_available: bool) -> Result<String, String> {
    if let Some((cached_url, avail_at)) = get_cached_stream_entry(&track.id) {
        if wait_for_available {
            let wait_secs = (avail_at - now_unix_secs()).max(0.0);
            if wait_secs > 0.0 {
                log_msg(&format!("Waiting {:.1}s for available_at on cached {}", wait_secs, track.id));
                std::thread::sleep(Duration::from_secs_f64(wait_secs));
            }
        }
        return Ok(cached_url);
    }

    let python = find_ytmusicapi_python().ok_or_else(|| "Python with ytmusicapi not found".to_string())?;
    let auth_path = find_ytm_auth_file().unwrap_or_default();
    let wait_flag = if wait_for_available { "1" } else { "0" };

    let output = Command::new(&python)
        .env("PATH", augmented_path_env())
        .args([
            "-c",
            YTM_BRIDGE_PY,
            "resolve",
            &auth_path,
            &track.id,
            &track.title,
            &track.uploader,
            wait_flag,
        ])
        .output()
        .map_err(|e| format!("Failed to run stream resolver: {}", e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().rev() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                let url = val.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let avail_at = val.get("available_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if url.starts_with("http") {
                    if let Ok(mut cache) = get_stream_cache().lock() {
                        cache.insert(track.id.clone(), (url.clone(), avail_at, Instant::now()));
                    }
                    return Ok(url);
                }
            }
        }
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let err = stderr
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("Stream unavailable")
        .to_string();
    log_msg(&format!("resolve_track_stream failed for {} ('{}'): {}", track.id, track.title, err));
    Err(err)
}

fn prefetch_upcoming_streams(tracks_to_prefetch: Vec<TrackItem>) {
    std::thread::spawn(move || {
        for track in tracks_to_prefetch {
            if get_cached_stream_entry(&track.id).is_some() {
                continue;
            }
            let _ = resolve_track_stream(&track, false);
        }
    });
}

fn hash_key(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

fn hash_key_bytes(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

const MAX_CACHED_TEXTURES: usize = 120;
const MAX_URL_HASH_ENTRIES: usize = 500;
const THUMB_WORKER_COUNT: usize = 3;

thread_local! {
    static TEXTURE_RAM_CACHE: RefCell<(HashMap<String, gtk4::gdk::Texture>, VecDeque<String>)> =
        RefCell::new((HashMap::new(), VecDeque::new()));
}

fn cache_get_texture(key: &str) -> Option<gtk4::gdk::Texture> {
    TEXTURE_RAM_CACHE.with(|cache| cache.borrow().0.get(key).cloned())
}

fn cache_insert_texture(key: String, texture: gtk4::gdk::Texture) {
    TEXTURE_RAM_CACHE.with(|cache| {
        let mut guard = cache.borrow_mut();
        if !guard.0.contains_key(&key) {
            while guard.0.len() >= MAX_CACHED_TEXTURES {
                if let Some(old_key) = guard.1.pop_front() {
                    guard.0.remove(&old_key);
                } else {
                    break;
                }
            }
            guard.1.push_back(key.clone());
        }
        guard.0.insert(key, texture);
    });
}

struct ThumbJob {
    url: String,
    cache_hash: String,
    tx: mpsc::Sender<Option<(glib::Bytes, bool, usize, String)>>,
}

static THUMB_POOL: OnceLock<Arc<(Mutex<VecDeque<ThumbJob>>, Condvar)>> = OnceLock::new();

fn process_thumb_job(job: ThumbJob) {
    let ThumbJob {
        url: url_owned,
        cache_hash,
        tx,
    } = job;

    let mut needs_download = false;
    let tmp_path = format!("{}/{}.tmp", THUMB_RAM_DIR, cache_hash);

    let active_map = ACTIVE_DOWNLOADS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut lock) = active_map.lock() {
        if lock.insert(url_owned.clone()) {
            needs_download = true;
        }
    }

    if needs_download {
        let status = Command::new("curl")
            .args(["-sL", "--max-time", "5", "-o", &tmp_path, &url_owned])
            .status();
        if let Ok(mut lock) = active_map.lock() {
            lock.remove(&url_owned);
        }
        if !status.map(|s| s.success()).unwrap_or(false) {
            let _ = std::fs::remove_file(&tmp_path);
        }
    } else {
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            let map = URL_TO_PIXEL_HASH.get_or_init(|| Mutex::new(HashMap::new()));
            if map.lock().unwrap().contains_key(&url_owned) {
                break;
            }
        }
    }

    let final_ph = URL_TO_PIXEL_HASH
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(&url_owned)
        .cloned();

    let mut cropped_result = None;
    if let Some(ph) = final_ph {
        let final_file = std::path::PathBuf::from(THUMB_RAM_DIR).join(format!("{}.jpg", ph));
        if let Some((bytes, has_alpha, rowstride)) = decode_and_crop_bytes(&final_file) {
            cropped_result = Some((bytes, has_alpha, rowstride, ph));
        }
    } else if std::path::Path::new(&tmp_path).exists() {
        if let Some((bytes, has_alpha, rowstride)) =
            decode_and_crop_bytes(std::path::Path::new(&tmp_path))
        {
            let pixel_hash = hash_key_bytes(&bytes);
            let final_file =
                std::path::PathBuf::from(THUMB_RAM_DIR).join(format!("{}.jpg", pixel_hash));
            if !final_file.exists() {
                let _ = std::fs::rename(&tmp_path, &final_file);
            } else {
                let _ = std::fs::remove_file(&tmp_path);
            }
            let mut map = URL_TO_PIXEL_HASH
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap();
            if map.len() >= MAX_URL_HASH_ENTRIES {
                map.clear();
            }
            map.insert(url_owned, pixel_hash.clone());
            cropped_result = Some((bytes, has_alpha, rowstride, pixel_hash));
        } else {
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
    let _ = tx.send(cropped_result);
}

fn enqueue_thumb_job(job: ThumbJob) {
    let pool = THUMB_POOL.get_or_init(|| {
        let shared = Arc::new((Mutex::new(VecDeque::<ThumbJob>::new()), Condvar::new()));
        for _ in 0..THUMB_WORKER_COUNT {
            let worker_shared = Arc::clone(&shared);
            std::thread::spawn(move || loop {
                let next_job = {
                    let (lock, cvar) = &*worker_shared;
                    let mut q = lock.lock().unwrap();
                    while q.is_empty() {
                        q = cvar.wait(q).unwrap();
                    }
                    q.pop_front().unwrap()
                };
                process_thumb_job(next_job);
            });
        }
        shared
    });

    let (lock, cvar) = &**pool;
    if let Ok(mut q) = lock.lock() {
        q.push_back(job);
        cvar.notify_one();
    }
}

fn decode_and_crop_bytes(path: &std::path::Path) -> Option<(glib::Bytes, bool, usize)> {
    use gtk4::gdk_pixbuf::{InterpType, Pixbuf};
    let pixbuf = Pixbuf::from_file(path).ok()?;
    let w = pixbuf.width();
    let h = pixbuf.height();
    if w <= 0 || h <= 0 {
        return None;
    }
    let side = w.min(h);
    let square = pixbuf.new_subpixbuf((w - side) / 2, (h - side) / 2, side, side);
    let scaled = square.scale_simple(36, 36, InterpType::Bilinear)?;
    Some((scaled.read_pixel_bytes(), scaled.has_alpha(), scaled.rowstride() as usize))
}

fn build_album_art_widget(_key: &str, thumb_url: &str) -> Box {
    let frame = Box::builder()
        .css_classes(vec!["music-art-frame".to_string()])
        .width_request(36)
        .height_request(36)
        .valign(Align::Center)
        .halign(Align::Center)
        .overflow(Overflow::Hidden)
        .build();

    if thumb_url.is_empty() {
        return frame;
    }

    let img = Image::builder().pixel_size(36).width_request(36).height_request(36).build();
    frame.append(&img);

    let url_owned = thumb_url.to_string();
    let known_pixel_hash = {
        let map = URL_TO_PIXEL_HASH.get_or_init(|| Mutex::new(HashMap::new()));
        map.lock().unwrap().get(&url_owned).cloned()
    };

    if let Some(ref ph) = known_pixel_hash {
        if let Some(tex) = cache_get_texture(ph) {
            img.set_paintable(Some(&tex));
            return frame;
        }
    }

    let _ = std::fs::create_dir_all(THUMB_RAM_DIR);
    let cache_hash = hash_key(&url_owned);
    let (tx, rx) = mpsc::channel::<Option<(glib::Bytes, bool, usize, String)>>();

    enqueue_thumb_job(ThumbJob {
        url: url_owned,
        cache_hash,
        tx,
    });

    let img_weak = img.downgrade();
    let mut ticks = 0u8;
    glib::timeout_add_local(Duration::from_millis(45), move || {
        let Some(i) = img_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        ticks += 1;
        if let Ok(res_opt) = rx.try_recv() {
            if let Some((bytes, has_alpha, rowstride, pixel_hash)) = res_opt {
                let fmt = if has_alpha {
                    gtk4::gdk::MemoryFormat::R8g8b8a8
                } else {
                    gtk4::gdk::MemoryFormat::R8g8b8
                };
                let mem_tex = gtk4::gdk::MemoryTexture::new(36, 36, fmt, &bytes, rowstride);
                let texture: gtk4::gdk::Texture = mem_tex.upcast();
                cache_insert_texture(pixel_hash, texture.clone());
                i.set_paintable(Some(&texture));
            }
            return glib::ControlFlow::Break;
        }
        if ticks > 120 {
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    frame
}

struct PlayerState {
    mpv_child: Option<Child>,
    is_paused: bool,
    is_resolving: bool,
    play_gen: Arc<AtomicU64>,
    worker_tx: mpsc::Sender<WorkerMsg>,
    current_track: Option<TrackItem>,
    started_at: Instant,
    has_playback_started: bool,
    reported_load_failure: bool,
    retry_count: u8,
    queue: Vec<TrackItem>,
    queue_idx: usize,
    queue_source: QueueSource,
    mpris: Option<MediaControls>,
    mpris_duration_synced: bool,
}

impl PlayerState {
    fn new(worker_tx: mpsc::Sender<WorkerMsg>) -> Self {
        let _ = std::fs::write(APP_LOG_FILE, "");
        log_msg("=== Initializing PlayerState ===");
        let config = PlatformConfig {
            dbus_name: "lily_ymusic",
            display_name: "Lily YouTube Music",
            hwnd: None,
        };
        let mpris = MediaControls::new(config).ok();

        let mut state = Self {
            mpv_child: None,
            is_paused: false,
            is_resolving: false,
            play_gen: Arc::new(AtomicU64::new(0)),
            worker_tx,
            retry_count: 0,
            current_track: None,
            started_at: Instant::now(),
            has_playback_started: false,
            reported_load_failure: false,
            queue: Vec::new(),
            queue_idx: 0,
            queue_source: QueueSource::Manual,
            mpris,
            mpris_duration_synced: false,
        };
        state.ensure_mpv_daemon();
        state
    }

    fn ensure_mpv_daemon(&mut self) {
        let alive = if let Some(ref mut child) = self.mpv_child {
            matches!(child.try_wait(), Ok(None))
        } else {
            false
        };

        if alive && std::path::Path::new(MPV_SOCKET).exists() {
            return;
        }

        if let Ok(mut old_stream) = UnixStream::connect(MPV_SOCKET) {
            let _ = old_stream.set_write_timeout(Some(Duration::from_millis(50)));
            let _ = old_stream.write_all(b"{\"command\": [\"quit\"]}\n");
            std::thread::sleep(Duration::from_millis(40));
        }

        let _ = std::fs::remove_file(MPV_SOCKET);
        let mut cmd = Command::new("mpv");
        cmd.env("PATH", augmented_path_env());
        cmd.args([
            "--idle=yes",
            "--no-video",
            "--no-ytdl",
            "--load-scripts=no",
            "--demuxer-lavf-o=rw_timeout=4000000",
            "--audio-buffer=0.2",
            &format!("--input-ipc-server={}", MPV_SOCKET),
        ]);

        match cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(child) => {
                log_msg(&format!("Spawned mpv daemon (pid={})", child.id()));
                self.mpv_child = Some(child);
            }
            Err(e) => log_msg(&format!("ERROR spawning mpv: {}", e)),
        }
    }

    fn send_mpv_load(&mut self, track: &TrackItem, direct_url: &str) {
        self.ensure_mpv_daemon();

        let load_cmd = serde_json::json!({
            "command": ["loadfile", direct_url, "replace"]
        })
        .to_string()
            + "\n";

        let title_cmd = serde_json::json!({
            "command": ["set_property", "force-media-title", track.title]
        })
        .to_string()
            + "\n";

        let unpause_cmd = b"{\"command\": [\"set_property\", \"pause\", false]}\n";

        let mut sent = false;
        for _ in 0..30 {
            if let Ok(mut stream) = UnixStream::connect(MPV_SOCKET) {
                let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
                let _ = stream.write_all(load_cmd.as_bytes());
                let _ = stream.write_all(title_cmd.as_bytes());
                let _ = stream.write_all(unpause_cmd);
                sent = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        if sent {
            self.is_paused = false;
            self.is_resolving = false;
            self.started_at = Instant::now();
            self.has_playback_started = false;
            self.reported_load_failure = false;

            let clean_artist = track.uploader.strip_suffix(" - Topic").unwrap_or(&track.uploader);
            let cover_url = if !track.thumb_url.is_empty() {
                track.thumb_url.clone()
            } else {
                format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", track.id)
            };

            if let Some(ref mut mpris) = self.mpris {
                let _ = mpris.set_metadata(MediaMetadata {
                    title: Some(&track.title),
                    artist: Some(clean_artist),
                    album: Some("YouTube Music"),
                    cover_url: Some(&cover_url),
                    duration: parse_mm_ss(&track.duration_str),
                });
                let _ = mpris.set_playback(MediaPlayback::Playing {
                    progress: Some(MediaPosition(Duration::ZERO)),
                });
            }
        }
    }

    fn play_track(&mut self, track: TrackItem) {
        self.ensure_mpv_daemon();
        let is_same_track = self
            .current_track
            .as_ref()
            .map(|c| c.id == track.id)
            .unwrap_or(false);
        if !is_same_track {
            self.retry_count = 0;
        }
        // ... rest of play_track unchanged ...
        let gen = self.play_gen.fetch_add(1, Ordering::Relaxed) + 1;
        self.current_track = Some(track.clone());
        self.is_paused = false;
        self.has_playback_started = false;
        self.reported_load_failure = false;

        // If already cached and available_at has passed, load immediately in 0.00s!
        if let Some((cached_url, avail_at)) = get_cached_stream_entry(&track.id) {
            if avail_at <= now_unix_secs() + 0.05 {
                log_msg(&format!("play_track (instant cached): id={} title='{}'", track.id, track.title));
                self.send_mpv_load(&track, &cached_url);
                self.trigger_prefetch();
                return;
            }
        }

        // Otherwise stop current audio and resolve (including available_at wait) on background thread
        if let Ok(mut stream) = UnixStream::connect(MPV_SOCKET) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(30)));
            let _ = stream.write_all(b"{\"command\": [\"stop\"]}\n");
        }

        self.is_resolving = true;
        self.started_at = Instant::now();
        log_msg(&format!("play_track (resolving): id={} title='{}'", track.id, track.title));

        let tx = self.worker_tx.clone();
        let track_bg = track.clone();
        std::thread::spawn(move || {
            let result = resolve_track_stream(&track_bg, true);
            let _ = tx.send(WorkerMsg::StreamResolved {
                play_gen: gen,
                track: track_bg,
                result,
            });
        });

        self.trigger_prefetch();
    }

    fn trigger_prefetch(&self) {
        let mut upcoming = Vec::new();
        for offset in 1..=3 {
            if let Some(next_track) = self.queue.get(self.queue_idx + offset) {
                if next_track.kind == MediaKind::Song {
                    upcoming.push(next_track.clone());
                }
            }
        }
        if !upcoming.is_empty() {
            prefetch_upcoming_streams(upcoming);
        }
    }

    fn query_progress(&mut self) -> Option<(f64, f64)> {
    if self.is_resolving {
        return None;
    }
    let track = self.current_track.clone()?;
    let pos = query_mpv_f64("time-pos")?;
    let dur = query_mpv_f64("duration")?;
    if dur > 0.0 {
        self.has_playback_started = true;
        let pos_clamped = pos.max(0.0);

        // Only push metadata to MPRIS once per track when real duration is known
        if !self.mpris_duration_synced {
            self.mpris_duration_synced = true;
            if let Some(ref mut mpris) = self.mpris {
                let clean_artist = track.uploader.strip_suffix(" - Topic").unwrap_or(&track.uploader);
                let cover_url = if !track.thumb_url.is_empty() {
                    track.thumb_url.clone()
                } else {
                    format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", track.id)
                };
                let _ = mpris.set_metadata(MediaMetadata {
                    title: Some(&track.title),
                    artist: Some(clean_artist),
                    album: Some("YouTube Music"),
                    cover_url: Some(&cover_url),
                    duration: Some(Duration::from_secs_f64(dur)),
                });
            }
        }

        Some((pos_clamped, dur))
    } else {
        None
    }
}

    fn seek_to(&mut self, seconds: f64) {
        if self.current_track.is_none() || self.is_resolving {
            return;
        }
        if let Ok(mut stream) = UnixStream::connect(MPV_SOCKET) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(30)));
            let cmd = format!("{{\"command\": [\"seek\", {:.2}, \"absolute\"]}}\n", seconds.max(0.0));
            let _ = stream.write_all(cmd.as_bytes());
        }
        if let Some(ref mut mpris) = self.mpris {
            let position = Some(MediaPosition(Duration::from_secs_f64(seconds.max(0.0))));
            let state = if self.is_paused {
                MediaPlayback::Paused { progress: position }
            } else {
                MediaPlayback::Playing { progress: position }
            };
            let _ = mpris.set_playback(state);
        }
    }

    fn set_volume(&self, volume: f64) {
        if let Ok(mut stream) = UnixStream::connect(MPV_SOCKET) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(30)));
            let cmd = format!("{{\"command\": [\"set_property\", \"volume\", {:.0}]}}\n", volume.clamp(0.0, 100.0));
            let _ = stream.write_all(cmd.as_bytes());
        }
    }

    fn play_next_in_queue(&mut self, track: TrackItem) -> Option<TrackItem> {
        if self.queue.is_empty() || self.current_track.is_none() {
            self.queue = vec![track.clone()];
            self.queue_idx = 0;
            self.play_track(track.clone());
            return Some(track);
        }
        let insert_pos = (self.queue_idx + 1).min(self.queue.len());
        self.queue.insert(insert_pos, track);
        self.trigger_prefetch();
        None
    }

    fn add_to_queue(&mut self, track: TrackItem) -> Option<TrackItem> {
        if self.queue.is_empty() || self.current_track.is_none() {
            self.queue = vec![track.clone()];
            self.queue_idx = 0;
            self.play_track(track.clone());
            return Some(track);
        }
        self.queue.push(track);
        self.trigger_prefetch();
        None
    }

    fn on_background_track_arrived(&mut self, gen_id: u64, track: TrackItem) {
        if track.kind != MediaKind::Song {
            return;
        }
        match self.queue_source {
            QueueSource::PlaylistSequential(g) if g == gen_id => self.queue.push(track),
            QueueSource::PlaylistShuffled(g) if g == gen_id => {
                let min_pos = (self.queue_idx + 1).min(self.queue.len());
                let span = self.queue.len() - min_pos + 1;
                let offset = random_usize() % span.max(1);
                self.queue.insert(min_pos + offset, track);
            }
            _ => {}
        }
    }

    fn check_autoplay_next(&mut self) -> Result<Option<TrackItem>, String> {
        if self.is_resolving {
            return Ok(None);
        }
        let Some(cur) = self.current_track.clone() else {
            return Ok(None);
        };
        if self.started_at.elapsed() < Duration::from_secs(2) {
            return Ok(None);
        }
        if query_mpv_bool("idle-active").unwrap_or(false) {
            if self.has_playback_started {
                log_msg(&format!("Track '{}' finished; advancing to next", cur.title));
                return Ok(self.play_next());
            } else if !self.reported_load_failure && self.started_at.elapsed() >= Duration::from_secs(3) {
                invalidate_cached_stream(&cur.id);
                if self.retry_count < 1 {
                    self.retry_count += 1;
                    log_msg(&format!(
                        "Cached stream failed for '{}' (id={}); evicting and retrying fresh resolve",
                        cur.title, cur.id
                    ));
                    self.play_track(cur);
                    return Ok(None);
                }
                self.reported_load_failure = true;
                let msg = format!("mpv failed to open stream for '{}'", cur.title);
                log_msg(&format!("ERROR: {}", msg));
                return Err(msg);
            }
        }
        Ok(None)
    }

    fn toggle_pause(&mut self) -> bool {
        if self.current_track.is_none() {
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

    fn play_next(&mut self) -> Option<TrackItem> {
        if self.queue.is_empty() {
            return None;
        }
        if self.queue_idx + 1 < self.queue.len() {
            self.queue_idx += 1;
            let next = self.queue[self.queue_idx].clone();
            self.play_track(next.clone());
            return Some(next);
        }
        None
    }

    fn play_prev(&mut self) -> Option<TrackItem> {
        if self.queue.is_empty() {
            return None;
        }
        if self.queue_idx > 0 {
            self.queue_idx -= 1;
            let prev = self.queue[self.queue_idx].clone();
            self.play_track(prev.clone());
            return Some(prev);
        }
        None
    }
}

fn query_mpv_f64(prop: &str) -> Option<f64> {
    let mut stream = UnixStream::connect(MPV_SOCKET).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(25)));
    let cmd = format!("{{\"command\": [\"get_property\", \"{}\"]}}\n", prop);
    stream.write_all(cmd.as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let val: serde_json::Value = serde_json::from_str(&line).ok()?;
    val.get("data")?.as_f64()
}

fn query_mpv_bool(prop: &str) -> Option<bool> {
    let mut stream = UnixStream::connect(MPV_SOCKET).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(25)));
    let cmd = format!("{{\"command\": [\"get_property\", \"{}\"]}}\n", prop);
    stream.write_all(cmd.as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let val: serde_json::Value = serde_json::from_str(&line).ok()?;
    val.get("data")?.as_bool()
}

fn query_mpv_string(prop: &str) -> Option<String> {
    let mut stream = UnixStream::connect(MPV_SOCKET).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(25)));
    let cmd = format!("{{\"command\": [\"get_property\", \"{}\"]}}\n", prop);
    stream.write_all(cmd.as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let val: serde_json::Value = serde_json::from_str(&line).ok()?;
    val.get("data")?.as_str().map(|s| s.trim().to_string())
}

fn format_mm_ss(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn parse_mm_ss(s: &str) -> Option<Duration> {
    let mut parts = s.split(':');
    let mins: u64 = parts.next()?.parse().ok()?;
    let secs: u64 = parts.next()?.parse().ok()?;
    Some(Duration::from_secs(mins * 60 + secs))
}

fn random_usize() -> usize {
    let mut buf = [0u8; 4];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    u32::from_le_bytes(buf) as usize
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
        let rnd = u32::from_le_bytes([rand_bytes[b], rand_bytes[b + 1], rand_bytes[b + 2], rand_bytes[b + 3]]) as usize;
        tracks.swap(i, rnd % (i + 1));
    }
}

fn fallback_playlist_thumb(playlist_url: &str, current_thumb: &str) -> String {
    if !current_thumb.is_empty() {
        return current_thumb.to_string();
    }
    if let Some(tracks) = load_cached_tracks(playlist_url) {
        if let Some(first) = tracks.first() {
            if !first.thumb_url.is_empty() {
                return first.thumb_url.clone();
            }
            if first.id.len() == 11 {
                return format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", first.id);
            }
        }
    }
    String::new()
}

fn fetch_user_playlists() -> Result<(String, Vec<PlaylistItem>), String> {
    let python = find_ytmusicapi_python()
        .ok_or_else(|| "Python with ytmusicapi not found (is ytm-player installed?)".to_string())?;
    let auth_path = find_ytm_auth_file()
        .ok_or_else(|| "No auth.json found in ~/.config/ytm-player/ (run 'ytm setup')".to_string())?;

    let output = Command::new(&python)
        .env("PATH", augmented_path_env())
        .args(["-c", YTM_BRIDGE_PY, "playlists", &auth_path])
        .output()
        .map_err(|e| format!("Failed to run ytmusicapi: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log_msg(&format!("fetch_user_playlists error: {}", stderr.trim()));
        let msg = stderr
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("ytmusicapi playlist fetch failed")
            .to_string();
        return Err(msg);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut playlists = Vec::new();
    for line in stdout.lines() {
        if let Ok(mut pl) = serde_json::from_str::<PlaylistItem>(line) {
            pl.thumb_url = fallback_playlist_thumb(&pl.url, &pl.thumb_url);
            playlists.push(pl);
        }
    }

    log_msg(&format!("Loaded {} playlists via ytmusicapi", playlists.len()));
    if playlists.is_empty() {
        return Err("ytmusicapi returned no playlists.".to_string());
    }
    Ok((auth_path, playlists))
}

fn music_cache_dir() -> std::path::PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    path.push("music_cache");
    let _ = std::fs::create_dir_all(&path);
    path
}

fn playlist_cache_path(url: &str) -> std::path::PathBuf {
    let safe_name: String = url.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    music_cache_dir().join(format!("{}.json", safe_name))
}

fn load_cached_tracks(url: &str) -> Option<Vec<TrackItem>> {
    let data = std::fs::read_to_string(playlist_cache_path(url)).ok()?;
    let tracks: Vec<TrackItem> = serde_json::from_str(&data).ok()?;
    if tracks.is_empty() { None } else { Some(tracks) }
}

fn save_cached_tracks(url: &str, tracks: &[TrackItem]) {
    if !tracks.is_empty() {
        if let Ok(json) = serde_json::to_string(tracks) {
            let _ = std::fs::write(playlist_cache_path(url), json);
        }
    }
}

fn run_ytmusic_tracks(
    auth_path: &str,
    mode: &str,
    target: &str,
    limit: u32,
    gen_id: u64,
    active_gen: &Arc<AtomicU64>,
    tx: &mpsc::Sender<WorkerMsg>,
) -> Result<usize, String> {
    let python = find_ytmusicapi_python().ok_or_else(|| "Python with ytmusicapi not found".to_string())?;

    let mut child = Command::new(&python)
        .env("PATH", augmented_path_env())
        .args(["-c", YTM_BRIDGE_PY, mode, auth_path, target, &limit.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ytmusicapi error: {}", e))?;

    let mut count = 0;
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if active_gen.load(Ordering::Relaxed) != gen_id {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(count);
            }
            if let Ok(track) = serde_json::from_str::<TrackItem>(&line) {
                count += 1;
                let _ = tx.send(WorkerMsg::TrackStreamItem { gen_id, track });
            }
        }
    }

    let status = child.wait().map_err(|e| format!("ytmusicapi wait error: {}", e))?;
    if !status.success() && count == 0 {
        let mut err_msg = String::from("ytmusicapi request failed");
        if let Some(mut stderr) = child.stderr.take() {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            log_msg(&format!("run_ytmusic_tracks stderr ({}): {}", target, buf.trim()));
            if let Some(last) = buf.lines().rfind(|l| !l.trim().is_empty()) {
                err_msg = last.to_string();
            }
        }
        return Err(err_msg);
    }
    Ok(count)
}

fn stream_ytm_tracks(
    auth_path_opt: Option<String>,
    title: String,
    query_or_url: String,
    is_search: bool,
    search_limit: u32,
    gen_id: u64,
    active_gen: Arc<AtomicU64>,
    tx: mpsc::Sender<WorkerMsg>,
) {
    let _ = tx.send(WorkerMsg::TrackStreamStart { gen_id, title });

    let auth_owned = auth_path_opt
        .filter(|s| !s.is_empty())
        .or_else(find_ytm_auth_file)
        .unwrap_or_default();

    if is_search {
        match run_ytmusic_tracks(&auth_owned, "search", &query_or_url, search_limit.max(1), gen_id, &active_gen, &tx) {
            Ok(_) => {
                if active_gen.load(Ordering::Relaxed) == gen_id {
                    let _ = tx.send(WorkerMsg::TrackStreamDone { gen_id });
                }
            }
            Err(error) => {
                if active_gen.load(Ordering::Relaxed) == gen_id {
                    let _ = tx.send(WorkerMsg::TrackStreamError { gen_id, error });
                }
            }
        }
        return;
    }

    if let Some(mut cached_tracks) = load_cached_tracks(&query_or_url) {
        let mut known_ids: HashSet<String> = cached_tracks.iter().map(|t| t.id.clone()).collect();
        for track in &cached_tracks {
            if active_gen.load(Ordering::Relaxed) != gen_id {
                return;
            }
            let _ = tx.send(WorkerMsg::TrackStreamItem { gen_id, track: track.clone() });
        }

        let (delta_tx, delta_rx) = mpsc::channel::<WorkerMsg>();
        let _ = run_ytmusic_tracks(&auth_owned, "tracks", &query_or_url, 25, gen_id, &active_gen, &delta_tx);

        let mut new_tracks = Vec::new();
        while let Ok(WorkerMsg::TrackStreamItem { track, .. }) = delta_rx.try_recv() {
            if known_ids.insert(track.id.clone()) {
                let _ = tx.send(WorkerMsg::TrackStreamItem { gen_id, track: track.clone() });
                new_tracks.push(track);
            }
        }

        if !new_tracks.is_empty() {
            new_tracks.append(&mut cached_tracks);
            save_cached_tracks(&query_or_url, &new_tracks);
        }
        if active_gen.load(Ordering::Relaxed) == gen_id {
            let _ = tx.send(WorkerMsg::TrackStreamDone { gen_id });
        }
        return;
    }

    let (spy_tx, spy_rx) = mpsc::channel::<WorkerMsg>();
    let res = run_ytmusic_tracks(&auth_owned, "tracks", &query_or_url, 0, gen_id, &active_gen, &spy_tx);

    let mut collected = Vec::new();
    while let Ok(msg) = spy_rx.try_recv() {
        if let WorkerMsg::TrackStreamItem { ref track, .. } = msg {
            collected.push(track.clone());
        }
        let _ = tx.send(msg);
    }

    match res {
        Ok(_) => {
            if active_gen.load(Ordering::Relaxed) == gen_id {
                save_cached_tracks(&query_or_url, &collected);
                let _ = tx.send(WorkerMsg::TrackStreamDone { gen_id });
            }
        }
        Err(error) => {
            if active_gen.load(Ordering::Relaxed) == gen_id {
                let _ = tx.send(WorkerMsg::TrackStreamError { gen_id, error });
            }
        }
    }
}

pub fn build_music_ui() -> Box {
    let root = Box::builder().orientation(Orientation::Vertical).hexpand(true).vexpand(true).build();
    let view_stack = Stack::new();
    view_stack.set_hhomogeneous(false);
    view_stack.set_hexpand(true);
    view_stack.set_vexpand(true);

    let music_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let top_nav_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let home_nav_btn = Button::builder()
        .label("Home")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .hexpand(true)
        .build();
    let playlists_nav_btn = Button::builder()
        .label("Playlists")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .build();
    top_nav_row.append(&home_nav_btn);
    top_nav_row.append(&playlists_nav_btn);

    let search_entry = SearchEntry::builder()
        .placeholder_text("Search YouTube Music...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let nav_stack = Stack::new();
    nav_stack.set_hhomogeneous(false);
    nav_stack.set_hexpand(true);
    nav_stack.set_vexpand(true);

    // PAGE 0: HOME
    let home_screen = Box::builder().orientation(Orientation::Vertical).spacing(6).hexpand(true).vexpand(true).build();
    let home_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();
    let home_content_box = Box::builder().orientation(Orientation::Vertical).spacing(6).build();
    home_scroll.set_child(Some(&home_content_box));
    home_screen.append(&home_scroll);

    // PAGE A: PLAYLISTS
    let playlists_screen = Box::builder().orientation(Orientation::Vertical).spacing(6).hexpand(true).vexpand(true).build();
    let pl_header_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let pl_header_lbl = Label::builder()
        .label("YOUR PLAYLISTS")
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    let reload_pl_btn = Button::builder().label("Reload").css_classes(vec!["plan-close-btn".to_string()]).build();
    pl_header_row.append(&pl_header_lbl);
    pl_header_row.append(&reload_pl_btn);

    let pl_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();
    let playlists_box = Box::builder().orientation(Orientation::Vertical).spacing(6).build();
    pl_scroll.set_child(Some(&playlists_box));
    playlists_screen.append(&pl_header_row);
    playlists_screen.append(&pl_scroll);

    // PAGE B: TRACKS
    let tracks_screen = Box::builder().orientation(Orientation::Vertical).spacing(6).hexpand(true).vexpand(true).build();
    let tr_top_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let back_to_pl_btn = Button::builder().label("<- Back").css_classes(vec!["plan-close-btn".to_string()]).build();
    let shuffle_btn = Button::builder()
        .label("Shuffle Play")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .hexpand(true)
        .build();
    let refresh_tracks_btn = Button::builder()
        .label("Refresh")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Clear cache and re-fetch this playlist")
        .build();
    tr_top_row.append(&back_to_pl_btn);
    tr_top_row.append(&shuffle_btn);
    tr_top_row.append(&refresh_tracks_btn);

    let active_pl_title_lbl = Label::builder()
        .label("PLAYLIST")
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-group-header".to_string()])
        .tooltip_text("Double-click to change max search results count")
        .build();

    let limit_edit_row = Box::builder().orientation(Orientation::Horizontal).spacing(4).visible(false).build();
    let limit_lbl = Label::builder().label("Max:").css_classes(vec!["fav-score-badge".to_string()]).build();
    let limit_entry = Entry::builder()
        .text("25")
        .width_chars(5)
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();
    let limit_save_btn = Button::builder().label("Save").css_classes(vec!["plan-close-btn".to_string()]).build();
    let limit_cancel_btn = Button::builder().label("×").css_classes(vec!["plan-line-del-btn".to_string()]).build();
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
    let track_list_box = Box::builder().orientation(Orientation::Vertical).spacing(6).build();
    tr_scroll.set_child(Some(&track_list_box));
    tracks_screen.append(&tr_top_row);
    tracks_screen.append(&active_pl_title_lbl);
    tracks_screen.append(&limit_edit_row);
    tracks_screen.append(&tr_scroll);

    // PAGE C: QUEUE
    let queue_screen = Box::builder().orientation(Orientation::Vertical).spacing(6).hexpand(true).vexpand(true).build();
    let q_top_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let back_from_q_btn = Button::builder().label("<- Back").css_classes(vec!["plan-close-btn".to_string()]).build();
    let q_title_lbl = Label::builder()
        .label("ACTIVE QUEUE")
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
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
    let queue_list_box = Box::builder().orientation(Orientation::Vertical).spacing(6).build();
    q_scroll.set_child(Some(&queue_list_box));
    queue_screen.append(&q_top_row);
    queue_screen.append(&q_scroll);

    nav_stack.add_named(&home_screen, Some("home"));
    nav_stack.add_named(&playlists_screen, Some("playlists"));
    nav_stack.add_named(&tracks_screen, Some("tracks"));
    nav_stack.add_named(&queue_screen, Some("queue"));

    // PLAYER DOCK
    let player_dock = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .css_classes(vec!["plan-composer-dock".to_string()])
        .build();
    let np_header_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let now_playing_lbl = Label::builder()
        .label("Not Playing")
        .halign(Align::Start)
        .valign(Align::Center)
        .css_classes(vec!["plan-step".to_string()])
        .build();
    let title_marquee_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::External)
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

    {
        let scroll_anim = title_marquee_scroll.clone();
        let dir = Rc::new(Cell::new(1.0_f64));
        let pause_ticks = Rc::new(Cell::new(30_u32));
        let last_seen_title = Rc::new(RefCell::new(String::new()));
        let lbl_watch = now_playing_lbl.clone();

        glib::timeout_add_local(Duration::from_millis(50), move || {
            let adj = scroll_anim.hadjustment();
            let cur_title = lbl_watch.text().to_string();
            if *last_seen_title.borrow() != cur_title {
                *last_seen_title.borrow_mut() = cur_title;
                adj.set_value(0.0);
                dir.set(1.0);
                pause_ticks.set(30);
                return glib::ControlFlow::Continue;
            }
            let max_scroll = (adj.upper() - adj.page_size()).max(0.0);
            if max_scroll <= 2.0 {
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
                pause_ticks.set(30);
            } else if next_val <= 0.0 {
                adj.set_value(0.0);
                dir.set(1.0);
                pause_ticks.set(35);
            } else {
                adj.set_value(next_val);
            }
            glib::ControlFlow::Continue
        });
    }

    let meta_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
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

    let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    seek_scale.set_draw_value(false);
    seek_scale.set_hexpand(true);
    seek_scale.set_css_classes(&["music-seek"]);

    let controls_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let prev_btn = Button::builder().label("󰒮").css_classes(vec!["plan-fmt-btn".to_string()]).hexpand(true).build();
    let play_pause_btn = Button::builder().label("Play / Pause").css_classes(vec!["primary-btn".to_string()]).hexpand(true).build();
    let next_btn = Button::builder().label("󰒭").css_classes(vec!["plan-fmt-btn".to_string()]).hexpand(true).build();
    controls_row.append(&prev_btn);
    controls_row.append(&play_pause_btn);
    controls_row.append(&next_btn);

    let vol_row = Box::builder().orientation(Orientation::Horizontal).spacing(8).build();
    let vol_lbl = Label::builder().label("Vol").css_classes(vec!["fav-score-badge".to_string()]).build();
    let vol_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    vol_scale.set_value(100.0);
    vol_scale.set_draw_value(false);
    vol_scale.set_hexpand(true);
    vol_scale.set_css_classes(&["music-seek"]);
    let vol_pct_lbl = Label::builder()
        .label("100%")
        .width_chars(4)
        .halign(Align::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    vol_row.append(&vol_lbl);
    vol_row.append(&vol_scale);
    vol_row.append(&vol_pct_lbl);

    player_dock.append(&np_header_row);
    player_dock.append(&meta_row);
    player_dock.append(&seek_scale);
    player_dock.append(&controls_row);
    player_dock.append(&vol_row);

    music_page.append(&top_nav_row);
    music_page.append(&search_entry);
    music_page.append(&nav_stack);
    music_page.append(&player_dock);

    // FALLBACK ERROR VIEW
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
    let err_open_btn = Button::builder().css_classes(vec!["fav-item-btn".to_string()]).build();
    let err_msg_lbl = Label::builder()
        .label("Run `ytm setup` in your terminal\nto generate auth.json.")
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .max_width_chars(24)
        .css_classes(vec!["plan-body".to_string()])
        .build();
    err_open_btn.set_child(Some(&err_msg_lbl));
    err_open_btn.connect_clicked(|_| {
        let _ = Command::new("xdg-open").arg("https://music.youtube.com").spawn();
    });
    let err_detail_lbl = Label::builder()
        .label("")
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .max_width_chars(26)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    let err_refresh_btn = Button::builder().label("Refresh Session").css_classes(vec!["plan-close-btn".to_string()]).build();
    error_page.append(&err_title);
    error_page.append(&err_open_btn);
    error_page.append(&err_detail_lbl);
    error_page.append(&err_refresh_btn);

    view_stack.add_named(&music_page, Some("music"));
    view_stack.add_named(&error_page, Some("error"));
    root.append(&view_stack);

    // State
    let (tx, rx) = mpsc::channel::<WorkerMsg>();
    let (mpris_tx, mpris_rx) = mpsc::channel::<MediaControlEvent>();
    let player = Rc::new(RefCell::new(PlayerState::new(tx.clone())));
    let stats = Rc::new(RefCell::new(MusicStatsData::load()));
    let active_auth_path = Rc::new(RefCell::new(find_ytm_auth_file().unwrap_or_default()));
    let all_playlists: Rc<RefCell<Vec<PlaylistItem>>> = Rc::new(RefCell::new(Vec::new()));
    let current_playlist_tracks: Rc<RefCell<Vec<TrackItem>>> = Rc::new(RefCell::new(Vec::new()));
    let track_filter_query = Rc::new(RefCell::new(String::new()));
    let pending_ui_tracks: Rc<RefCell<VecDeque<(u64, TrackItem, bool)>>> = Rc::new(RefCell::new(VecDeque::new()));
    let active_stream_gen = Arc::new(AtomicU64::new(0));
    let current_view_title = Rc::new(RefCell::new(String::new()));
    let current_view_url = Rc::new(RefCell::new(String::new()));
    let is_stream_finished = Rc::new(Cell::new(false));
    let prev_nav_page = Rc::new(RefCell::new("home".to_string()));
    let search_limit = Rc::new(Cell::new(25u32));
    let last_search_query = Rc::new(RefCell::new(String::new()));
    let last_user_seek = Rc::new(Cell::new(Instant::now() - Duration::from_secs(10)));

    if let Some(ref mut mpris) = player.borrow_mut().mpris {
        let _ = mpris.attach(move |event| {
            let _ = mpris_tx.send(event);
        });
    }

    title_marquee_scroll.set_tooltip_text(Some("Left-click to copy: song | album | artist"));
    let title_click = GestureClick::builder().button(1).build();
    let player_copy = player.clone();
    let artist_lbl_feedback = now_artist_lbl.clone();
    title_click.connect_pressed(move |_, _, _, _| {
        let Some(track) = player_copy.borrow().current_track.clone() else {
            return;
        };
        let song = track.title.trim().to_string();
        let album = query_mpv_string("metadata/by-key/album")
            .or_else(|| query_mpv_string("metadata/by-key/Album"))
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "Single".to_string());
        let raw_artist = query_mpv_string("metadata/by-key/artist")
            .or_else(|| query_mpv_string("metadata/by-key/Artist"))
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| track.uploader.clone());
        let artist = raw_artist.strip_suffix(" - Topic").unwrap_or(&raw_artist).trim().to_string();
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

    let player_seek = player.clone();
    let last_seek_stamp = last_user_seek.clone();
    let time_lbl_seek = time_lbl.clone();
    let player_vol = player.clone();
    let vol_pct_clone = vol_pct_lbl.clone();
    vol_scale.connect_value_changed(move |scale| {
        let val = scale.value().clamp(0.0, 100.0);
        vol_pct_clone.set_label(&format!("{:.0}%", val));
        player_vol.borrow().set_volume(val);
    });
    seek_scale.connect_change_value(move |scale, _, new_val| {
        let max_dur = scale.adjustment().upper();
        let target = new_val.clamp(0.0, max_dur);
        time_lbl_seek.set_label(&format!("{} / {}", format_mm_ss(target), format_mm_ss(max_dur)));
        if last_seek_stamp.get().elapsed() >= Duration::from_millis(80) {
            last_seek_stamp.set(Instant::now());
            player_seek.borrow_mut().seek_to(target);
        }
        glib::Propagation::Proceed
    });

    let q_win_start = Rc::new(Cell::new(0usize));
    let refresh_queue_ui: Rc<dyn Fn()> = {
        let q_box = queue_list_box.clone();
        let q_btn = queue_view_btn.clone();
        let player_q = player.clone();
        let np_t_q = now_playing_lbl.clone();
        let np_a_q = now_artist_lbl.clone();
        let pp_q = play_pause_btn.clone();
        let q_ws = q_win_start.clone();
        let q_scroll_jump = q_scroll.clone();

        let refresh_rc: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let refresh_weak = refresh_rc.clone();

        let closure: Rc<dyn Fn()> = Rc::new(move || {
            let cur_idx = player_q.borrow().queue_idx;
            let desired_start = cur_idx.saturating_sub(4);
            q_ws.set(desired_start);
            render_queue_view(&q_box, &q_btn, &player_q, &np_t_q, &np_a_q, &pp_q, &refresh_weak, desired_start);
            let target_y = (cur_idx.saturating_sub(2) as f64) * 56.0;
            let adj = q_scroll_jump.vadjustment();
            glib::idle_add_local_once(move || {
                adj.set_value(target_y);
            });
        });
        *refresh_rc.borrow_mut() = Some(closure.clone());

        let q_vadj = q_scroll.vadjustment();
        let q_ws_scroll = q_win_start.clone();
        let q_box_s = queue_list_box.clone();
        let q_btn_s = queue_view_btn.clone();
        let player_s = player.clone();
        let np_t_s = now_playing_lbl.clone();
        let np_a_s = now_artist_lbl.clone();
        let pp_s = play_pause_btn.clone();
        let refresh_s = refresh_rc.clone();

        q_vadj.connect_value_changed(move |adj| {
            let scroll_y = adj.value().max(0.0) as usize;
            let desired = (scroll_y / 56).saturating_sub(8);
            let current = q_ws_scroll.get();
            if desired.abs_diff(current) >= 6 || (desired == 0 && current != 0) {
                q_ws_scroll.set(desired);
                render_queue_view(&q_box_s, &q_btn_s, &player_s, &np_t_s, &np_a_s, &pp_s, &refresh_s, desired);
            }
        });
        closure
    };

    let open_playlist_or_album: Rc<dyn Fn(PlaylistItem)> = {
        let nav_o = nav_stack.clone();
        let home_btn_o = home_nav_btn.clone();
        let pl_btn_o = playlists_nav_btn.clone();
        let search_o = search_entry.clone();
        let filter_o = track_filter_query.clone();
        let gen_o = active_stream_gen.clone();
        let auth_o = active_auth_path.clone();
        let tx_o = tx.clone();
        let stats_o = stats.clone();
        let url_o = current_view_url.clone();

        Rc::new(move |pl: PlaylistItem| {
            stats_o.borrow_mut().record_playlist(pl.clone());
            *filter_o.borrow_mut() = String::new();
            *url_o.borrow_mut() = pl.url.clone();
            search_o.set_text("");
            search_o.set_placeholder_text(Some("Filter songs in this list..."));
            home_btn_o.remove_css_class("active");
            pl_btn_o.add_css_class("active");
            nav_o.set_visible_child_name("tracks");

            let new_gen = gen_o.fetch_add(1, Ordering::Relaxed) + 1;
            let auth = auth_o.borrow().clone();
            let tx_w = tx_o.clone();
            let gen_arc = gen_o.clone();
            let title_owned = pl.title.to_uppercase();
            let url_owned = pl.url.clone();

            std::thread::spawn(move || {
                let auth_opt = if auth.is_empty() { None } else { Some(auth) };
                stream_ytm_tracks(auth_opt, title_owned, url_owned, false, 50, new_gen, gen_arc, tx_w);
            });
        })
    };

    {
        let url_ref = current_view_url.clone();
        let title_ref = current_view_title.clone();
        let gen_ref = active_stream_gen.clone();
        let auth_ref = active_auth_path.clone();
        let tx_ref = tx.clone();

        refresh_tracks_btn.connect_clicked(move |_| {
            let url = url_ref.borrow().clone();
            let title = title_ref.borrow().clone();
            if url.is_empty() || title.starts_with("SEARCH:") {
                return;
            }
            let _ = std::fs::remove_file(playlist_cache_path(&url));
            let new_gen = gen_ref.fetch_add(1, Ordering::Relaxed) + 1;
            let auth = auth_ref.borrow().clone();
            let tx_w = tx_ref.clone();
            let gen_arc = gen_ref.clone();
            std::thread::spawn(move || {
                let auth_opt = if auth.is_empty() { None } else { Some(auth) };
                stream_ytm_tracks(auth_opt, title, url, false, 50, new_gen, gen_arc, tx_w);
            });
        });
    }

    let refresh_home_ui: Rc<dyn Fn()> = {
        let h_box = home_content_box.clone();
        let stats_h = stats.clone();
        let all_pl_h = all_playlists.clone();
        let open_pl_h = open_playlist_or_album.clone();
        let player_h = player.clone();
        let np_t_h = now_playing_lbl.clone();
        let np_a_h = now_artist_lbl.clone();
        let pp_h = play_pause_btn.clone();
        let refresh_q_h = refresh_queue_ui.clone();

        Rc::new(move || {
            render_home_view(&h_box, &stats_h, &all_pl_h.borrow(), &open_pl_h, &player_h, &np_t_h, &np_a_h, &pp_h, &refresh_q_h);
        })
    };

    let render_virtual_window: Rc<dyn Fn(usize)>;
    {
        const ROW_HEIGHT_PX: i32 = 60;
        const WINDOW_SIZE: usize = 35;

        let pending_pop = pending_ui_tracks.clone();
        let cur_tracks_pop = current_playlist_tracks.clone();
        let tr_box_pop = track_list_box.clone();
        let tr_lbl_pop = active_pl_title_lbl.clone();
        let view_title_pop = current_view_title.clone();
        let stream_done_pop = is_stream_finished.clone();
        let gen_pop = active_stream_gen.clone();
        let player_pop = player.clone();
        let stats_pop = stats.clone();
        let np_t_pop = now_playing_lbl.clone();
        let np_a_pop = now_artist_lbl.clone();
        let pp_pop = play_pause_btn.clone();
        let refresh_q_pop = refresh_queue_ui.clone();
        let open_pl_pop = open_playlist_or_album.clone();
        let q_btn_pop = queue_view_btn.clone();
        let filter_pop = track_filter_query.clone();

        let win_start = Rc::new(Cell::new(0usize));
        let win_end = Rc::new(Cell::new(0usize));
        let last_gen_seen = Rc::new(Cell::new(0u64));
        let scroll_cascade_queue: Rc<RefCell<VecDeque<(usize, TrackItem)>>> = Rc::new(RefCell::new(VecDeque::new()));
        let bottom_spacer_ref: Rc<RefCell<Option<Box>>> = Rc::new(RefCell::new(None));
        let total_filtered_count = Rc::new(Cell::new(0usize));

        render_virtual_window = {
            let tr_box_v = track_list_box.clone();
            let cur_tracks_v = current_playlist_tracks.clone();
            let filter_v = track_filter_query.clone();
            let ws_v = win_start.clone();
            let we_v = win_end.clone();
            let cascade_q_v = scroll_cascade_queue.clone();
            let b_spacer_v = bottom_spacer_ref.clone();
            let total_cnt_v = total_filtered_count.clone();

            Rc::new(move |requested_start: usize| {
                let all_snapshot = cur_tracks_v.borrow();
                let fq = filter_v.borrow().clone();
                let filtered: Vec<(usize, TrackItem)> = all_snapshot
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| fq.is_empty() || t.title.to_lowercase().contains(&fq) || t.uploader.to_lowercase().contains(&fq))
                    .map(|(i, t)| (i, t.clone()))
                    .collect();

                let total = filtered.len();
                total_cnt_v.set(total);
                cascade_q_v.borrow_mut().clear();
                while let Some(child) = tr_box_v.first_child() {
                    tr_box_v.remove(&child);
                }
                if total == 0 {
                    *b_spacer_v.borrow_mut() = None;
                    return;
                }

                let start = requested_start.min(total.saturating_sub(WINDOW_SIZE));
                let end = (start + WINDOW_SIZE).min(total);
                ws_v.set(start);
                we_v.set(start);

                if start > 0 {
                    tr_box_v.append(&Box::builder().height_request((start as i32) * ROW_HEIGHT_PX).build());
                }
                let bottom_spacer = Box::builder().height_request(((total - start) as i32) * ROW_HEIGHT_PX).build();
                tr_box_v.append(&bottom_spacer);
                *b_spacer_v.borrow_mut() = Some(bottom_spacer);

                let mut q = cascade_q_v.borrow_mut();
                for item in filtered.into_iter().skip(start).take(end - start) {
                    q.push_back(item);
                }
            })
        };

        let ws_timer = win_start.clone();
        let we_timer = win_end.clone();
        let last_gen_timer = last_gen_seen.clone();
        let cascade_q_timer = scroll_cascade_queue.clone();
        let b_spacer_timer = bottom_spacer_ref.clone();
        let total_cnt_timer = total_filtered_count.clone();

        glib::timeout_add_local(Duration::from_millis(10), move || {
            let next_scroll_card = cascade_q_timer.borrow_mut().pop_front();
            if let Some((orig_idx, track)) = next_scroll_card {
                let is_search = view_title_pop.borrow().starts_with("SEARCH:");
                let cur_gen = gen_pop.load(Ordering::Relaxed);
                if let Some(ref spacer) = *b_spacer_timer.borrow() {
                    tr_box_pop.remove(spacer);
                }
                append_single_track_row(
                    &tr_box_pop,
                    orig_idx,
                    cur_gen,
                    track,
                    is_search,
                    &cur_tracks_pop,
                    &player_pop,
                    &stats_pop,
                    &np_t_pop,
                    &np_a_pop,
                    &pp_pop,
                    &refresh_q_pop,
                    &open_pl_pop,
                );
                let new_end = we_timer.get() + 1;
                we_timer.set(new_end);
                let remaining_below = total_cnt_timer.get().saturating_sub(new_end);
                if remaining_below > 0 {
                    if let Some(ref spacer) = *b_spacer_timer.borrow() {
                        spacer.set_height_request((remaining_below as i32) * ROW_HEIGHT_PX);
                        tr_box_pop.append(spacer);
                    }
                }
            }

            let mut batch_loop = 0;
            let mut added_beyond_window = false;
            loop {
                let Some((item_gen, track, is_search)) = pending_pop.borrow_mut().pop_front() else {
                    break;
                };
                let current_gen = gen_pop.load(Ordering::Relaxed);
                if item_gen != current_gen {
                    continue;
                }
                if last_gen_timer.get() != current_gen {
                    last_gen_timer.set(current_gen);
                    ws_timer.set(0);
                    we_timer.set(0);
                    cascade_q_timer.borrow_mut().clear();
                    *b_spacer_timer.borrow_mut() = None;
                }
                let idx = {
                    let mut list = cur_tracks_pop.borrow_mut();
                    let idx = list.len();
                    list.push(track.clone());
                    idx
                };
                let fq = filter_pop.borrow().clone();
                if idx < WINDOW_SIZE && ws_timer.get() == 0 && fq.is_empty() {
                    we_timer.set(idx + 1);
                    append_single_track_row(
                        &tr_box_pop,
                        idx,
                        item_gen,
                        track.clone(),
                        is_search,
                        &cur_tracks_pop,
                        &player_pop,
                        &stats_pop,
                        &np_t_pop,
                        &np_a_pop,
                        &pp_pop,
                        &refresh_q_pop,
                        &open_pl_pop,
                    );
                } else {
                    added_beyond_window = true;
                }

                let new_q_len = {
                    let mut p = player_pop.borrow_mut();
                    let before_len = p.queue.len();
                    p.on_background_track_arrived(item_gen, track);
                    if p.queue.len() != before_len { Some(p.queue.len()) } else { None }
                };
                if let Some(q_len) = new_q_len {
                    q_btn_pop.set_label(&format!("Queue ({})", q_len));
                }

                let total_loaded = idx + 1;
                let base_title = view_title_pop.borrow().clone();
                if stream_done_pop.get() && pending_pop.borrow().is_empty() {
                    tr_lbl_pop.set_label(&format!("{} ({})", base_title, total_loaded));
                } else {
                    tr_lbl_pop.set_label(&format!("{} ({}...)", base_title, total_loaded));
                }

                if idx < WINDOW_SIZE {
                    break;
                }
                batch_loop += 1;
                if batch_loop >= 35 {
                    break;
                }
            }

            if added_beyond_window && pending_pop.borrow().is_empty() {
                let total = cur_tracks_pop.borrow().len();
                total_cnt_timer.set(total);
                let remaining = total.saturating_sub(we_timer.get());
                if remaining > 0 && b_spacer_timer.borrow().is_none() {
                    let spacer = Box::builder().height_request((remaining as i32) * ROW_HEIGHT_PX).build();
                    tr_box_pop.append(&spacer);
                    *b_spacer_timer.borrow_mut() = Some(spacer);
                }
            }
            glib::ControlFlow::Continue
        });

        let vadj = tr_scroll.vadjustment();
        let ws_scroll = win_start.clone();
        let render_win_scroll = render_virtual_window.clone();
        let scroll_seq = Rc::new(Cell::new(0u32));

        vadj.connect_value_changed(move |adj| {
            let scroll_y = adj.value().max(0.0) as usize;
            let desired_start = (scroll_y / (ROW_HEIGHT_PX as usize)).saturating_sub(6);
            let current_start = ws_scroll.get();
            if desired_start.abs_diff(current_start) >= 8 || (desired_start == 0 && current_start != 0) {
                let seq = scroll_seq.get().wrapping_add(1);
                scroll_seq.set(seq);
                let seq_check = scroll_seq.clone();
                let render_cb = render_win_scroll.clone();
                glib::timeout_add_local_once(Duration::from_millis(55), move || {
                    if seq_check.get() == seq {
                        render_cb(desired_start);
                    }
                });
            }
        });
    }

    {
        let nav_h = nav_stack.clone();
        let home_btn = home_nav_btn.clone();
        let pl_btn = playlists_nav_btn.clone();
        let search_h = search_entry.clone();
        let refresh_h = refresh_home_ui.clone();
        home_nav_btn.connect_clicked(move |_| {
            home_btn.add_css_class("active");
            pl_btn.remove_css_class("active");
            search_h.set_text("");
            search_h.set_placeholder_text(Some("Search YouTube Music..."));
            refresh_h();
            nav_h.set_visible_child_name("home");
        });
    }
    {
        let nav_p = nav_stack.clone();
        let home_btn = home_nav_btn.clone();
        let pl_btn = playlists_nav_btn.clone();
        let search_p = search_entry.clone();
        let pl_box_p = playlists_box.clone();
        let all_pl_p = all_playlists.clone();
        let open_pl_p = open_playlist_or_album.clone();
        playlists_nav_btn.connect_clicked(move |_| {
            pl_btn.add_css_class("active");
            home_btn.remove_css_class("active");
            search_p.set_text("");
            search_p.set_placeholder_text(Some("Filter your playlists..."));
            render_playlists_list(&pl_box_p, &all_pl_p.borrow(), "", &open_pl_p);
            nav_p.set_visible_child_name("playlists");
        });
    }

    {
        let nav_chg = nav_stack.clone();
        let pl_box_chg = playlists_box.clone();
        let all_pl_chg = all_playlists.clone();
        let open_pl_chg = open_playlist_or_album.clone();
        let view_title_chg = current_view_title.clone();
        let filter_chg = track_filter_query.clone();
        let render_win_chg = render_virtual_window.clone();

        search_entry.connect_search_changed(move |entry| {
            let q = entry.text().trim().to_lowercase();
            let page = nav_chg.visible_child_name().map(|s| s.to_string()).unwrap_or_else(|| "home".to_string());
            if page == "playlists" {
                render_playlists_list(&pl_box_chg, &all_pl_chg.borrow(), &q, &open_pl_chg);
            } else if page == "tracks" && !view_title_chg.borrow().starts_with("SEARCH:") {
                *filter_chg.borrow_mut() = q;
                render_win_chg(0);
            }
        });
    }

    {
        let tx_search = tx.clone();
        let auth_search = active_auth_path.clone();
        let nav_search = nav_stack.clone();
        let view_title_s = current_view_title.clone();
        let view_url_s = current_view_url.clone();
        let filter_s = track_filter_query.clone();
        let limit_cell_search = search_limit.clone();
        let last_q_search = last_search_query.clone();
        let gen_search = active_stream_gen.clone();

        search_entry.connect_activate(move |entry| {
            let q = entry.text().trim().to_string();
            if q.is_empty() {
                return;
            }
            let page = nav_search.visible_child_name().map(|s| s.to_string()).unwrap_or_else(|| "home".to_string());
            if page == "home" || (page == "tracks" && view_title_s.borrow().starts_with("SEARCH:")) {
                *filter_s.borrow_mut() = String::new();
                *view_url_s.borrow_mut() = String::new();
                *last_q_search.borrow_mut() = q.clone();
                let max_n = limit_cell_search.get();
                nav_search.set_visible_child_name("tracks");

                let new_gen = gen_search.fetch_add(1, Ordering::Relaxed) + 1;
                let tx_w = tx_search.clone();
                let auth = auth_search.borrow().clone();
                let gen_arc = gen_search.clone();
                let title_str = format!("SEARCH: {}", q.to_uppercase());
                std::thread::spawn(move || {
                    let auth_opt = if auth.is_empty() { None } else { Some(auth) };
                    stream_ytm_tracks(auth_opt, title_str, q, true, max_n, new_gen, gen_arc, tx_w);
                });
            }
        });
    }

    let nav_q_btn = nav_stack.clone();
    let prev_nav_q = prev_nav_page.clone();
    let refresh_on_open_q = refresh_queue_ui.clone();
    queue_view_btn.connect_clicked(move |_| {
        let cur = nav_q_btn.visible_child_name().map(|s| s.to_string()).unwrap_or_else(|| "home".to_string());
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
            p.queue_source = QueueSource::Manual;
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
    let auth_limit = active_auth_path.clone();
    let gen_limit = active_stream_gen.clone();

    let commit_limit = Rc::new(move || {
        if let Ok(parsed) = entry_save.text().trim().parse::<u32>() {
            let clamped = parsed.clamp(1, 500);
            limit_cell_save.set(clamped);
            let q = last_q_save.borrow().clone();
            if !q.is_empty() {
                let new_gen = gen_limit.fetch_add(1, Ordering::Relaxed) + 1;
                let tx_w = tx_limit.clone();
                let auth = auth_limit.borrow().clone();
                let gen_arc = gen_limit.clone();
                let title_str = format!("SEARCH: {}", q.to_uppercase());
                std::thread::spawn(move || {
                    let auth_opt = if auth.is_empty() { None } else { Some(auth) };
                    stream_ytm_tracks(auth_opt, title_str, q, true, clamped, new_gen, gen_arc, tx_w);
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
        Rc::new(move || {
            v_stack.set_visible_child_name("music");
            pl_lbl.set_label("LOADING PLAYLISTS...");
            let tx_w = tx_pl.clone();
            std::thread::spawn(move || {
                let res = fetch_user_playlists();
                let _ = tx_w.send(WorkerMsg::PlaylistsLoaded(res));
            });
        })
    };

    refresh_home_ui();
    nav_stack.set_visible_child_name("home");
    load_playlists_action();

    let retry_pl = load_playlists_action.clone();
    err_refresh_btn.connect_clicked(move |_| retry_pl());

    let reload_pl = load_playlists_action.clone();
    reload_pl_btn.connect_clicked(move |_| reload_pl());

    let nav_back = nav_stack.clone();
    let search_back = search_entry.clone();
    let filter_back = track_filter_query.clone();
    let view_title_back = current_view_title.clone();
    let home_btn_back = home_nav_btn.clone();
    let pl_btn_back = playlists_nav_btn.clone();
    let refresh_h_back = refresh_home_ui.clone();
    back_to_pl_btn.connect_clicked(move |_| {
        *filter_back.borrow_mut() = String::new();
        search_back.set_text("");
        if view_title_back.borrow().starts_with("SEARCH:") {
            home_btn_back.add_css_class("active");
            pl_btn_back.remove_css_class("active");
            search_back.set_placeholder_text(Some("Search YouTube Music..."));
            refresh_h_back();
            nav_back.set_visible_child_name("home");
        } else {
            pl_btn_back.add_css_class("active");
            home_btn_back.remove_css_class("active");
            search_back.set_placeholder_text(Some("Filter your playlists..."));
            nav_back.set_visible_child_name("playlists");
        }
    });

    let tracks_for_shuffle = current_playlist_tracks.clone();
    let player_shuffle = player.clone();
    let stats_shuffle = stats.clone();
    let gen_shuffle = active_stream_gen.clone();
    let np_t_shuf = now_playing_lbl.clone();
    let np_a_shuf = now_artist_lbl.clone();
    let pp_shuf = play_pause_btn.clone();
    let refresh_q_shuf = refresh_queue_ui.clone();
    shuffle_btn.connect_clicked(move |_| {
        let mut list: Vec<TrackItem> = tracks_for_shuffle
            .borrow()
            .iter()
            .filter(|t| t.kind == MediaKind::Song)
            .cloned()
            .collect();
        if list.is_empty() {
            return;
        }
        shuffle_tracks(&mut list);
        let first = list[0].clone();
        stats_shuffle.borrow_mut().record_song(first.clone());
        np_t_shuf.set_label(&first.title);
        np_a_shuf.set_label(&first.uploader);
        pp_shuf.set_label("Pause");

        let cur_gen = gen_shuffle.load(Ordering::Relaxed);
        {
            let mut p = player_shuffle.borrow_mut();
            p.queue = list;
            p.queue_idx = 0;
            p.queue_source = QueueSource::PlaylistShuffled(cur_gen);
            p.play_track(first);
        }
        refresh_q_shuf();
    });

    let player_pp = player.clone();
    let pp_btn_clone = play_pause_btn.clone();
    play_pause_btn.connect_clicked(move |_| {
        let paused = player_pp.borrow_mut().toggle_pause();
        pp_btn_clone.set_label(if paused { "Resume" } else { "Pause" });
    });

    let player_next = player.clone();
    let np_title_next = now_playing_lbl.clone();
    let np_artist_next = now_artist_lbl.clone();
    let pp_next = play_pause_btn.clone();
    let refresh_q_next = refresh_queue_ui.clone();
    next_btn.connect_clicked(move |_| {
        let next_opt = player_next.borrow_mut().play_next();
        if let Some(track) = next_opt {
            np_title_next.set_label(&track.title);
            np_artist_next.set_label(&track.uploader);
            pp_next.set_label("Pause");
            refresh_q_next();
        }
    });

    let player_prev = player.clone();
    let np_title_prev = now_playing_lbl.clone();
    let np_artist_prev = now_artist_lbl.clone();
    let pp_prev = play_pause_btn.clone();
    let refresh_q_prev = refresh_queue_ui.clone();
    prev_btn.connect_clicked(move |_| {
        let prev_opt = player_prev.borrow_mut().play_prev();
        if let Some(track) = prev_opt {
            np_title_prev.set_label(&track.title);
            np_artist_prev.set_label(&track.uploader);
            pp_prev.set_label("Pause");
            refresh_q_prev();
        }
    });

    let v_stack_poll = view_stack.clone();
    let n_stack_poll = nav_stack.clone();
    let pl_box_poll = playlists_box.clone();
    let pl_lbl_poll = pl_header_lbl.clone();
    let tr_box_poll = track_list_box.clone();
    let tr_lbl_poll = active_pl_title_lbl.clone();
    let err_detail_poll = err_detail_lbl.clone();
    let auth_poll = active_auth_path.clone();
    let all_pl_poll = all_playlists.clone();
    let cur_tracks_poll = current_playlist_tracks.clone();
    let pending_poll = pending_ui_tracks.clone();
    let view_title_poll = current_view_title.clone();
    let stream_done_poll = is_stream_finished.clone();
    let gen_poll = active_stream_gen.clone();
    let player_poll = player.clone();
    let np_title_poll = now_playing_lbl.clone();
    let np_artist_poll = now_artist_lbl.clone();
    let pp_poll = play_pause_btn.clone();
    let seek_poll = seek_scale.clone();
    let time_lbl_poll = time_lbl.clone();
    let last_seek_poll = last_user_seek.clone();
    let open_pl_poll = open_playlist_or_album.clone();
    let refresh_h_poll = refresh_home_ui.clone();
    let refresh_q_poll = refresh_queue_ui.clone();

    glib::timeout_add_local(Duration::from_millis(150), move || {
        {
            let autoplay_res = player_poll.borrow_mut().check_autoplay_next();
            match autoplay_res {
                Ok(Some(next_track)) => {
                    np_title_poll.set_label(&next_track.title);
                    np_artist_poll.set_label(&next_track.uploader);
                    pp_poll.set_label("Pause");
                    seek_poll.set_value(0.0);
                    refresh_q_poll();
                }
                Ok(None) => {}
                Err(err_msg) => {
                    np_artist_poll.set_label(&err_msg);
                    pp_poll.set_label("Play / Pause");
                }
            }
        }

        if last_seek_poll.get().elapsed() >= Duration::from_millis(500) {
            let prog = player_poll.borrow_mut().query_progress();
            if let Some((pos, dur)) = prog {
                seek_poll.set_range(0.0, dur.max(1.0));
                seek_poll.set_value(pos);
                time_lbl_poll.set_label(&format!("{} / {}", format_mm_ss(pos), format_mm_ss(dur)));
            }
        }

        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::PlaylistsLoaded(Ok((auth_path, playlists))) => {
                    *auth_poll.borrow_mut() = auth_path;
                    *all_pl_poll.borrow_mut() = playlists.clone();
                    v_stack_poll.set_visible_child_name("music");
                    pl_lbl_poll.set_label(&format!("YOUR PLAYLISTS ({})", playlists.len()));
                    pl_lbl_poll.set_tooltip_text(None);
                    render_playlists_list(&pl_box_poll, &playlists, "", &open_pl_poll);
                    refresh_h_poll();
                }
                WorkerMsg::PlaylistsLoaded(Err(e)) => {
                    pl_lbl_poll.set_label("PLAYLISTS (AUTH FAILED - SEARCH STILL WORKS)");
                    pl_lbl_poll.set_tooltip_text(Some(&e));
                    err_detail_poll.set_label(&e);
                    v_stack_poll.set_visible_child_name("music");
                }
                WorkerMsg::TrackStreamStart { gen_id, title } => {
                    if gen_id == gen_poll.load(Ordering::Relaxed) {
                        while let Some(child) = tr_box_poll.first_child() {
                            tr_box_poll.remove(&child);
                        }
                        cur_tracks_poll.borrow_mut().clear();
                        pending_poll.borrow_mut().clear();
                        *view_title_poll.borrow_mut() = title.clone();
                        stream_done_poll.set(false);
                        v_stack_poll.set_visible_child_name("music");
                        n_stack_poll.set_visible_child_name("tracks");
                        tr_lbl_poll.set_label(&format!("LOADING {}...", title));
                    }
                }
                WorkerMsg::TrackStreamItem { gen_id, track } => {
                    if gen_id == gen_poll.load(Ordering::Relaxed) {
                        let is_search = view_title_poll.borrow().starts_with("SEARCH:");
                        pending_poll.borrow_mut().push_back((gen_id, track, is_search));
                    }
                }
                WorkerMsg::TrackStreamDone { gen_id } => {
                    if gen_id == gen_poll.load(Ordering::Relaxed) {
                        stream_done_poll.set(true);
                        if pending_poll.borrow().is_empty() {
                            let count = cur_tracks_poll.borrow().len();
                            let title = view_title_poll.borrow().clone();
                            tr_lbl_poll.set_label(&format!("{} ({})", title, count));
                        }
                    }
                }
                WorkerMsg::TrackStreamError { gen_id, error } => {
                    if gen_id == gen_poll.load(Ordering::Relaxed) {
                        tr_lbl_poll.set_label(&format!("ERROR: {}", error));
                    }
                }
                WorkerMsg::StreamResolved { play_gen, track, result } => {
                    let mut p = player_poll.borrow_mut();
                    if play_gen == p.play_gen.load(Ordering::Relaxed) {
                        p.is_resolving = false;
                        match result {
                            Ok(direct_url) => {
                                log_msg(&format!("StreamResolved OK for '{}' (id={})", track.title, track.id));
                                p.send_mpv_load(&track, &direct_url);
                            }
                            Err(err) => {
                                log_msg(&format!("StreamResolved ERR for '{}' (id={}): {}", track.title, track.id, err));
                                p.reported_load_failure = true;
                                np_artist_poll.set_label(&format!("Unavailable: {}", err));
                                pp_poll.set_label("Play / Pause");
                            }
                        }
                    }
                }
            }
        }

        while let Ok(event) = mpris_rx.try_recv() {
            match event {
                MediaControlEvent::Toggle | MediaControlEvent::Pause | MediaControlEvent::Play => {
                    let paused = player_poll.borrow_mut().toggle_pause();
                    pp_poll.set_label(if paused { "Resume" } else { "Pause" });
                }
                MediaControlEvent::Next => {
                    let next_opt = player_poll.borrow_mut().play_next();
                    if let Some(track) = next_opt {
                        np_title_poll.set_label(&track.title);
                        np_artist_poll.set_label(&track.uploader);
                        pp_poll.set_label("Pause");
                        refresh_q_poll();
                    }
                }
                MediaControlEvent::Previous => {
                    let prev_opt = player_poll.borrow_mut().play_prev();
                    if let Some(track) = prev_opt {
                        np_title_poll.set_label(&track.title);
                        np_artist_poll.set_label(&track.uploader);
                        pp_poll.set_label("Pause");
                        refresh_q_poll();
                    }
                }
                MediaControlEvent::SetPosition(MediaPosition(pos)) => {
                    player_poll.borrow_mut().seek_to(pos.as_secs_f64());
                }
                MediaControlEvent::SeekBy(direction, delta) => {
                    let prog = player_poll.borrow_mut().query_progress();
                    if let Some((cur_pos, _)) = prog {
                        let offset = match direction {
                            souvlaki::SeekDirection::Forward => delta.as_secs_f64(),
                            souvlaki::SeekDirection::Backward => -delta.as_secs_f64(),
                        };
                        player_poll.borrow_mut().seek_to((cur_pos + offset).max(0.0));
                    }
                }
                _ => {}
            }
        }

        glib::ControlFlow::Continue
    });

    root
}

#[allow(clippy::too_many_arguments)]
fn render_home_view(
    container: &Box,
    stats_rc: &Rc<RefCell<MusicStatsData>>,
    all_playlists: &[PlaylistItem],
    open_playlist_cb: &Rc<dyn Fn(PlaylistItem)>,
    player: &Rc<RefCell<PlayerState>>,
    np_title: &Label,
    np_artist: &Label,
    pp_btn: &Button,
    refresh_queue_ui: &Rc<dyn Fn()>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let (top_pls, top_songs) = {
        let st = stats_rc.borrow();
        (st.top_playlists(5), st.top_songs(12))
    };

    let pl_hdr = Label::builder()
        .label("COMMONLY PLAYED PLAYLISTS")
        .halign(Align::Start)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    container.append(&pl_hdr);

    let pls_to_show: Vec<PlaylistItem> = if !top_pls.is_empty() {
        top_pls.into_iter().map(|(_, p)| p).collect()
    } else {
        all_playlists.iter().take(3).cloned().collect()
    };

    if pls_to_show.is_empty() {
        let loading_lbl = Label::builder()
            .label("Loading your playlists...")
            .halign(Align::Start)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        container.append(&loading_lbl);
    } else {
        for pl in pls_to_show {
            let card_class = if pl.is_album { "music-item-album" } else { "music-item-playlist" };
            let btn = Button::builder().css_classes(vec![card_class.to_string()]).hexpand(true).build();
            let row = Box::builder().orientation(Orientation::Horizontal).spacing(8).build();
            let art = build_album_art_widget(&pl.url, &pl.thumb_url);
            let title_lbl = Label::builder()
                .label(&pl.title)
                .halign(Align::Start)
                .hexpand(true)
                .width_chars(1)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            let arrow = Label::builder().label("->").css_classes(vec!["fav-score-badge".to_string()]).build();
            row.append(&art);
            row.append(&title_lbl);
            row.append(&arrow);
            btn.set_child(Some(&row));

            let cb = open_playlist_cb.clone();
            let pl_clone = pl.clone();
            btn.connect_clicked(move |_| cb(pl_clone.clone()));
            container.append(&btn);
        }
    }

    let songs_hdr = Label::builder()
        .label("COMMONLY PLAYED SONGS")
        .halign(Align::Start)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .margin_top(6)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    container.append(&songs_hdr);

    if top_songs.is_empty() {
        let empty_s = Label::builder()
            .label("Play songs or search above\nto build your top tracks!")
            .halign(Align::Start)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        container.append(&empty_s);
    } else {
        let dummy_tracks: Rc<RefCell<Vec<TrackItem>>> = Rc::new(RefCell::new(top_songs.iter().map(|(_, t)| t.clone()).collect()));
        for (idx, (_, track)) in top_songs.into_iter().enumerate() {
            append_single_track_row(
                container,
                idx,
                0,
                track,
                true,
                &dummy_tracks,
                player,
                stats_rc,
                np_title,
                np_artist,
                pp_btn,
                refresh_queue_ui,
                open_playlist_cb,
            );
        }
    }
}

fn render_playlists_list(
    container: &Box,
    playlists: &[PlaylistItem],
    filter_query: &str,
    open_playlist_cb: &Rc<dyn Fn(PlaylistItem)>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    for pl in playlists {
        if !filter_query.is_empty() && !pl.title.to_lowercase().contains(filter_query) {
            continue;
        }
        let card_class = if pl.is_album { "music-item-album" } else { "music-item-playlist" };
        let btn = Button::builder().css_classes(vec![card_class.to_string()]).hexpand(true).build();
        let row = Box::builder().orientation(Orientation::Horizontal).spacing(8).build();
        let art = build_album_art_widget(&pl.url, &pl.thumb_url);
        let title_lbl = Label::builder()
            .label(&pl.title)
            .halign(Align::Start)
            .hexpand(true)
            .width_chars(1)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        let arrow = Label::builder().label("->").css_classes(vec!["fav-score-badge".to_string()]).build();
        row.append(&art);
        row.append(&title_lbl);
        row.append(&arrow);
        btn.set_child(Some(&row));

        let cb = open_playlist_cb.clone();
        let pl_clone = pl.clone();
        btn.connect_clicked(move |_| cb(pl_clone.clone()));
        container.append(&btn);
    }
}

#[allow(clippy::too_many_arguments)]
fn append_single_track_row(
    container: &Box,
    idx: usize,
    stream_gen: u64,
    track: TrackItem,
    is_search_result: bool,
    current_playlist_tracks: &Rc<RefCell<Vec<TrackItem>>>,
    player: &Rc<RefCell<PlayerState>>,
    stats_rc: &Rc<RefCell<MusicStatsData>>,
    np_title: &Label,
    np_artist: &Label,
    pp_btn: &Button,
    refresh_queue_ui: &Rc<dyn Fn()>,
    open_playlist_cb: &Rc<dyn Fn(PlaylistItem)>,
) {
    let row_box = Box::builder().orientation(Orientation::Horizontal).build();
    let card_class = match track.kind {
        MediaKind::Song => "fav-item-btn",
        MediaKind::Album => "music-item-album",
        MediaKind::Playlist => "music-item-playlist",
    };

    let mut btn_classes = vec![card_class.to_string()];
    let is_playing = if let Some(ref cur) = player.borrow().current_track {
        cur.id == track.id
    } else {
        false
    };
    if is_playing {
        btn_classes.push("pinned".to_string());
    }

    let btn = Button::builder()
        .css_classes(btn_classes)
        .hexpand(true)
        .tooltip_text(match track.kind {
            MediaKind::Song => "Left-click to play | Right-click for Play Next / Add to Queue",
            MediaKind::Album => "Left-click to open Album",
            MediaKind::Playlist => "Left-click to open Playlist",
        })
        .build();

    let outer_row = Box::builder().orientation(Orientation::Horizontal).spacing(8).build();
    let thumb = if !track.thumb_url.is_empty() {
        track.thumb_url.clone()
    } else if track.id.len() == 11 {
        format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", track.id)
    } else {
        String::new()
    };
    let art = build_album_art_widget(&track.id, &thumb);
    outer_row.append(&art);

    let col = Box::builder().orientation(Orientation::Vertical).spacing(2).hexpand(true).build();
    let top_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let title_lbl = Label::builder()
        .label(&track.title)
        .halign(Align::Start)
        .hexpand(true)
        .width_chars(1)
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
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    col.append(&top_row);
    col.append(&artist_lbl);
    outer_row.append(&col);
    btn.set_child(Some(&outer_row));

    let track_clone = track.clone();
    let tracks_rc_click = current_playlist_tracks.clone();
    let player_click = player.clone();
    let stats_click = stats_rc.clone();
    let title_ui = np_title.clone();
    let artist_ui = np_artist.clone();
    let pp_ui = pp_btn.clone();
    let refresh_q_click = refresh_queue_ui.clone();
    let open_pl_click = open_playlist_cb.clone();

    btn.connect_clicked(move |_| {
        if track_clone.kind != MediaKind::Song && !track_clone.list_url.is_empty() {
            open_pl_click(PlaylistItem {
                title: track_clone.title.clone(),
                url: track_clone.list_url.clone(),
                thumb_url: track_clone.thumb_url.clone(),
                is_album: track_clone.kind == MediaKind::Album,
            });
            return;
        }

        stats_click.borrow_mut().record_song(track_clone.clone());
        title_ui.set_label(&track_clone.title);
        artist_ui.set_label(&track_clone.uploader);
        pp_ui.set_label("Pause");

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
                p.queue = tracks_rc_click.borrow().clone();
                p.queue_idx = idx.min(p.queue.len().saturating_sub(1));
                p.queue_source = QueueSource::PlaylistSequential(stream_gen);
            }
            p.play_track(track_clone.clone());
        }
        refresh_q_click();
    });

    if track.kind == MediaKind::Song {
        let right_click = GestureClick::builder().button(3).build();
        let row_weak = row_box.downgrade();
        let track_ctx = track.clone();
        let player_ctx = player.clone();
        let title_ctx = np_title.clone();
        let artist_ctx = np_artist.clone();
        let pp_ctx = pp_btn.clone();
        let dur_ctx = dur_lbl.clone();
        let orig_dur_ctx = track.duration_str.clone();
        let refresh_q_ctx = refresh_queue_ui.clone();

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

            let menu_box = Box::builder().orientation(Orientation::Vertical).spacing(4).build();
            let play_next_btn = Button::builder().label("Play Next").css_classes(vec!["fav-hide-btn".to_string()]).build();
            let add_queue_btn = Button::builder().label("Add to Queue").css_classes(vec!["fav-hide-btn".to_string()]).build();
            menu_box.append(&play_next_btn);
            menu_box.append(&add_queue_btn);
            popover.set_child(Some(&menu_box));

            let track_pn = track_ctx.clone();
            let player_pn = player_ctx.clone();
            let title_pn = title_ctx.clone();
            let artist_pn = artist_ctx.clone();
            let pp_pn = pp_ctx.clone();
            let dur_pn = dur_ctx.clone();
            let orig_dur_pn = orig_dur_ctx.clone();
            let pop_pn = popover.downgrade();
            let refresh_q_pn = refresh_q_ctx.clone();

            play_next_btn.connect_clicked(move |_| {
                if let Some(p) = pop_pn.upgrade() {
                    p.popdown();
                }
                let started = player_pn.borrow_mut().play_next_in_queue(track_pn.clone());
                if let Some(started_track) = started {
                    title_pn.set_label(&started_track.title);
                    artist_pn.set_label(&started_track.uploader);
                    pp_pn.set_label("Pause");
                }
                refresh_q_pn();
                dur_pn.set_label("Next!");
                let dur_reset = dur_pn.clone();
                let orig = orig_dur_pn.clone();
                glib::timeout_add_local_once(Duration::from_millis(1000), move || dur_reset.set_label(&orig));
            });

            let track_aq = track_ctx.clone();
            let player_aq = player_ctx.clone();
            let title_aq = title_ctx.clone();
            let artist_aq = artist_ctx.clone();
            let pp_aq = pp_ctx.clone();
            let dur_aq = dur_ctx.clone();
            let orig_dur_aq = orig_dur_ctx.clone();
            let pop_aq = popover.downgrade();
            let refresh_q_aq = refresh_q_ctx.clone();

            add_queue_btn.connect_clicked(move |_| {
                if let Some(p) = pop_aq.upgrade() {
                    p.popdown();
                }
                let started = player_aq.borrow_mut().add_to_queue(track_aq.clone());
                if let Some(started_track) = started {
                    title_aq.set_label(&started_track.title);
                    artist_aq.set_label(&started_track.uploader);
                    pp_aq.set_label("Pause");
                }
                refresh_q_aq();
                dur_aq.set_label("Queued!");
                let dur_reset = dur_aq.clone();
                let orig = orig_dur_aq.clone();
                glib::timeout_add_local_once(Duration::from_millis(1000), move || dur_reset.set_label(&orig));
            });

            popover.popup();
        });
        btn.add_controller(right_click);
    }

    row_box.append(&btn);
    container.append(&row_box);
}

#[allow(clippy::too_many_arguments)]
fn render_queue_view(
    container: &Box,
    queue_btn: &Button,
    player: &Rc<RefCell<PlayerState>>,
    np_title: &Label,
    np_artist: &Label,
    pp_btn: &Button,
    refresh_self: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    requested_start: usize,
) {
    const ROW_HEIGHT_PX: i32 = 56;
    const WINDOW_SIZE: usize = 35;

    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let (queue_items, current_idx) = {
        let p = player.borrow();
        (p.queue.clone(), p.queue_idx)
    };

    let total = queue_items.len();
    queue_btn.set_label(&format!("Queue ({})", total));

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

    let start_show = requested_start.min(total.saturating_sub(WINDOW_SIZE));
    let end_show = (start_show + WINDOW_SIZE).min(total);

    if start_show > 0 {
        container.append(&Box::builder().height_request((start_show as i32) * ROW_HEIGHT_PX).build());
    }

    for (idx, track) in queue_items.into_iter().enumerate().skip(start_show).take(end_show - start_show) {
        let is_current = idx == current_idx;
        let row = Box::builder().orientation(Orientation::Horizontal).spacing(4).build();

        let mut btn_classes = vec!["fav-item-btn".to_string()];
        if is_current {
            btn_classes.push("pinned".to_string());
        }
        let btn = Button::builder().css_classes(btn_classes).hexpand(true).build();
        let outer_row = Box::builder().orientation(Orientation::Horizontal).spacing(8).build();

        let thumb = if !track.thumb_url.is_empty() {
            track.thumb_url.clone()
        } else if track.id.len() == 11 {
            format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", track.id)
        } else {
            String::new()
        };
        let art = build_album_art_widget(&track.id, &thumb);
        outer_row.append(&art);

        let col = Box::builder().orientation(Orientation::Vertical).spacing(2).hexpand(true).build();
        let top_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();

        let prefix = if is_current { "[NOW] " } else { "" };
        let title_lbl = Label::builder()
            .label(&format!("{}{}", prefix, track.title))
            .halign(Align::Start)
            .hexpand(true)
            .width_chars(1)
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
            .width_chars(1)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        col.append(&top_row);
        col.append(&artist_lbl);
        outer_row.append(&col);
        btn.set_child(Some(&outer_row));

        let player_jump = player.clone();
        let title_jump = np_title.clone();
        let artist_jump = np_artist.clone();
        let pp_jump = pp_btn.clone();
        let track_jump = track.clone();
        let refresh_jump = refresh_self.clone();
        btn.connect_clicked(move |_| {
            {
                let mut p = player_jump.borrow_mut();
                p.queue_idx = idx;
                p.play_track(track_jump.clone());
            }
            title_jump.set_label(&track_jump.title);
            artist_jump.set_label(&track_jump.uploader);
            pp_jump.set_label("Pause");
            if let Some(ref cb) = *refresh_jump.borrow() {
                cb();
            }
        });

        let del_btn = Button::builder()
            .label("×")
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

    let remaining_below = total.saturating_sub(end_show);
    if remaining_below > 0 {
        container.append(&Box::builder().height_request((remaining_below as i32) * ROW_HEIGHT_PX).build());
    }
}