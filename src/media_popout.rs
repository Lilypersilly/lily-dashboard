//! Standalone bottom-right layer-shell window (`lily-media-popout`) for live media.
//!
//! Polls MPRIS playback via `playerctl`. Collapses to a thin edge sliver when idle,
//! pops up a compact pill when the dashboard opens or the sliver is hovered, and
//! expands on click into a card with cover art and previous/play-pause/next controls.

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box, Button, EventControllerMotion, GestureClick, Label,
    Orientation, Overflow, Picture, Revealer, RevealerTransitionType,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone)]
pub struct MediaPopoutHandle {
    on_dashboard_open: Rc<dyn Fn(bool)>,
}

impl MediaPopoutHandle {
    pub fn set_dashboard_open(&self, open: bool) {
        (self.on_dashboard_open)(open);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MediaInfo {
    status: String, // "Playing", "Paused", or ""
    player: String,
    title: String,
    artist: String,
    art_url: String,
}

impl MediaInfo {
    fn is_active(&self) -> bool {
        (self.status == "Playing" || self.status == "Paused") && !self.title.is_empty()
    }
}

enum MediaMsg {
    State(MediaInfo),
    ArtBytes(String, Vec<u8>),
}

fn query_playerctl() -> MediaInfo {
    let fmt = "{{status}}\t{{playerName}}\t{{title}}\t{{artist}}\t{{mpris:artUrl}}";
    let Ok(out) = Command::new("playerctl")
        .args(["metadata", "--format", fmt])
        .output()
    else {
        return MediaInfo {
            status: String::new(),
            player: String::new(),
            title: String::new(),
            artist: String::new(),
            art_url: String::new(),
        };
    };

    if !out.status.success() {
        return MediaInfo {
            status: String::new(),
            player: String::new(),
            title: String::new(),
            artist: String::new(),
            art_url: String::new(),
        };
    }

    let line = String::from_utf8_lossy(&out.stdout);
    let first = line.lines().next().unwrap_or("");
    let parts: Vec<&str> = first.split('\t').collect();
    MediaInfo {
        status: parts.first().copied().unwrap_or("").trim().to_string(),
        player: parts.get(1).copied().unwrap_or("").trim().to_string(),
        title: parts.get(2).copied().unwrap_or("").trim().to_string(),
        artist: parts.get(3).copied().unwrap_or("").trim().to_string(),
        art_url: parts.get(4).copied().unwrap_or("").trim().to_string(),
    }
}

fn load_art_bytes(art_url: &str) -> Option<Vec<u8>> {
    if art_url.is_empty() {
        return None;
    }
    if let Some(path_str) = art_url.strip_prefix("file://") {
        let decoded = glib::filename_from_uri(art_url)
            .map(|(p, _)| p)
            .unwrap_or_else(|_| std::path::PathBuf::from(path_str));
        return std::fs::read(decoded).ok();
    }
    if art_url.starts_with("http://") || art_url.starts_with("https://") {
        let out = Command::new("curl")
            .args(["-sL", "--max-time", "4", "--max-filesize", "4194304", art_url])
            .output()
            .ok()?;
        if out.status.success() && !out.stdout.is_empty() {
            return Some(out.stdout);
        }
    }
    None
}

fn bytes_to_cover_texture(bytes: &[u8]) -> Option<gdk::Texture> {
    use gtk4::gdk_pixbuf::{InterpType, PixbufLoader};
    let loader = PixbufLoader::new();
    loader.write(bytes).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    let w = pixbuf.width();
    let h = pixbuf.height();
    if w <= 0 || h <= 0 {
        return None;
    }
    let target_w = 220;
    let target_h = 124;
    let scale = (target_w as f64 / w as f64).min(target_h as f64 / h as f64);
    let nw = ((w as f64 * scale).round() as i32).max(1);
    let nh = ((h as f64 * scale).round() as i32).max(1);
    let scaled = pixbuf.scale_simple(nw, nh, InterpType::Bilinear)?;
    Some(gdk::Texture::for_pixbuf(&scaled))
}

fn move_popout_to_focused_monitor(window: &ApplicationWindow) {
    let mut active_name: Option<String> = None;
    if let Ok(output) = Command::new("hyprctl").args(["monitors", "-j"]).output() {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
            if let Some(monitors) = json.as_array() {
                active_name = monitors
                    .iter()
                    .find(|m| m.get("focused").and_then(|f| f.as_bool()).unwrap_or(false))
                    .and_then(|m| m.get("name"))
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string());
            }
        }
    }
    let Some(target) = active_name else { return };
    let Some(display) = gdk::Display::default() else { return };
    let mons = display.monitors();
    for i in 0..mons.n_items() {
        if let Some(mon) = mons.item(i).and_downcast::<gdk::Monitor>() {
            if mon.connector().as_deref() == Some(target.as_str()) {
                window.set_monitor(&mon);
                break;
            }
        }
    }
}

/// Spawns an independent bottom-right corner layer-shell Now Playing popout.
/// - Hidden completely when no media is active.
/// - Slides down to a tiny 6px edge sliver nestled in the Hyprland bottom gap when the dashboard is closed.
/// - Pops up into the Now Playing pill automatically whenever the dashboard opens (or when you hover the sliver).
pub fn init_media_popout(app: &Application) -> MediaPopoutHandle {
    let win = ApplicationWindow::builder()
        .application(app)
        .title("Lily Media Popout")
        .decorated(false)
        .resizable(false)
        .build();

    win.init_layer_shell();
    win.set_namespace("lily-media-popout");
    win.set_layer(Layer::Top);
    win.set_keyboard_mode(KeyboardMode::None);
    win.set_anchor(Edge::Bottom, true);
    win.set_anchor(Edge::Right, true);
    win.set_margin(Edge::Bottom, 2);
    win.set_margin(Edge::Right, 14);

    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .halign(Align::End)
        .valign(Align::End)
        .build();

    // --- EXPANDED NOW PLAYING CARD (slides up above the pill on click) ---
    let card_revealer = Revealer::builder()
        .transition_type(RevealerTransitionType::SlideUp)
        .transition_duration(200)
        .reveal_child(false)
        .build();

    let card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .width_request(260)
        .css_classes(vec!["media-popout-card".to_string()])
        .build();

    let card_hdr = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    let hdr_lbl = Label::builder()
        .label("  Now Playing")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["plan-h2".to_string()])
        .build();
    let player_lbl = Label::builder()
        .label("")
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    let collapse_btn = Button::builder()
        .label("v")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .tooltip_text("Collapse")
        .build();
    card_hdr.append(&hdr_lbl);
    card_hdr.append(&player_lbl);
    card_hdr.append(&collapse_btn);

    let art_frame = Box::builder()
        .halign(Align::Center)
        .valign(Align::Center)
        .width_request(220)
        .height_request(124)
        .overflow(Overflow::Hidden)
        .css_classes(vec!["media-art-box".to_string()])
        .visible(false)
        .build();
    let art_img = Picture::builder()
        .can_shrink(true)
        .width_request(220)
        .height_request(124)
        .hexpand(true)
        .vexpand(true)
        .build();
    art_frame.append(&art_img);

    let card_title = Label::builder()
        .label("")
        .halign(Align::Center)
        .max_width_chars(28)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["media-popout-title".to_string()])
        .build();
    let card_artist = Label::builder()
        .label("")
        .halign(Align::Center)
        .max_width_chars(30)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();

    let controls_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .homogeneous(true)
        .margin_top(4)
        .build();
    let prev_btn = Button::builder()
        .label("󰒮")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Previous")
        .build();
    let play_btn = Button::builder()
        .label("󰏤")
        .css_classes(vec!["primary-btn".to_string()])
        .tooltip_text("Play / Pause")
        .build();
    let next_btn = Button::builder()
        .label("󰒭")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Next")
        .build();
    controls_row.append(&prev_btn);
    controls_row.append(&play_btn);
    controls_row.append(&next_btn);

    card.append(&card_hdr);
    card.append(&art_frame);
    card.append(&card_title);
    card.append(&card_artist);
    card.append(&controls_row);
    card_revealer.set_child(Some(&card));

    // --- PILL REVEALER (pops up when dashboard is open or sliver is hovered) ---
    let pill_revealer = Revealer::builder()
        .transition_type(RevealerTransitionType::SlideUp)
        .transition_duration(180)
        .reveal_child(false)
        .build();

    let pill_btn = Button::builder()
        .halign(Align::End)
        .margin_bottom(2)
        .css_classes(vec!["media-popout-pill".to_string()])
        .tooltip_text("Left-click to expand | Middle-click to Play/Pause")
        .build();
    let pill_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();
    let pill_icon = Label::builder()
        .label("")
        .css_classes(vec!["power-btn-lbl".to_string()])
        .build();
    let pill_title = Label::builder()
        .label("")
        .max_width_chars(26)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["media-pill-lbl".to_string()])
        .build();
    pill_row.append(&pill_icon);
    pill_row.append(&pill_title);
    pill_btn.set_child(Some(&pill_row));
    pill_revealer.set_child(Some(&pill_btn));

    // --- ULTRA-MINIMAL GAP SLIVER (fits inside the ~8px gap at the bottom edge of the screen!) ---
    let sliver = Box::builder()
        .halign(Align::End)
        .valign(Align::End)
        .width_request(96)
        .height_request(6)
        .css_classes(vec!["media-edge-sliver".to_string()])
        .tooltip_text("Hover for Now Playing")
        .build();

    root.append(&card_revealer);
    root.append(&pill_revealer);
    root.append(&sliver);
    win.set_child(Some(&root));

    let is_expanded = Rc::new(Cell::new(false));
    let is_hovered = Rc::new(Cell::new(false));
    let dash_is_open = Rc::new(Cell::new(false));

    let sync_reveal: Rc<dyn Fn()> = {
        let pill_rev = pill_revealer.clone();
        let card_rev = card_revealer.clone();
        let exp = is_expanded.clone();
        let hov = is_hovered.clone();
        let dash = dash_is_open.clone();
        let sliv = sliver.clone();
        Rc::new(move || {
            let show_pill = dash.get() || hov.get() || exp.get();
            pill_rev.set_reveal_child(show_pill);
            card_rev.set_reveal_child(exp.get());
            if show_pill {
                sliv.add_css_class("active");
            } else {
                sliv.remove_css_class("active");
            }
        })
    };

    // Hovering the tiny gap sliver pops up the pill smoothly
    let motion = EventControllerMotion::new();
    {
        let hov = is_hovered.clone();
        let sync = sync_reveal.clone();
        motion.connect_enter(move |_, _, _| {
            hov.set(true);
            sync();
        });
    }
    {
        let hov = is_hovered.clone();
        let sync = sync_reveal.clone();
        motion.connect_leave(move |_| {
            hov.set(false);
            let hov_c = hov.clone();
            let sync_c = sync.clone();
            glib::timeout_add_local_once(Duration::from_millis(240), move || {
                if !hov_c.get() {
                    sync_c();
                }
            });
        });
    }
    win.add_controller(motion);

    // Toggle expanded card on pill click
    {
        let exp_c = is_expanded.clone();
        let sync = sync_reveal.clone();
        pill_btn.connect_clicked(move |_| {
            exp_c.set(!exp_c.get());
            sync();
        });
    }
    {
        let exp_c = is_expanded.clone();
        let sync = sync_reveal.clone();
        collapse_btn.connect_clicked(move |_| {
            exp_c.set(false);
            sync();
        });
    }

    // Middle-click pill to play/pause
    let mid_click = GestureClick::builder().button(2).build();
    mid_click.connect_pressed(move |_, _, _, _| {
        let _ = Command::new("playerctl").arg("play-pause").spawn();
    });
    pill_btn.add_controller(mid_click);

    // Media control buttons
    prev_btn.connect_clicked(move |_| {
        let _ = Command::new("playerctl").arg("previous").spawn();
    });
    play_btn.connect_clicked(move |_| {
        let _ = Command::new("playerctl").arg("play-pause").spawn();
    });
    next_btn.connect_clicked(move |_| {
        let _ = Command::new("playerctl").arg("next").spawn();
    });

    // Background MPRIS poller
    let (tx, rx) = mpsc::channel::<MediaMsg>();
    let tx_bg = tx.clone();
    std::thread::spawn(move || {
        let mut last: Option<MediaInfo> = None;
        let mut last_art_url = String::new();
        loop {
            let cur = query_playerctl();
            if last.as_ref() != Some(&cur) {
                let art_changed = cur.art_url != last_art_url && !cur.art_url.is_empty();
                let art_url_clone = cur.art_url.clone();
                let _ = tx_bg.send(MediaMsg::State(cur.clone()));
                if art_changed {
                    last_art_url = art_url_clone.clone();
                    if let Some(bytes) = load_art_bytes(&art_url_clone) {
                        let _ = tx_bg.send(MediaMsg::ArtBytes(art_url_clone, bytes));
                    }
                } else if cur.art_url.is_empty() {
                    last_art_url.clear();
                }
                last = Some(cur);
            }
            std::thread::sleep(Duration::from_millis(1200));
        }
    });

    let win_r = win.clone();
    let exp_r = is_expanded.clone();
    let sync_r = sync_reveal.clone();
    let sliv_r = sliver.clone();
    let cur_art_url = Rc::new(RefCell::new(String::new()));

    glib::timeout_add_local(Duration::from_millis(200), move || {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                MediaMsg::State(info) => {
                    if !info.is_active() {
                        exp_r.set(false);
                        sync_r();
                        win_r.set_visible(false);
                        art_frame.set_visible(false);
                        cur_art_url.borrow_mut().clear();
                    } else {
                        let is_playing = info.status == "Playing";
                        pill_icon.set_label(if is_playing { "" } else { "󰏤" });
                        play_btn.set_label(if is_playing { "󰏤" } else { "󰐊" });
                        pill_title.set_label(&info.title);
                        card_title.set_label(&info.title);
                        card_artist.set_label(&info.artist);
                        card_artist.set_visible(!info.artist.is_empty());
                        player_lbl.set_label(&info.player);
                        sliv_r.set_tooltip_text(Some(&format!(
                            "{} {}",
                            if is_playing { "" } else { "󰏤" },
                            info.title
                        )));
                        if is_playing {
                            sliv_r.remove_css_class("paused");
                        } else {
                            sliv_r.add_css_class("paused");
                        }

                        if info.art_url.is_empty() {
                            art_frame.set_visible(false);
                            cur_art_url.borrow_mut().clear();
                        } else if *cur_art_url.borrow() != info.art_url {
                            *cur_art_url.borrow_mut() = info.art_url.clone();
                            art_frame.set_visible(false);
                        }

                        if !win_r.is_visible() {
                            move_popout_to_focused_monitor(&win_r);
                            win_r.set_visible(true);
                        }
                        sync_r();
                    }
                }
                MediaMsg::ArtBytes(url, bytes) => {
                    if *cur_art_url.borrow() == url {
                        if let Some(tex) = bytes_to_cover_texture(&bytes) {
                            art_img.set_paintable(Some(&tex));
                            art_frame.set_visible(true);
                        }
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });

    let on_dashboard_open: Rc<dyn Fn(bool)> = {
        let dash_c = dash_is_open.clone();
        let exp_c = is_expanded.clone();
        let sync_c = sync_reveal.clone();
        let win_c = win.clone();
        Rc::new(move |open: bool| {
            dash_c.set(open);
            if !open {
                exp_c.set(false);
            } else if win_c.is_visible() {
                move_popout_to_focused_monitor(&win_c);
            }
            sync_c();
        })
    };

    MediaPopoutHandle { on_dashboard_open }
}
