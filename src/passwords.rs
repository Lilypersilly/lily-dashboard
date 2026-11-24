//! Simple local password list tab.
//!
//! Stores entries in `~/.config/lily-dashboard/passwords.txt` with `0600` permissions
//! and provides search, click-to-copy, and inline add/delete.

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

#[derive(Clone)]
struct PasswordEntry {
    line_idx: usize,
    service: String,
    username: String,
    password: String,
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
                service: parts[0].to_string(),
                username: parts[1].to_string(),
                password: parts[2].to_string(),
            });
        }
    }

    entries.sort_by(|a, b| a.service.to_lowercase().cmp(&b.service.to_lowercase()));
    entries
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

pub fn build_passwords_ui(popover_guard: Rc<Cell<usize>>) -> Box {
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

    // Track which line_idx values currently have their password revealed
    let revealed_set: Rc<RefCell<HashSet<usize>>> = Rc::new(RefCell::new(HashSet::new()));
    let current_query = Rc::new(RefCell::new(String::new()));

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
    form_box.append(&action_row);

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
    );

    // Search filter handler
    let list_search = list_container.clone();
    let query_search = current_query.clone();
    let rev_search = revealed_set.clone();
    let guard_search = popover_guard.clone();
    search_entry.connect_search_changed(move |entry| {
        let q = entry.text().trim().to_lowercase();
        *query_search.borrow_mut() = q.clone();
        populate_password_list(&list_search, &q, &rev_search, &guard_search);
    });

    // Save New Login handler
    let list_save = list_container.clone();
    let query_save = current_query.clone();
    let rev_save = revealed_set.clone();
    let guard_save = popover_guard.clone();
    let srv_save = service_entry.clone();
    let usr_save = user_entry.clone();
    let pwd_save = pass_entry.clone();
    let form_save = form_box.clone();
    let toggle_save = toggle_creator_btn.clone();

    let commit_save = Rc::new(move || {
        let s = srv_save.text().trim().to_string();
        let u = usr_save.text().trim().to_string();
        let p = pwd_save.text().trim().to_string();
        if s.is_empty() || p.is_empty() {
            return;
        }
        append_entry(&s, &u, &p);
        srv_save.set_text("");
        usr_save.set_text("");
        pwd_save.set_text("");
        form_save.set_visible(false);
        toggle_save.set_visible(true);
        populate_password_list(&list_save, &query_save.borrow(), &rev_save, &guard_save);
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
    revealed_set: &Rc<RefCell<HashSet<usize>>>,
    popover_guard: &Rc<Cell<usize>>,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let header = Label::builder()
        .label("SAVED LOGINS")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .margin_top(2)
        .build();
    container.append(&header);

    let entries = load_entries();
    let mut shown = 0;

    for item in entries {
        if !query.is_empty() {
            let s_match = item.service.to_lowercase().contains(query);
            let u_match = item.username.to_lowercase().contains(query);
            if !s_match && !u_match {
                continue;
            }
        }

        let is_revealed = revealed_set.borrow().contains(&item.line_idx);

        let row_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .build();

        let btn = Button::builder()
            .css_classes(vec!["fav-item-btn".to_string()])
            .hexpand(true)
            .tooltip_text("Left-click to copy password | Right-click for options")
            .build();

        let inner_col = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(2)
            .build();

        let top_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .build();

        let service_lbl = Label::builder()
            .label(&item.service)
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
        let idx = item.line_idx;
        let rev_ctx = revealed_set.clone();
        let cont_ctx = container.clone();
        let query_ctx = query.to_string();

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
                .label("Delete Login")
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
            let pop_rev = popover.downgrade();
            reveal_btn.connect_clicked(move |_| {
                if let Some(p) = pop_rev.upgrade() {
                    p.popdown();
                }
                {
                    let mut set = rev_toggle.borrow_mut();
                    if set.contains(&idx) {
                        set.remove(&idx);
                    } else {
                        set.insert(idx);
                    }
                }
                populate_password_list(&cont_rev, &query_rev, &rev_toggle, &guard_rev);
            });

            // 4. Delete Login
            let cont_del = cont_ctx.clone();
            let query_del = query_ctx.clone();
            let rev_del = rev_ctx.clone();
            let guard_del = guard_ctx.clone();
            let pop_del = popover.downgrade();
            delete_btn.connect_clicked(move |_| {
                if let Some(p) = pop_del.upgrade() {
                    p.popdown();
                }
                delete_entry(idx);
                rev_del.borrow_mut().remove(&idx);
                populate_password_list(&cont_del, &query_del, &rev_del, &guard_del);
            });

            popover.popup();
        });
        btn.add_controller(right_click);

        row_box.append(&btn);
        container.append(&row_box);
        shown += 1;
    }

    if shown == 0 {
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