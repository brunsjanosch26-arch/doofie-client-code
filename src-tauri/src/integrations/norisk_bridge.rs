//! Bridge to a locally installed NoRisk Client launcher.
//!
//! The doofieclient does not bundle the (large) NoRisk mod jars anymore. Instead,
//! it reuses the mods that the *real* NoRisk launcher already has on the user's PC:
//!
//!   * `%APPDATA%\norisk\NoRiskClientV3\norisk_modpacks.json`  – the full pack
//!     definition with per-Minecraft-version compatibility for every mod. This is
//!     merged into the doofie pack config so `doofie-prod` works for every version
//!     NoRisk supports (and self-updates whenever the NoRisk launcher updates it).
//!
//!   * `%APPDATA%\norisk\NoRiskClientV3\meta\mod_cache`  – the already downloaded
//!     mod jars. We copy the newest matching jar from here into the doofie cache
//!     (rebranding NoRisk -> Doofie on the way) so nothing has to be bundled into
//!     the installer and everything works offline for versions the user has played.

use crate::integrations::doofie_packs::NoriskModpacksConfig;
use log::{debug, info, warn};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

/// Root directory of a locally installed NoRisk Client launcher, if present.
/// On Windows this resolves to `%APPDATA%\norisk\NoRiskClientV3`.
pub fn norisk_launcher_root() -> Option<PathBuf> {
    let root = dirs::config_dir()?.join("norisk").join("NoRiskClientV3");
    if root.is_dir() {
        Some(root)
    } else {
        None
    }
}

/// Path to the NoRisk launcher's mod cache, if present.
pub fn norisk_mod_cache_dir() -> Option<PathBuf> {
    let dir = norisk_launcher_root()?.join("meta").join("mod_cache");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// Reads and parses the NoRisk launcher's `norisk_modpacks.json`, if present.
pub fn read_norisk_modpacks() -> Option<NoriskModpacksConfig> {
    let path = norisk_launcher_root()?.join("norisk_modpacks.json");
    let data = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<NoriskModpacksConfig>(&data) {
        Ok(cfg) => {
            info!(
                "[NoriskBridge] Loaded NoRisk modpacks from {:?} ({} packs)",
                path,
                cfg.packs.len()
            );
            Some(cfg)
        }
        Err(e) => {
            warn!("[NoriskBridge] Failed to parse {:?}: {}", path, e);
            None
        }
    }
}

/// Merges the locally installed NoRisk launcher's pack config into the doofie config.
///
/// The NoRisk `norisk-prod` pack (which carries full per-version compatibility for all
/// mods) is mapped onto the doofie pack ids (`doofie-prod`, `doofie-stable`) so that
/// selecting a doofie pack resolves to the full, multi-version mod list. All NoRisk
/// maven repositories are merged in so the mod sources resolve, and the remaining
/// NoRisk packs are added under their own ids without clobbering existing doofie packs.
pub fn merge_into(config: &mut NoriskModpacksConfig) {
    let nr = match read_norisk_modpacks() {
        Some(c) => c,
        None => {
            debug!("[NoriskBridge] No local NoRisk launcher found; keeping doofie config as-is.");
            return;
        }
    };

    // Merge repositories first (NoRisk wins) so maven sources like `noriskproduction` resolve.
    for (k, v) in &nr.repositories {
        config.repositories.insert(k.clone(), v.clone());
    }

    // Map the NoRisk source pack onto the doofie pack ids, preserving doofie branding + assets.
    if let Some(src) = nr.packs.get("norisk-prod").cloned() {
        for target in ["doofie-prod", "doofie-stable"] {
            let mut def = src.clone();
            if let Some(existing) = config.packs.get(target) {
                def.display_name = existing.display_name.clone();
                def.description = existing.description.clone();
                def.assets = existing.assets.clone();
            }
            info!(
                "[NoriskBridge] Mapped NoRisk 'norisk-prod' ({} mods) onto '{}'",
                def.mods.len(),
                target
            );
            config.packs.insert(target.to_string(), def);
        }
    } else {
        warn!("[NoriskBridge] NoRisk config has no 'norisk-prod' pack; nothing mapped.");
    }

    // Carry over all other NoRisk packs without overwriting existing doofie packs.
    for (id, def) in nr.packs {
        config.packs.entry(id).or_insert(def);
    }
}

/// Finds the newest jar in the NoRisk mod cache that matches a given mod artifact,
/// loader and Minecraft version, regardless of the exact build number.
///
/// NoRisk cache filenames look like `nrc-client-26.2.5992545-working+fabric.1.21.11.jar`,
/// i.e. `<artifactId>-<build>+<loader>.<mcVersion>.jar`. We match on the stable
/// `<artifactId>-` prefix and the `+<loader>.<mcVersion>.jar` suffix so a slightly older
/// cached build is still used (the JSON often points at a newer build than what is cached).
pub fn find_cached_jar(artifact_id: &str, loader: &str, mc_version: &str) -> Option<PathBuf> {
    let cache = norisk_mod_cache_dir()?;
    let prefix = format!("{}-", artifact_id);
    let suffix = format!("+{}.{}.jar", loader, mc_version);

    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&cache).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) || !name.ends_with(&suffix) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map_or(true, |(t, _)| mtime > *t) {
            best = Some((mtime, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// Length-preserving NoRisk -> Doofie branding replacements for jar contents.
/// Each replacement keeps the exact byte length so zip entry sizes stay valid.
fn brand_replacements() -> Vec<(&'static [u8], Vec<u8>)> {
    // (needle, replacement) — replacement is padded/truncated to needle length below.
    let raw: [(&'static [u8], &'static [u8]); 8] = [
        (&b"NoRisk \xe2\x9a\xa1 Client"[..], &b"Doofie Client"[..]),
        (&b"NORISK \xe2\x9a\xa1 CLIENT"[..], &b"DOOFIE CLIENT"[..]),
        (&b"NoRisk \xe2\x9a\xa1"[..], &b"Doofie"[..]),
        (&b"NoRisk Client"[..], &b"Doofie Client"[..]),
        (&b"NORISK CLIENT"[..], &b"DOOFIE CLIENT"[..]),
        (&b"norisk client"[..], &b"doofie client"[..]),
        (&b"NoRiskClient"[..], &b"DoofieClient"[..]),
        (&b"norisk-client"[..], &b"doofie-client"[..]),
    ];
    raw.iter()
        .map(|(needle, repl)| {
            let mut r = repl.to_vec();
            if r.len() < needle.len() {
                r.resize(needle.len(), b' ');
            } else {
                r.truncate(needle.len());
            }
            (*needle, r)
        })
        .collect()
}

/// Applies the branding replacements to a byte buffer in place.
fn patch_branding_bytes(data: &[u8], repls: &[(&'static [u8], Vec<u8>)]) -> Vec<u8> {
    let mut out = data.to_vec();
    for (needle, repl) in repls {
        if out.windows(needle.len()).any(|w| w == *needle) {
            out = replace_all(&out, needle, repl);
        }
    }
    out
}

fn replace_all(haystack: &[u8], needle: &[u8], repl: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if i + needle.len() <= haystack.len() && &haystack[i..i + needle.len()] == needle {
            out.extend_from_slice(repl);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// Rewrites a jar in place, applying the NoRisk -> Doofie branding string patch to every
/// entry. Replacements are length-preserving, so this only touches branding text/bytecode
/// constants and never changes functionality. No-op (Ok) if the file is not a valid zip.
pub fn rebrand_jar_file(path: &Path) -> std::io::Result<()> {
    let data = std::fs::read(path)?;
    let mut archive = match zip::ZipArchive::new(Cursor::new(&data)) {
        Ok(a) => a,
        Err(_) => return Ok(()), // not a zip / unreadable — leave untouched
    };

    let repls = brand_replacements();
    let mut out_buf = Vec::with_capacity(data.len());
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut out_buf));
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();
            if entry.is_dir() {
                let opts = zip::write::SimpleFileOptions::default();
                writer.add_directory(name, opts)?;
                continue;
            }
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf)?;
            let patched = patch_branding_bytes(&buf, &repls);

            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(entry.compression())
                .unix_permissions(entry.unix_mode().unwrap_or(0o644));
            writer.start_file(name, opts)?;
            writer.write_all(&patched)?;
        }
        writer.finish()?;
    }

    std::fs::write(path, out_buf)?;
    Ok(())
}

/// Provides a mod jar for `dest_path` by copying the newest matching jar from the local
/// NoRisk mod cache (rebranding if requested). Returns `true` if a jar was provided.
/// Runs blocking file IO; call from a blocking context (e.g. `spawn_blocking`).
pub fn provide_from_cache(
    artifact_id: &str,
    loader: &str,
    mc_version: &str,
    dest_path: &Path,
    rebrand: bool,
) -> bool {
    let src = match find_cached_jar(artifact_id, loader, mc_version) {
        Some(p) => p,
        None => return false,
    };
    if let Some(parent) = dest_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::copy(&src, dest_path) {
        warn!(
            "[NoriskBridge] Failed to copy {:?} -> {:?}: {}",
            src, dest_path, e
        );
        return false;
    }
    info!(
        "[NoriskBridge] Provided '{}' for MC {}/{} from NoRisk cache: {:?}",
        artifact_id, mc_version, loader, src
    );
    if rebrand {
        if let Err(e) = rebrand_jar_file(dest_path) {
            warn!("[NoriskBridge] Rebrand failed for {:?}: {}", dest_path, e);
        }
    }
    true
}
