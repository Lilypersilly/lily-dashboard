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
    ("clipboard.rs", "1992902480c74f3648c01dab361d779a34ec6c3b4d2eb1bc66caace9f1020a0a"),
    ("drive.rs", "b33efb89afcc39cbcba51a55838ffbd3cdcb636570c904dc2debd2463354e00b"),
    ("favorites.rs", "78c57a211add6ffbee27066fe537465b5611acdb12962f4e2bc57aa33be26e6c"),
    ("launcher.rs", "65c79507384f40165402078b2c57bf4f5da19e7330f7daaa99755944eb7b733c"),
    ("locker.rs", "d575676a468bcfad8ad9ff2b99c5d961c0f4bc827456e14a719de98f0c39010d"),
    ("music.rs", "e723ecbc40be12112a8536c6b3a6cd0cf900d66fa2a8930c8ccb6c987a890421"),
    ("passwords.rs", "f34cdb09d298730a5476001d67247d6ffefcde5236980b7ba4785469a04d44ac"),
    ("plans.rs", "bd62245632ee80187a433ad6e430a87ae1d69bae0899d95c279d5d8fb58ce28f"),
    ("system.rs", "3d670e3b5eb0339bca70eb30b429622a396c98f85c1273e6308ef6b5bf8d8e40"),
    ("timers.rs", "ec4eea1f912b976e080a65f6656c661dbe94328206e1e833952a29fe106d93d3"),
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
            let is_trusted = is_official || is_user_trusted;

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
