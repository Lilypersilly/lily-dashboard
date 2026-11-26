# lily-dashboard

A slide-out Wayland sidebar for Hyprland written in Rust with GTK4 and `gtk4-layer-shell`.

It sits on the left edge of the active monitor, replaces Waybar (workspaces, system tray, notifications, battery, clock, weather, media), and bundles an app launcher, clipboard manager, system settings panel, music player, floating sticky notes, notes/plans editor, calendar, timers, cloud uploader, password list, and encrypted file locker into a single process.

## How it works

- **Single instance toggle**: Built on `gtk4::Application` (`com.lily.dashboard`). Launching the binary a second time (for example from a `SUPER` keybind) signals the running instance to slide open or closed instead of starting a new process.
- **Click-through when closed**: The layer-shell window reserves an 856px wide surface so both the 400px main panel and an optional 400px slide-out detail pane (`Plans` or `System`) can animate smoothly. When closed, `surface.set_input_region()` shrinks the pointer hitbox down to a 10x104px edge trigger so the rest of the screen stays click-through.
- **Tab lifecycle & RAM**: Stateless tabs (`Run`, `Clipboard`, `Passwords`, `Calendar`) are built when opened and destroyed when switching away. Stateful tabs (`Drive`, `Locker`, `Music`, `Timers`, `System`) are built lazily on first visit and kept alive so active uploads, playback, timers, or unlocked vaults are not interrupted. `Favorites` only populates rows while visible.
- **Built-in native theming**: Includes a built-in Rust theme engine (`themes.rs`) with 17+ curated presets (Catppuccin Mocha/Macchiato/Latte, Rosé Pine & Dawn, Tokyo Night, Dracula, Nord, Everforest, Kanagawa, Gruvbox Dark/Light, One Dark, Solarized Dark, OLED Pitch, Marshmellow Dark/Light), unified **Global** vs **Dash-only** application, dynamic KDE/Dolphin color schemes (`LilyDynamic.colors`), passwordless Papirus folder color overlays, and a 6-color Theme Editor with `.json` fine-tuning. Also watches `~/.config/theme-switch/current.conf` via `gio::FileMonitor` for live CSS reloads.
- **Hyprland blur**: On startup, `main()` adds `ext_background_effect_manager_v1` to `GDK_WAYLAND_DISABLE`. GTK 4.24 otherwise sends an empty Wayland blur region that overrides Hyprland's `layer_rule` blur, and using GTK CSS `backdrop-filter` causes frame drops during the slide animation. Disabling that protocol lets Hyprland's GPU Dual-Kawase blur handle the surfaces directly.

- **Bundled default font**: Ships **Josefin Sans** (SIL OFL-1.1 in `assets/fonts/`) embedded into the binary and auto-installed to `~/.local/share/fonts/lily-dashboard/` on first run if not already present, plus a live UI font picker with typeface previews and font scaling (`75%`–`135%`).
- **Low SSD wear**: Normal clipboard history and wallpaper thumbnails live only in RAM; music thumbnails/logs (`/tmp`) and decrypted locker previews (`/dev/shm`) use `tmpfs`; session auto-save defaults to off (and ignores window-title changes when enabled); sticky note edits/drags/strokes debounce writes to 900ms; and `Favorites` batches watcher score writes to at most once every 60 seconds.

## Code layout (`src/`)

| File | What it does |
| --- | --- |
| `config.rs` | Centralized `~/.config/lily-dashboard/config.json` loader/saver with first-run tool auto-detection, bundled Josefin Sans font installer, 6 button styles, modular power-row config, full layout/chrome settings, one-click Theme Sharing JSON export/import, built-in fallback color presets, and Hyprland (Hyprlua + classic) IPC helpers. |
| `main.rs` | Window setup, layer-shell config (full left/right mirroring, configurable trigger position/size/hover-to-open, monitor pinning, panel width, animation speed), dynamic input region, live CSS theming (6 button themes, font family & scale, corner rounding & opacity, hairline scrollbars), dynamic sidebar navigation with custom separators, modular 1–3 button power row (with customizable dropdown menu), chrome toggles, and top-bar quick sticky-note, capture, and color-picker buttons. |
| `tray.rs` | Top workspace bar (Hyprland socket listener, click to switch, scroll to cycle, hover popover with a proportional window layout mini-map + focus/close actions), D-Bus `StatusNotifierWatcher` system tray, mouse battery reader (`/sys/class/power_supply`), and SwayNC notification badge + right-click DND toggle. |
| `launcher.rs` | `Run` tab (default). Scans `.desktop` entries via `gio::AppInfo`, supports fuzzy filtering, pinning, and optional background binary preloading (`cat <bin> > /dev/null`) to warm page cache. |
| `favorites.rs` | `Favorites` tab. Tracks frequently accessed files/folders via `notify` filesystem watcher, scores usage in RAM (flushing `ignore.json` at most once every 60s), lets you pin or ignore paths, and opens items with `xdg-open`. |
| `clipboard.rs` | `Clipboard` tab. Runs background `wl-paste --watch` listeners for text (up to 200 items) and PNG images (up to 20 items, 15 MB cap). Normal history lives only in RAM; pinned items persist to `clipboard_pins.json`. |
| `plans.rs` | `Plans` tab. Plain-text/markdown checklist and notes editor backed by `~/Documents/plans` (or `~/documents/plans`), with a 400px slide-out detail pane, checkboxes, drag-and-drop line reordering, and inline editing. |
| `calendar.rs` | `Calendar` tab. Monthly GTK calendar with per-day notes saved in `calendar_events.json`. |
| `timers.rs` | `Timers` tab. Countdown timers (`15m`, `2h`, `45s`) and process watchers that alert via `notify-send` + `paplay` when a PID exits or stays idle at 0% CPU. |
| `music.rs` | `Music` tab. YouTube Music client backed by `yt-dlp` and a local `mpv` IPC socket (`/tmp/lily-ymusic-mpv.sock`), with playlists, intermixed song/album search (playlists at the bottom), pastel kind-accent tabs, right-click playlist/album queue actions (play next, append, even-spread stratified shuffle), direction-aware incremental scroll infill, seek/volume controls, MPRIS integration via `souvlaki`, and a bounded in-memory thumbnail cache. |
| `stickies.rs` | Floating sticky notes (`lily-sticky` layer-shell windows) that appear only while the dashboard is open. Each note supports typing and freehand Cairo drawing on the same card (5 ink colors, S/M/L width, undo/clear), 5 pastel tints, header dragging and invisible 4-corner resizing via Hyprland's global cursor IPC, and debounced persistence in `stickies.json`. |
| `media_popout.rs` | Independent bottom-right layer-shell window (`lily-media-popout`) for MPRIS `playerctl` playback. Collapses to a tiny edge sliver when idle, expands to a compact pill when the dashboard is open or the sliver is hovered, and clicks open to a card with cover art and transport controls. |
| `drive.rs` | `Drive` tab. GUI wrapper around `rclone copy` with remote path, optional bandwidth limit, native file/folder chooser, live progress parsing, and cancel support. |
| `passwords.rs` | `Passwords` tab. Two-tier credential list: instant local logins in `passwords.txt` (`0600`) plus **Load Locker Passwords**, which decrypts (or auto-creates) an encrypted `passwords.txt` from the unlocked Locker strictly in RAM, auto-saves new/deleted Locker logins back to the vault, and zeroizes RAM on unload, lock, or exit. |
| `locker.rs` | `Locker` tab. Encrypted file vault using Argon2id + HKDF-SHA256 + streaming AES-256-GCM (`1 MB` chunks) packed into `.lilar` block files (up to 10 GB per block). Decrypted previews extract to `/dev/shm/lily-locker` (RAM tmpfs) and are cleaned up on lock; also exposes an in-RAM bridge for `Passwords` so vaulted credentials never touch disk or tmpfs. |
| `system.rs` | `System` tab with a 400px slide-out detail pane covering: **Sound** (`pactl` sinks, sources, per-app streams), **Network** (`nmcli` active connections, Wi-Fi scan/connect, VPNs), **Themes** (unified theme list with per-theme **Global** and **Dash** buttons, live 6-color Theme Editor, and `.json` fine-tuning), **Wallpapers** (`swww`/`awww`/`hyprpaper`/custom multi-monitor picker with vertical crop pairs, RAM-only thumbnails), **Displays** (`hyprctl monitors`), **Keybinds** (live `.lua` Hyprlua & `.conf` classic Hyprland viewer + hotkey editor with key recorder, presets, safe delete, `.bak` backups, and auto-reload), **Overview** (CPU/RAM/GPU stats, configured screenshot modes, color picker, DND, Suspend, Reload Hyprland), and **Settings** (collapsible categories with live self-previewing Button Styles, UI Font picker & scaling, modular 1–3 Power Buttons, Layout & Behaviour, 2-panel drag-and-drop Sidebar Tabs & Separators editor, one-click Theme Sharing JSON, Hyprland live sliders, apps/backends, and paths). |
| `themes.rs` | Native Rust system-wide theme engine replacing external theme scripts. Derives full desktop palettes (surfaces, muted text, contrast text, 16-color terminal ANSI palette, KDE/Dolphin `LilyDynamic.colors` + `kdeglobals`, GTK3/4 CSS & tooltips, Hyprland `marshmellow.lua`, Kitty, Wofi, SwayNC, SwayOSD, `qt6ct`, `btop`) from 6 core colors, builds a lightweight (<1 MB, zero `sudo`) Papirus folder-color overlay in `~/.local/share/icons/Lily-Papirus-{Dark,Light}`, and saves editable user themes in `~/.config/lily-dashboard/themes/*.json`. |
| `weather.rs` | Compact sidebar weather badge + click-to-open forecast popover backed by Open-Meteo (location search + daily/hourly forecast). |
| `session.rs` | Saves and restores mapped Hyprland windows (workspace, geometry, float/fullscreen state, command line). Controlled by the sidebar's Load / Save / Auto-save buttons (auto-save defaults to off to avoid background SSD writes; when enabled it checks every 30s and ignores window-title changes). |

## Files & state on disk

- `~/.config/lily-dashboard/`
  - `config.json` — main user settings (button style, UI font & scale, modular power buttons, layout & chrome toggles, sidebar tab order/separators, Hyprland appearance, tool backends, paths, and theme preset/custom palette)
  - `themes/*.json` — user-created and fine-tuned 6-color themes
  - `clipboard_pins.json` — pinned clipboard entries only (normal history stays in RAM)
  - `calendar_events.json` — per-day calendar notes
  - `stickies.json` — floating sticky notes (position, size, tint, text, and drawing strokes)
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
hl.layer_rule({ name = "lily-sticky-blur", match = { namespace = "^lily-sticky$" }, blur = true, ignore_alpha = 0.25 })
```

## Runtime tools used

Core UI runs with just GTK4 and `gtk4-layer-shell`. Individual tabs shell out to standard desktop tools where needed:

- **Hyprland / Wayland**: `hyprctl`, `wl-paste`, `wl-copy`, `swww`, `slurp`, `grim`, `lumine-capture`
- **System / Audio / Network**: `pactl`, `nmcli`, `nvidia-smi` (optional, for GPU stats), `swaync-client`, `notify-send`, `paplay`
- **Media / Cloud**: `playerctl`, `mpv`, `yt-dlp`, `rclone`
