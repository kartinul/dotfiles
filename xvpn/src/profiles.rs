//! Named vless configs and which one is active.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{self, read_json, write_secret};
use crate::Result;

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub path: PathBuf,
    pub server: Option<String>,
    /// Creation time, the basis of the order `xvpn profile use 1` refers to.
    pub created: u64,
}

/// Seconds since the epoch, 0 when the mtime is unavailable.
fn mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Profile {
    fn load(name: &str, path: PathBuf) -> Result<Self> {
        let cfg = read_json(&path)?;
        Ok(Profile {
            created: mtime(&path),
            name: name.to_string(),
            path,
            server: config::server_of(&cfg).map(str::to_string),
        })
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err("profile name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err("profile name is too long (max 64)".into());
    }
    if name.starts_with('.') {
        return Err("profile name cannot start with '.'".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "invalid profile name '{name}': use letters, digits, '-', '_' and '.'"
        ));
    }
    Ok(())
}

pub fn path_for(root: &Path, name: &str) -> PathBuf {
    config::profiles_dir(root).join(format!("{name}.json"))
}

pub fn exists(root: &Path, name: &str) -> bool {
    path_for(root, name).is_file()
}

pub fn list(root: &Path) -> Vec<Profile> {
    let Ok(entries) = fs::read_dir(config::profiles_dir(root)) else {
        return Vec::new();
    };
    let mut out: Vec<Profile> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "json" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            Some(Profile::load(&name, path.clone()).unwrap_or(Profile {
                created: mtime(&path),
                name,
                path,
                server: None,
            }))
        })
        .collect();
    // Creation order, so `xvpn profile use 1` means the same profile until
    // something is added or removed. Name is the tiebreak: readdir order is
    // arbitrary and two profiles can share an mtime to the second.
    out.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Resolve a profile reference to its name.
///
/// A reference is a name or a `1`-based position, where positions are the
/// order `list` prints: 1 is the first created. An exact name match wins over
/// a position, so a profile actually named `1` stays reachable.
///
/// Positions shift when profiles are added or removed, which is why a name is
/// the better thing to script with.
pub fn resolve(root: &Path, reference: &str) -> Result<String> {
    if exists(root, reference) {
        return Ok(reference.to_string());
    }
    let profiles = list(root);
    if let Some(index) = reference.parse::<usize>().ok().filter(|n| *n >= 1) {
        return profiles
            .get(index - 1)
            .map(|p| p.name.clone())
            .ok_or_else(|| {
                format!(
                    "no profile {index} (have {}; see `xvpn profile list`)",
                    profiles.len()
                )
            });
    }
    // A bare number past the end is far more likely a typo in a name.
    Err(format!(
        "no such profile: {reference}\n\
         hint: use the name, or its position from `xvpn profile list`"
    ))
}

pub fn active_name(root: &Path) -> Option<String> {
    let name = fs::read_to_string(config::active_file(root)).ok()?;
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn ensure_profiles_dir(root: &Path) -> Result<()> {
    fs::create_dir_all(config::profiles_dir(root))
        .map_err(|e| format!("creating profiles dir: {e}"))
}

fn store(root: &Path, name: &str, config: &Value) -> Result<()> {
    ensure_profiles_dir(root)?;
    write_secret(&path_for(root, name), config)
}

// Validates on a temp file first, so a bad link cannot leave a broken profile
// behind. The `.json` suffix matters: xray picks its parser from the extension.
fn store_checked(root: &Path, name: &str, config: &Value) -> Result<()> {
    ensure_profiles_dir(root)?;
    let tmp = config::profiles_dir(root).join(format!(".tmp-{name}.json"));
    write_secret(&tmp, config)?;
    if let Err(e) = config::check_xray(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path_for(root, name))
        .map_err(|e| format!("{}: {e}", path_for(root, name).display()))
}

/// Point `active` at `reference`, which may be a name or a `1`-based position.
pub fn activate(root: &Path, reference: &str) -> Result<()> {
    let name = resolve(root, reference)?;
    if !exists(root, &name) {
        return Err(format!("no such profile: {name}"));
    }
    fs::write(config::active_file(root), format!("{name}\n"))
        .map_err(|e| format!("writing active profile: {e}"))?;
    materialize(root)
}

// xray.json stays as a copy so the supervisor has one obvious file to read.
fn materialize(root: &Path) -> Result<()> {
    let name = active_name(root).ok_or("no active profile (run `xvpn profile list`)")?;
    let cfg = read_json(&path_for(root, &name))?;
    write_secret(&root.join(config::XRAY), &cfg)
}

pub fn resolve_active(root: &Path) -> Result<(String, PathBuf)> {
    if let Some(name) = active_name(root) {
        let path = path_for(root, &name);
        if path.is_file() {
            return Ok((name, path));
        }
        return Err(format!(
            "active profile '{name}' is missing {}",
            path.display()
        ));
    }
    let legacy = root.join(config::XRAY);
    legacy
        .is_file()
        .then(|| ("default".to_string(), legacy.clone()))
        .ok_or_else(|| "no VPN configured (run `xvpn import`)".to_string())
}

pub fn active_config(root: &Path) -> Result<(String, Value)> {
    let (name, path) = resolve_active(root)?;
    let cfg = read_json(&path)?;
    Ok((name, cfg))
}

pub fn validate(root: &Path, name: &str) -> Result<()> {
    let path = path_for(root, name);
    if !path.is_file() {
        return Err(format!("no such profile: {name}"));
    }
    config::check_xray(&path)
}

pub fn remove(root: &Path, name: &str) -> Result<()> {
    let path = path_for(root, name);
    if !path.is_file() {
        return Err(format!("no such profile: {name}"));
    }
    fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if active_name(root).as_deref() == Some(name) {
        match list(root).first() {
            Some(next) => activate(root, &next.name)?,
            None => {
                fs::remove_file(config::active_file(root)).ok();
                fs::remove_file(root.join(config::XRAY)).ok();
            }
        }
    }
    Ok(())
}

// Only when profiles/ is absent: activate keeps writing xray.json, so keying
// this on that file alone would migrate on every command.
pub fn migrate_legacy(root: &Path) -> Result<Option<String>> {
    if config::profiles_dir(root).is_dir() || exists(root, "default") {
        return Ok(None);
    }
    let legacy = root.join(config::XRAY);
    if !legacy.is_file() {
        return Ok(None);
    }
    let cfg = read_json(&legacy)?;
    store(root, "default", &cfg)?;
    if let Err(e) = validate(root, "default") {
        eprintln!("warning: existing config did not validate: {e}");
    }
    activate(root, "default")?;
    Ok(Some("default".to_string()))
}

pub fn import(root: &Path, name: Option<&str>, link: &str, force: bool) -> Result<String> {
    let parsed = config::parse_link(link)?;
    let cfg = config::xray_config(&parsed);

    let name = match name {
        Some(n) => n.to_string(),
        None => match active_name(root) {
            Some(n) => n,
            None => slug_of(&parsed.host),
        },
    };
    validate_name(&name)?;

    let path = path_for(root, &name);
    if path.exists() && !force {
        return Err(format!(
            "profile '{name}' already exists (use --force, or `xvpn profile use {name}`)"
        ));
    }
    store_checked(root, &name, &cfg)?;
    activate(root, &name)?;
    Ok(name)
}

fn slug_of(host: &str) -> String {
    let slug: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_ascii_lowercase();
    if slug.is_empty() {
        "default".to_string()
    } else {
        slug
    }
}

pub fn print_list(root: &Path) -> Result<()> {
    let profiles = list(root);
    let active = active_name(root);
    if profiles.is_empty() {
        println!("no profiles yet — add one with `xvpn import 'vless://...'`");
        return Ok(());
    }
    let width = profiles
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    // Numbered in creation order so a position can be used instead of a name.
    for (i, p) in profiles.into_iter().enumerate() {
        let marker = if active.as_deref() == Some(p.name.as_str()) {
            '*'
        } else {
            ' '
        };
        println!(
            "{marker} {:>2}. {:width$}  {}",
            i + 1,
            p.name,
            p.server.as_deref().unwrap_or("(unreadable)"),
            width = width
        );
    }
    Ok(())
}
