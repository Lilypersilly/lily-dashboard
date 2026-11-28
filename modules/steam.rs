// @dash-id: steam
// @dash-title: Steam
// @dash-lifecycle: stateless

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Entry, Image, Label, Orientation, Picture, PolicyType,
    ScrolledWindow,
};

#[derive(Clone)]
struct SteamGame {
    appid: String,
    name: String,
    last_played: u64,
    size_on_disk: u64,
    install_path: Option<PathBuf>,
    compat_path: Option<PathBuf>,
    icon_path: Option<PathBuf>,
    banner_path: Option<PathBuf>,
}

fn extract_vdf_value(line: &str, key: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut current = String::new();

    for ch in line.chars() {
        if ch == '"' {
            if in_quotes {
                parts.push(current.clone());
                current.clear();
            }
            in_quotes = !in_quotes;
        } else if in_quotes {
            current.push(ch);
        }
    }

    if parts.len() >= 2 && parts[0].eq_ignore_ascii_case(key) {
        Some(parts[1].clone())
    } else {
        None
    }
}

fn steam_root_candidates() -> Vec<PathBuf> {
    let home = glib::home_dir();
    vec![
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ]
}

fn find_library_caches() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut caches = Vec::new();
    for root in steam_root_candidates() {
        let cache = root.join("appcache/librarycache");
        if cache.is_dir() {
            if let Ok(canon) = fs::canonicalize(&cache) {
                if seen.insert(canon.clone()) {
                    caches.push(canon);
                }
            }
        }
    }
    caches
}

fn find_steam_libraries() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut libs = Vec::new();

    for root in steam_root_candidates() {
        let steamapps = root.join("steamapps");
        if !steamapps.is_dir() {
            continue;
        }

        if let Ok(canon) = fs::canonicalize(&steamapps) {
            if seen.insert(canon.clone()) {
                libs.push(canon);
            }
        }

        let vdf_path = steamapps.join("libraryfolders.vdf");
        if let Ok(content) = fs::read_to_string(&vdf_path) {
            for line in content.lines() {
                if let Some(raw_path) = extract_vdf_value(line, "path") {
                    let clean_path = raw_path.replace("\\\\", "\\");
                    let extra = PathBuf::from(clean_path).join("steamapps");
                    if extra.is_dir() {
                        if let Ok(canon) = fs::canonicalize(&extra) {
                            if seen.insert(canon.clone()) {
                                libs.push(canon);
                            }
                        }
                    }
                }
            }
        }
    }

    libs
}

fn is_image_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).as_deref(),
        Some("jpg" | "jpeg" | "png")
    )
}

fn find_artwork(caches: &[PathBuf], appid: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut icon: Option<PathBuf> = None;
    let mut banner: Option<PathBuf> = None;

    for cache in caches {
        // 1. Classic flat layout (<appid>_icon.jpg, <appid>_header.jpg, etc.)
        if icon.is_none() {
            for suffix in ["_icon.jpg", "_icon.png"] {
                let p = cache.join(format!("{}{}", appid, suffix));
                if p.is_file() {
                    icon = Some(p);
                    break;
                }
            }
        }
        if banner.is_none() {
            for suffix in ["_header.jpg", "_library_header.jpg", "_library_hero.jpg"] {
                let p = cache.join(format!("{}{}", appid, suffix));
                if p.is_file() {
                    banner = Some(p);
                    break;
                }
            }
        }

        // 2. Newer per-appid subfolder layout (<appid>/...)
        let subdir = cache.join(appid);
        if subdir.is_dir() {
            if let Ok(entries) = fs::read_dir(&subdir) {
                let mut imgs: Vec<(PathBuf, String, u64)> = Vec::new();
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() && is_image_file(&p) {
                        let name = p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_lowercase();
                        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        imgs.push((p, name, size));
                    }
                }

                if icon.is_none() {
                    if let Some((p, _, _)) = imgs.iter().find(|(_, n, _)| n.contains("icon")) {
                        icon = Some(p.clone());
                    } else if let Some((p, _, _)) =
                        imgs.iter().min_by_key(|(_, _, sz)| *sz)
                    {
                        icon = Some(p.clone());
                    }
                }

                if banner.is_none() {
                    for needle in ["header", "library_hero", "capsule"] {
                        if let Some((p, _, _)) = imgs.iter().find(|(_, n, _)| n.contains(needle)) {
                            banner = Some(p.clone());
                            break;
                        }
                    }
                    if banner.is_none() {
                        if let Some((p, _, _)) = imgs
                            .iter()
                            .filter(|(_, n, _)| !n.contains("icon") && !n.contains("logo"))
                            .max_by_key(|(_, _, sz)| *sz)
                        {
                            banner = Some(p.clone());
                        }
                    }
                }
            }
        }

        if icon.is_some() && banner.is_some() {
            break;
        }
    }

    // Fallback: use banner as icon if no dedicated icon exists
    if icon.is_none() {
        icon = banner.clone();
    }

    (icon, banner)
}

fn is_non_game_tool(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("proton ")
        || lower == "proton experimental"
        || lower == "proton hotfix"
        || lower.starts_with("steam linux runtime")
        || lower.starts_with("steamworks common redistributables")
}

fn parse_manifest(path: &Path, steamapps: &Path, caches: &[PathBuf]) -> Option<SteamGame> {
    let content = fs::read_to_string(path).ok()?;
    let mut appid = None;
    let mut name = None;
    let mut installdir = None;
    let mut last_played = 0u64;
    let mut size_on_disk = 0u64;

    for line in content.lines() {
        if appid.is_none() {
            if let Some(v) = extract_vdf_value(line, "appid") {
                appid = Some(v);
                continue;
            }
        }
        if name.is_none() {
            if let Some(v) = extract_vdf_value(line, "name") {
                name = Some(v);
                continue;
            }
        }
        if installdir.is_none() {
            if let Some(v) = extract_vdf_value(line, "installdir") {
                installdir = Some(v);
                continue;
            }
        }
        if let Some(v) = extract_vdf_value(line, "LastPlayed") {
            if let Ok(ts) = v.parse::<u64>() {
                last_played = ts;
            }
        }
        if let Some(v) = extract_vdf_value(line, "SizeOnDisk") {
            if let Ok(sz) = v.parse::<u64>() {
                size_on_disk = sz;
            }
        }
    }

    let appid = appid?;
    let name = name?;
    if is_non_game_tool(&name) {
        return None;
    }

    let install_path = installdir
        .map(|d| steamapps.join("common").join(d))
        .filter(|p| p.is_dir());

    let compat_dir = steamapps.join("compatdata").join(&appid);
    let compat_path = if compat_dir.is_dir() {
        Some(compat_dir)
    } else {
        None
    };

    let (icon_path, banner_path) = find_artwork(caches, &appid);

    Some(SteamGame {
        appid,
        name,
        last_played,
        size_on_disk,
        install_path,
        compat_path,
        icon_path,
        banner_path,
    })
}

fn scan_steam_games() -> Vec<SteamGame> {
    let caches = find_library_caches();
    let mut by_id: HashMap<String, SteamGame> = HashMap::new();

    for lib in find_steam_libraries() {
        let Ok(entries) = fs::read_dir(&lib) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if fname.starts_with("appmanifest_") && fname.ends_with(".acf") {
                if let Some(game) = parse_manifest(&path, &lib, &caches) {
                    by_id.insert(game.appid.clone(), game);
                }
            }
        }
    }

    let mut games: Vec<SteamGame> = by_id.into_values().collect();
    games.sort_by(|a, b| {
        b.last_played
            .cmp(&a.last_played)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    games
}

fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return "Unknown".to_string();
    }
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= GB {
        format!("{:.2} GB", bytes as f64 / GB)
    } else {
        format!("{:.0} MB", bytes as f64 / MB)
    }
}

fn format_last_played(ts: u64) -> String {
    if ts == 0 {
        return "Never".to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now <= ts {
        return "Just now".to_string();
    }
    let diff = now - ts;
    let days = diff / 86_400;
    if days == 0 {
        "Today".to_string()
    } else if days == 1 {
        "Yesterday".to_string()
    } else if days < 30 {
        format!("{} days ago", days)
    } else if days < 365 {
        let months = days / 30;
        if months == 1 {
            "1 month ago".to_string()
        } else {
            format!("{} months ago", months)
        }
    } else {
        let years = days / 365;
        if years == 1 {
            "1 year ago".to_string()
        } else {
            format!("{} years ago", years)
        }
    }
}

#[derive(Clone)]
struct DetailPaneUi {
    pane: Box,
    banner: Picture,
    title: Label,
    appid_lbl: Label,
    size_lbl: Label,
    played_lbl: Label,
    path_lbl: Label,
    compat_btn: Button,
    folder_btn: Button,
    selected: Rc<RefCell<Option<SteamGame>>>,
}

impl DetailPaneUi {
    fn show_game(&self, game: &SteamGame) {
        *self.selected.borrow_mut() = Some(game.clone());

        if let Some(ref bpath) = game.banner_path {
            self.banner.set_filename(Some(bpath));
            self.banner.set_visible(true);
        } else {
            self.banner.set_filename(None::<&Path>);
            self.banner.set_visible(false);
        }

        self.title.set_label(&game.name);
        self.appid_lbl.set_label(&format!("APP ID: {}", game.appid));
        self.size_lbl
            .set_label(&format!("Install Size: {}", format_size(game.size_on_disk)));
        self.played_lbl
            .set_label(&format!("Last Played: {}", format_last_played(game.last_played)));

        if let Some(ref p) = game.install_path {
            self.path_lbl.set_label(&p.to_string_lossy());
            self.folder_btn.set_visible(true);
        } else {
            self.path_lbl.set_label("Install path not found");
            self.folder_btn.set_visible(false);
        }

        self.compat_btn.set_visible(game.compat_path.is_some());
        self.pane.set_visible(true);
    }
}

fn build_detail_pane(close_drawer: &Rc<dyn Fn()>) -> DetailPaneUi {
    let pane = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .width_request(400)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(vec!["plan-detail-pane".to_string()])
        .visible(false)
        .build();

    let top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();

    let header = Label::builder()
        .label("GAME DETAILS")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let close_btn = Button::builder()
        .label("x")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();
    let pane_c = pane.clone();
    close_btn.connect_clicked(move |_| pane_c.set_visible(false));

    top_row.append(&header);
    top_row.append(&close_btn);
    pane.append(&top_row);

    let banner = Picture::builder()
        .height_request(165)
        .can_shrink(true)
        .visible(false)
        .build();
    pane.append(&banner);

    let title = Label::builder()
        .halign(Align::Start)
        .wrap(true)
        .css_classes(vec!["plan-h1".to_string()])
        .build();
    pane.append(&title);

    let appid_lbl = Label::builder()
        .halign(Align::Start)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    pane.append(&appid_lbl);

    let card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .margin_top(4)
        .css_classes(vec!["sys-card".to_string()])
        .build();

    let size_lbl = Label::builder()
        .halign(Align::Start)
        .css_classes(vec!["plan-body".to_string()])
        .margin_top(8)
        .margin_start(10)
        .margin_end(10)
        .build();

    let played_lbl = Label::builder()
        .halign(Align::Start)
        .css_classes(vec!["plan-body".to_string()])
        .margin_start(10)
        .margin_end(10)
        .build();

    let path_lbl = Label::builder()
        .halign(Align::Start)
        .wrap(true)
        .css_classes(vec!["plan-subnote".to_string()])
        .margin_start(10)
        .margin_end(10)
        .margin_bottom(8)
        .build();

    card.append(&size_lbl);
    card.append(&played_lbl);
    card.append(&path_lbl);
    pane.append(&card);

    let selected: Rc<RefCell<Option<SteamGame>>> = Rc::new(RefCell::new(None));

    let launch_btn = Button::builder()
        .label("Launch Game")
        .css_classes(vec!["primary-btn".to_string()])
        .build();
    {
        let sel = selected.clone();
        let close_cb = close_drawer.clone();
        launch_btn.connect_clicked(move |_| {
            if let Some(ref g) = *sel.borrow() {
                let _ = Command::new("xdg-open")
                    .arg(format!("steam://rungameid/{}", g.appid))
                    .spawn();
                close_cb();
            }
        });
    }
    pane.append(&launch_btn);

    let folder_btn = Button::builder()
        .label("Open Install Folder")
        .css_classes(vec!["fav-item-btn".to_string()])
        .build();
    {
        let sel = selected.clone();
        let close_cb = close_drawer.clone();
        folder_btn.connect_clicked(move |_| {
            if let Some(ref g) = *sel.borrow() {
                if let Some(ref p) = g.install_path {
                    let _ = Command::new("xdg-open").arg(p).spawn();
                    close_cb();
                }
            }
        });
    }
    pane.append(&folder_btn);

    let compat_btn = Button::builder()
        .label("Open Proton Prefix")
        .css_classes(vec!["fav-item-btn".to_string()])
        .build();
    {
        let sel = selected.clone();
        let close_cb = close_drawer.clone();
        compat_btn.connect_clicked(move |_| {
            if let Some(ref g) = *sel.borrow() {
                if let Some(ref p) = g.compat_path {
                    let _ = Command::new("xdg-open").arg(p).spawn();
                    close_cb();
                }
            }
        });
    }
    pane.append(&compat_btn);

    let store_btn = Button::builder()
        .label("Open Steam Store Page")
        .css_classes(vec!["fav-item-btn".to_string()])
        .build();
    {
        let sel = selected.clone();
        let close_cb = close_drawer.clone();
        store_btn.connect_clicked(move |_| {
            if let Some(ref g) = *sel.borrow() {
                let _ = Command::new("xdg-open")
                    .arg(format!("steam://store/{}", g.appid))
                    .spawn();
                close_cb();
            }
        });
    }
    pane.append(&store_btn);

    DetailPaneUi {
        pane,
        banner,
        title,
        appid_lbl,
        size_lbl,
        played_lbl,
        path_lbl,
        compat_btn,
        folder_btn,
        selected,
    }
}

fn populate_list(
    list_box: &Box,
    games: &[SteamGame],
    query: &str,
    close_drawer: &Rc<dyn Fn()>,
    detail: &DetailPaneUi,
) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let q = query.trim().to_lowercase();
    let mut count = 0usize;

    for game in games {
        if !q.is_empty() && !game.name.to_lowercase().contains(&q) {
            continue;
        }
        count += 1;

        let outer = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();

        let row_content = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();

        if let Some(ref ipath) = game.icon_path {
            let pic = Picture::for_filename(ipath);
            pic.set_size_request(24, 24);
            pic.set_can_shrink(true);
            row_content.append(&pic);
        } else {
            let fallback = Image::from_icon_name("applications-games-symbolic");
            fallback.set_pixel_size(24);
            row_content.append(&fallback);
        }

        let title = Label::builder()
            .label(&game.name)
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-body".to_string()])
            .build();
        row_content.append(&title);

        let info_btn = Button::builder()
            .child(&row_content)
            .hexpand(true)
            .css_classes(vec!["fav-item-btn".to_string()])
            .build();

        let game_c = game.clone();
        let detail_c = detail.clone();
        info_btn.connect_clicked(move |_| {
            detail_c.show_game(&game_c);
        });

        let play_btn = Button::builder()
            .label("▶")
            .tooltip_text("Launch immediately")
            .css_classes(vec!["fav-item-btn".to_string()])
            .build();

        let appid = game.appid.clone();
        let close_cb = close_drawer.clone();
        play_btn.connect_clicked(move |_| {
            let _ = Command::new("xdg-open")
                .arg(format!("steam://rungameid/{}", appid))
                .spawn();
            close_cb();
        });

        outer.append(&info_btn);
        outer.append(&play_btn);
        list_box.append(&outer);
    }

    if count == 0 {
        let empty = Label::builder()
            .label(if games.is_empty() {
                "No installed Steam games found."
            } else {
                "No games match your search."
            })
            .halign(Align::Start)
            .css_classes(vec!["plan-subnote".to_string()])
            .margin_top(6)
            .build();
        list_box.append(&empty);
    }
}

pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let games = Rc::new(scan_steam_games());
    let detail = build_detail_pane(&ctx.close_drawer);

    let header = Label::builder()
        .label(&format!("STEAM LIBRARY ({})", games.len()))
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    root.append(&header);

    let search = Entry::builder()
        .placeholder_text("Search installed games...")
        .build();
    root.append(&search);

    let list_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    let scroller = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(PolicyType::Never)
        .child(&list_box)
        .build();
    root.append(&scroller);

    populate_list(&list_box, &games, "", &ctx.close_drawer, &detail);

    let list_c = list_box.clone();
    let games_c = games.clone();
    let close_c = ctx.close_drawer.clone();
    let detail_c = detail.clone();
    search.connect_changed(move |entry| {
        populate_list(&list_c, &games_c, &entry.text(), &close_c, &detail_c);
    });

    crate::module_api::ModuleHandle {
        widget: root.upcast(),
        detail_pane: Some(detail.pane.upcast()),
        on_show: None,
        on_hide: None,
    }
}