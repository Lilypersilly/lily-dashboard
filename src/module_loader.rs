//! Modular drop-in loader, SHA-256 trust verifier, `Cargo.toml` Dashpendencies merger,
//! and `src/modules_gen.rs` code generator.
//!
//! When the user clicks **Reload Dashboard**:
//! 1. Scans `<repo>/modules/*.rs`.
//! 2. Computes SHA-256 of every `.rs` file and compares against `OFFICIAL_MODULE_HASHES`
//!    and `~/.config/lily-dashboard/trusted_modules.json`.
//! 3. If any untrusted or modified third-party module is found, the caller presents a
//!    confirmation dialog requiring the user to type `I am not an idiot.` before compiling.
//! 4. Once trusted, generates `src/modules_gen.rs`, merges `// Dashpendencies:` into the
//!    fenced block of `Cargo.toml`, and builds (rolling back on compile failure).

use gtk4::glib;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIRM_PHRASE: &str = "I am not an idiot.";
pub const CONFIRM_PHRASE_PERMANENT: &str = "I am not an idiot. I am smart!";
const TRUST_ALL_KEY: &str = "__trust_all__";
const DEP_START: &str = "# --- BEGIN MODULE DEPENDENCIES (managed by lily-dashboard) ---";
const DEP_END: &str = "# --- END MODULE DEPENDENCIES ---";

/// Core dependencies already present in Cargo.toml that modules must not duplicate.
const CORE_CRATES: &[&str] = &[
    "gtk4",
    "gtk4-layer-shell",
    "serde",
    "serde_json",
    "sha2",
];

/// Built-in SHA-256 safe list for official authored modules shipped with lily-dashboard.
/// Populated for each release; any third-party or modified module triggers the trust gate.
pub const OFFICIAL_MODULE_HASHES: &[(&str, &str)] = &[
    ("calendar.rs", "c4ca163b679e95358df5506de473ee06e2abd9fda61a660c4255e41e9e0e20ff"),
    ("clipboard.rs", "8d40884f49b7240a5a8c7621141356de2f196d9146b04c8cfee4f8e7f963d4e3"),
    ("drive.rs", "b33efb89afcc39cbcba51a55838ffbd3cdcb636570c904dc2debd2463354e00b"),
    ("favorites.rs", "72b05148295f1222d134faf34e129e1e4e43e661861aa8bdc4e18eb916b85d53"),
    ("launcher.rs", "dc1bb2f1e58ea96e46b6a6fbd60fa894013079f178aa6c8abfc5fe7a6a128a4c"),
    ("locker.rs", "4cee3389b7c0d76fda7094faf1ce95ff0cb5ff9acfef8161eb4de2d45c66abdc"),
    ("music.rs", "5deb0fb7561b01ba6273a3ba059e6cdbb083468e4986911ffe42b5ac28e63da0"),
    ("passwords.rs", "bc367dccb91b44197cd3e0ef4f5d37fb7f954d8840c102713107d6bbec610070"),
    ("plans.rs", "48caab6f214a96d8745a09e723f029089e4085fcf0dee4cb189d210385c7e3be"),
    ("scicalc.rs", "19fbcb7574e64ec0909b68e9b6d672415dd02ce83f4589f5ab7abb14c18657b1"),
    ("services.rs", "a9a65ca8e7a2084bf3e9f8cd534495ba23d872e7bab4e43128b6d0e0486173c9"),
    ("system.rs", "c0fa8c54e2bb367f6d5d5e9e6a34fb88063300f06cc9b7cd6d4988e529a32877"),
    ("tasks.rs", "f5b54095008b3c3038995680c70273108d05a23c5347632412a6e7770903f380"),
    ("timers.rs", "808bc9651210c6058886c490b04ae85c962cc5e6e133f14b924db690531162c1"),
    ("usb_drives.rs", "c25e7f109678498ffbbf3e2f561f40e8c57a2693d0007c66cb0155882cb6d390"),
];

#[derive(Clone, Debug)]
pub struct ScannedModule {
    pub file_name: String,
    pub mod_ident: String,
    pub id: String,
    pub title: String,
    pub lifecycle: String,
    pub has_init: bool,
    pub has_shutdown: bool,
    pub has_visibility: bool,
    pub dashpendencies: Vec<(String, String)>,
    pub sha256: String,
    #[allow(dead_code)]
    pub is_official: bool,
    pub is_trusted: bool,
}

#[derive(Clone, Debug)]
pub struct ModuleScanResult {
    pub modules: Vec<ScannedModule>,
    pub untrusted: Vec<ScannedModule>,
}

pub struct SyncBackup {
    cargo_toml: String,
    modules_gen_rs: Option<String>,
}

fn trusted_file_path() -> PathBuf {
    let dir = glib::user_config_dir().join("lily-dashboard");
    let _ = fs::create_dir_all(&dir);
    dir.join("trusted_modules.json")
}

fn load_trusted_hashes() -> HashMap<String, String> {
    fs::read_to_string(trusted_file_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn trust_modules(mods: &[ScannedModule]) {
    let mut map = load_trusted_hashes();
    for m in mods {
        map.insert(m.file_name.clone(), m.sha256.clone());
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = fs::write(trusted_file_path(), json);
    }
}

pub fn trust_all_permanently() {
    let mut map = load_trusted_hashes();
    map.insert(TRUST_ALL_KEY.to_string(), "true".to_string());
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = fs::write(trusted_file_path(), json);
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn sanitize_ident(stem: &str) -> String {
    let mut out = String::new();
    for (i, c) in stem.chars().enumerate() {
        let ch = if c.is_ascii_alphanumeric() || c == '_' {
            c.to_ascii_lowercase()
        } else {
            '_'
        };
        if i == 0 && ch.is_ascii_digit() {
            out.push('_');
        }
        out.push(ch);
    }
    if out.is_empty() {
        "custom_mod".to_string()
    } else {
        out
    }
}

fn title_case(stem: &str) -> String {
    stem.split(|c| c == '_' || c == '-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses a `// Dashpendencies: ...` line into `(crate_name, toml_value)` pairs.
/// Supports:
/// - `reqwest = "0.12", chrono = "0.4"`
/// - `souvlaki = { version = "0.7", default-features = false, features = ["use_zbus"] }`
/// - Shorthand: `notify 6.1` -> `("notify", "\"6.1\"")`
fn parse_dashpendencies_line(raw: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    // Split on top-level commas (ignoring commas inside `{ ... }` or `[ ... ]`)
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = raw.as_bytes();
    for i in 0..=bytes.len() {
        let at_end = i == bytes.len();
        if !at_end {
            match bytes[i] {
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth = (depth - 1).max(0),
                _ => {}
            }
        }
        if at_end || (bytes[i] == b',' && depth == 0) {
            let chunk = raw[start..i].trim();
            start = i + 1;
            if chunk.is_empty() {
                continue;
            }
            if let Some((name, val)) = chunk.split_once('=') {
                let n = name.trim();
                let v = val.trim();
                if !n.is_empty() && !v.is_empty() {
                    let formatted = if v.starts_with('"') || v.starts_with('{') {
                        v.to_string()
                    } else {
                        format!("\"{}\"", v.trim_matches('"'))
                    };
                    items.push((n.to_string(), formatted));
                }
            } else {
                // Shorthand: `notify 6.1`
                let mut parts = chunk.split_whitespace();
                if let (Some(n), Some(v)) = (parts.next(), parts.next()) {
                    let formatted = if v.starts_with('"') || v.starts_with('{') {
                        v.to_string()
                    } else {
                        format!("\"{}\"", v.trim_matches('"'))
                    };
                    items.push((n.to_string(), formatted));
                }
            }
        }
    }
    items
}

pub fn scan_modules(repo_path: &Path) -> ModuleScanResult {
    let mod_dir = repo_path.join("modules");
    let trusted_map = load_trusted_hashes();
    let trust_all = trusted_map
        .get(TRUST_ALL_KEY)
        .map(|v| v == "true")
        .unwrap_or(false);
    let mut modules = Vec::new();

    if let Ok(rd) = fs::read_dir(&mod_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Some(file_name) = path.file_name().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            if file_name.starts_with('.') || file_name == "mod.rs" {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let sha256 = sha256_hex(&bytes);
            let content = String::from_utf8_lossy(&bytes);
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "mod".to_string());

            let mut id = sanitize_ident(&stem);
            let mut title = title_case(&stem);
            let mut lifecycle = "stateful".to_string();
            let mut has_init = false;
            let mut has_shutdown = false;
            let mut has_visibility = false;
            let mut dashpendencies = Vec::new();

            // Only scan top 40 lines for metadata header comments
            for line in content.lines().take(40) {
                let t = line.trim();
                let Some(comment) = t.strip_prefix("//") else {
                    continue;
                };
                let c = comment.trim();
                if let Some(v) = c.strip_prefix("@dash-id:") {
                    let s = v.trim();
                    if !s.is_empty() {
                        id = s.to_string();
                    }
                } else if let Some(v) = c.strip_prefix("@dash-title:") {
                    let s = v.trim();
                    if !s.is_empty() {
                        title = s.to_string();
                    }
                } else if let Some(v) = c.strip_prefix("@dash-lifecycle:") {
                    let s = v.trim().to_lowercase();
                    if matches!(s.as_str(), "stateless" | "stateful" | "visible_only") {
                        lifecycle = s;
                    }
                } else if let Some(v) = c.strip_prefix("@dash-hooks:") {
                    for h in v.split(',').map(|x| x.trim().to_lowercase()) {
                        match h.as_str() {
                            "init" => has_init = true,
                            "shutdown" => has_shutdown = true,
                            "visibility" => has_visibility = true,
                            _ => {}
                        }
                    }
                } else if let Some(v) = c
                    .strip_prefix("Dashpendencies:")
                    .or_else(|| c.strip_prefix("dashpendencies:"))
                    .or_else(|| c.strip_prefix("@dash-deps:"))
                {
                    dashpendencies.extend(parse_dashpendencies_line(v));
                }
            }

            let is_official = OFFICIAL_MODULE_HASHES
                .iter()
                .any(|&(f, h)| f == file_name && h == sha256);
            let is_user_trusted = trusted_map
                .get(&file_name)
                .map(|h| h == &sha256)
                .unwrap_or(false);
            let is_trusted = trust_all || is_official || is_user_trusted;

            modules.push(ScannedModule {
                file_name,
                mod_ident: sanitize_ident(&stem),
                id,
                title,
                lifecycle,
                has_init,
                has_shutdown,
                has_visibility,
                dashpendencies,
                sha256,
                is_official,
                is_trusted,
            });
        }
    }

    modules.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    let untrusted = modules.iter().filter(|m| !m.is_trusted).cloned().collect();
    ModuleScanResult { modules, untrusted }
}

/// Writes `src/modules_gen.rs` and updates the fenced module dependencies block in `Cargo.toml`.
/// Returns a `SyncBackup` so the caller can restore both files if `cargo build` fails.
pub fn sync_modules_to_repo(
    repo_path: &Path,
    modules: &[ScannedModule],
) -> Result<SyncBackup, String> {
    let cargo_path = repo_path.join("Cargo.toml");
    let gen_path = repo_path.join("src").join("modules_gen.rs");

    let old_cargo = fs::read_to_string(&cargo_path)
        .map_err(|e| format!("Failed to read Cargo.toml: {}", e))?;
    let old_gen = fs::read_to_string(&gen_path).ok();

    // 1. Generate src/modules_gen.rs
    let mut gen = String::from(
        "// AUTO-GENERATED by lily-dashboard module loader from `modules/*.rs`.\n\
         // Do not edit by hand — drop `.rs` files in `modules/` and press Reload Dashboard.\n\n\
         use crate::module_api::{ModuleDescriptor, ModuleLifecycle};\n\n",
    );

    for m in modules {
        gen.push_str(&format!(
            "#[path = \"../modules/{}\"]\npub mod {};\n",
            m.file_name, m.mod_ident
        ));
    }

    gen.push_str("\npub fn all_modules() -> Vec<ModuleDescriptor> {\n    vec![\n");
    for m in modules {
        let lc = match m.lifecycle.as_str() {
            "stateless" => "ModuleLifecycle::Stateless",
            "visible_only" => "ModuleLifecycle::VisibleOnly",
            _ => "ModuleLifecycle::Stateful",
        };
        let init_expr = if m.has_init {
            format!("Some({}::init)", m.mod_ident)
        } else {
            "None".to_string()
        };
        let shut_expr = if m.has_shutdown {
            format!("Some({}::shutdown)", m.mod_ident)
        } else {
            "None".to_string()
        };
        let vis_expr = if m.has_visibility {
            format!("Some({}::set_enabled)", m.mod_ident)
        } else {
            "None".to_string()
        };
        gen.push_str(&format!(
            "        ModuleDescriptor {{\n\
             \x20           id: {:?},\n\
             \x20           title: {:?},\n\
             \x20           lifecycle: {},\n\
             \x20           build: {}::build,\n\
             \x20           init: {},\n\
             \x20           shutdown: {},\n\
             \x20           on_visibility: {},\n\
             \x20       }},\n",
            m.id, m.title, lc, m.mod_ident, init_expr, shut_expr, vis_expr
        ));
    }
    gen.push_str("    ]\n}\n");

    fs::write(&gen_path, &gen)
        .map_err(|e| format!("Failed to write src/modules_gen.rs: {}", e))?;

    // 2. Merge Dashpendencies into Cargo.toml fenced block
    let mut deps: BTreeMap<String, String> = BTreeMap::new();
    for m in modules {
        for (k, v) in &m.dashpendencies {
            if !CORE_CRATES.contains(&k.as_str()) {
                deps.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }

    let mut dep_block = String::new();
    dep_block.push_str(DEP_START);
    dep_block.push('\n');
    for (k, v) in &deps {
        dep_block.push_str(&format!("{} = {}\n", k, v));
    }
    dep_block.push_str(DEP_END);

    let new_cargo = if let (Some(s_idx), Some(e_idx)) = (old_cargo.find(DEP_START), old_cargo.find(DEP_END)) {
        let end_after = e_idx + DEP_END.len();
        format!("{}{}{}", &old_cargo[..s_idx], dep_block, &old_cargo[end_after..])
    } else {
        format!("{}\n{}\n", old_cargo.trim_end(), dep_block)
    };

    if new_cargo != old_cargo {
        fs::write(&cargo_path, &new_cargo)
            .map_err(|e| format!("Failed to update Cargo.toml: {}", e))?;
    }

    Ok(SyncBackup {
        cargo_toml: old_cargo,
        modules_gen_rs: old_gen,
    })
}

pub fn restore_backup(repo_path: &Path, backup: &SyncBackup) {
    let _ = fs::write(repo_path.join("Cargo.toml"), &backup.cargo_toml);
    let gen_path = repo_path.join("src").join("modules_gen.rs");
    if let Some(ref prev) = backup.modules_gen_rs {
        let _ = fs::write(&gen_path, prev);
    }
}
