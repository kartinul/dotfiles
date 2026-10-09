//! Paths, JSON, process IO, and xray/sing-box config generation.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

use crate::Result;

pub const MODE: &str = "mode";
pub const XRAY: &str = "xray.json";
pub const ON: &str = "modes/on.json";
pub const DEFAULT: &str = "modes/default.json";
pub const CONF: &str = "xvpn.conf";
pub const PROFILES: &str = "profiles";
pub const ACTIVE: &str = "active";
/// Profile names, one per line, in the order `profile list` numbers them.
pub const ORDER: &str = "order";

/// `$XDG_CONFIG_HOME`, or `~/.config` when unset.
fn config_home() -> PathBuf {
    if let Some(dir) = env_var("XDG_CONFIG_HOME") {
        return dir;
    }
    home().join(".config")
}

/// `$XDG_STATE_HOME`, or `~/.local/state` when unset.
///
/// Runtime state lives here rather than in `/var/run` because `/var/run` is
/// root-owned, so a user-space supervisor could not write to it.
fn state_home() -> PathBuf {
    if let Some(dir) = env_var("XDG_STATE_HOME") {
        return dir;
    }
    home().join(".local").join("state")
}

fn home() -> PathBuf {
    #[cfg(unix)]
    if let Some(dir) = sudo_user_home() {
        return dir;
    }
    env_var("HOME").unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(unix)]
fn sudo_user_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let sudo_user = std::env::var_os("SUDO_USER")?;
    let c_user = std::ffi::CString::new(sudo_user.as_bytes()).ok()?;
    unsafe {
        let pwd = libc::getpwnam(c_user.as_ptr());
        if !pwd.is_null() && !(*pwd).pw_dir.is_null() {
            let dir = std::ffi::CStr::from_ptr((*pwd).pw_dir);
            if let Ok(dir_str) = dir.to_str() {
                return Some(PathBuf::from(dir_str));
            }
        }
    }
    None
}

fn env_var(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// What the supervisor is currently doing, read by `xvpn` status.
pub fn state_file() -> PathBuf {
    if let Some(p) = env_var("XVPN_STATE") {
        return p;
    }
    state_home().join("xvpn").join("current")
}

/// Held for the supervisor's lifetime. A second supervisor that cannot take it
/// exits instead of reconciling the same root against the first one.
pub fn lock_file() -> PathBuf {
    if let Some(p) = env_var("XVPN_LOCK") {
        return p;
    }
    state_home().join("xvpn").join("supervisor.lock")
}

/// Per-proxy log. Under the user's state dir so a non-root supervisor can
/// always open it; `/var/log/xvpn-*.log` was root-owned and unwritable.
pub fn log_file(program: &str) -> PathBuf {
    state_home().join("xvpn").join(format!("{program}.log"))
}

/// Everything the tool keeps: config next to the binary's owner's dotfiles,
/// runtime state under XDG.
///
/// Falls back to `~/.xvpn` when the primary root is not writable, so a
/// read-only `~/.config` (a shared machine, a sandbox) does not strand state
/// in a directory nobody can write to.
pub fn root() -> PathBuf {
    if let Some(dir) = env_var("XVPN_DIR") {
        return dir;
    }
    let primary = config_home().join("xvpn");
    if is_writable(&primary) {
        primary
    } else {
        home().join(".xvpn")
    }
}

/// True when a probe file can be created in `dir`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_writable(dir: &Path) -> bool {
    let _ = fs::create_dir_all(dir);
    let probe = dir.join(".xvpn_write_test");
    let _ = fs::remove_file(&probe);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map(|f| {
            drop(f);
            let _ = fs::remove_file(&probe);
            true
        })
        .unwrap_or(false)
}

/// Roots earlier versions could resolve to, kept only to explain a stale one.
///
/// Deliberately never `/usr/local` itself: that is a shared system directory,
/// and treating it as an xvpn root is how state ended up in files like
/// `/usr/local/mode` that no unprivileged command could rewrite.
pub fn legacy_roots() -> Vec<PathBuf> {
    [
        PathBuf::from("/usr/local/etc/xvpn"),
        PathBuf::from("/usr/local/xvpn"),
        home().join(".xvpn"),
    ]
    .into_iter()
    .collect()
}

/// Move state into the current root, copying from the first legacy root that
/// has any. Copies rather than moves, so the old copy survives as a fallback.
///
/// Only the known filenames are considered. A blanket directory copy would be
/// catastrophic if the legacy root resolved to something like `/usr/local`.
pub fn migrate_state() -> crate::Result<Option<PathBuf>> {
    let root = root();
    let from = legacy_roots()
        .into_iter()
        .find(|old| *old != root && (old.join(MODE).is_file() || old.join(PROFILES).is_dir()))
        .ok_or_else(|| "nothing to migrate: no state in any previous location".to_string())?;

    fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    for name in [MODE, XRAY, CONF, ACTIVE, ORDER] {
        let src = from.join(name);
        if src.is_file() && !root.join(name).exists() {
            fs::copy(&src, root.join(name)).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    // Profiles and the generated sidecars, recursively but shallowly.
    for dir in [PROFILES, "modes"] {
        let src = from.join(dir);
        if src.is_dir() && !root.join(dir).exists() {
            copy_tree(&src, &root.join(dir)).map_err(|e| format!("{dir}: {e}"))?;
        }
    }
    Ok(Some(from))
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)?.flatten() {
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// One-time note about state left behind elsewhere. Reported, never moved.
pub fn stale_root_notice() -> Option<String> {
    if env_var("XVPN_DIR").is_some() {
        return None;
    }
    let current = root();
    if current.join(PROFILES).is_dir() || current.join(MODE).is_file() {
        return None;
    }
    legacy_roots()
        .into_iter()
        .find(|old| old != &current && (old.join(MODE).is_file() || old.join(PROFILES).is_dir()))
        .map(|old| {
            format!(
                "found xvpn state in {} but this build uses {}\n\
                 run `xvpn migrate` to move it across",
                old.display(),
                current.display()
            )
        })
}

pub fn profiles_dir(root: &Path) -> PathBuf {
    root.join(PROFILES)
}

/// Whether this process runs as root.
///
/// Decides whether sing-box can bring up its tun interface: on macOS there is
/// no TProxy, so `on` mode means `auto_route`, which needs privilege.
pub fn is_root() -> bool {
    // SAFETY: `geteuid` takes no arguments, cannot fail, and has no
    // preconditions.
    unsafe { libc::geteuid() == 0 }
}

pub fn active_file(root: &Path) -> PathBuf {
    root.join(ACTIVE)
}

pub fn order_file(root: &Path) -> PathBuf {
    root.join(ORDER)
}

// json

pub fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value).expect("serialize");
    text.push('\n');
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

// Profiles hold a bearer credential, so owner-only.
pub fn write_secret(path: &Path, value: &Value) -> Result<()> {
    write_json(path, value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

// process

pub fn argv<I, S>(values: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    values
        .into_iter()
        .map(|v| v.as_ref().to_os_string())
        .collect()
}

pub fn run(prog: &str, args: &[&str]) -> Result<()> {
    let out = output(prog, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(failure(&out))
    }
}

pub fn output(prog: &str, args: &[&str]) -> Result<Output> {
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run {prog}: {e}"))
}

pub fn check_with(tool: &str, args: &[OsString]) -> Result<()> {
    match Command::new(tool).args(args).stdin(Stdio::null()).output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(failure(&out)),
        Err(e) => Err(format!("could not run {tool}: {e}")),
    }
}

fn failure(out: &Output) -> String {
    for stream in [
        String::from_utf8_lossy(&out.stderr).to_string(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    ] {
        if let Some(msg) = pick_error(&stream) {
            return msg;
        }
    }
    format!("exited with {}", out.status)
}

fn clean_line(raw: &str) -> Option<String> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix("Failed to start: main: failed to load config files: [") {
        let reason = rest.split_once("] ").map(|(_, r)| r).unwrap_or(rest);
        return (!reason.is_empty()).then(|| reason.to_string());
    }
    if line.starts_with("FATAL") || line.starts_with("ERROR") {
        if let Some(i) = line.find(']') {
            let reason = line[i + 1..].trim();
            if !reason.is_empty() {
                return Some(reason.to_string());
            }
        }
    }
    if line.contains("[Info]") || line.contains("[Warning]") || line.contains("[Debug]") {
        return None;
    }
    if line.starts_with("Xray ") || line.contains("anti-censorship") {
        return None;
    }
    Some(line.to_string())
}

fn pick_error(output: &str) -> Option<String> {
    let lines: Vec<String> = output.lines().filter_map(clean_line).collect();
    lines
        .iter()
        .find(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("failed") || l.contains("invalid") || l.contains("rejected")
        })
        .cloned()
        .or_else(|| lines.last().cloned())
}

// vless

// `http`/h2 is absent on purpose: Xray removed HTTP/2 and folded it into XHTTP.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Tcp,
    Ws,
    Grpc,
    XHttp,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Security {
    None,
    Tls,
    Reality,
}

pub struct Link {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub transport: Transport,
    pub security: Security,
    params: Vec<(String, String)>,
}

impl Link {
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn param_or<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.param(key).unwrap_or(fallback)
    }

    fn validate(&self) -> Result<()> {
        if self.id.is_empty() {
            return Err("link has no user id".into());
        }
        if self.security != Security::Reality {
            return Ok(());
        }
        match self.param("pbk") {
            None | Some("") => return Err("reality link is missing `pbk`".into()),
            Some(k) if !valid_reality_key(k) => {
                return Err(format!("`pbk` is not a valid Reality public key: {k}"))
            }
            Some(_) => {}
        }
        match self.param("sid") {
            None | Some("") => Err("reality link is missing `sid`".into()),
            Some(s) if !valid_short_id(s) => Err(format!("`sid` must be hex, got: {s}")),
            Some(_) => Ok(()),
        }
    }
}

fn valid_reality_key(key: &str) -> bool {
    key.len() == 43
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_short_id(sid: &str) -> bool {
    !sid.is_empty() && sid.len() <= 16 && sid.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_transport(raw: Option<&str>) -> Result<Transport> {
    match raw.unwrap_or("tcp") {
        "tcp" => Ok(Transport::Tcp),
        "ws" | "websocket" => Ok(Transport::Ws),
        "grpc" => Ok(Transport::Grpc),
        "xhttp" | "splithttp" => Ok(Transport::XHttp),
        "http" | "h2" => Err("type=http/h2 was removed from Xray and merged into XHTTP. \
             Re-export the link choosing XHTTP."
            .into()),
        other => Err(format!(
            "unsupported transport `type={other}`. Supported: tcp, ws, grpc, xhttp."
        )),
    }
}

fn parse_security(raw: Option<&str>) -> Result<Security> {
    match raw.unwrap_or("none") {
        "none" => Ok(Security::None),
        "tls" | "xtls" => Ok(Security::Tls),
        "reality" => Ok(Security::Reality),
        other => Err(format!(
            "unsupported security `security={other}`. Supported: none, tls, reality."
        )),
    }
}

pub fn parse_link(link: &str) -> Result<Link> {
    let rest = link
        .strip_prefix("vless://")
        .ok_or_else(|| "that does not look like a vless:// link".to_string())?;

    let (userinfo, host_part) = rest.split_once('@').unwrap_or((rest, ""));
    let (host_port, query) = host_part
        .split_once('?')
        .map_or((host_part, ""), |(hp, q)| (hp, q));
    let (query, _) = query.split_once('#').unwrap_or((query, ""));
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(443)),
        None => (host_port.to_string(), 443),
    };

    let params: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), url_decode(v)))
        .collect();

    let parsed = Link {
        id: url_decode(userinfo),
        host,
        port,
        transport: parse_transport(param_of(&params, "type"))?,
        security: parse_security(param_of(&params, "security"))?,
        params,
    };
    parsed.validate()?;
    Ok(parsed)
}

fn param_of<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn url_decode(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    out.push(byte as char);
                    continue;
                }
            }
            out.push('%');
            out.push_str(&hex);
        } else {
            out.push(c);
        }
    }
    out
}

// xray

pub const SOCKS_PORT: u16 = 10808;
pub const HTTP_PORT: u16 = 10809;
pub const TPROXY_PORT: u16 = 12345;

pub fn xray_config(link: &Link, socks_port: u16, http_port: u16) -> Value {
    let mut user = json!({
        "id": &link.id,
        "encryption": link.param_or("encryption", "none"),
    });
    if let Some(flow) = link.param("flow") {
        user["flow"] = json!(flow);
    }

    let network = match link.transport {
        Transport::Tcp => "tcp",
        Transport::Ws => "ws",
        Transport::Grpc => "grpc",
        Transport::XHttp => "xhttp",
    };
    let security = match link.security {
        Security::None => "none",
        Security::Tls => "tls",
        Security::Reality => "reality",
    };
    let mut stream = json!({ "network": network, "security": security });

    if link.security == Security::Reality {
        let mut reality = json!({
            "serverName": link.param_or("sni", ""),
            "publicKey": link.param_or("pbk", ""),
            "shortId": link.param_or("sid", ""),
            "fingerprint": link.param_or("fp", "chrome"),
        });
        if let Some(spx) = link.param("spx") {
            reality["spiderX"] = json!(spx);
        }
        stream["realitySettings"] = reality;
    } else if link.security == Security::Tls {
        let mut tls = json!({
            "serverName": link.param("sni").unwrap_or(&link.host),
            "fingerprint": link.param_or("fp", "chrome"),
        });
        if let Some(alpn) = link.param("alpn") {
            tls["alpn"] = json!(alpn.split(',').map(str::to_string).collect::<Vec<_>>());
        }
        stream["tlsSettings"] = tls;
    }

    match link.transport {
        Transport::Ws => {
            let mut ws = json!({ "path": url_decode(link.param_or("path", "/")) });
            if let Some(host) = link.param("host") {
                ws["headers"] = json!({ "Host": host });
            }
            stream["wsSettings"] = ws;
        }
        Transport::Grpc => {
            stream["grpcSettings"] = json!({ "serviceName": link.param_or("serviceName", "") });
        }
        Transport::XHttp => {
            let mut x = json!({ "path": url_decode(link.param_or("path", "/")) });
            if let Some(host) = link.param("host") {
                x["host"] = json!(host);
            }
            if let Some(mode) = link.param("mode") {
                x["mode"] = json!(mode);
            }
            stream["xhttpSettings"] = x;
        }
        Transport::Tcp => {}
    }

    json!({
        "log": { "loglevel": "warning" },
        "inbounds": [
            {
                "listen": "127.0.0.1",
                "port": socks_port,
                "protocol": "socks",
                "settings": { "udp": true },
            },
            { "listen": "127.0.0.1", "port": http_port, "protocol": "http" },
        ],
        "outbounds": [
            {
                "tag": "proxy",
                "protocol": "vless",
                "settings": {
                    "vnext": [{ "address": &link.host, "port": link.port, "users": [user] }]
                },
                "streamSettings": stream,
            },
            { "tag": "direct", "protocol": "freedom" },
        ],
    })
}

pub fn check_xray(path: &Path) -> Result<()> {
    check_with(
        "xray",
        &argv([
            "run".as_ref(),
            "-test".as_ref(),
            "-c".as_ref(),
            path.as_os_str(),
        ]),
    )
    .map_err(|e| format!("xray rejected {}: {e}", path.display()))
}

pub fn server_of(config: &Value) -> Option<&str> {
    config["outbounds"][0]["settings"]["vnext"][0]["address"].as_str()
}

pub fn direct_xray_config(socks_port: u16, http_port: u16) -> Value {
    json!({
        "log": { "loglevel": "warning" },
        "inbounds": [
            {
                "listen": "127.0.0.1",
                "port": socks_port,
                "protocol": "socks",
                "settings": { "udp": true },
            },
            { "listen": "127.0.0.1", "port": http_port, "protocol": "http" },
        ],
        "outbounds": [
            { "tag": "direct", "protocol": "freedom" },
        ],
    })
}

// sing-box

pub const PROXY_TOOLS: [&str; 6] = ["curl", "git", "brew", "ssh", "node", "python3"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Selective,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ingress {
    Tun,
    TProxy,
}

pub fn singbox_config(scope: Scope, ingress: Ingress) -> Value {
    let mut rules = vec![
        json!({ "process_name": ["xray"], "outbound": "direct" }),
        json!({ "action": "sniff" }),
        json!({ "protocol": "dns", "action": "hijack-dns" }),
    ];
    if scope == Scope::Selective {
        rules.push(json!({ "process_name": PROXY_TOOLS, "outbound": "xray" }));
    } else {
        rules.push(json!({ "ip_is_private": true, "outbound": "direct" }));
    }

    json!({
        "log": { "level": "warn" },
        "dns": {
            "servers": [
                { "tag": "remote", "type": "https", "server": "1.1.1.1", "detour": "xray" },
            ],
            "final": "remote",
        },
        "inbounds": [match ingress {
            Ingress::Tun => json!({
                "type": "tun", "address": ["172.19.0.1/30"], "auto_route": true,
            }),
            Ingress::TProxy => json!({
                "type": "tproxy", "listen": "0.0.0.0", "port": TPROXY_PORT,
            }),
        }],
        "outbounds": [
            { "type": "socks", "tag": "xray", "server": "127.0.0.1", "server_port": SOCKS_PORT },
            { "type": "direct", "tag": "direct" },
        ],
        "route": {
            "auto_detect_interface": true,
            "find_process": true,
            // 1.14 removed per-process DNS rule items with no replacement.
            "default_domain_resolver": "remote",
            "rules": rules,
            "final": match scope {
                Scope::Selective => "direct",
                Scope::Global => "xray",
            },
        }
    })
}

fn is_user_rule(rule: &Value) -> bool {
    rule.get("process_path_regex").is_some() || rule.get("domain_suffix").is_some()
}

pub fn migrate_singbox(existing: &Value, scope: Scope, ingress: Ingress) -> Value {
    let mut fresh = singbox_config(scope, ingress);
    let carried: Vec<Value> = existing
        .get("route")
        .and_then(|r| r.get("rules"))
        .and_then(|r| r.as_array())
        .map(|rules| rules.iter().filter(|r| is_user_rule(r)).cloned().collect())
        .unwrap_or_default();
    if let Some(rules) = fresh
        .get_mut("route")
        .and_then(|r| r.get_mut("rules"))
        .and_then(|r| r.as_array_mut())
    {
        rules.extend(carried);
    }
    fresh
}

pub fn check_singbox(path: &Path) -> Result<()> {
    check_with(
        "sing-box",
        &argv(["check".as_ref(), "-c".as_ref(), path.as_os_str()]),
    )
    .map_err(|e| format!("sing-box rejected {}: {e}", path.display()))
}
