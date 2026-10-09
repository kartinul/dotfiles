//! Named vless configs and which one is active.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{self, read_json, write_secret};
use crate::output as out;
use crate::Result;

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub path: PathBuf,
    pub server: Option<String>,
}

impl Profile {
    fn load(name: &str, path: PathBuf) -> Result<Self> {
        let cfg = read_json(&path)?;
        Ok(Profile {
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

/// Profiles in display order.
///
/// The order lives in the `order` file, one name per line, so it does not move
/// when a profile is rewritten. Names absent from the file are appended in
/// alphabetical order, which keeps a hand-dropped profile visible; names in the
/// file with no profile behind them are dropped, so a stale line cannot block
/// the positions after it.
pub fn list(root: &Path) -> Vec<Profile> {
    let on_disk = disk_names(root);
    if on_disk.is_empty() {
        return Vec::new();
    }

    let recorded = read_order(root);
    let seeding = recorded.is_empty();
    // read_order already drops duplicates and blanks, so the only filtering
    // left is dropping names whose profile is gone.
    let mut ordered: Vec<String> = recorded
        .iter()
        .filter(|n| on_disk.contains(n))
        .cloned()
        .collect();

    let mut missing: Vec<String> = on_disk
        .into_iter()
        .filter(|n| !ordered.contains(n))
        .collect();
    missing.sort();
    ordered.extend(missing);

    // Seed the file on first use so the order is explicit from then on.
    if seeding {
        let _ = write_order(root, &ordered);
    }

    ordered
        .into_iter()
        .map(|name| {
            let path = path_for(root, &name);
            Profile::load(&name, path.clone()).unwrap_or(Profile {
                name,
                path,
                server: None,
            })
        })
        .collect()
}

/// Profile names present in `profiles/`.
fn disk_names(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(config::profiles_dir(root)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "json" {
                return None;
            }
            path.file_stem()?.to_str().map(str::to_string)
        })
        .collect()
}

/// The recorded order, empty when there is no file.
pub fn read_order(root: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(config::order_file(root)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for line in content.lines() {
        let name = line.trim();
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn write_order(root: &Path, names: &[String]) -> Result<()> {
    let content = if names.is_empty() {
        String::new()
    } else {
        format!("{}\n", names.join("\n"))
    };
    fs::write(config::order_file(root), content).map_err(|e| format!("order: {e}"))
}

/// Record `name`, keeping an existing position so `--force` does not move it.
fn record(root: &Path, name: &str) -> Result<()> {
    let mut names = read_order(root);
    if !names.iter().any(|n| n == name) {
        names.push(name.to_string());
    }
    write_order(root, &names)
}

/// Forget `name`, so removing a profile does not leave a stale line.
fn forget(root: &Path, name: &str) -> Result<()> {
    let names: Vec<String> = read_order(root).into_iter().filter(|n| n != name).collect();
    write_order(root, &names)
}

/// Resolve a profile reference to its name.
///
/// A reference is a name or a `1`-based position, where positions come from the
/// `order` file. An exact name match wins over a position, so a profile
/// actually named `1` stays reachable.
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
    forget(root, name)?;
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
    record(root, "default")?;
    activate(root, "default")?;
    Ok(Some("default".to_string()))
}

pub fn import(root: &Path, name: Option<&str>, link: &str, force: bool) -> Result<String> {
    let parsed = config::parse_link(link)?;
    let cfg = config::xray_config(&parsed, config::SOCKS_PORT, config::HTTP_PORT);

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
    // After storing: `activate` resolves through `list`, so recording first is
    // what puts a brand-new profile in the order at all.
    record(root, &name)?;
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
        out::status("→", "no profiles yet. add one with `xvpn import 'vless://...'`");
        return Ok(());
    }

    // Prepare data for the table
    let header = vec!["", "ID", "name", "server"];

    // We need to store the owned strings so they live long enough for the table call
    let mut storage: Vec<Vec<String>> = Vec::new();
    storage.push(header.iter().map(|s| s.to_string()).collect());

    for (i, p) in profiles.iter().enumerate() {
        let marker = if active.as_deref() == Some(p.name.as_str()) {
            "*"
        } else {
            ""
        };
        let index = (i + 1).to_string();
        let name = p.name.clone();
        let server = p.server.clone().unwrap_or_else(|| "(unreadable)".to_string());

        storage.push(vec![marker.to_string(), index, name, server]);
    }

    let rows: Vec<Vec<&str>> = storage
        .iter()
        .map(|row| row.iter().map(|s| s.as_str()).collect())
        .collect();

    out::table(&rows, true);
    Ok(())
}
