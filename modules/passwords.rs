// @dash-id: passwords
// @dash-title: Passwords
// @dash-lifecycle: stateless
// @dash-hooks: init, shutdown
//! Local password list tab with optional encrypted Locker vault integration.
//!
//! Everyday logins live in `~/.config/lily-dashboard/passwords.txt` (`0600`) for
//! instant access. Sensitive logins can be loaded directly from the unlocked Locker's
//! `passwords.txt` into RAM (zeroized on lock/unload/exit) and auto-save back to the
//! encrypted vault whenever added or deleted.

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Entry, GestureClick, Label, Orientation, PolicyType, Popover,
    ScrolledWindow, SearchEntry,
};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

thread_local! {
    /// Decrypted contents of the Locker's `passwords.txt` held strictly in RAM.
    /// Zeroized and cleared whenever the Locker locks, the user clicks Unload, or the app exits.
    static LOCKER_PASSWORDS_RAM: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Zeroizes and clears any decrypted Locker passwords currently held in RAM.
pub fn wipe_locker_ram_cache() {
    LOCKER_PASSWORDS_RAM.with(|slot| {
        if let Some(mut s) = slot.borrow_mut().take() {
            unsafe {
                for b in s.as_bytes_mut() {
                    std::ptr::write_volatile(b, 0);
                }
            }
            std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        }
    });
}

fn is_locker_loaded() -> bool {
    LOCKER_PASSWORDS_RAM.with(|slot| slot.borrow().is_some())
}

#[derive(Clone)]
struct PasswordEntry {
    line_idx: usize,
    from_locker: bool,
    service: String,
    username: String,
    password: String,
}

fn parse_entries(content: &str, from_locker: bool) -> Vec<PasswordEntry> {
    let mut entries = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = trimmed.splitn(3, '|').map(|s| s.trim()).collect();
        if parts.len() == 3 {
            entries.push(PasswordEntry {
                line_idx,
                from_locker,
                service: parts[0].to_string(),
                username: parts[1].to_string(),
                password: parts[2].to_string(),
            });
        }
    }

    entries.sort_by(|a, b| a.service.to_lowercase().cmp(&b.service.to_lowercase()));
    entries
}

fn load_locker_entries_from_ram() -> Option<Vec<PasswordEntry>> {
    LOCKER_PASSWORDS_RAM.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|content| parse_entries(content, true))
    })
}

fn append_locker_entry(service: &str, username: &str, password: &str) -> Result<(), String> {
    let clean_s = service.replace('|', "-");
    let clean_u = username.replace('|', "-");
    let new_line = format!("{} | {} | {}\n", clean_s, clean_u, password);

    let updated = LOCKER_PASSWORDS_RAM.with(|slot| {
        let borrow = slot.borrow();
        let Some(existing) = borrow.as_ref() else {
            return Err("Locker passwords not loaded".to_string());
        };
        let mut next = existing.clone();
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&new_line);
        Ok(next)
    })?;

    crate::module_api::save_vault_passwords(&updated)?;
    LOCKER_PASSWORDS_RAM.with(|slot| {
        *slot.borrow_mut() = Some(updated);
    });
    Ok(())
}

fn delete_locker_entry(target_line_idx: usize) -> Result<(), String> {
    let updated = LOCKER_PASSWORDS_RAM.with(|slot| {
        let borrow = slot.borrow();
        let Some(existing) = borrow.as_ref() else {
            return Err("Locker passwords not loaded".to_string());
        };
        let mut lines: Vec<&str> = existing.lines().collect();
        if target_line_idx < lines.len() {
            lines.remove(target_line_idx);
        }
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        Ok(out)
    })?;

    crate::module_api::save_vault_passwords(&updated)?;
    LOCKER_PASSWORDS_RAM.with(|slot| {
        *slot.borrow_mut() = Some(updated);
    });
    Ok(())
}

fn passwords_file_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    let file_path = path.join("passwords.txt");
    if !file_path.exists() {
        let _ = fs::write(&file_path, "");
    }
    // Restrict file permissions to 0600 (read/write by owner only)
    if let Ok(meta) = fs::metadata(&file_path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(&file_path, perms);
    }
    file_path
}

fn load_entries() -> Vec<PasswordEntry> {
    let path = passwords_file_path();
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_entries(&content, false)
}

fn append_entry(service: &str, username: &str, password: &str) {
    let path = passwords_file_path();
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let needs_newline = !existing.is_empty() && !existing.ends_with('\n');

    // Replace any accidental '|' characters so the delimiter stays clean
    let clean_s = service.replace('|', "-");
    let clean_u = username.replace('|', "-");

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        if needs_newline {
            let _ = writeln!(file);
        }
        let _ = writeln!(file, "{} | {} | {}", clean_s, clean_u, password);
    }
}

fn delete_entry(target_line_idx: usize) {
    let path = passwords_file_path();
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    let mut lines: Vec<&str> = content.lines().collect();
    if target_line_idx < lines.len() {
        lines.remove(target_line_idx);
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = fs::write(&path, out);
    }
}

/// Pulls cryptographically secure local bytes and modulates them using a 
/// Quantum-Seeded Random API. Falls back to pure local entropy if offline.
fn generate_quantum_password(len: usize) -> (String, bool) {
    const CHARSET: &[u8] =
        b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789!@#$%&*-_";
    
    // 1. Base Entropy: Generate local cryptographically secure bytes
    let mut random_bytes = vec![0u8; len * 2];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut random_bytes);
    }

    // 2. Quantum Modulation: Fetch a 9-digit quantum number from Lizaonair API
    let mut quantum_success = false;
    let url = "https://lizaonair.com/qrng/?min=100000000&max=999999999";
    
    if let Ok(output) = std::process::Command::new("curl")
        .args([
            "-sL", 
            "-H", "Accept: application/json", 
            "--max-time", "2", 
            url
        ])
        .output() 
    {
        if output.status.success() {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                // Ensure "value" is an actual number (not null!)
                if let Some(num) = json.get("value").and_then(|v| v.as_u64()) {
                    let quantum_entropy = num.to_string().into_bytes();
                    
                    if !quantum_entropy.is_empty() {
                        for (i, byte) in random_bytes.iter_mut().enumerate() {
                            let q_byte = quantum_entropy[i % quantum_entropy.len()];
                            *byte ^= q_byte;
                        }
                        quantum_success = true;
                    }
                }
            }
        }
    }

    // 3. Convert modulated bytes to characters using Rejection Sampling
    let mut password = String::new();
    let max_valid = 256 - (256 % CHARSET.len()); 

    for byte in random_bytes {
        let b = byte as usize;
        if b < max_valid {
            password.push(CHARSET[b % CHARSET.len()] as char);
            if password.len() == len {
                break;
            }
        }
    }

    // 4. In the extraordinarily rare case that rejection sampling discarded too many bytes
    while password.len() < len {
        let mut fallback = [0u8; 1];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = std::io::Read::read_exact(&mut f, &mut fallback);
            let b = fallback[0] as usize;
            if b < max_valid {
                password.push(CHARSET[b % CHARSET.len()] as char);
            }
        } else {
            // Absolute failsafe so it never infinite loops if the OS is deeply broken
            password.push(CHARSET[password.len() % CHARSET.len()] as char);
        }
    }

    (password, quantum_success)
}

fn copy_to_clipboard(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

pub fn build_passwords_ui(
    popover_guard: Rc<Cell<usize>>,
    open_locker_cb: Rc<dyn Fn()>,
) -> Box {
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
        .placeholder_text("Search logins...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    // --- Locker Passwords Action Bar ---
    let locker_bar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let load_locker_btn = Button::builder()
        .label("󰌾 Load Locker Passwords")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .tooltip_text("Decrypt Locker's passwords.txt into RAM (creates it if missing)")
        .build();

    let unload_locker_btn = Button::builder()
        .label("Unload")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Wipe Locker passwords from RAM")
        .visible(false)
        .build();

    locker_bar.append(&load_locker_btn);
    locker_bar.append(&unload_locker_btn);

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
    root.append(&locker_bar);
    root.append(&scrolled);

    // Track which (from_locker, line_idx) pairs currently have their password revealed
    let revealed_set: Rc<RefCell<HashSet<(bool, usize)>>> = Rc::new(RefCell::new(HashSet::new()));
    let current_query = Rc::new(RefCell::new(String::new()));
    let save_to_locker = Rc::new(Cell::new(is_locker_loaded()));

    // --- Collapsible "+ New Login" Creator Dock at the Bottom ---
    let creator_wrapper = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();

    let toggle_creator_btn = Button::builder()
        .label("+ New Login")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let form_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["plan-composer-dock".to_string()])
        .visible(false)
        .build();

    let service_entry = Entry::builder()
        .placeholder_text("Service (e.g. GitHub, Steam)...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let user_entry = Entry::builder()
        .placeholder_text("Username / Email...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let pass_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .build();

    let pass_entry = Entry::builder()
        .placeholder_text("Password...")
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();

    let gen_btn = Button::builder()
        .label("Gen")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Generate random 18-character password")
        .build();

    let pass_for_gen = pass_entry.clone();
    gen_btn.connect_clicked(move |btn| {
        btn.set_sensitive(false);
        btn.set_label("...");

        let (tx, rx) = std::sync::mpsc::channel::<(String, bool)>();
        let btn_clone = btn.clone();
        let pass_entry_clone = pass_for_gen.clone();

        std::thread::spawn(move || {
            let result = generate_quantum_password(18);
            let _ = tx.send(result);
        });

        glib::timeout_add_local(Duration::from_millis(50), move || {
            match rx.try_recv() {
                Ok((pwd, is_quantum)) => {
                    pass_entry_clone.set_text(&pwd);
                    btn_clone.set_sensitive(true);
                    
                    if is_quantum {
                        // Success! Flash "Quantum!" so you know it used the API
                        btn_clone.set_label("Quantum!");
                        let btn_reset = btn_clone.clone();
                        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                            btn_reset.set_label("Gen");
                        });
                    } else {
                        // Offline/Error! Flash "Failed" so you know it used the fallback
                        btn_clone.set_label("Failed");
                        let btn_reset = btn_clone.clone();
                        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                            btn_reset.set_label("Gen");
                        });
                    }
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Prevents infinite "..." loading if the background thread ever drops!
                    btn_clone.set_sensitive(true);
                    btn_clone.set_label("Error");
                    glib::ControlFlow::Break
                }
            }
        });
    });

    pass_row.append(&pass_entry);
    pass_row.append(&gen_btn);

    let target_btn = Button::builder()
        .label("Save to: 󰌾 Locker")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .tooltip_text("Toggle whether new logins save to the encrypted Locker or plaintext passwords.txt")
        .visible(is_locker_loaded())
        .build();

    {
        let stl = save_to_locker.clone();
        target_btn.connect_clicked(move |btn| {
            let next = !stl.get();
            stl.set(next);
            if next {
                btn.set_label("Save to: 󰌾 Locker");
                btn.add_css_class("active");
            } else {
                btn.set_label("Save to: Plaintext");
                btn.remove_css_class("active");
            }
        });
    }

    let action_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let save_btn = Button::builder()
        .label("Save Login")
        .css_classes(vec!["plan-close-btn".to_string()])
        .hexpand(true)
        .build();

    let cancel_btn = Button::builder()
        .label("Cancel")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();

    action_row.append(&save_btn);
    action_row.append(&cancel_btn);

    form_box.append(&service_entry);
    form_box.append(&user_entry);
    form_box.append(&pass_row);
    form_box.append(&target_btn);
    form_box.append(&action_row);

    let sync_locker_ui: Rc<dyn Fn()> = {
        let load_b = load_locker_btn.clone();
        let unload_b = unload_locker_btn.clone();
        let tgt_b = target_btn.clone();
        let stl = save_to_locker.clone();
        Rc::new(move || {
            let loaded = is_locker_loaded();
            if loaded {
                let count = load_locker_entries_from_ram().map(|v| v.len()).unwrap_or(0);
                load_b.set_label(&format!("󰌿 Locker Loaded ({})", count));
                load_b.add_css_class("active");
                load_b.set_sensitive(false);
                unload_b.set_visible(true);
                tgt_b.set_visible(true);
                if stl.get() {
                    tgt_b.set_label("Save to: 󰌾 Locker");
                    tgt_b.add_css_class("active");
                } else {
                    tgt_b.set_label("Save to: Plaintext");
                    tgt_b.remove_css_class("active");
                }
            } else {
                load_b.set_label("󰌾 Load Locker Passwords");
                load_b.remove_css_class("active");
                load_b.set_sensitive(true);
                unload_b.set_visible(false);
                tgt_b.set_visible(false);
            }
        })
    };
    sync_locker_ui();

    // Load Locker Passwords button handler
    {
        let list_l = list_container.clone();
        let query_l = current_query.clone();
        let rev_l = revealed_set.clone();
        let guard_l = popover_guard.clone();
        let sync_l = sync_locker_ui.clone();
        let stl_l = save_to_locker.clone();
        let open_locker = open_locker_cb.clone();
        load_locker_btn.connect_clicked(move |btn| {
            match crate::module_api::load_or_create_vault_passwords() {
                Ok(content) => {
                    LOCKER_PASSWORDS_RAM.with(|slot| {
                        *slot.borrow_mut() = Some(content);
                    });
                    stl_l.set(true);
                    sync_l();
                    populate_password_list(&list_l, &query_l.borrow(), &rev_l, &guard_l, &sync_l);
                }
                Err(e) if e == "LOCKED" => {
                    btn.set_label("Unlock Locker first...");
                    let open_now = open_locker.clone();
                    let btn_reset = btn.clone();
                    let sync_reset = sync_l.clone();
                    glib::timeout_add_local_once(Duration::from_millis(350), move || {
                        let _ = btn_reset;
                        sync_reset();
                        open_now();
                    });
                }
                Err(e) => {
                    btn.set_label(&format!("Error: {}", e));
                    let sync_reset = sync_l.clone();
                    glib::timeout_add_local_once(Duration::from_millis(2000), move || {
                        sync_reset();
                    });
                }
            }
        });
    }

    // Unload Locker Passwords button handler
    {
        let list_u = list_container.clone();
        let query_u = current_query.clone();
        let rev_u = revealed_set.clone();
        let guard_u = popover_guard.clone();
        let sync_u = sync_locker_ui.clone();
        let stl_u = save_to_locker.clone();
        unload_locker_btn.connect_clicked(move |_| {
            wipe_locker_ram_cache();
            stl_u.set(false);
            rev_u.borrow_mut().retain(|(from_locker, _)| !*from_locker);
            sync_u();
            populate_password_list(&list_u, &query_u.borrow(), &rev_u, &guard_u, &sync_u);
        });
    }

    let form_toggle = form_box.clone();
    let btn_toggle = toggle_creator_btn.clone();
    let srv_focus = service_entry.clone();
    toggle_creator_btn.connect_clicked(move |btn| {
        btn.set_visible(false);
        form_toggle.set_visible(true);
        srv_focus.grab_focus();
    });

    let form_cancel = form_box.clone();
    cancel_btn.connect_clicked(move |_| {
        form_cancel.set_visible(false);
        btn_toggle.set_visible(true);
    });

    creator_wrapper.append(&toggle_creator_btn);
    creator_wrapper.append(&form_box);
    root.append(&creator_wrapper);

    populate_password_list(
        &list_container,
        &current_query.borrow(),
        &revealed_set,
        &popover_guard,
        &sync_locker_ui,
    );

    // Search filter handler
    let list_search = list_container.clone();
    let query_search = current_query.clone();
    let rev_search = revealed_set.clone();
    let guard_search = popover_guard.clone();
    let sync_search = sync_locker_ui.clone();
    search_entry.connect_search_changed(move |entry| {
        let q = entry.text().trim().to_lowercase();
        *query_search.borrow_mut() = q.clone();
        populate_password_list(&list_search, &q, &rev_search, &guard_search, &sync_search);
    });

    // Save New Login handler
    let list_save = list_container.clone();
    let query_save = current_query.clone();
    let rev_save = revealed_set.clone();
    let guard_save = popover_guard.clone();
    let sync_save = sync_locker_ui.clone();
    let stl_save = save_to_locker.clone();
    let srv_save = service_entry.clone();
    let usr_save = user_entry.clone();
    let pwd_save = pass_entry.clone();
    let form_save = form_box.clone();
    let toggle_save = toggle_creator_btn.clone();
    let save_btn_status = save_btn.clone();

    let commit_save = Rc::new(move || {
        let s = srv_save.text().trim().to_string();
        let u = usr_save.text().trim().to_string();
        let p = pwd_save.text().trim().to_string();
        if s.is_empty() || p.is_empty() {
            return;
        }
        if is_locker_loaded() && stl_save.get() {
            if let Err(e) = append_locker_entry(&s, &u, &p) {
                save_btn_status.set_label(&format!("Locker error: {}", e));
                let sb_reset = save_btn_status.clone();
                glib::timeout_add_local_once(Duration::from_millis(1800), move || {
                    sb_reset.set_label("Save Login");
                });
                return;
            }
        } else {
            append_entry(&s, &u, &p);
        }
        srv_save.set_text("");
        usr_save.set_text("");
        pwd_save.set_text("");
        form_save.set_visible(false);
        toggle_save.set_visible(true);
        sync_save();
        populate_password_list(&list_save, &query_save.borrow(), &rev_save, &guard_save, &sync_save);
    });

    let commit_btn = commit_save.clone();
    save_btn.connect_clicked(move |_| commit_btn());

    let commit_enter = commit_save.clone();
    pass_entry.connect_activate(move |_| commit_enter());

    root
}

fn populate_password_list(
    container: &Box,
    query: &str,
    revealed_set: &Rc<RefCell<HashSet<(bool, usize)>>>,
    popover_guard: &Rc<Cell<usize>>,
    sync_locker_ui: &Rc<dyn Fn()>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let mut total_shown = 0;

    // 1. Locker Passwords section (when loaded into RAM)
    if let Some(locker_entries) = load_locker_entries_from_ram() {
        let locker_header = Label::builder()
            .label("󰌾 LOCKER PASSWORDS")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .margin_top(2)
            .build();
        container.append(&locker_header);

        let mut locker_shown = 0;
        for item in locker_entries {
            if !query.is_empty() {
                let s_match = item.service.to_lowercase().contains(query);
                let u_match = item.username.to_lowercase().contains(query);
                if !s_match && !u_match {
                    continue;
                }
            }
            append_entry_row(container, &item, query, revealed_set, popover_guard, sync_locker_ui);
            locker_shown += 1;
            total_shown += 1;
        }

        if locker_shown == 0 && query.is_empty() {
            let hint = Label::builder()
                .label("No Locker logins yet — new logins auto-save here.")
                .halign(Align::Start)
                .css_classes(vec!["fav-score-badge".to_string()])
                .margin_start(4)
                .margin_bottom(4)
                .build();
            container.append(&hint);
        }
    }

    // 2. Plaintext Saved Logins section
    let header = Label::builder()
        .label("SAVED LOGINS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(if is_locker_loaded() { 8 } else { 2 })
        .build();
    container.append(&header);

    let entries = load_entries();
    let mut plain_shown = 0;
    for item in entries {
        if !query.is_empty() {
            let s_match = item.service.to_lowercase().contains(query);
            let u_match = item.username.to_lowercase().contains(query);
            if !s_match && !u_match {
                continue;
            }
        }
        append_entry_row(container, &item, query, revealed_set, popover_guard, sync_locker_ui);
        plain_shown += 1;
        total_shown += 1;
    }

    if total_shown == 0 && plain_shown == 0 {
        let empty_lbl = Label::builder()
            .label(if query.is_empty() {
                "No logins saved yet.\nClick '+ New Login' below!"
            } else {
                "No matching logins."
            })
            .justify(gtk4::Justification::Center)
            .css_classes(vec!["fav-score-badge".to_string()])
            .margin_top(20)
            .build();
        container.append(&empty_lbl);
    }
}

fn append_entry_row(
    container: &Box,
    item: &PasswordEntry,
    query: &str,
    revealed_set: &Rc<RefCell<HashSet<(bool, usize)>>>,
    popover_guard: &Rc<Cell<usize>>,
    sync_locker_ui: &Rc<dyn Fn()>,
) {
    let key = (item.from_locker, item.line_idx);
    let is_revealed = revealed_set.borrow().contains(&key);

    let row_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .build();

    let btn = Button::builder()
        .css_classes(vec!["fav-item-btn".to_string()])
        .hexpand(true)
        .tooltip_text(if item.from_locker {
            "Encrypted in Locker • Left-click to copy password | Right-click for options"
        } else {
            "Left-click to copy password | Right-click for options"
        })
        .build();

    let inner_col = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .build();

    let top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let service_text = if item.from_locker {
        format!("󰌾 {}", item.service)
    } else {
        item.service.clone()
    };
    let service_lbl = Label::builder()
        .label(&service_text)
        .halign(Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();

    let status_lbl = Label::builder()
        .label("Copy")
        .css_classes(vec!["fav-score-badge".to_string()])
        .halign(Align::End)
        .build();

    top_row.append(&service_lbl);
    top_row.append(&status_lbl);
    inner_col.append(&top_row);

    if !item.username.is_empty() {
        let user_lbl = Label::builder()
            .label(&item.username)
            .halign(Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["fav-score-badge".to_string()])
            .build();
        inner_col.append(&user_lbl);
    }

    if is_revealed {
        let pass_lbl = Label::builder()
            .label(&format!("Pass: {}", item.password))
            .halign(Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(vec!["plan-subnote".to_string()])
            .build();
        inner_col.append(&pass_lbl);
    }

    btn.set_child(Some(&inner_col));

    // Left-Click: Copy Password immediately & flash "Copied!"
    let pass_copy = item.password.clone();
    let status_flash = status_lbl.clone();
    btn.connect_clicked(move |_| {
        copy_to_clipboard(&pass_copy);
        status_flash.set_label("Copied!");
        let status_reset = status_flash.clone();
        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
            status_reset.set_label("Copy");
        });
    });

    // Lazily build Right-Click Context Menu on demand & unparent on close
    let right_click = GestureClick::builder().button(3).build();
    let row_weak = row_box.downgrade();
    let guard_ctx = popover_guard.clone();
    let usr_ctx = item.username.clone();
    let pwd_ctx = item.password.clone();
    let status_ctx = status_lbl.clone();
    let from_locker = item.from_locker;
    let idx = item.line_idx;
    let rev_ctx = revealed_set.clone();
    let cont_ctx = container.clone();
    let query_ctx = query.to_string();
    let sync_ctx = sync_locker_ui.clone();

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

        let guard_map = guard_ctx.clone();
        popover.connect_map(move |_| {
            guard_map.set(guard_map.get() + 1);
        });
        let guard_unmap = guard_ctx.clone();
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

        let copy_user_btn = Button::builder()
            .label("Copy Username")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let copy_pass_btn = Button::builder()
            .label("Copy Password")
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let reveal_btn = Button::builder()
            .label(if is_revealed {
                "Hide Password"
            } else {
                "Show Password"
            })
            .css_classes(vec!["fav-hide-btn".to_string()])
            .build();

        let delete_btn = Button::builder()
            .label(if from_locker {
                "Delete from Locker"
            } else {
                "Delete Login"
            })
            .css_classes(vec!["plan-delete-menu-btn".to_string()])
            .build();

        menu_box.append(&copy_user_btn);
        menu_box.append(&copy_pass_btn);
        menu_box.append(&reveal_btn);
        menu_box.append(&delete_btn);
        popover.set_child(Some(&menu_box));

        // 1. Copy Username
        let usr_str = usr_ctx.clone();
        let pop_usr = popover.downgrade();
        let status_usr = status_ctx.clone();
        copy_user_btn.connect_clicked(move |_| {
            if let Some(p) = pop_usr.upgrade() {
                p.popdown();
            }
            copy_to_clipboard(&usr_str);
            status_usr.set_label("User Copied!");
            let status_reset = status_usr.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                status_reset.set_label("Copy");
            });
        });

        // 2. Copy Password
        let pwd_str = pwd_ctx.clone();
        let pop_pwd = popover.downgrade();
        let status_pwd = status_ctx.clone();
        copy_pass_btn.connect_clicked(move |_| {
            if let Some(p) = pop_pwd.upgrade() {
                p.popdown();
            }
            copy_to_clipboard(&pwd_str);
            status_pwd.set_label("Copied!");
            let status_reset = status_pwd.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                status_reset.set_label("Copy");
            });
        });

        // 3. Show / Hide Password
        let rev_toggle = rev_ctx.clone();
        let cont_rev = cont_ctx.clone();
        let query_rev = query_ctx.clone();
        let guard_rev = guard_ctx.clone();
        let sync_rev = sync_ctx.clone();
        let pop_rev = popover.downgrade();
        reveal_btn.connect_clicked(move |_| {
            if let Some(p) = pop_rev.upgrade() {
                p.popdown();
            }
            {
                let mut set = rev_toggle.borrow_mut();
                if set.contains(&(from_locker, idx)) {
                    set.remove(&(from_locker, idx));
                } else {
                    set.insert((from_locker, idx));
                }
            }
            populate_password_list(&cont_rev, &query_rev, &rev_toggle, &guard_rev, &sync_rev);
        });

        // 4. Delete Login
        let cont_del = cont_ctx.clone();
        let query_del = query_ctx.clone();
        let rev_del = rev_ctx.clone();
        let guard_del = guard_ctx.clone();
        let sync_del = sync_ctx.clone();
        let pop_del = popover.downgrade();
        delete_btn.connect_clicked(move |_| {
            if let Some(p) = pop_del.upgrade() {
                p.popdown();
            }
            if from_locker {
                let _ = delete_locker_entry(idx);
            } else {
                delete_entry(idx);
            }
            rev_del.borrow_mut().remove(&(from_locker, idx));
            sync_del();
            populate_password_list(&cont_del, &query_del, &rev_del, &guard_del, &sync_del);
        });

        popover.popup();
    });
    btn.add_controller(right_click);

    row_box.append(&btn);
    container.append(&row_box);
}

pub fn init() {
    crate::module_api::register_ram_wipe_hook(wipe_locker_ram_cache);
}

pub fn shutdown() {
    wipe_locker_ram_cache();
}

pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    let sw = ctx.switch_tab.clone();
    let w = build_passwords_ui(
        ctx.popover_guard.clone(),
        std::rc::Rc::new(move || sw("locker")),
    );
    crate::module_api::ModuleHandle::simple(w)
}
