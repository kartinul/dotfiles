//! Paths, JSON, process IO, and xray/sing-box config generation.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

use crate::Result;

pub const MODE: &str = "mode";
pub const XRAY: &str = "xray.json";
pub const ON: &str = "on.json";
pub const SELECTIVE: &str = "selective.json";
pub const CONF: &str = "xvpn.conf";
pub const PROFILES: &str = "profiles";
pub const ACTIVE: &str = "active";
/// Profile names, one per line, in the order `profile list` numbers them.
pub const ORDER: &str = "order";

pub fn state_file() -> PathBuf {
    if let Ok(p) = std::env::var("XVPN_STATE") {
        return PathBuf::from(p);
    }
    PathBuf::from("/var/run/xvpn.current")
}

pub fn root() -> PathBuf {
    if let Ok(dir) = std::env::var("XVPN_DIR") {
        return PathBuf::from(dir);
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/local/bin/xvpn"));
    let real = fs::canonicalize(&exe).unwrap_or(exe);
    if let Some(candidate) = real.parent().and_then(|p| p.parent()) {
        // Only accept the candidate if it actually looks like an xvpn dir
        // (has the mode file or the profiles directory).
        if candidate.join(MODE).is_file() || candidate.join(PROFILES).is_dir() {
            return candidate.to_path_buf();
        }
    }
    // Installed layout: binary at /usr/local/bin/xvpn, configs at /usr/local/etc/xvpn.
    PathBuf::from("/usr/local/etc/xvpn")
}

pub fn profiles_dir(root: &Path) -> PathBuf {
    root.join(PROFILES)
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

pub fn capture(prog: &str, args: &[&str]) -> Option<String> {
    let out = output(prog, args).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn output(prog: &str, args: &[&str]) -> Result<Output> {
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

// xray reports errors on stdout, sing-box on stderr. Check both, drop banners.
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Ws,
    Grpc,
    XHttp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
        // xray reports a bad key as `invalid "password"`, so catch it here.
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
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                out.push(byte as char);
            }
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

pub fn xray_config(link: &Link) -> Value {
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
                "port": SOCKS_PORT,
                "protocol": "socks",
                "settings": { "udp": true },
            },
            { "listen": "127.0.0.1", "port": HTTP_PORT, "protocol": "http" },
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

/// A rule the user added, as opposed to one `singbox_config` generates.
///
/// Both rule kinds must survive a rewrite, otherwise `repair` / `import` /
/// `profile use` silently drop whatever the user routed.
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
