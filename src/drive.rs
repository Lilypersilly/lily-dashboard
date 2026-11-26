//! Cloud upload tab wrapping `rclone copy`.
//!
//! Lets the user pick a file or directory, set a remote path and optional bandwidth
//! limit, stream live transfer progress into a progress bar, and cancel in-flight jobs.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Entry, FileChooserAction, FileChooserNative, Label, Orientation,
    PolicyType, ProgressBar, ResponseType, ScrolledWindow,
};
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

enum UploadMsg {
    // Changed percent to Option<f64> so it matches lines that don't have a percentage!
    Progress { percent: Option<f64>, text: String },
    Done(bool),
    Error(String),
}

pub fn build_drive_ui() -> Box {
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let title_lbl = Label::builder()
        .label("RCLONE CLOUD UPLOAD")
        .css_classes(vec!["fav-group-header".to_string()])
        .halign(Align::Start)
        .build();
    root.append(&title_lbl);

    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let content = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(16)
        .build();
    scroll.set_child(Some(&content));
    root.append(&scroll);

    if !crate::config::bin_exists("rclone") {
        let warn_card = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .css_classes(vec!["plan-check-row".to_string()])
            .build();
        warn_card.append(
            &Label::builder()
                .label("⚠ Missing dependency: rclone")
                .halign(Align::Start)
                .css_classes(vec!["plan-step".to_string()])
                .build(),
        );
        warn_card.append(
            &Label::builder()
                .label("Install via your package manager (e.g. sudo pacman -S rclone) and run `rclone config`.")
                .halign(Align::Start)
                .wrap(true)
                .css_classes(vec!["plan-subnote".to_string()])
                .build(),
        );
        content.append(&warn_card);
    }

    // 1. Remote Path Config
    let remote_lbl = Label::builder()
        .label("Remote Destination:")
        .halign(Align::Start)
        .css_classes(vec!["plan-step".to_string()])
        .build();
    let default_remote = crate::config::get().paths.rclone_default_remote;
    let remote_entry = Entry::builder()
        .text(&default_remote)
        .placeholder_text("e.g., gdrive:/Backups")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    content.append(&remote_lbl);
    content.append(&remote_entry);

    // 2. Speed Limit
    let limit_lbl = Label::builder()
        .label("Bandwidth Limit:")
        .halign(Align::Start)
        .css_classes(vec!["plan-step".to_string()])
        .build();
    let limit_entry = Entry::builder()
        .placeholder_text("e.g., 5M, 500K (Leave blank for unlimited)")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    content.append(&limit_lbl);
    content.append(&limit_entry);

    // 3. File / Folder Picker
    let picker_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();

    let pick_file_btn = Button::builder()
        .label("Select File")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .build();

    let pick_folder_btn = Button::builder()
        .label("Select Folder")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .build();

    picker_row.append(&pick_file_btn);
    picker_row.append(&pick_folder_btn);
    content.append(&picker_row);

    let selected_path_rc = Rc::new(RefCell::new(String::new()));
    let selected_lbl = Label::builder()
        .label("Selected: None")
        .halign(Align::Start)
        .wrap(true)
        .width_chars(1) // <-- ADD THIS
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();
    content.append(&selected_lbl);

    // 4. Progress and Upload Controls
    let progress_bar = ProgressBar::builder()
        .fraction(0.0)
        .css_classes(vec!["drive-progress".to_string()])
        .build();

    let status_lbl = Label::builder()
        .label("Ready.")
        .halign(Align::Center)
        .width_chars(1) // <-- ADD THIS
        .ellipsize(gtk4::pango::EllipsizeMode::End) // <-- ADD THIS
        .css_classes(vec!["plan-body".to_string()])
        .build();

    let action_btn = Button::builder()
        .label("Start Upload")
        .css_classes(vec!["primary-btn".to_string()])
        .build();

    content.append(&progress_bar);
    content.append(&status_lbl);
    content.append(&action_btn);

    // --- LOGIC WIRING ---

    // Select File using bulletproof FileChooserNative
    let path_clone_f = selected_path_rc.clone();
    let lbl_clone_f = selected_lbl.clone();
    pick_file_btn.connect_clicked(move |_| {
        let dialog = FileChooserNative::builder()
            .title("Select File")
            .action(FileChooserAction::Open)
            .build();
            
        let path_cb = path_clone_f.clone();
        let lbl_cb = lbl_clone_f.clone();
        dialog.connect_response(move |d, res| {
            if res == ResponseType::Accept {
                if let Some(file) = d.file() {
                    if let Some(p) = file.path() {
                        let s = p.to_string_lossy().to_string();
                        *path_cb.borrow_mut() = s.clone();
                        lbl_cb.set_label(&format!("Selected File:\n{}", s));
                    }
                }
            }
        });
        dialog.show();
    });

    // Select Folder using bulletproof FileChooserNative
    let path_clone_d = selected_path_rc.clone();
    let lbl_clone_d = selected_lbl.clone();
    pick_folder_btn.connect_clicked(move |_| {
        let dialog = FileChooserNative::builder()
            .title("Select Folder")
            .action(FileChooserAction::SelectFolder)
            .build();
            
        let path_cb = path_clone_d.clone();
        let lbl_cb = lbl_clone_d.clone();
        dialog.connect_response(move |d, res| {
            if res == ResponseType::Accept {
                if let Some(file) = d.file() {
                    if let Some(p) = file.path() {
                        let s = p.to_string_lossy().to_string();
                        *path_cb.borrow_mut() = s.clone();
                        lbl_cb.set_label(&format!("Selected Folder:\n{}", s));
                    }
                }
            }
        });
        dialog.show();
    });

    // Upload & Cancel Action
    let active_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let is_uploading = Rc::new(Cell::new(false));

    let (tx, rx) = mpsc::channel::<UploadMsg>();

    let btn_click = action_btn.clone();
    let status_click = status_lbl.clone();
    let pb_click = progress_bar.clone();
    let remote_src = remote_entry.clone();
    let limit_src = limit_entry.clone();
    let child_ref = active_child.clone();
    let uploading_state = is_uploading.clone();
    let path_src = selected_path_rc.clone();

    action_btn.connect_clicked(move |_| {
        if uploading_state.get() {
            // Cancel current upload
            if let Ok(mut lock) = child_ref.lock() {
                if let Some(mut child) = lock.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            uploading_state.set(false);
            btn_click.set_label("Start Upload");
            btn_click.remove_css_class("active");
            status_click.set_label("Upload cancelled.");
            pb_click.set_fraction(0.0);
            return;
        }

        let local_path = path_src.borrow().clone();
        if local_path.is_empty() {
            status_click.set_label("Please select a file or folder first!");
            return;
        }

        let remote = remote_src.text().trim().to_string();
        if remote.is_empty() {
            status_click.set_label("Remote destination cannot be empty!");
            return;
        }

        // Smart Pathing: If picking a folder, append the folder's name to the remote!
        let src_path = Path::new(&local_path);
        let dest_path = if src_path.is_dir() {
            let folder_name = src_path.file_name().unwrap_or_default().to_string_lossy();
            let separator = if remote.ends_with('/') || remote.ends_with(':') { "" } else { "/" };
            format!("{}{}{}", remote, separator, folder_name)
        } else {
            remote
        };

        let bwlimit = limit_src.text().trim().to_string();

        uploading_state.set(true);
        btn_click.set_label("Cancel Upload");
        btn_click.add_css_class("active");
        status_click.set_label("Starting rclone...");
        pb_click.set_fraction(0.0);

        let tx_bg = tx.clone();
        let child_bg = child_ref.clone();

        std::thread::spawn(move || {
            let mut cmd = Command::new("rclone");
            cmd.args([
                "copy",
                &local_path,
                &dest_path,
                "-v",
                "--stats",
                "1s",
                "--stats-one-line",
                "--log-format", // <-- Drops the ugly date/INFO prefix natively!
                "",
            ]);

            if !bwlimit.is_empty() {
                cmd.args(["--bwlimit", &bwlimit]);
            }

            cmd.stderr(Stdio::piped());
            cmd.stdout(Stdio::null());

            match cmd.spawn() {
                Ok(mut child) => {
                    let stderr = child.stderr.take().expect("Failed to grab stderr");
                    if let Ok(mut lock) = child_bg.lock() {
                        *lock = Some(child);
                    }

                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        let mut percent = None;
                        for part in line.split(',') {
                            let p = part.trim();
                            if p.ends_with('%') {
                                if let Ok(pct) = p.trim_end_matches('%').parse::<f64>() {
                                    percent = Some(pct / 100.0);
                                }
                            }
                        }
                        let _ = tx_bg.send(UploadMsg::Progress {
                            percent,
                            text: line.trim().to_string(),
                        });
                    }

                    let is_alive = { child_bg.lock().unwrap().is_some() };
                    if is_alive {
                        if let Ok(mut lock) = child_bg.lock() {
                            if let Some(mut proc) = lock.take() {
                                let status = proc.wait().unwrap();
                                let _ = tx_bg.send(UploadMsg::Done(status.success()));
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx_bg.send(UploadMsg::Error(format!("Failed to start rclone: {}", e)));
                }
            }
        });
    });

    let pb_poll = progress_bar.clone();
    let status_poll = status_lbl.clone();
    let btn_poll = action_btn.clone();
    let uploading_poll = is_uploading.clone();

    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UploadMsg::Progress { percent, text } => {
                    if let Some(p) = percent {
                        pb_poll.set_fraction(p);
                    }
                    if !text.is_empty() {
                        status_poll.set_label(&text);
                    }
                }
                UploadMsg::Done(success) => {
                    uploading_poll.set(false);
                    btn_poll.set_label("Start Upload");
                    btn_poll.remove_css_class("active");
                    if success {
                        pb_poll.set_fraction(1.0);
                        status_poll.set_label("Upload Complete! ✓");
                    } else {
                        status_poll.set_label("Upload failed or cancelled.");
                    }
                }
                UploadMsg::Error(err) => {
                    uploading_poll.set(false);
                    btn_poll.set_label("Start Upload");
                    btn_poll.remove_css_class("active");
                    status_poll.set_label(&err);
                }
            }
        }
        glib::ControlFlow::Continue
    });

    root
}