# lily-dashboard

A slide-out Wayland sidebar for Hyprland written in Rust with GTK4 and `gtk4-layer-shell`.

It sits on the left edge of the active monitor, replaces Waybar (workspaces, system tray, notifications, battery, clock, weather, media), and bundles an app launcher, clipboard manager, system settings panel, music player, notes/plans editor, calendar, timers, cloud uploader, password list, and encrypted file locker into a single process.

## How it works

- **Single instance toggle**: Built on `gtk4::Application` (`com.lily.dashboard`). Launching the binary a second time (for example from a `SUPER` keybind) signals the running instance to slide open or closed instead of starting a new process.
- **Click-through when closed**: The layer-shell window reserves an 856px wide surface so both the 400px main panel and an optional 400px slide-out detail pane (`Plans` or `System`) can animate smoothly. When closed, `surface.set_input_region()` shrinks the pointer hitbox down to a 10x104px edge trigger so the rest of the screen stays click-through.
- **Tab lifecycle & RAM**: Stateless tabs (`Run`, `Clipboard`, `Passwords`, `Calendar`) are built when opened and destroyed when switching away. Stateful tabs (`Drive`, `Locker`, `Music`, `Timers`, `System`) are built lazily on first visit and kept alive so active uploads, playback, timers, or unlocked vaults are not interrupted. `Favorites` only populates rows while visible.
- **Live theming**: `init_theme()` watches `~/.config/theme-switch/current.conf` via `gio::FileMonitor` and reloads the GTK CSS whenever the theme changes.
- **Hyprland blur**: On startup, `main()` adds `ext_background_effect_manager_v1` to `GDK_WAYLAND_DISABLE`. GTK 4.24 otherwise sends an empty Wayland blur region that overrides Hyprland's `layer_rule` blur, and using GTK CSS `backdrop-filter` causes frame drops during the slide animation. Disabling that protocol lets Hyprland's GPU Dual-Kawase blur handle the surfaces directly.

- **Low SSD wear**: Normal clipboard history and wallpaper thumbnails live only in RAM; music thumbnails/logs (`/tmp`) and decrypted locker previews (`/dev/shm`) use `tmpfs`; session auto-save defaults to off (and ignores window-title changes when enabled); and `Favorites` batches watcher score writes to at most once every 60 seconds.

## Code layout (`src/`)

| File | What it does |
| --- | --- |
| `main.rs` | Window setup, layer-shell config, slide animation, dynamic input region, live CSS theming, sidebar navigation, footer (session load/save/auto-save, power/restart, weather, Nixie clock, self-rebuild button), and top-bar quick capture/color-picker buttons. |
| `tray.rs` | Top workspace bar (Hyprland socket listener, click to switch, scroll to cycle, hover popover with a proportional window layout mini-map + focus/close actions), D-Bus `StatusNotifierWatcher` system tray, mouse battery reader (`/sys/class/power_supply`), and SwayNC notification badge + right-click DND toggle. |
| `launcher.rs` | `Run` tab (default). Scans `.desktop` entries via `gio::AppInfo`, supports fuzzy filtering, pinning, and optional background binary preloading (`cat <bin> > /dev/null`) to warm page cache. |
| `favorites.rs` | `Favorites` tab. Tracks frequently accessed files/folders via `notify` filesystem watcher, scores usage in RAM (flushing `ignore.json` at most once every 60s), lets you pin or ignore paths, and opens items with `xdg-open`. |
| `clipboard.rs` | `Clipboard` tab. Runs background `wl-paste --watch` listeners for text (up to 200 items) and PNG images (up to 20 items, 15 MB cap). Normal history lives only in RAM; pinned items persist to `clipboard_pins.json`. |
| `plans.rs` | `Plans` tab. Plain-text/markdown checklist and notes editor backed by `~/Documents/plans` (or `~/documents/plans`), with a 400px slide-out detail pane, checkboxes, drag-and-drop line reordering, and inline editing. |
| `calendar.rs` | `Calendar` tab. Monthly GTK calendar with per-day notes saved in `calendar_events.json`. |
| `timers.rs` | `Timers` tab. Countdown timers (`15m`, `2h`, `45s`) and process watchers that alert via `notify-send` + `paplay` when a PID exits or stays idle at 0% CPU. |
| `music.rs` | `Music` tab. YouTube Music client backed by `yt-dlp` and a local `mpv` IPC socket (`/tmp/lily-ymusic-mpv.sock`), with playlists, search, queue management, seek/volume controls, MPRIS integration via `souvlaki`, and a bounded in-memory thumbnail cache. |
| `media_popout.rs` | Independent bottom-right layer-shell window (`lily-media-popout`) for MPRIS `playerctl` playback. Collapses to a tiny edge sliver when idle, expands to a compact pill when the dashboard is open or the sliver is hovered, and clicks open to a card with cover art and transport controls. |
| `drive.rs` | `Drive` tab. GUI wrapper around `rclone copy` with remote path, optional bandwidth limit, native file/folder chooser, live progress parsing, and cancel support. |
| `passwords.rs` | `Passwords` tab. Simple local credential list stored in `passwords.txt` (`0600` permissions) with search, click-to-copy, and inline add/delete. |
| `locker.rs` | `Locker` tab. Encrypted file vault using Argon2id + HKDF-SHA256 + streaming AES-256-GCM (`1 MB` chunks) packed into `.lilar` block files (up to 10 GB per block). Decrypted previews extract to `/dev/shm/lily-locker` (RAM tmpfs) and are cleaned up on lock. |
| `system.rs` | `System` tab with a 400px slide-out detail pane covering: **Sound** (`pactl` sinks, sources, per-app streams), **Network** (`nmcli` active connections, Wi-Fi scan/connect, VPNs), **Themes** (`theme-switch` palette previews), **Wallpapers** (`swww` multi-monitor picker with vertical crop pairs in `~/Pictures/Wallpapers/vertical/`, RAM-only thumbnails), **Displays** (`hyprctl monitors`), **Keybinds** (live parser for `~/.config/hypr/binds.lua` with search), and **Overview** (CPU/RAM/GPU stats, `lumine-capture` screenshot modes, `slurp` + `grim` color picker, DND, Suspend, Reload Hyprland). |
| `weather.rs` | Compact sidebar weather badge + click-to-open forecast popover backed by Open-Meteo (location search + daily/hourly forecast). |
| `session.rs` | Saves and restores mapped Hyprland windows (workspace, geometry, float/fullscreen state, command line). Controlled by the sidebar's Load / Save / Auto-save buttons (auto-save defaults to off to avoid background SSD writes; when enabled it checks every 30s and ignores window-title changes). |

## Files & state on disk

- `~/.config/lily-dashboard/`
  - `clipboard_pins.json` — pinned clipboard entries only (normal history stays in RAM)
  - `calendar_events.json` — per-day calendar notes
  - `passwords.txt` — owner-only (`0600`) password entries
  - `ignore.json` / `launcher.json` / `weather.json` / `current_session.json` / `previous_session.json` / `session_prefs.json` — module preferences and snapshots
- `~/.config/theme-switch/current.conf` — active color palette (watched live)
- `~/.config/hypr/binds.lua` — parsed live by `System -> Keybinds`
- `~/.config/hypr/.current_wallpaper` — active wallpaper path shared with startup scripts
- `~/Pictures/Wallpapers/` (and optional `vertical/` subfolder) — wallpaper images
- `~/Documents/plans/` (or `~/documents/plans/`) — plan/checklist files

## Building & running

```bash
cargo build --release
./target/release/lily-dashboard
```

The sidebar footer's **Reload Dashboard** button runs `cargo build --release` in the repo directory and restarts the binary cleanly on success (if the build fails, the running dashboard stays up and logs the error to `/tmp/lily-reload.log`).

### Hyprland setup

In `~/.config/hypr/binds.lua` (tap `SUPER` to toggle):

```lua
hl.bind("SUPER + Super_L", hl.dsp.exec_cmd("/path/to/lily-dashboard/target/release/lily-dashboard"), { release = true })
```

In `~/.config/hypr/rules.lua` (frosted glass blur):

```lua
hl.layer_rule({ name = "lily-dashboard-blur", match = { namespace = "^lily-dashboard$" }, blur = true, blur_popups = true, ignore_alpha = 0.25 })
hl.layer_rule({ name = "lily-media-popout-blur", match = { namespace = "^lily-media-popout$" }, blur = true, ignore_alpha = 0.25 })
```

## Runtime tools used

Core UI runs with just GTK4 and `gtk4-layer-shell`. Individual tabs shell out to standard desktop tools where needed:

- **Hyprland / Wayland**: `hyprctl`, `wl-paste`, `wl-copy`, `swww`, `slurp`, `grim`, `lumine-capture`
- **System / Audio / Network**: `pactl`, `nmcli`, `nvidia-smi` (optional, for GPU stats), `swaync-client`, `notify-send`, `paplay`
- **Media / Cloud**: `playerctl`, `mpv`, `yt-dlp`, `rclone`
