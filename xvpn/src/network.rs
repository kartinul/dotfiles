//! Platform lookups, registered networks, and selective-mode app/site rules.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::config::{read_json, write_json};
use crate::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
}

pub fn os() -> Os {
    if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else {
        panic!("xvpn supports macOS and Linux")
    }
}

pub fn os_name() -> &'static str {
    match os() {
        Os::MacOs => "macOS",
        Os::Linux => "Linux",
    }
}

pub fn supports_gateway() -> bool {
    os() == Os::Linux
}

/// The default gateway's address, if one is configured.
pub fn router() -> Option<String> {
    imp::router()
}

/// Resolvers configured for the current network.
pub fn dns_servers() -> Vec<String> {
    imp::active_device()
        .map(|dev| imp::dns_servers(&dev))
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
mod imp {
    fn capture(prog: &str, args: &[&str]) -> Option<String> {
        use crate::config::output;
        let out = output(prog, args).ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn wifi_device() -> Option<String> {
        let out = capture("networksetup", &["-listallhardwareports"])?;
        let mut lines = out.lines();
        while let Some(line) = lines.next() {
            if line.contains("Wi-Fi") || line.contains("AirPort") {
                let dev = lines
                    .next()?
                    .split_once(':')
                    .map(|(_, dev)| dev.trim())
                    .unwrap_or_default();
                if !dev.is_empty() {
                    return Some(dev.to_string());
                }
            }
        }
        None
    }

    pub fn dns_servers(dev: &str) -> Vec<String> {
        let Some(out) = capture("ipconfig", &["getoption", dev, "domain_name_server"]) else {
            return Vec::new();
        };
        out.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && super::prefix4(l).is_some())
            .map(str::to_string)
            .collect()
    }

    pub fn active_device() -> Option<String> {
        if let Some(out) = capture("route", &["-n", "get", "default"]) {
            for line in out.lines() {
                if let Some(iface) = line.trim().strip_prefix("interface:") {
                    let iface = iface.trim();
                    if !iface.is_empty() && !iface.starts_with("utun") {
                        return Some(iface.to_string());
                    }
                }
            }
        }
        wifi_device()
    }

    pub fn router() -> Option<String> {
        let dev = active_device()?;
        capture("ipconfig", &["getoption", &dev, "router"])
            .map(|out| out.trim().to_string())
            .filter(|s| super::prefix4(s).is_some())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::parse_proc_route;
    use std::fs;

    fn wifi_devices() -> Vec<String> {
        let Ok(entries) = fs::read_dir("/sys/class/net") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.path().join("wireless").is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    }

    /// Resolvers from `/etc/resolv.conf`, which is where a Linux DHCP lease lands.
    pub fn active_device() -> Option<String> {
        wifi_devices().into_iter().next()
    }

    pub fn dns_servers(_dev: &str) -> Vec<String> {
        let Ok(text) = fs::read_to_string("/etc/resolv.conf") else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("nameserver "))
            .map(str::trim)
            // `127.0.0.53` is systemd-resolved's stub, present on every host and
            // identical everywhere: it says nothing about the network. Loopback
            // in general is local, so a resolver there would register this machine
            // as a network — which is not a thing.
            .filter(|addr| !super::is_loopback(addr) && super::prefix4(addr).is_some())
            .map(str::to_string)
            .collect()
    }

    pub fn router() -> Option<String> {
        parse_proc_route(&fs::read_to_string("/proc/net/route").ok()?)
    }
}

// Hex columns, little-endian, default route = zero destination.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_route(text: &str) -> Option<String> {
    text.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 || cols[1] != "00000000" || cols[2].len() != 8 {
            return None;
        }
        let bytes: Option<Vec<u8>> = cols[2]
            .as_bytes()
            .chunks(2)
            .rev()
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok())
            .collect();
        let b = bytes?;
        Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]))
    })
}

// registered networks

pub const CONF_DEFAULT: &str = "# xvpn.conf — generated by `xvpn use`\n\
# Networks listed here use selective (default) mode.\n\
SELECTIVE_DNS=()\n";

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Networks {
    pub dns: BTreeSet<String>,
}

impl Networks {
    pub fn matches(&self, resolvers: &[String]) -> bool {
        resolvers.iter().any(|live| {
            self.dns.contains(live) || self.dns.iter().any(|known| same_subnet24(known, live))
        })
    }
}

fn same_subnet24(a: &str, b: &str) -> bool {
    match (prefix4(a), prefix4(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_loopback(addr: &str) -> bool {
    matches!(prefix4(addr), Some([127, _, _]))
}

fn prefix4(addr: &str) -> Option<[u8; 3]> {
    let parts: Vec<&str> = addr.trim().split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let octets: Vec<u8> = parts
        .iter()
        .map(|p| p.parse::<u8>().ok())
        .collect::<Option<Vec<u8>>>()?;
    Some([octets[0], octets[1], octets[2]])
}

pub fn read_networks(root: &Path) -> Networks {
    let Ok(content) = fs::read_to_string(root.join(crate::config::CONF)) else {
        return Networks::default();
    };
    let mut dns = BTreeSet::new();
    for line in content.lines() {
        let Some(raw) = line.strip_prefix("SELECTIVE_DNS=") else {
            continue;
        };
        for value in parse_array(raw) {
            if prefix4(&value).is_some() {
                dns.insert(value);
            }
        }
    }
    Networks { dns }
}

pub fn write_networks(root: &Path, networks: &Networks) -> Result<()> {
    let quote = |items: &BTreeSet<String>| {
        items
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let content = format!(
        "# xvpn.conf — generated by `xvpn use`\nSELECTIVE_DNS=({})\n",
        quote(&networks.dns),
    );
    fs::write(root.join(crate::config::CONF), content).map_err(|e| format!("xvpn.conf: {e}"))
}

fn parse_array(value: &str) -> Vec<String> {
    let start = value.find('(').map(|i| i + 1).unwrap_or(0);
    let end = value.rfind(')').unwrap_or(value.len());
    value[start..end]
        .split('"')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Register the current network for selective mode.
///
/// Takes no argument: the resolvers DHCP handed out identify the network, so
/// there is nothing to choose and nothing to get wrong.
pub fn register(root: &Path) -> Result<String> {
    let resolvers = dns_servers();
    if resolvers.is_empty() {
        return Err(
            "no DNS server configured on this network, so there is nothing to \
             identify it by\n\
             hint: is the Wi-Fi connected? ethernet without DHCP gives no clue"
                .to_string(),
        );
    }
    let mut nets = read_networks(root);
    let already = resolvers.iter().all(|r| nets.dns.contains(r));
    for resolver in &resolvers {
        nets.dns.insert(resolver.clone());
    }
    write_networks(root, &nets)?;

    Ok(if already {
        "Already registered this network".to_string()
    } else {
        format!("Registered: {}", resolvers.join(", "))
    })
}

/// Drop `dns` from selective mode, or the current network when none is given.
pub fn unregister(root: &Path, dns: Option<&str>) -> Result<String> {
    let mut nets = read_networks(root);

    let targets: Vec<String> = match dns {
        Some(d) => vec![d.trim().to_string()],
        None => dns_servers(),
    };
    if targets.is_empty() {
        return Err("no DNS server configured on this network".to_string());
    }

    let removed: Vec<String> = targets
        .iter()
        .filter(|t| nets.dns.remove(*t))
        .cloned()
        .collect();
    write_networks(root, &nets)?;

    Ok(if removed.is_empty() {
        format!("Not registered: {}", targets.join(", "))
    } else {
        format!("Removed: {}", removed.join(", "))
    })
}

// app rules

pub fn normalize_app(app: &str) -> Result<String> {
    let trimmed = app.trim();
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    let name = Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(trimmed);
    let name = name.strip_suffix(".app").unwrap_or(name).trim();
    if name.is_empty() {
        return Err("invalid app name".to_string());
    }
    Ok(name.to_string())
}

// Exactly one backslash: two would mean "literal backslash then any char".
fn regex_for(app: &str) -> String {
    let bs: char = '\\';
    format!(".*/{}{bs}.app/.*", app.replace('.', "\\."))
}

// Anchors on `.app`; splitting on the last `/` lands in the trailing `/.*`.
fn app_from_regex(pattern: &str) -> Option<String> {
    let (head, _) = pattern.split_once(".app")?;
    let name = head.rsplit('/').next()?.replace('\\', "");
    (!name.is_empty() && !name.contains('*')).then_some(name)
}

fn rule_matches(rule: &Value, regex: &str) -> bool {
    rule.get("process_path_regex")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|p| p.as_str().is_some_and(|s| s.contains(regex)))
        })
        .unwrap_or(false)
}

pub fn app_names(config: &Value) -> Vec<String> {
    let mut apps: Vec<String> = Vec::new();
    for section in ["dns", "route"] {
        let Some(rules) = config
            .get(section)
            .and_then(|v| v.get("rules"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for rule in rules {
            let Some(pats) = rule.get("process_path_regex").and_then(|v| v.as_array()) else {
                continue;
            };
            for pat in pats.iter().filter_map(|p| p.as_str()) {
                if let Some(app) = app_from_regex(pat) {
                    if !apps.contains(&app) {
                        apps.push(app);
                    }
                }
            }
        }
    }
    apps.sort();
    apps
}

pub fn listed_apps(path: &Path) -> Result<Vec<String>> {
    Ok(app_names(&read_json(path)?))
}

pub fn edit_app(path: &Path, add: bool, app: &str) -> Result<()> {
    let app = normalize_app(app)?;
    let mut cfg = read_json(path)
        .map_err(|e| format!("{e}\nhint: run `xvpn reset` to recreate default.json"))?;

    let regex = regex_for(&app);
    let Some(rules) = cfg
        .get_mut("route")
        .and_then(|r| r.get_mut("rules"))
        .and_then(|r| r.as_array_mut())
    else {
        return Err("default.json has no route.rules array".to_string());
    };

    if add {
        if rules.iter().any(|r| rule_matches(r, &regex)) {
            return Err(format!("{app} already exists"));
        }
        rules.push(json!({ "process_path_regex": [regex], "outbound": "xray" }));
    } else {
        let before = rules.len();
        rules.retain(|r| !rule_matches(r, &regex));
        if rules.len() == before {
            return Err(format!("{app} was not in the list"));
        }
    }

    write_json(path, &cfg)?;
    crate::config::check_singbox(path)
}

// site rules

// A bare suffix covers the apex and every subdomain: sing builds its matcher
// with generateLegacy=false, so `netflix.com` also matches www.netflix.com.
// That is why `www.` is stripped rather than stored.
pub fn normalize_site(site: &str) -> Result<String> {
    let cleaned = site.trim().to_ascii_lowercase();
    let bare = cleaned.strip_prefix("www.").unwrap_or(&cleaned);
    let bare = bare.strip_suffix('.').unwrap_or(bare);

    if bare.is_empty() {
        return Err("that is not a hostname".to_string());
    }
    // A scheme, path, port, or userinfo would never match a real request host.
    if bare.contains("://") || bare.contains('/') || bare.contains('@') || bare.contains(' ') {
        return Err(format!(
            "`{site}` looks like a URL, not a hostname\n\
             hint: use a bare domain, e.g. `xvpn sites set netflix.com`"
        ));
    }
    if !bare.split('.').all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) {
        return Err(format!(
            "`{site}` is not a valid domain\n\
             hint: use a bare domain, e.g. `xvpn sites set netflix.com`"
        ));
    }
    Ok(bare.to_string())
}

fn site_rule_contains(rule: &Value, site: &str) -> bool {
    rule.get("domain_suffix")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().any(|d| d.as_str() == Some(site)))
        .unwrap_or(false)
}

pub fn site_names(config: &Value) -> Vec<String> {
    let mut sites: Vec<String> = Vec::new();
    let Some(rules) = config
        .get("route")
        .and_then(|v| v.get("rules"))
        .and_then(|v| v.as_array())
    else {
        return sites;
    };
    for rule in rules {
        let Some(domains) = rule.get("domain_suffix").and_then(|v| v.as_array()) else {
            continue;
        };
        for domain in domains.iter().filter_map(|d| d.as_str()) {
            if !sites.iter().any(|s| s == domain) {
                sites.push(domain.to_string());
            }
        }
    }
    sites.sort();
    sites
}

pub fn listed_sites(path: &Path) -> Result<Vec<String>> {
    Ok(site_names(&read_json(path)?))
}

pub fn edit_site(path: &Path, add: bool, site: &str) -> Result<()> {
    let site = normalize_site(site)?;
    let mut cfg = read_json(path)
        .map_err(|e| format!("{e}\nhint: run `xvpn reset` to recreate default.json"))?;

    let Some(rules) = cfg
        .get_mut("route")
        .and_then(|r| r.get_mut("rules"))
        .and_then(|r| r.as_array_mut())
    else {
        return Err("default.json has no route.rules array".to_string());
    };

    if add {
        if rules.iter().any(|r| site_rule_contains(r, &site)) {
            return Err(format!("{site} already exists"));
        }
        rules.push(json!({ "domain_suffix": [site], "outbound": "xray" }));
    } else {
        let before = rules.len();
        rules.retain(|r| !site_rule_contains(r, &site));
        if rules.len() == before {
            return Err(format!("{site} was not in the list"));
        }
    }

    write_json(path, &cfg)?;
    crate::config::check_singbox(path)
}

// Outcomes, not failures, so callers must not exit non-zero.
pub fn is_noop(err: &Error) -> bool {
    err.contains("already exists") || err.contains("was not in the list")
}
