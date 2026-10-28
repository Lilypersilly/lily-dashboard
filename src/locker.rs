use aead::stream::{DecryptorBE32, EncryptorBE32};
use aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Entry, FileChooserAction, FileChooserNative, GestureClick, Label,
    Orientation, PolicyType, Popover, ProgressBar, ResponseType, ScrolledWindow, SearchEntry,
    Stack,
};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

const KEYFILE_NAME: &str = ".lily_master.key";
const INDEX_FILENAME: &str = "vault.lilar";
const RAM_EXTRACT_DIR: &str = "/dev/shm/lily-locker";

const CHUNK_SIZE: usize = 1024 * 1024; // 1 MB plaintext chunks
const TAG_SIZE: usize = 16; // 16-byte AES-GCM authentication tag per chunk
const CIPHER_CHUNK_SIZE: usize = CHUNK_SIZE + TAG_SIZE;
const MAX_BLOCK_SIZE: u64 = 10 * 1024 * 1024 * 1024; // 10 GB ceiling per .lilar block

#[derive(Serialize, Deserialize, Default, Clone)]
struct LockerAppConfig {
    vault_dir: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChunkSegment {
    pub block_id: u32,
    pub offset: u64,
    pub cipher_len: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct VaultFileEntry {
    pub name: String,
    pub original_size: u64,
    pub file_salt: [u8; 16],
    pub stream_nonce: [u8; 7],
    pub segments: Vec<ChunkSegment>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct VaultIndex {
    pub next_block_id: u32,
    pub max_size_gb: u64,
    pub files: Vec<VaultFileEntry>,
    pub dead_bytes_per_block: HashMap<u32, u64>,
}

impl Default for VaultIndex {
    fn default() -> Self {
        Self {
            next_block_id: 1,
            max_size_gb: 50, // Default 50 GB soft limit
            files: Vec::new(),
            dead_bytes_per_block: HashMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct VaultHeader {
    pub kek_salt: [u8; 16],
    pub dek_wrap_nonce: [u8; 12],
    pub wrapped_dek: Vec<u8>,
    pub requires_pin: bool,
    pub index_nonce: [u8; 12],
    pub encrypted_index: Vec<u8>,
}

struct UnlockedSession {
    vault_dir: PathBuf,
    dek: [u8; 32],
    kek_salt: [u8; 16],
    dek_wrap_nonce: [u8; 12],
    wrapped_dek: Vec<u8>,
    requires_pin: bool,
    index: VaultIndex,
}

impl Drop for UnlockedSession {
    fn drop(&mut self) {
        // Zeroize DEK in memory on lock/drop
        for b in &mut self.dek {
            *b = 0;
        }
    }
}

enum WorkerEvent {
    Progress(f64, String),
    AddDone(Result<VaultFileEntry, String>),
    ReplaceDone(Result<( usize, VaultFileEntry), String>),
    ExtractDone(Result<(PathBuf, bool), String>),
    VacuumDone(Result<VaultIndex, String>),
}

fn app_config_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("locker.json");
    path
}

fn load_app_config() -> LockerAppConfig {
    if let Ok(data) = fs::read_to_string(app_config_path()) {
        if let Ok(cfg) = serde_json::from_str::<LockerAppConfig>(&data) {
            return cfg;
        }
    }
    LockerAppConfig::default()
}

fn save_app_config(vault_dir: &str) {
    let cfg = LockerAppConfig {
        vault_dir: vault_dir.to_string(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&cfg) {
        let _ = fs::write(app_config_path(), json);
    }
}

/// Generates cryptographically secure bytes modulated with Quantum entropy (XOR pool)
fn generate_quantum_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }

    let url = "https://lizaonair.com/qrng/?min=100000000&max=999999999";
    if let Ok(output) = Command::new("curl")
        .args(["-sL", "-H", "Accept: application/json", "--max-time", "2", url])
        .output()
    {
        if output.status.success() {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(num) = json.get("value").and_then(|v| v.as_u64()) {
                    let q_bytes = num.to_string().into_bytes();
                    if !q_bytes.is_empty() {
                        for (i, b) in bytes.iter_mut().enumerate() {
                            *b ^= q_bytes[i % q_bytes.len()];
                        }
                    }
                }
            }
        }
    }
    bytes
}

fn random_fixed<const N: usize>() -> [u8; N] {
    let mut arr = [0u8; N];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut arr);
    }
    arr
}

/// Scans mounted GIO volumes & standard Linux media directories for `.lily_master.key`
fn find_usb_keyfile() -> Option<PathBuf> {
    let monitor = gio::VolumeMonitor::get();
    for mount in monitor.mounts() {
        if let Some(root_path) = mount.root().path() {
            let candidate = root_path.join(KEYFILE_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let user = std::env::var("USER").unwrap_or_else(|_| "lilypersilly".to_string());
    for base_str in [format!("/run/media/{}", user), format!("/media/{}", user)] {
        let base = PathBuf::from(base_str);
        if let Ok(entries) = fs::read_dir(base) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(KEYFILE_NAME);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Derives the 32-byte KEK from `(USB Keyfile Bytes || 4-digit PIN)` using Argon2id
fn derive_kek(usb_key_bytes: &[u8], pin: &str, salt: &[u8; 16]) -> Result<[u8; 32], String> {
    let mut combined = Vec::with_capacity(usb_key_bytes.len() + pin.len());
    combined.extend_from_slice(usb_key_bytes);
    combined.extend_from_slice(pin.as_bytes());

    let params = Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| e.to_string())?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut kek = [0u8; 32];
    argon2
        .hash_password_into(&combined, salt, &mut kek)
        .map_err(|e| format!("Argon2 error: {}", e))?;
    Ok(kek)
}

/// Derives a unique per-file 32-byte subkey from the Master DEK and the file's 16-byte salt
fn derive_file_subkey(master_dek: &[u8; 32], file_salt: &[u8; 16]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(file_salt), master_dek);
    let mut subkey = [0u8; 32];
    hk.expand(b"lily-quantum-locker-file-key", &mut subkey)
        .expect("HKDF expand failed");
    subkey
}

fn block_path(vault_dir: &Path, block_id: u32) -> PathBuf {
    vault_dir.join(format!("block_{:03}.lilar", block_id))
}

/// Calculates total physical bytes occupied by all `block_*.lilar` files in the vault
fn calculate_vault_physical_bytes(vault_dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(vault_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("block_") && name.ends_with(".lilar") {
                    if let Ok(meta) = p.metadata() {
                        total += meta.len();
                    }
                }
            }
        }
    }
    total
}

/// Rotates `vault.lilar` -> `.bak1` .. `.bak5` and atomically writes the updated index
fn save_vault_index(session: &UnlockedSession) -> Result<(), String> {
    let index_json =
        serde_json::to_vec(&session.index).map_err(|e| format!("Serialize error: {}", e))?;

    let cipher = Aes256Gcm::new((&session.dek).into());
    let index_nonce = random_fixed::<12>();
    let encrypted_index = cipher
        .encrypt(Nonce::from_slice(&index_nonce), index_json.as_ref())
        .map_err(|_| "Failed to encrypt index".to_string())?;

    let header = VaultHeader {
        kek_salt: session.kek_salt,
        dek_wrap_nonce: session.dek_wrap_nonce,
        wrapped_dek: session.wrapped_dek.clone(),
        requires_pin: session.requires_pin,
        index_nonce,
        encrypted_index,
    };

    let serialized_header =
        serde_json::to_vec_pretty(&header).map_err(|e| format!("Header error: {}", e))?;

    let main_path = session.vault_dir.join(INDEX_FILENAME);
    let tmp_path = session.vault_dir.join("vault.lilar.tmp");

    // 1. Write to temp file first
    fs::write(&tmp_path, &serialized_header).map_err(|e| format!("Write error: {}", e))?;

    // 2. Rotate rolling backups (.bak4 -> .bak5, ..., vault.lilar -> .bak1)
    if main_path.exists() {
        for i in (1..=4).rev() {
            let src = session.vault_dir.join(format!("vault.lilar.bak{}", i));
            let dst = session.vault_dir.join(format!("vault.lilar.bak{}", i + 1));
            if src.exists() {
                let _ = fs::rename(&src, &dst);
            }
        }
        let bak1 = session.vault_dir.join("vault.lilar.bak1");
        let _ = fs::copy(&main_path, &bak1);
    }

    // 3. Atomic rename over vault.lilar
    fs::rename(&tmp_path, &main_path).map_err(|e| format!("Atomic rename failed: {}", e))?;
    Ok(())
}

/// Stream-encrypts a file from `src_path` in 1MB chunks into `block_XXX.lilar` (spanning blocks if >10GB)
fn stream_encrypt_file_to_blocks(
    vault_dir: &Path,
    master_dek: &[u8; 32],
    active_block_id: &mut u32,
    src_path: &Path,
    display_name: String,
    tx: &mpsc::Sender<WorkerEvent>,
) -> Result<VaultFileEntry, String> {
    let mut src_file = File::open(src_path).map_err(|e| format!("Open error: {}", e))?;
    let orig_size = src_file.metadata().map(|m| m.len()).unwrap_or(0);

    let file_salt = random_fixed::<16>();
    let stream_nonce = random_fixed::<7>();
    let subkey = derive_file_subkey(master_dek, &file_salt);

    let aead = Aes256Gcm::new((&subkey).into());
    let mut encryptor_opt = Some(EncryptorBE32::from_aead(aead, (&stream_nonce).into()));

    let mut segments: Vec<ChunkSegment> = Vec::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut bytes_processed = 0u64;

    if *active_block_id == 0 {
        *active_block_id = 1;
    }

    loop {
        let mut read_len = 0;
        while read_len < CHUNK_SIZE {
            match src_file.read(&mut buf[read_len..]) {
                Ok(0) => break,
                Ok(n) => read_len += n,
                Err(e) => return Err(format!("Read error: {}", e)),
            }
        }

        let is_eof = read_len < CHUNK_SIZE || {
            let pos = src_file.stream_position().unwrap_or(orig_size);
            pos >= orig_size
        };

        let chunk_slice = &buf[..read_len];
        let ciphertext = if is_eof {
            encryptor_opt
                .take()
                .ok_or_else(|| "Encryptor already consumed".to_string())?
                .encrypt_last(Payload {
                    msg: chunk_slice,
                    aad: b"",
                })
                .map_err(|_| "Stream encrypt_last failed".to_string())?
        } else {
            encryptor_opt
                .as_mut()
                .ok_or_else(|| "Encryptor already consumed".to_string())?
                .encrypt_next(Payload {
                    msg: chunk_slice,
                    aad: b"",
                })
                .map_err(|_| "Stream encrypt_next failed".to_string())?
        };

        // Check if current block would exceed 10GB ceiling (unless current block is empty)
        let b_path = block_path(vault_dir, *active_block_id);
        let cur_block_len = b_path.metadata().map(|m| m.len()).unwrap_or(0);
        if cur_block_len > 0 && cur_block_len + (ciphertext.len() as u64) > MAX_BLOCK_SIZE {
            *active_block_id += 1;
        }

        let target_block_id = *active_block_id;
        let target_path = block_path(vault_dir, target_block_id);
        let mut block_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&target_path)
            .map_err(|e| format!("Block open error: {}", e))?;

        let write_offset = block_file.metadata().map(|m| m.len()).unwrap_or(0);
        block_file
            .write_all(&ciphertext)
            .map_err(|e| format!("Block write error: {}", e))?;

        let c_len = ciphertext.len() as u64;
        if let Some(last_seg) = segments.last_mut() {
            if last_seg.block_id == target_block_id
                && last_seg.offset + last_seg.cipher_len == write_offset
            {
                last_seg.cipher_len += c_len;
            } else {
                segments.push(ChunkSegment {
                    block_id: target_block_id,
                    offset: write_offset,
                    cipher_len: c_len,
                });
            }
        } else {
            segments.push(ChunkSegment {
                block_id: target_block_id,
                offset: write_offset,
                cipher_len: c_len,
            });
        }

        bytes_processed += read_len as u64;
        if orig_size > 0 {
            let pct = (bytes_processed as f64 / orig_size as f64).clamp(0.0, 1.0);
            let _ = tx.send(WorkerEvent::Progress(
                pct,
                format!("Encrypting {} ({:.0}%)...", display_name, pct * 100.0),
            ));
        }

        if is_eof {
            break;
        }
    }

    Ok(VaultFileEntry {
        name: display_name,
        original_size: orig_size,
        file_salt,
        stream_nonce,
        segments,
    })
}

/// Stream-decrypts a `VaultFileEntry` from its `.lilar` blocks into `dest_path`
fn stream_decrypt_file_from_blocks(
    vault_dir: &Path,
    master_dek: &[u8; 32],
    entry: &VaultFileEntry,
    dest_path: &Path,
    tx: &mpsc::Sender<WorkerEvent>,
) -> Result<(), String> {
    if let Some(parent) = dest_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut out_file = File::create(dest_path).map_err(|e| format!("Create error: {}", e))?;
    if dest_path.starts_with(RAM_EXTRACT_DIR) {
        if let Ok(meta) = out_file.metadata() {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(dest_path, perms);
        }
    }

    let subkey = derive_file_subkey(master_dek, &entry.file_salt);
    let aead = Aes256Gcm::new((&subkey).into());
    let mut decryptor_opt = Some(DecryptorBE32::from_aead(aead, (&entry.stream_nonce).into()));

    let total_cipher: u64 = entry.segments.iter().map(|s| s.cipher_len).sum();
    let mut cipher_processed = 0u64;

    for (seg_idx, seg) in entry.segments.iter().enumerate() {
        let is_last_seg = seg_idx + 1 == entry.segments.len();
        let b_path = block_path(vault_dir, seg.block_id);
        let mut block_file =
            File::open(&b_path).map_err(|e| format!("Missing block {:03}: {}", seg.block_id, e))?;

        block_file
            .seek(SeekFrom::Start(seg.offset))
            .map_err(|e| format!("Seek error: {}", e))?;

        let mut remaining_in_seg = seg.cipher_len as usize;
        while remaining_in_seg > 0 {
            let to_read = remaining_in_seg.min(CIPHER_CHUNK_SIZE);
            let mut c_buf = vec![0u8; to_read];
            block_file
                .read_exact(&mut c_buf)
                .map_err(|e| format!("Block read error: {}", e))?;

            remaining_in_seg -= to_read;
            cipher_processed += to_read as u64;

            let is_eof = is_last_seg && remaining_in_seg == 0;
            let plaintext = if is_eof {
                decryptor_opt
                    .take()
                    .ok_or_else(|| "Decryptor already consumed".to_string())?
                    .decrypt_last(Payload {
                        msg: &c_buf,
                        aad: b"",
                    })
                    .map_err(|_| "Corrupted block or authentication tag mismatch!".to_string())?
            } else {
                decryptor_opt
                    .as_mut()
                    .ok_or_else(|| "Decryptor already consumed".to_string())?
                    .decrypt_next(Payload {
                        msg: &c_buf,
                        aad: b"",
                    })
                    .map_err(|_| "Corrupted chunk authentication tag!".to_string())?
            };

            out_file
                .write_all(&plaintext)
                .map_err(|e| format!("Write error: {}", e))?;

            if total_cipher > 0 {
                let pct = (cipher_processed as f64 / total_cipher as f64).clamp(0.0, 1.0);
                let _ = tx.send(WorkerEvent::Progress(
                    pct,
                    format!("Decrypting {} ({:.0}%)...", entry.name, pct * 100.0),
                ));
            }

            if is_eof {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Surgical Atomic Vacuum:
/// Finds any block with dead space, streams all surviving segments into a brand-new block ID,
/// updates the index, and returns the compacted index + list of old block IDs to delete after saving.
fn run_atomic_vacuum(
    vault_dir: &Path,
    mut index: VaultIndex,
    tx: &mpsc::Sender<WorkerEvent>,
) -> Result<(VaultIndex, Vec<u32>), String> {
    let fragmented_blocks: Vec<u32> = index
        .dead_bytes_per_block
        .iter()
        .filter(|&(_, &dead)| dead > 0)
        .map(|(&b_id, _)| b_id)
        .collect();

    if fragmented_blocks.is_empty() {
        return Ok((index, Vec::new()));
    }

    let mut old_blocks_to_remove = Vec::new();

    for (b_idx, old_block_id) in fragmented_blocks.iter().copied().enumerate() {
        let old_path = block_path(vault_dir, old_block_id);
        if !old_path.exists() {
            index.dead_bytes_per_block.remove(&old_block_id);
            continue;
        }

        index.next_block_id += 1;
        let new_block_id = index.next_block_id;
        let new_path = block_path(vault_dir, new_block_id);

        let mut old_file =
            File::open(&old_path).map_err(|e| format!("Vacuum open error: {}", e))?;
        let mut new_file: Option<File> = None;
        let mut new_offset = 0u64;

        let mut copy_buf = vec![0u8; 1024 * 1024];

        for file_entry in &mut index.files {
            for seg in &mut file_entry.segments {
                if seg.block_id == old_block_id {
                    let out_f = match new_file.as_mut() {
                        Some(f) => f,
                        None => {
                            let f = File::create(&new_path)
                                .map_err(|e| format!("Vacuum create error: {}", e))?;
                            new_file = Some(f);
                            new_file.as_mut().unwrap()
                        }
                    };

                    old_file
                        .seek(SeekFrom::Start(seg.offset))
                        .map_err(|e| e.to_string())?;

                    let mut rem = seg.cipher_len as usize;
                    let start_offset = new_offset;
                    while rem > 0 {
                        let step = rem.min(copy_buf.len());
                        old_file
                            .read_exact(&mut copy_buf[..step])
                            .map_err(|e| e.to_string())?;
                        out_f
                            .write_all(&copy_buf[..step])
                            .map_err(|e| e.to_string())?;
                        rem -= step;
                        new_offset += step as u64;
                    }

                    seg.block_id = new_block_id;
                    seg.offset = start_offset;
                }
            }
        }

        index.dead_bytes_per_block.remove(&old_block_id);
        old_blocks_to_remove.push(old_block_id);

        let pct = (b_idx + 1) as f64 / fragmented_blocks.len() as f64;
        let _ = tx.send(WorkerEvent::Progress(
            pct,
            format!("Vacuumed block {:03} -> {:03}...", old_block_id, new_block_id),
        ));
    }

    Ok((index, old_blocks_to_remove))
}

fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

pub fn build_locker_ui(popover_guard: Rc<Cell<usize>>) -> Box {
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

    let stack = Stack::new();
    stack.set_hhomogeneous(false);
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    root.append(&stack);

    let session_rc: Rc<RefCell<Option<UnlockedSession>>> = Rc::new(RefCell::new(None));
    let (tx, rx) = mpsc::channel::<WorkerEvent>();

    // =========================================================================
    // VIEW 1: INITIALIZE NEW VAULT SCREEN
    // =========================================================================
    let setup_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .build();

    let setup_hdr = Label::builder()
        .label("INITIALIZE QUANTUM LOCKER")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    setup_page.append(&setup_hdr);

    let pick_vault_btn = Button::builder()
        .label("1. Select Vault Folder")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();

    let vault_dir_lbl = Label::builder()
        .label("Vault Folder: None")
        .halign(Align::Start)
        .wrap(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    let pick_usb_btn = Button::builder()
        .label("2. Select Mounted USB Stick")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();

    let usb_dir_lbl = Label::builder()
        .label("USB Stick: None")
        .halign(Align::Start)
        .wrap(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    let max_gb_entry = Entry::builder()
        .text("50")
        .placeholder_text("Max Locker Size in GB (e.g. 50)")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let setup_pin_entry = Entry::builder()
        .placeholder_text("4-Digit Master PIN...")
        .max_length(4)
        .visibility(false)
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let init_btn = Button::builder()
        .label("Create Quantum Vault")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let setup_status_lbl = Label::builder()
        .label("")
        .wrap(true)
        .width_chars(1)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();

    setup_page.append(&pick_vault_btn);
    setup_page.append(&vault_dir_lbl);
    setup_page.append(&pick_usb_btn);
    setup_page.append(&usb_dir_lbl);
    setup_page.append(&max_gb_entry);
    setup_page.append(&setup_pin_entry);
    setup_page.append(&init_btn);
    setup_page.append(&setup_status_lbl);

    // =========================================================================
    // VIEW 2: LOCKED SCREEN (EVENT-DRIVEN USB + 4-DIGIT PIN)
    // =========================================================================
    let lock_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .valign(Align::Center)
        .build();

    let lock_title = Label::builder()
        .label("QUANTUM LOCKER")
        .css_classes(vec!["plan-h1".to_string()])
        .halign(Align::Center)
        .build();

    let usb_status_lbl = Label::builder()
        .label("Insert USB Keyfile (.lily_master.key)...")
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .max_width_chars(26)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    let unlock_pin_entry = Entry::builder()
        .placeholder_text("Enter 4-Digit PIN")
        .max_length(4)
        .visibility(false)
        .xalign(0.5)
        .css_classes(vec!["fav-search".to_string()])
        .visible(false)
        .build();

    let lock_err_lbl = Label::builder()
        .label("")
        .halign(Align::Center)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();

    let change_vault_btn = Button::builder()
        .label("Change / Setup Vault Folder")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .margin_top(16)
        .build();

    lock_page.append(&lock_title);
    lock_page.append(&usb_status_lbl);
    lock_page.append(&unlock_pin_entry);
    lock_page.append(&lock_err_lbl);
    lock_page.append(&change_vault_btn);

    // =========================================================================
    // VIEW 3: UNLOCKED VAULT SCREEN
    // =========================================================================
    let vault_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .hexpand(true)
        .vexpand(true)
        .build();

    let top_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let vault_hdr_lbl = Label::builder()
        .label("QUANTUM VAULT")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let vacuum_btn = Button::builder()
        .label("Vacuum")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Reclaim dead space from replaced/deleted files")
        .build();

    let lock_now_btn = Button::builder()
        .label("Lock")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .tooltip_text("Zeroize keys in RAM and wipe /dev/shm")
        .build();

    top_row.append(&vault_hdr_lbl);
    top_row.append(&vacuum_btn);
    top_row.append(&lock_now_btn);

    // Soft-Limit Capacity Progress Bar & Inline Editor
    let cap_bar = ProgressBar::builder()
        .fraction(0.0)
        .css_classes(vec!["drive-progress".to_string()])
        .build();

    let cap_lbl = Label::builder()
        .label("0.00 GB / 50 GB (Double-click to change max size)")
        .halign(Align::Start)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["fav-score-badge".to_string()])
        .tooltip_text("Double-click to adjust Max Locker Size (GB)")
        .build();

    let cap_edit_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .visible(false)
        .build();

    let cap_edit_entry = Entry::builder()
        .placeholder_text("Max GB...")
        .width_chars(6)
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();

    let cap_save_btn = Button::builder()
        .label("Save Limit")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    let cap_cancel_btn = Button::builder()
        .label("x")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .build();

    cap_edit_row.append(&cap_edit_entry);
    cap_edit_row.append(&cap_save_btn);
    cap_edit_row.append(&cap_cancel_btn);

    let search_entry = SearchEntry::builder()
        .placeholder_text("Filter vault files...")
        .css_classes(vec!["fav-search".to_string()])
        .build();

    let files_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();

    let files_list_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    files_scroll.set_child(Some(&files_list_box));

    let status_bar_lbl = Label::builder()
        .label("Ready.")
        .halign(Align::Start)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();

    let add_file_btn = Button::builder()
        .label("+ Add File to Vault")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    vault_page.append(&top_row);
    vault_page.append(&cap_bar);
    vault_page.append(&cap_lbl);
    vault_page.append(&cap_edit_row);
    vault_page.append(&search_entry);
    vault_page.append(&files_scroll);
    vault_page.append(&status_bar_lbl);
    vault_page.append(&add_file_btn);

    stack.add_named(&setup_page, Some("setup"));
    stack.add_named(&lock_page, Some("lock"));
    stack.add_named(&vault_page, Some("vault"));

    // =========================================================================
    // STATE & LOGIC WIRING
    // =========================================================================
    let initial_cfg = load_app_config();
    let chosen_vault_dir = Rc::new(RefCell::new(initial_cfg.vault_dir.clone()));
    let chosen_usb_dir = Rc::new(RefCell::new(String::new()));
    let active_dialog: Rc<RefCell<Option<FileChooserNative>>> = Rc::new(RefCell::new(None));
    let search_query = Rc::new(RefCell::new(String::new()));

    if !initial_cfg.vault_dir.is_empty() {
        vault_dir_lbl.set_label(&format!("Vault Folder: {}", initial_cfg.vault_dir));
    }

    // Helper: Refresh the Unlocked Vault File List + Capacity Bar
    let refresh_vault_ui: Rc<dyn Fn()> = {
        let sess_r = session_rc.clone();
        let list_r = files_list_box.clone();
        let bar_r = cap_bar.clone();
        let lbl_r = cap_lbl.clone();
        let vac_btn_r = vacuum_btn.clone();
        let status_r = status_bar_lbl.clone();
        let query_r = search_query.clone();
        let guard_r = popover_guard.clone();
        let tx_r = tx.clone();
        let dialog_r = active_dialog.clone();

        let self_rc: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let self_weak = self_rc.clone();

        let closure: Rc<dyn Fn()> = Rc::new(move || {
            while let Some(child) = list_r.first_child() {
                list_r.remove(&child);
            }

            let sess_borrow = sess_r.borrow();
            let Some(sess) = sess_borrow.as_ref() else {
                return;
            };

            let phys_bytes = calculate_vault_physical_bytes(&sess.vault_dir);
            let max_bytes = sess.index.max_size_gb.saturating_mul(1024 * 1024 * 1024);
            let dead_bytes: u64 = sess.index.dead_bytes_per_block.values().sum();

            let frac = if max_bytes > 0 {
                (phys_bytes as f64 / max_bytes as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            bar_r.set_fraction(frac);

            let dead_pct = if phys_bytes > 0 {
                (dead_bytes as f64 / phys_bytes as f64) * 100.0
            } else {
                0.0
            };

            lbl_r.set_label(&format!(
                "{} / {} GB ({:.0}% dead)",
                format_bytes(phys_bytes),
                sess.index.max_size_gb,
                dead_pct
            ));

            if dead_bytes > 0 {
                vac_btn_r.set_label(&format!("Vacuum ({})", format_bytes(dead_bytes)));
            } else {
                vac_btn_r.set_label("Vacuum");
            }

            let q = query_r.borrow().clone();
            let mut shown = 0;

            for (idx, file_entry) in sess.index.files.iter().enumerate() {
                if !q.is_empty() && !file_entry.name.to_lowercase().contains(&q) {
                    continue;
                }
                shown += 1;

                let row_box = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .build();

                let btn = Button::builder()
                    .css_classes(vec!["fav-item-btn".to_string()])
                    .hexpand(true)
                    .tooltip_text(
                        "Left-click to decrypt & view in RAM (/dev/shm) | Right-click for Copy / Replace / Delete",
                    )
                    .build();

                let inner = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .spacing(6)
                    .build();

                let name_lbl = Label::builder()
                    .label(&file_entry.name)
                    .halign(Align::Start)
                    .hexpand(true)
                    .width_chars(1)
                    .ellipsize(gtk4::pango::EllipsizeMode::Middle)
                    .build();

                let size_lbl = Label::builder()
                    .label(&format_bytes(file_entry.original_size))
                    .css_classes(vec!["fav-score-badge".to_string()])
                    .halign(Align::End)
                    .build();

                inner.append(&name_lbl);
                inner.append(&size_lbl);
                btn.set_child(Some(&inner));

                // Left-Click: Decrypt straight to `/dev/shm/lily-locker/<name>` (RAM) and open
                let entry_open = file_entry.clone();
                let v_dir_open = sess.vault_dir.clone();
                let dek_open = sess.dek;
                let tx_open = tx_r.clone();
                btn.connect_clicked(move |_| {
                    let ram_path = PathBuf::from(RAM_EXTRACT_DIR).join(&entry_open.name);
                    let v_dir = v_dir_open.clone();
                    let entry_c = entry_open.clone();
                    let tx_bg = tx_open.clone();
                    std::thread::spawn(move || {
                        let res = stream_decrypt_file_from_blocks(
                            &v_dir, &dek_open, &entry_c, &ram_path, &tx_bg,
                        )
                        .map(|_| (ram_path, true));
                        let _ = tx_bg.send(WorkerEvent::ExtractDone(res));
                    });
                });

                // Right-Click Context Menu: Copy | Replace (from RAM) | Replace (Pick File) | Delete
                let popover = Popover::builder()
                    .css_classes(vec!["fav-popover".to_string()])
                    .autohide(true)
                    .build();
                popover.set_parent(&row_box);

                let guard_map = guard_r.clone();
                popover.connect_map(move |_| {
                    guard_map.set(guard_map.get() + 1);
                });
                let guard_unmap = guard_r.clone();
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

                let copy_btn = Button::builder()
                    .label("Copy to Host...")
                    .css_classes(vec!["fav-hide-btn".to_string()])
                    .build();

                let replace_ram_btn = Button::builder()
                    .label("Replace from RAM Edit")
                    .css_classes(vec!["fav-hide-btn".to_string()])
                    .build();

                let replace_pick_btn = Button::builder()
                    .label("Replace with File...")
                    .css_classes(vec!["fav-hide-btn".to_string()])
                    .build();

                let del_btn = Button::builder()
                    .label("Delete from Vault")
                    .css_classes(vec!["plan-delete-menu-btn".to_string()])
                    .build();

                menu_box.append(&copy_btn);
                menu_box.append(&replace_ram_btn);
                menu_box.append(&replace_pick_btn);
                menu_box.append(&del_btn);
                popover.set_child(Some(&menu_box));

                let right_click = GestureClick::builder().button(3).build();
                let pop_open = popover.clone();
                right_click.connect_pressed(move |_, _, _, _| {
                    pop_open.popup();
                });
                btn.add_controller(right_click);

                // 1. Copy to Host...
                {
                    let pop_c = popover.clone();
                    let dlg_slot = dialog_r.clone();
                    let entry_c = file_entry.clone();
                    let v_dir_c = sess.vault_dir.clone();
                    let dek_c = sess.dek;
                    let tx_c = tx_r.clone();
                    copy_btn.connect_clicked(move |_| {
                        pop_c.popdown();
                        let dialog = FileChooserNative::builder()
                            .title("Select Destination Folder for Copy")
                            .action(FileChooserAction::SelectFolder)
                            .build();
                        let entry_inner = entry_c.clone();
                        let v_dir_inner = v_dir_c.clone();
                        let tx_inner = tx_c.clone();
                        let dlg_clear = dlg_slot.clone();
                        dialog.connect_response(move |d, res| {
                            if res == ResponseType::Accept {
                                if let Some(folder) = d.file().and_then(|f| f.path()) {
                                    let dst = folder.join(&entry_inner.name);
                                    let v_d = v_dir_inner.clone();
                                    let e_cl = entry_inner.clone();
                                    let t_cl = tx_inner.clone();
                                    std::thread::spawn(move || {
                                        let r = stream_decrypt_file_from_blocks(
                                            &v_d, &dek_c, &e_cl, &dst, &t_cl,
                                        )
                                        .map(|_| (dst, false));
                                        let _ = t_cl.send(WorkerEvent::ExtractDone(r));
                                    });
                                }
                            }
                            *dlg_clear.borrow_mut() = None;
                        });
                        dialog.show();
                        *dlg_slot.borrow_mut() = Some(dialog);
                    });
                }

                // 2. Replace from RAM Edit (`/dev/shm/lily-locker/<name>`)
                {
                    let pop_rr = popover.clone();
                    let entry_rr = file_entry.clone();
                    let v_dir_rr = sess.vault_dir.clone();
                    let dek_rr = sess.dek;
                    let mut next_blk = sess.index.next_block_id;
                    let max_b = max_bytes;
                    let phys_b = phys_bytes;
                    let status_rr = status_r.clone();
                    let tx_rr = tx_r.clone();
                    replace_ram_btn.connect_clicked(move |_| {
                        pop_rr.popdown();
                        let ram_file = PathBuf::from(RAM_EXTRACT_DIR).join(&entry_rr.name);
                        if !ram_file.is_file() {
                            status_rr.set_label("No edited file found in /dev/shm! Left-click to open first.");
                            return;
                        }
                        let new_len = ram_file.metadata().map(|m| m.len()).unwrap_or(0);
                        if phys_b + new_len > max_b {
                            status_rr.set_label("Locker full! Increase max size, delete files, or Vacuum.");
                            return;
                        }
                        let v_d = v_dir_rr.clone();
                        let name_s = entry_rr.name.clone();
                        let t_cl = tx_rr.clone();
                        std::thread::spawn(move || {
                            let res = stream_encrypt_file_to_blocks(
                                &v_d,
                                &dek_rr,
                                &mut next_blk,
                                &ram_file,
                                name_s,
                                &t_cl,
                            )
                            .map(|new_entry| {
                                let _ = fs::remove_file(&ram_file);
                                (idx, new_entry)
                            });
                            let _ = t_cl.send(WorkerEvent::ReplaceDone(res));
                        });
                    });
                }

                // 3. Replace with File... (FileChooserNative)
                {
                    let pop_rp = popover.clone();
                    let dlg_slot = dialog_r.clone();
                    let entry_rp = file_entry.clone();
                    let v_dir_rp = sess.vault_dir.clone();
                    let dek_rp = sess.dek;
                    let mut next_blk = sess.index.next_block_id;
                    let max_b = max_bytes;
                    let phys_b = phys_bytes;
                    let status_rp = status_r.clone();
                    let tx_rp = tx_r.clone();
                    replace_pick_btn.connect_clicked(move |_| {
                        pop_rp.popdown();
                        let dialog = FileChooserNative::builder()
                            .title("Select Replacement File")
                            .action(FileChooserAction::Open)
                            .build();
                        let entry_inner = entry_rp.clone();
                        let v_dir_inner = v_dir_rp.clone();
                        let status_inner = status_rp.clone();
                        let tx_inner = tx_rp.clone();
                        let dlg_clear = dlg_slot.clone();
                        dialog.connect_response(move |d, res| {
                            if res == ResponseType::Accept {
                                if let Some(src_p) = d.file().and_then(|f| f.path()) {
                                    let new_len = src_p.metadata().map(|m| m.len()).unwrap_or(0);
                                    if phys_b + new_len > max_b {
                                        status_inner.set_label(
                                            "Locker full! Increase max size, delete files, or Vacuum.",
                                        );
                                    } else {
                                        let v_d = v_dir_inner.clone();
                                        let name_s = entry_inner.name.clone();
                                        let t_cl = tx_inner.clone();
                                        std::thread::spawn(move || {
                                            let r = stream_encrypt_file_to_blocks(
                                                &v_d,
                                                &dek_rp,
                                                &mut next_blk,
                                                &src_p,
                                                name_s,
                                                &t_cl,
                                            )
                                            .map(|new_e| (idx, new_e));
                                            let _ = t_cl.send(WorkerEvent::ReplaceDone(r));
                                        });
                                    }
                                }
                            }
                            *dlg_clear.borrow_mut() = None;
                        });
                        dialog.show();
                        *dlg_slot.borrow_mut() = Some(dialog);
                    });
                }

                // 4. Delete from Vault
                {
                    let pop_d = popover.clone();
                    let sess_del = sess_r.clone();
                    let status_del = status_r.clone();
                    let refresh_del = self_weak.clone();
                    del_btn.connect_clicked(move |_| {
                        pop_d.popdown();
                        {
                            let mut borrow = sess_del.borrow_mut();
                            if let Some(s) = borrow.as_mut() {
                                if idx < s.index.files.len() {
                                    let removed = s.index.files.remove(idx);
                                    for seg in removed.segments {
                                        *s.index
                                            .dead_bytes_per_block
                                            .entry(seg.block_id)
                                            .or_insert(0) += seg.cipher_len;
                                    }
                                    let _ = save_vault_index(s);
                                    status_del.set_label(&format!("Deleted {}.", removed.name));
                                }
                            }
                        }
                        if let Some(ref cb) = *refresh_del.borrow() {
                            cb();
                        }
                    });
                }

                row_box.append(&btn);
                list_r.append(&row_box);
            }

            if shown == 0 {
                let empty_lbl = Label::builder()
                    .label("Vault is empty.\nClick '+ Add File to Vault' below!")
                    .justify(gtk4::Justification::Center)
                    .css_classes(vec!["fav-score-badge".to_string()])
                    .margin_top(20)
                    .build();
                list_r.append(&empty_lbl);
            }
        });

        *self_rc.borrow_mut() = Some(closure.clone());
        closure
    };

    // Helper: Lock the Vault & Zeroize RAM + `/dev/shm/lily-locker`
    let lock_vault_action: Rc<dyn Fn()> = {
        let sess_l = session_rc.clone();
        let stk_l = stack.clone();
        let pin_l = unlock_pin_entry.clone();
        let usb_lbl_l = usb_status_lbl.clone();
        let err_l = lock_err_lbl.clone();
        let v_dir_l = chosen_vault_dir.clone();

        Rc::new(move || {
            *sess_l.borrow_mut() = None;
            let _ = fs::remove_dir_all(RAM_EXTRACT_DIR);
            pin_l.set_text("");
            err_l.set_label("");

            let v_dir = v_dir_l.borrow().clone();
            if v_dir.is_empty() || !Path::new(&v_dir).join(INDEX_FILENAME).exists() {
                stk_l.set_visible_child_name("setup");
                return;
            }

            stk_l.set_visible_child_name("lock");
            if let Some(key_path) = find_usb_keyfile() {
                usb_lbl_l.set_label(&format!(
                    "USB Keyfile Ready:\n{}",
                    key_path.display()
                ));
                pin_l.set_visible(true);
                pin_l.grab_focus();
            } else {
                usb_lbl_l.set_label("Insert USB Keyfile (.lily_master.key)...");
                pin_l.set_visible(false);
            }
        })
    };

    // Initialize UI state on startup
    lock_vault_action();

    // =========================================================================
    // ZERO-POLLING EVENT-DRIVEN USB MONITOR (`gio::VolumeMonitor`)
    // =========================================================================
    let vol_monitor = gio::VolumeMonitor::get();
    {
        let lock_cb = lock_vault_action.clone();
        let sess_chk = session_rc.clone();
        vol_monitor.connect_mount_added(move |_, _| {
            if sess_chk.borrow().is_none() {
                lock_cb();
            }
        });
    }
    {
        let lock_cb = lock_vault_action.clone();
        vol_monitor.connect_mount_removed(move |_, _| {
            if find_usb_keyfile().is_none() {
                // USB Key was pulled out! Lock immediately and wipe RAM!
                lock_cb();
            }
        });
    }
    // Keep `vol_monitor` alive for the lifetime of the widget
    root.connect_destroy(move |_| {
        let _keep_alive = &vol_monitor;
    });

    // =========================================================================
    // SETUP SCREEN HANDLERS
    // =========================================================================
    {
        let dlg_slot = active_dialog.clone();
        let v_dir_rc = chosen_vault_dir.clone();
        let v_lbl = vault_dir_lbl.clone();
        pick_vault_btn.connect_clicked(move |_| {
            let dialog = FileChooserNative::builder()
                .title("Select Vault Storage Folder")
                .action(FileChooserAction::SelectFolder)
                .build();
            let v_rc = v_dir_rc.clone();
            let lbl = v_lbl.clone();
            let dlg_clear = dlg_slot.clone();
            dialog.connect_response(move |d, res| {
                if res == ResponseType::Accept {
                    if let Some(p) = d.file().and_then(|f| f.path()) {
                        let s = p.to_string_lossy().to_string();
                        *v_rc.borrow_mut() = s.clone();
                        save_app_config(&s);
                        lbl.set_label(&format!("Vault Folder: {}", s));
                    }
                }
                *dlg_clear.borrow_mut() = None;
            });
            dialog.show();
            *dlg_slot.borrow_mut() = Some(dialog);
        });
    }

    {
        let dlg_slot = active_dialog.clone();
        let u_dir_rc = chosen_usb_dir.clone();
        let u_lbl = usb_dir_lbl.clone();
        pick_usb_btn.connect_clicked(move |_| {
            let dialog = FileChooserNative::builder()
                .title("Select Mounted USB Stick")
                .action(FileChooserAction::SelectFolder)
                .build();
            let u_rc = u_dir_rc.clone();
            let lbl = u_lbl.clone();
            let dlg_clear = dlg_slot.clone();
            dialog.connect_response(move |d, res| {
                if res == ResponseType::Accept {
                    if let Some(p) = d.file().and_then(|f| f.path()) {
                        let s = p.to_string_lossy().to_string();
                        *u_rc.borrow_mut() = s.clone();
                        lbl.set_label(&format!("USB Stick: {}", s));
                    }
                }
                *dlg_clear.borrow_mut() = None;
            });
            dialog.show();
            *dlg_slot.borrow_mut() = Some(dialog);
        });
    }

    {
        let v_dir_rc = chosen_vault_dir.clone();
        let u_dir_rc = chosen_usb_dir.clone();
        let max_entry = max_gb_entry.clone();
        let pin_entry = setup_pin_entry.clone();
        let status_lbl = setup_status_lbl.clone();
        let sess_init = session_rc.clone();
        let stk_init = stack.clone();
        let refresh_init = refresh_vault_ui.clone();

        init_btn.connect_clicked(move |_| {
            let v_str = v_dir_rc.borrow().clone();
            let u_str = u_dir_rc.borrow().clone();
            let pin = pin_entry.text().trim().to_string();
            let max_gb: u64 = max_entry.text().trim().parse().unwrap_or(50).max(1);

            if v_str.is_empty() {
                status_lbl.set_label("Select a Vault Storage Folder first!");
                return;
            }
            let v_path = PathBuf::from(&v_str);

            // If `vault.lilar` already exists in that folder, just switch to Lock screen!
            if v_path.join(INDEX_FILENAME).exists() {
                save_app_config(&v_str);
                stk_init.set_visible_child_name("lock");
                return;
            }

            if u_str.is_empty() {
                status_lbl.set_label("Select your USB stick folder to store .lily_master.key!");
                return;
            }
            if pin.len() != 4 || !pin.chars().all(|c| c.is_ascii_digit()) {
                status_lbl.set_label("PIN must be 4 digits (e.g. 1234)!");
                return;
            }

            let usb_key_path = PathBuf::from(&u_str).join(KEYFILE_NAME);
            let usb_key_bytes = if usb_key_path.is_file() {
                fs::read(&usb_key_path).unwrap_or_else(|_| generate_quantum_bytes(64))
            } else {
                let fresh = generate_quantum_bytes(64);
                if let Err(e) = fs::write(&usb_key_path, &fresh) {
                    status_lbl.set_label(&format!("Failed to write USB keyfile: {}", e));
                    return;
                }
                fresh
            };

            let dek_vec = generate_quantum_bytes(32);
            let mut dek = [0u8; 32];
            dek.copy_from_slice(&dek_vec[..32]);

            let kek_salt = random_fixed::<16>();
            let Ok(kek) = derive_kek(&usb_key_bytes, &pin, &kek_salt) else {
                status_lbl.set_label("Key derivation failed!");
                return;
            };

            let dek_wrap_nonce = random_fixed::<12>();
            let kek_cipher = Aes256Gcm::new((&kek).into());
            let Ok(wrapped_dek) =
                kek_cipher.encrypt(Nonce::from_slice(&dek_wrap_nonce), dek.as_ref())
            else {
                status_lbl.set_label("Failed to wrap DEK!");
                return;
            };

            let mut index = VaultIndex::default();
            index.max_size_gb = max_gb;

            let new_sess = UnlockedSession {
                vault_dir: v_path,
                dek,
                kek_salt,
                dek_wrap_nonce,
                wrapped_dek,
                requires_pin: true,
                index,
            };

            if let Err(e) = save_vault_index(&new_sess) {
                status_lbl.set_label(&e);
                return;
            }

            save_app_config(&v_str);
            pin_entry.set_text("");
            *sess_init.borrow_mut() = Some(new_sess);
            refresh_init();
            stk_init.set_visible_child_name("vault");
        });
    }

    {
        let stk_chg = stack.clone();
        change_vault_btn.connect_clicked(move |_| {
            stk_chg.set_visible_child_name("setup");
        });
    }

    // =========================================================================
    // 4-DIGIT AUTO-UNLOCK HANDLER
    // =========================================================================
    {
        let v_dir_u = chosen_vault_dir.clone();
        let err_u = lock_err_lbl.clone();
        let sess_u = session_rc.clone();
        let stk_u = stack.clone();
        let refresh_u = refresh_vault_ui.clone();

        unlock_pin_entry.connect_changed(move |entry| {
            let pin = entry.text().to_string();
            if pin.len() < 4 {
                return;
            }

            let Some(usb_key_path) = find_usb_keyfile() else {
                err_u.set_label("USB Keyfile missing!");
                entry.set_text("");
                return;
            };

            let Ok(usb_bytes) = fs::read(&usb_key_path) else {
                err_u.set_label("Cannot read USB Keyfile!");
                entry.set_text("");
                return;
            };

            let v_path = PathBuf::from(v_dir_u.borrow().clone());
            let index_path = v_path.join(INDEX_FILENAME);
            let Ok(raw_header) = fs::read(&index_path) else {
                err_u.set_label("Cannot read vault.lilar!");
                entry.set_text("");
                return;
            };

            let Ok(header) = serde_json::from_slice::<VaultHeader>(&raw_header) else {
                err_u.set_label("Corrupted vault.lilar header!");
                entry.set_text("");
                return;
            };

            let Ok(kek) = derive_kek(&usb_bytes, &pin, &header.kek_salt) else {
                err_u.set_label("Argon2 error!");
                entry.set_text("");
                return;
            };

            let kek_cipher = Aes256Gcm::new((&kek).into());
            let Ok(dek_bytes) = kek_cipher.decrypt(
                Nonce::from_slice(&header.dek_wrap_nonce),
                header.wrapped_dek.as_ref(),
            ) else {
                err_u.set_label("Wrong PIN or wrong USB key!");
                entry.set_text("");
                return;
            };

            if dek_bytes.len() != 32 {
                err_u.set_label("Invalid DEK length!");
                entry.set_text("");
                return;
            }

            let mut dek = [0u8; 32];
            dek.copy_from_slice(&dek_bytes);

            let dek_cipher = Aes256Gcm::new((&dek).into());
            let Ok(index_json) = dek_cipher.decrypt(
                Nonce::from_slice(&header.index_nonce),
                header.encrypted_index.as_ref(),
            ) else {
                err_u.set_label("Index authentication failed!");
                entry.set_text("");
                return;
            };

            let Ok(index) = serde_json::from_slice::<VaultIndex>(&index_json) else {
                err_u.set_label("Failed to parse VaultIndex!");
                entry.set_text("");
                return;
            };

            entry.set_text("");
            err_u.set_label("");

            *sess_u.borrow_mut() = Some(UnlockedSession {
                vault_dir: v_path,
                dek,
                kek_salt: header.kek_salt,
                dek_wrap_nonce: header.dek_wrap_nonce,
                wrapped_dek: header.wrapped_dek,
                requires_pin: header.requires_pin,
                index,
            });

            refresh_u();
            stk_u.set_visible_child_name("vault");
        });
    }

    // =========================================================================
    // UNLOCKED VAULT CONTROLS (LOCK, SOFT LIMIT, SEARCH, ADD, VACUUM)
    // =========================================================================
    {
        let lock_cb = lock_vault_action.clone();
        lock_now_btn.connect_clicked(move |_| lock_cb());
    }

    // Double-Click Capacity Label -> Edit Max Locker Size (GB)
    {
        let dbl = GestureClick::builder().button(1).build();
        let lbl_hide = cap_lbl.clone();
        let row_show = cap_edit_row.clone();
        let entry_init = cap_edit_entry.clone();
        let sess_c = session_rc.clone();
        dbl.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                if let Some(ref s) = *sess_c.borrow() {
                    entry_init.set_text(&s.index.max_size_gb.to_string());
                }
                lbl_hide.set_visible(false);
                row_show.set_visible(true);
                entry_init.grab_focus();
            }
        });
        cap_lbl.add_controller(dbl);

        let lbl_cancel = cap_lbl.clone();
        let row_cancel = cap_edit_row.clone();
        cap_cancel_btn.connect_clicked(move |_| {
            row_cancel.set_visible(false);
            lbl_cancel.set_visible(true);
        });

        let lbl_save = cap_lbl.clone();
        let row_save = cap_edit_row.clone();
        let entry_save = cap_edit_entry.clone();
        let sess_save = session_rc.clone();
        let refresh_save = refresh_vault_ui.clone();
        let commit_cap = Rc::new(move || {
            if let Ok(new_gb) = entry_save.text().trim().parse::<u64>() {
                let clamped = new_gb.max(1);
                if let Some(ref mut s) = *sess_save.borrow_mut() {
                    s.index.max_size_gb = clamped;
                    let _ = save_vault_index(s);
                }
                refresh_save();
            }
            row_save.set_visible(false);
            lbl_save.set_visible(true);
        });

        let c_btn = commit_cap.clone();
        cap_save_btn.connect_clicked(move |_| c_btn());
        let c_ent = commit_cap.clone();
        cap_edit_entry.connect_activate(move |_| c_ent());
    }

    // Search Filter
    {
        let q_rc = search_query.clone();
        let refresh_s = refresh_vault_ui.clone();
        search_entry.connect_search_changed(move |entry| {
            *q_rc.borrow_mut() = entry.text().trim().to_lowercase();
            refresh_s();
        });
    }

    // "+ Add File to Vault" Button (Enforces Soft Limit!)
    {
        let dlg_slot = active_dialog.clone();
        let sess_add = session_rc.clone();
        let status_add = status_bar_lbl.clone();
        let tx_add = tx.clone();

        add_file_btn.connect_clicked(move |_| {
            let (v_dir, dek, mut next_blk, max_bytes, phys_bytes) = {
                let borrow = sess_add.borrow();
                let Some(s) = borrow.as_ref() else {
                    return;
                };
                let phys = calculate_vault_physical_bytes(&s.vault_dir);
                let max_b = s.index.max_size_gb.saturating_mul(1024 * 1024 * 1024);
                (
                    s.vault_dir.clone(),
                    s.dek,
                    s.index.next_block_id,
                    max_b,
                    phys,
                )
            };

            if phys_bytes >= max_bytes {
                status_add.set_label("Locker full, increase size or delete files!");
                return;
            }

            let dialog = FileChooserNative::builder()
                .title("Select File to Encrypt into Vault")
                .action(FileChooserAction::Open)
                .build();

            let status_inner = status_add.clone();
            let tx_inner = tx_add.clone();
            let dlg_clear = dlg_slot.clone();
            dialog.connect_response(move |d, res| {
                if res == ResponseType::Accept {
                    if let Some(src_path) = d.file().and_then(|f| f.path()) {
                        let file_len = src_path.metadata().map(|m| m.len()).unwrap_or(0);
                        let est_cipher_len =
                            file_len + ((file_len / (CHUNK_SIZE as u64)) + 1) * (TAG_SIZE as u64);

                        if phys_bytes + est_cipher_len > max_bytes {
                            status_inner.set_label("Locker full, increase size or delete files!");
                        } else {
                            let fname = src_path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| "file.bin".to_string());
                            let v_d = v_dir.clone();
                            let t_cl = tx_inner.clone();
                            std::thread::spawn(move || {
                                let r = stream_encrypt_file_to_blocks(
                                    &v_d,
                                    &dek,
                                    &mut next_blk,
                                    &src_path,
                                    fname,
                                    &t_cl,
                                );
                                let _ = t_cl.send(WorkerEvent::AddDone(r));
                            });
                        }
                    }
                }
                *dlg_clear.borrow_mut() = None;
            });
            dialog.show();
            *dlg_slot.borrow_mut() = Some(dialog);
        });
    }

    // Atomic Vacuum Button
    {
        let sess_vac = session_rc.clone();
        let status_vac = status_bar_lbl.clone();
        let tx_vac = tx.clone();
        vacuum_btn.connect_clicked(move |_| {
            let (v_dir, idx_clone) = {
                let borrow = sess_vac.borrow();
                let Some(s) = borrow.as_ref() else {
                    return;
                };
                let dead: u64 = s.index.dead_bytes_per_block.values().sum();
                if dead == 0 {
                    status_vac.set_label("No dead space to vacuum!");
                    return;
                }
                (s.vault_dir.clone(), s.index.clone())
            };

            status_vac.set_label("Starting Atomic Vacuum...");
            let t_cl = tx_vac.clone();
            std::thread::spawn(move || {
                let res = run_atomic_vacuum(&v_dir, idx_clone, &t_cl).map(|(new_idx, old_ids)| {
                    for old_id in old_ids {
                        let _ = fs::remove_file(block_path(&v_dir, old_id));
                    }
                    new_idx
                });
                let _ = t_cl.send(WorkerEvent::VacuumDone(res));
            });
        });
    }

    // Background Worker Event Poller (Updates Progress & Commits Index Changes)
    {
        let sess_poll = session_rc.clone();
        let status_poll = status_bar_lbl.clone();
        let refresh_poll = refresh_vault_ui.clone();

        glib::timeout_add_local(Duration::from_millis(80), move || {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    WorkerEvent::Progress(_pct, msg) => {
                        status_poll.set_label(&msg);
                    }
                    WorkerEvent::AddDone(Ok(new_entry)) => {
                        {
                            let mut borrow = sess_poll.borrow_mut();
                            if let Some(s) = borrow.as_mut() {
                                if let Some(last_seg) = new_entry.segments.last() {
                                    s.index.next_block_id =
                                        s.index.next_block_id.max(last_seg.block_id);
                                }
                                s.index.files.push(new_entry.clone());
                                let _ = save_vault_index(s);
                            }
                        }
                        status_poll.set_label(&format!("Added {} to vault.", new_entry.name));
                        refresh_poll();
                    }
                    WorkerEvent::AddDone(Err(e)) => {
                        status_poll.set_label(&format!("Add failed: {}", e));
                    }
                    WorkerEvent::ReplaceDone(Ok((target_idx, updated_entry))) => {
                        {
                            let mut borrow = sess_poll.borrow_mut();
                            if let Some(s) = borrow.as_mut() {
                                if let Some(last_seg) = updated_entry.segments.last() {
                                    s.index.next_block_id =
                                        s.index.next_block_id.max(last_seg.block_id);
                                }
                                if target_idx < s.index.files.len() {
                                    let old = std::mem::replace(
                                        &mut s.index.files[target_idx],
                                        updated_entry.clone(),
                                    );
                                    for seg in old.segments {
                                        *s.index
                                            .dead_bytes_per_block
                                            .entry(seg.block_id)
                                            .or_insert(0) += seg.cipher_len;
                                    }
                                    let _ = save_vault_index(s);
                                }
                            }
                        }
                        status_poll
                            .set_label(&format!("Replaced {} in vault.", updated_entry.name));
                        refresh_poll();
                    }
                    WorkerEvent::ReplaceDone(Err(e)) => {
                        status_poll.set_label(&format!("Replace failed: {}", e));
                    }
                    WorkerEvent::ExtractDone(Ok((path, open_in_app))) => {
                        if open_in_app {
                            status_poll.set_label(&format!(
                                "Opened in RAM: {}",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            ));
                            if let Ok(uri) = glib::filename_to_uri(&path, None) {
                                let _ = gio::AppInfo::launch_default_for_uri(
                                    &uri,
                                    None::<&gio::AppLaunchContext>,
                                );
                            }
                        } else {
                            status_poll.set_label(&format!("Copied to {}", path.display()));
                        }
                    }
                    WorkerEvent::ExtractDone(Err(e)) => {
                        status_poll.set_label(&format!("Decrypt failed: {}", e));
                    }
                    WorkerEvent::VacuumDone(Ok(compacted_index)) => {
                        {
                            let mut borrow = sess_poll.borrow_mut();
                            if let Some(s) = borrow.as_mut() {
                                s.index = compacted_index;
                                let _ = save_vault_index(s);
                            }
                        }
                        status_poll.set_label("Atomic Vacuum complete! Dead space reclaimed.");
                        refresh_poll();
                    }
                    WorkerEvent::VacuumDone(Err(e)) => {
                        status_poll.set_label(&format!("Vacuum failed: {}", e));
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    root
}
