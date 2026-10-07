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

pub fn current_ssid() -> String {
    imp::current_ssid()
}

pub fn gateway_mac() -> Option<String> {
    imp::gateway_mac()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{looks_like_mac, normalize_mac};
    use crate::config::capture;

    /// The Wi-Fi interface name, e.g. `en0`.
    ///
    /// `networksetup -listallhardwareports` prints a header line, then a
    /// `Device: en0` line. Only the value after the colon is the device name:
    /// returning the whole line yields "Device: en0", which every downstream
    /// `route`/`ipconfig` call rejects, so the SSID and the gateway MAC both
    /// come back empty.
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

    /// SSID, or empty when hidden or unavailable.
    ///
    /// `ipconfig getsummary` is tried first, then `networksetup
    /// -getairportnetwork`. macOS 15 redacts the SSID from `getsummary`
    /// (`SSID : <redacted>`) unless the caller holds the Location
    /// permission, so on a current OS the first lookup always reports a
    /// visible network as hidden. `-getairportnetwork` is not redacted.
    ///
    /// A genuinely hidden network reports "You are not associated with an
    /// AirPort network.", which is not an SSID.
    pub fn current_ssid() -> String {
        let Some(dev) = wifi_device() else {
            return String::new();
        };
        let from_summary = capture("ipconfig", &["getsummary", &dev])
            .and_then(|out| {
                out.lines()
                    .find_map(|l| l.trim().strip_prefix("SSID : "))
                    .map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty() && s != "<redacted>");

        if let Some(ssid) = from_summary {
            return ssid;
        }

        capture("networksetup", &["-getairportnetwork", &dev])
            .map(|out| {
                out.trim()
                    .strip_prefix("Current Wi-Fi Network: ")
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty() && !s.contains("not associated"))
            .unwrap_or_default()
    }

    /// MAC of the default gateway.
    ///
    /// Reads the neighbour table out of `netstat -rn -f inet`, not `arp`:
    /// spawned from a plain `exec` (stdin closed, no tty) `arp` returns empty
    /// stdout with exit 0, or `-- no entry` with exit 1, even though the entry
    /// is there — the same calls work from a shell or Python. `netstat` reads
    /// the same table over a different interface and is unaffected.
    ///
    /// The row looks like `172.16.164.1  cc:ed:4d:70:13:5f  UHLWIir  en0`.
    pub fn gateway_mac() -> Option<String> {
        let dev = wifi_device()?;
        let routes = capture("route", &["-n", "get", "-ifscope", &dev, "default"])?;
        let gw = routes
            .lines()
            .find_map(|l| l.trim().strip_prefix("gateway: "))
            .map(str::trim)?;

        let table = capture("netstat", &["-rn", "-f", "inet"])?;
        table
            .lines()
            // Skip the `172.16.164.1/32 link#12 ...` route row: it carries no
            // MAC, so requiring both the bare address and a MAC token skips it.
            .filter(|l| l.split_whitespace().next() == Some(gw))
            .find_map(|l| {
                l.split_whitespace()
                    .find(|t| looks_like_mac(t))
                    .and_then(normalize_mac)
            })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{normalize_mac, parse_proc_route};
    use crate::config::capture;
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

    pub fn current_ssid() -> String {
        for dev in wifi_devices() {
            let Some(out) = capture("iw", &["dev", &dev, "link"]) else {
                continue;
            };
            if let Some(ssid) = out.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("SSID: ")
                    .map(|s| s.trim().to_string())
            }) {
                if !ssid.is_empty() {
                    return ssid;
                }
            }
        }
        String::new()
    }

    pub fn gateway_mac() -> Option<String> {
        let gw = parse_proc_route(&fs::read_to_string("/proc/net/route").ok()?)?;
        let arp = fs::read_to_string("/proc/net/arp").ok()?;
        arp.lines()
            .skip(1)
            .filter_map(|l| {
                let cols: Vec<&str> = l.split_whitespace().collect();
                (cols.len() >= 4 && cols[0] == gw).then_some(cols[3])
            })
            .find_map(normalize_mac)
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

#[cfg(target_os = "macos")]
fn looks_like_mac(token: &str) -> bool {
    token.len() == 17 && token.contains(':')
}

pub fn normalize_mac(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_hexdigit() || *c == ':' || *c == '-')
        .map(|c| if c == '-' { ':' } else { c })
        .collect();
    let parts: Vec<&str> = cleaned.split(':').collect();
    (parts.len() == 6 && parts.iter().all(|p| p.len() == 2)).then(|| parts.join(":"))
}

// registered networks

pub const CONF_DEFAULT: &str = "# xvpn.conf — generated by `xvpn use`\n\
# Networks listed here use selective (default) mode.\n\
SELECTIVE_SSIDS=()\n\
SELECTIVE_GWMACS=()\n";

// Independent sets, not pairs: a hidden network has no SSID, so matching also
// tests the gateway MAC.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Networks {
    pub ssids: BTreeSet<String>,
    pub macs: BTreeSet<String>,
}

impl Networks {
    pub fn matches(&self, ssid: &str, mac: &str) -> bool {
        (!ssid.is_empty() && self.ssids.contains(ssid))
            || (!mac.is_empty() && self.macs.contains(mac))
    }
}

pub fn read_networks(root: &Path) -> Networks {
    let Ok(content) = fs::read_to_string(root.join(crate::config::CONF)) else {
        return Networks::default();
    };
    let (mut ssids, mut macs) = (BTreeSet::new(), BTreeSet::new());
    for line in content.lines() {
        let (is_mac, raw) = if let Some(v) = line.strip_prefix("SELECTIVE_SSIDS=") {
            (false, v)
        } else if let Some(v) = line.strip_prefix("SELECTIVE_GWMACS=") {
            (true, v)
        } else {
            continue;
        };
        for value in parse_array(raw) {
            if is_mac {
                if let Some(mac) = normalize_mac(&value) {
                    macs.insert(mac);
                }
            } else if !value.is_empty() {
                ssids.insert(value);
            }
        }
    }
    Networks { ssids, macs }
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
        "# xvpn.conf — generated by `xvpn use`\nSELECTIVE_SSIDS=({})\nSELECTIVE_GWMACS=({})\n",
        quote(&networks.ssids),
        quote(&networks.macs),
    );
    fs::write(root.join(crate::config::CONF), content).map_err(|e| format!("xvpn.conf: {e}"))
}

// Trimming matters: `("a" "b")` otherwise yields a bogus `" "` entry.
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

pub fn use_current(root: &Path) -> Result<String> {
    let ssid = current_ssid();
    let mac = gateway_mac();
    let mut nets = read_networks(root);

    // MAC alone is enough: `Networks::matches` tests either field, and the
    // gateway MAC is readable even when macOS redacts the SSID. Only bail when
    // there is nothing at all to match on.
    if ssid.is_empty() && mac.is_none() {
        return Err(
            "could not identify this network: no SSID and no gateway MAC\n\
             hint: is the Wi-Fi connected? ethernet has no SSID to register"
                .to_string(),
        );
    }
    if !ssid.is_empty() {
        nets.ssids.insert(ssid.clone());
    }
    if let Some(mac) = &mac {
        nets.macs.insert(mac.clone());
    }
    write_networks(root, &nets)?;

    let shown = if ssid.is_empty() {
        "(unavailable — matched by MAC)"
    } else {
        &ssid
    };
    Ok(format!(
        "Added: SSID='{shown}' gateway MAC='{}'",
        mac.unwrap_or_else(|| "(none)".into())
    ))
}

pub fn forget_current(root: &Path) -> Result<String> {
    let ssid = current_ssid();
    let mac = gateway_mac();
    let mut nets = read_networks(root);

    let had = (!ssid.is_empty() && nets.ssids.remove(&ssid))
        || mac.as_ref().is_some_and(|m| nets.macs.remove(m));
    write_networks(root, &nets)?;

    let shown = if ssid.is_empty() { "(hidden)" } else { &ssid };
    Ok(if had {
        format!("Removed: SSID='{shown}'")
    } else {
        format!("Not registered: SSID='{shown}'")
    })
}

// app rules

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
    let mut cfg = read_json(path)
        .map_err(|e| format!("{e}\nhint: run `xvpn reset` to recreate selective.json"))?;

    let regex = regex_for(app);
    let Some(rules) = cfg
        .get_mut("route")
        .and_then(|r| r.get_mut("rules"))
        .and_then(|r| r.as_array_mut())
    else {
        return Err("selective.json has no route.rules array".to_string());
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
        .map_err(|e| format!("{e}\nhint: run `xvpn reset` to recreate selective.json"))?;

    let Some(rules) = cfg
        .get_mut("route")
        .and_then(|r| r.get_mut("rules"))
        .and_then(|r| r.as_array_mut())
    else {
        return Err("selective.json has no route.rules array".to_string());
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
