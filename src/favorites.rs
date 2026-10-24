use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, Orientation, PolicyType, Popover, ScrolledWindow,
    SearchEntry,
};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Default)]
struct FavoritesConfig {
    #[serde(default)]
    ignored_paths: HashSet<String>,
    #[serde(default)]
    pinned_paths: HashSet<String>,
    #[serde(default)]
    saved_scores: HashMap<String, u32>,
}

pub struct FavoritesState {
    scores: HashMap<PathBuf, u32>,
    ignored: HashSet<String>,
    pinned: HashSet<String>,
    search_query: String,
    list_container: Box,
    popover_guard: Rc<Cell<usize>>,
}

impl FavoritesState {
    fn config_path() -> PathBuf {
        let mut path = glib::user_config_dir();
        path.push("lily-dashboard");
        let _ = fs::create_dir_all(&path);
        path.push("ignore.json");
        path
    }

    fn load_config() -> FavoritesConfig {
        let path = Self::config_path();
        if let Ok(data) = fs::read_to_string(path) {
            if let Ok(cfg) = serde_json::from_str::<FavoritesConfig>(&data) {
                return cfg;
            }
        }
        FavoritesConfig::default()
    }

    fn save_config(&self) {
        let saved_scores: HashMap<String, u32> = self
            .scores
            .iter()
            .map(|(k, &v)| (k.to_string_lossy().to_string(), v))
            .collect();

        let cfg = FavoritesConfig {
            ignored_paths: self.ignored.clone(),
            pinned_paths: self.pinned.clone(),
            saved_scores,
        };
        if let Ok(json) = serde_json::to_string_pretty(&cfg) {
            let _ = fs::write(Self::config_path(), json);
        }
    }

    pub fn ignore_file(&mut self, path_str: String) {
        self.pinned.remove(&path_str);
        self.ignored.insert(path_str);
        self.save_config();
    }

    pub fn toggle_pin(&mut self, path_str: String) {
        if self.pinned.contains(&path_str) {
            self.pinned.remove(&path_str);
        } else {
            self.pinned.insert(path_str);
        }
        self.save_config();
    }

    pub fn record_event(&mut self, path: PathBuf, weight: u32, persist: bool) {
        if path
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            return;
        }
        if path.is_file() && path.extension().is_some() {
            *self.scores.entry(path).or_insert(0) += weight;
            if persist {
                self.save_config();
            }
        }
    }

    pub fn seed_initial_files(&mut self, saved_scores: HashMap<String, u32>) {
        // 1. Restore persistent scores from disk first
        for (path_str, score) in saved_scores {
            let path = PathBuf::from(path_str);
            if path.is_file() {
                self.scores.insert(path, score);
            }
        }

        // 2. Ensure Recently Used files are present (without overwriting higher saved scores)
        let recent_mgr = gtk4::RecentManager::default();
        for item in recent_mgr.items() {
            let uri = item.uri();
            if let Ok((path, _)) = glib::filename_from_uri(&uri) {
                if !self.scores.contains_key(&path) {
                    let count = item.applications().len() as u32;
                    self.record_event(path, count.max(2), false);
                }
            }
        }

        // 3. Ensure common folders are represented
        let home = glib::home_dir();
        for folder in ["Documents", "Downloads", "Videos", "Music", "Pictures"] {
            let dir = home.join(folder);
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten().take(25) {
                    let path = entry.path();
                    if path.is_file() && !self.scores.contains_key(&path) {
                        self.record_event(path, 1, false);
                    }
                }
            }
        }

        self.save_config();
    }
}

pub fn build_favorites_ui(popover_guard: Rc<Cell<usize>>) -> Box {
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

    let search_entry = SearchEntry::builder()
        .placeholder_text("Filter favorites...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

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
    root.append(&search_entry);
    root.append(&scrolled);

    let cfg = FavoritesState::load_config();
    let state = Rc::new(RefCell::new(FavoritesState {
        scores: HashMap::new(),
        ignored: cfg.ignored_paths,
        pinned: cfg.pinned_paths,
        search_query: String::new(),
        list_container,
        popover_guard,
    }));

    {
        let mut st = state.borrow_mut();
        st.seed_initial_files(cfg.saved_scores);
    }
    refresh_list(&state);

    let state_search = state.clone();
    search_entry.connect_search_changed(move |entry| {
        state_search.borrow_mut().search_query = entry.text().to_lowercase();
        refresh_list(&state_search);
    });

    let recent_mgr = gtk4::RecentManager::default();
    let state_recent = state.clone();
    recent_mgr.connect_changed(move |mgr| {
        let mut st = state_recent.borrow_mut();
        for item in mgr.items().into_iter().take(5) {
            let uri = item.uri();
            if let Ok((path, _)) = glib::filename_from_uri(&uri) {
                st.record_event(path, 1, true);
            }
        }
        drop(st);
        refresh_list(&state_recent);
    });

    start_notify_daemon(state);

    root
}

fn start_notify_daemon(state: Rc<RefCell<FavoritesState>>) {
    let (tx, rx) = mpsc::channel::<PathBuf>();

    std::thread::spawn(move || {
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    if matches!(
                        event.kind,
                        EventKind::Access(_) | EventKind::Modify(_) | EventKind::Create(_)
                    ) {
                        for path in event.paths {
                            let _ = tx.send(path);
                        }
                    }
                }
            }) {
                Ok(w) => w,
                Err(_) => return,
            };

        let home = glib::home_dir();
        for folder in ["Documents", "Downloads", "Videos", "Music", "Pictures"] {
            let dir = home.join(folder);
            if dir.exists() {
                let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
            }
        }

        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    });

    glib::timeout_add_local(Duration::from_millis(500), move || {
        let mut updated = false;
        while let Ok(path) = rx.try_recv() {
            state.borrow_mut().record_event(path, 1, false);
            updated = true;
        }
        if updated {
            state.borrow().save_config();
            refresh_list(&state);
        }
        glib::ControlFlow::Continue
    });
}

pub fn refresh_list(state_rc: &Rc<RefCell<FavoritesState>>) {
    let (grouped, container, query_empty) = {
        let state = state_rc.borrow();

        while let Some(child) = state.list_container.first_child() {
            state.list_container.remove(&child);
        }

        let mut groups: BTreeMap<String, Vec<(PathBuf, String, u32, bool)>> = BTreeMap::new();

        for (path, &score) in &state.scores {
            let path_str = path.to_string_lossy().to_string();
            if state.ignored.contains(&path_str) || !path.exists() {
                continue;
            }

            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if !state.search_query.is_empty()
                && !file_name.to_lowercase().contains(&state.search_query)
            {
                continue;
            }

            let is_pinned = state.pinned.contains(&path_str);

            let ext = path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                .unwrap_or_else(|| "other".to_string());

            groups
                .entry(ext)
                .or_default()
                .push((path.clone(), file_name, score, is_pinned));
        }

        for files in groups.values_mut() {
            files.sort_by(|a, b| {
                b.3.cmp(&a.3)
                    .then_with(|| b.2.cmp(&a.2))
                    .then_with(|| a.1.cmp(&b.1))
            });
        }

        (
            groups,
            state.list_container.clone(),
            state.search_query.is_empty(),
        )
    };

    if grouped.is_empty() {
        let empty_lbl = Label::builder()
            .label(if query_empty {
                "No favorites recorded yet."
            } else {
                "No matching files."
            })
            .css_classes(vec!["fav-empty".to_string()])
            .margin_top(20)
            .build();
        container.append(&empty_lbl);
        return;
    }

    for (ext, files) in grouped {
        let header = Label::builder()
            .label(&ext.to_uppercase())
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(6)
            .build();
        container.append(&header);

        for (path, file_name, score, is_pinned) in files.into_iter().take(10) {
            let row = build_file_row(&path, &file_name, score, is_pinned, state_rc);
            container.append(&row);
        }
    }
}

fn build_file_row(
    path: &Path,
    file_name: &str,
    score: u32,
    is_pinned: bool,
    state_rc: &Rc<RefCell<FavoritesState>>,
) -> Box {
    let row_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .build();

    let mut btn_classes = vec!["fav-item-btn".to_string()];
    if is_pinned {
        btn_classes.push("pinned".to_string());
    }

    let btn = Button::builder()
        .css_classes(btn_classes)
        .hexpand(true)
        .tooltip_text(path.to_string_lossy().as_ref())
        .build();

    let inner_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let name_lbl = Label::builder()
        .label(file_name)
        .halign(Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .build();

    let badge_text = if is_pinned {
        format!("[PIN] {}", score)
    } else {
        format!("{}", score)
    };

    let score_lbl = Label::builder()
        .label(&badge_text)
        .css_classes(vec!["fav-score-badge".to_string()])
        .halign(Align::End)
        .build();

    inner_box.append(&name_lbl);
    inner_box.append(&score_lbl);
    btn.set_child(Some(&inner_box));

    let path_open = path.to_path_buf();
    let state_open = state_rc.clone();
    btn.connect_clicked(move |_| {
        if let Ok(uri) = glib::filename_to_uri(&path_open, None) {
            let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
            state_open.borrow_mut().record_event(path_open.clone(), 2, true);
            refresh_list(&state_open);
        }
    });

    // Right-click Context Menu Popover
    let popover = Popover::builder()
        .css_classes(vec!["fav-popover".to_string()])
        .autohide(true)
        .build();
    popover.set_parent(&row_box);

    let guard = state_rc.borrow().popover_guard.clone();
    let guard_map = guard.clone();
    popover.connect_map(move |_| {
        guard_map.set(guard_map.get() + 1);
    });
    let guard_unmap = guard.clone();
    popover.connect_unmap(move |_| {
        let g = guard_unmap.clone();
        glib::timeout_add_local_once(Duration::from_millis(150), move || {
            g.set(g.get().saturating_sub(1));
        });
    });

    let menu_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .build();

    let pin_btn = Button::builder()
        .label(if is_pinned { "Unpin" } else { "Pin to Top" })
        .css_classes(vec!["fav-hide-btn".to_string()])
        .build();

    let folder_btn = Button::builder()
        .label("Open Containing Folder")
        .css_classes(vec!["fav-hide-btn".to_string()])
        .build();

    let hide_btn = Button::builder()
        .label("Hide from Favorites")
        .css_classes(vec!["plan-delete-menu-btn".to_string()])
        .build();

    menu_box.append(&pin_btn);
    menu_box.append(&folder_btn);
    menu_box.append(&hide_btn);
    popover.set_child(Some(&menu_box));

    let right_click = GestureClick::builder().button(3).build();
    let pop_clone = popover.clone();
    right_click.connect_pressed(move |_, _, _, _| {
        pop_clone.popup();
    });
    btn.add_controller(right_click);

    let path_str = path.to_string_lossy().to_string();

    let path_pin = path_str.clone();
    let state_pin = state_rc.clone();
    let pop_pin = popover.clone();
    pin_btn.connect_clicked(move |_| {
        pop_pin.popdown();
        state_pin.borrow_mut().toggle_pin(path_pin.clone());
        refresh_list(&state_pin);
    });

    let parent_dir = path.parent().map(|p| p.to_path_buf());
    let pop_folder = popover.clone();
    folder_btn.connect_clicked(move |_| {
        pop_folder.popdown();
        if let Some(ref dir) = parent_dir {
            if let Ok(uri) = glib::filename_to_uri(dir, None) {
                let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
            }
        }
    });

    let state_hide = state_rc.clone();
    let pop_hide = popover.clone();
    hide_btn.connect_clicked(move |_| {
        pop_hide.popdown();
        state_hide.borrow_mut().ignore_file(path_str.clone());
        refresh_list(&state_hide);
    });

    row_box.append(&btn);
    row_box
}