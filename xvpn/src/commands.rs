//! What each subcommand does.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::net::TcpStream;

use crate::cli::{apps_args, sites_args, AppsAction, Mode, ProfileCmd, SitesAction};
use crate::config::{self, Ingress, Scope};
use crate::network;
use crate::output as out;
use crate::profiles;
use crate::supervisor;
use crate::{Error, Result};

pub fn use_profile(root: &Path, index: u16) -> Result<()> {
    profiles::migrate_legacy(root)?;
    if index == 0 {
        return set_mode(root, Mode::Off);
    }
    let mode = read_mode(root);
    let profiles = profiles::list(root);
    let name = profiles
        .get(index as usize - 1)
        .map(|p| p.name.clone())
        .ok_or_else(|| {
            format!(
                "no profile {index} (have {}; see `xvpn profile list`)",
                profiles.len()
            )
        })?;
    profiles::activate(root, &name)?;
    out::status("✓", &format!("active profile: {name}"));
    out::kv("mode", &mode);
    refresh_sidecars(root)?;
    out::status("→", "supervisor will reload");
    Ok(())
}

pub fn run(result: Result<()>) {
    if let Err(e) = result {
        if !e.is_empty() {
            eprintln!("Error: {e}");
        }
        std::process::exit(1);
    }
}

fn read_mode(root: &Path) -> String {
    fs::read_to_string(root.join(config::MODE))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unset".to_string())
}

pub fn status(root: &Path) -> Result<()> {
    let mode = read_mode(root);
    let active = fs::read_to_string(config::state_file())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "stopped".to_string());

    out::kv("mode", &mode);
    out::kv("active", &active);
    out::kv("root", &root.display().to_string());

    if let Some(notice) = config::stale_root_notice() {
        out::warn(&notice);
    }

    match profiles::active_name(root) {
        Some(name) => {
            let server = profiles::list(root)
                .into_iter()
                .find(|p| p.name == name)
                .and_then(|p| p.server)
                .unwrap_or_else(|| "(unreadable)".into());
            out::kv("profile", &format!("{name} ({server})"));
        }
        None => out::kv("profile", "none (run `xvpn import`)"),
    }

    if mode == "default" {
        print_routing(root);
    }
    Ok(())
}

/// Move state out of a root an older build resolved to.
pub fn migrate() -> Result<()> {
    match config::migrate_state()? {
        Some(from) => out::status("✓", &format!(
            "migrated state from {} to {}",
            from.display(),
            config::root().display()
        )),
        None => out::status("→", "nothing to migrate"),
    }
    Ok(())
}

fn print_routing(root: &Path) {
    let path = root.join(config::DEFAULT);
    let cfg = match config::read_json(&path) {
        Ok(c) => c,
        Err(_) => {
            out::kv("routed", "unknown (run `xvpn repair`)");
            return;
        }
    };
    let apps = network::app_names(&cfg);
    let sites = network::site_names(&cfg);

    out::section("always proxied (built in):");
    out::list(&config::PROXY_TOOLS.iter().map(|s| s.as_ref()).collect::<Vec<_>>());

    if !apps.is_empty() {
        out::section("apps:");
        out::list(&apps.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    }
    if !sites.is_empty() {
        out::section("sites (and subdomains):");
        out::list(&sites.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    }
    if apps.is_empty() && sites.is_empty() {
        out::status("→", "nothing else routed, everything goes direct");
    } else {
        out::status("→", "everything else direct");
    }
}

pub fn set_mode(root: &Path, mode: Mode) -> Result<()> {
    let mode = mode.as_str();
    let path = root.join(config::MODE);
    fs::write(&path, mode).map_err(|e| format!("writing mode: {e}"))?;
    let written = std::time::SystemTime::now();
    out::kv("mode", mode);

    if !supervisor::is_running() {
        out::warn("no supervisor is running, so nothing was applied");
        out::info("start one with `make agent` (auto-start) or `xvpn supervise`");
        return Ok(());
    }

    match wait_for_state(written, Duration::from_secs(10)) {
        Some(state) => match state.as_str() {
            "on" | "selective" => out::kv("active", &state),
            "failed" => {
                return Err(
                    "supervisor rejected the config and stayed down — see `xvpn logs`".into(),
                )
            }
            other => out::kv("active", &format!("{other} (nothing is routing)")),
        },
        None => {
            out::warn("no state update received from supervisor");
        }
    }
    Ok(())
}

/// Wait for the supervisor to republish state after `changed_at`.
///
/// Compares mtime rather than the value, so re-issuing the mode you are
/// already in still reports the live state instead of timing out.
fn wait_for_state(changed_at: std::time::SystemTime, timeout: Duration) -> Option<String> {
    let path = config::state_file();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let fresh = meta.modified().map(|m| m >= changed_at).unwrap_or(false);
        if !fresh {
            continue;
        }
        let state = fs::read_to_string(&path).ok()?.trim().to_string();
        if !state.is_empty() {
            return Some(state);
        }
    }
    fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn import(root: &Path, link: Option<&str>, force: bool) -> Result<()> {
    profiles::migrate_legacy(root)?;
    let link = read_link(link)?;
    let name = profiles::import(root, None, &link, force)?;
    out::status("✓", &format!("saved profile '{name}'"));
    refresh_sidecars(root)?;
    out::status("→", "active. the supervisor will reload");
    Ok(())
}

pub fn profile(root: &Path, cmd: ProfileCmd) -> Result<()> {
    profiles::migrate_legacy(root)?;
    match cmd {
        ProfileCmd::Add { name, link, force } => {
            let link = read_link(link.as_deref())?;
            let saved = profiles::import(root, Some(&name), &link, force)?;
            out::status("✓", &format!("saved profile '{saved}'"));
            refresh_sidecars(root)?;
            out::status("→", "active. the supervisor will reload");
            Ok(())
        }
        ProfileCmd::List => profiles::print_list(root),
        ProfileCmd::Use { name } => {
            let name = profiles::resolve(root, &name)?;
            profiles::validate(root, &name)?;
            profiles::activate(root, &name)?;
            out::status("✓", &format!("active profile: {name}"));
            refresh_sidecars(root)?;
            out::status("→", "supervisor will reload");
            Ok(())
        }
        ProfileCmd::Remove { name } => {
            let name = profiles::resolve(root, &name)?;
            profiles::remove(root, &name)?;
            out::status("✓", &format!("removed profile '{name}'"));
            Ok(())
        }
        ProfileCmd::Show { name } => {
            let name = match name {
                Some(n) => profiles::resolve(root, &n)?,
                None => match profiles::active_name(root) {
                    Some(n) => n,
                    None => return Err("no profiles configured".into()),
                },
            };
            let cfg = config::read_json(&profiles::path_for(root, &name))?;
            println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
            Ok(())
        }
    }
}

fn read_link(link: Option<&str>) -> Result<String> {
    match link {
        Some(l) => Ok(l.trim().to_string()),
        None => {
            print!("Paste vless:// link: ");
            io::stdout().flush().map_err(|e| e.to_string())?;
            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| format!("reading input: {e}"))?;
            Ok(input.trim().to_string())
        }
    }
}

pub fn apps(root: &Path, action: AppsAction, app: Option<String>) -> Result<()> {
    let (action, app) = apps_args(action, app)?;
    match (action, app) {
        (AppsAction::Show, _) => list_apps(root),
        (AppsAction::Set, Some(app)) => add_app(root, &app),
        (AppsAction::Remove, Some(app)) => remove_app(root, &app),
        _ => Err("invalid apps arguments".into()),
    }
}

pub fn list_apps(root: &Path) -> Result<()> {
    let apps = network::listed_apps(&root.join(config::DEFAULT))?;
    out::section("apps:");
    out::list(&apps.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    Ok(())
}

pub fn add_app(root: &Path, app: &str) -> Result<()> {
    edit_app(root, true, app)
}

pub fn remove_app(root: &Path, app: &str) -> Result<()> {
    edit_app(root, false, app)
}

fn edit_app(root: &Path, add: bool, app: &str) -> Result<()> {
    let shown = network::normalize_app(app)?;
    announce_edit(
        network::edit_app(&root.join(config::DEFAULT), add, &shown),
        add,
        &shown,
    )
}

pub fn sites(root: &Path, action: SitesAction, site: Option<String>) -> Result<()> {
    let (action, site) = sites_args(action, site)?;
    match (action, site) {
        (SitesAction::Show, _) => list_sites(root),
        (SitesAction::Set, Some(site)) => add_site(root, &site),
        (SitesAction::Remove, Some(site)) => remove_site(root, &site),
        _ => Err("invalid sites arguments".into()),
    }
}

pub fn list_sites(root: &Path) -> Result<()> {
    let sites = network::listed_sites(&root.join(config::DEFAULT))?;
    out::section("sites (and subdomains):");
    out::list(&sites.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    Ok(())
}

pub fn add_site(root: &Path, site: &str) -> Result<()> {
    edit_site(root, true, site)
}

pub fn remove_site(root: &Path, site: &str) -> Result<()> {
    edit_site(root, false, site)
}

fn edit_site(root: &Path, add: bool, site: &str) -> Result<()> {
    // Echo the normalised name, so `www.` stripping is visible in the output
    // and matches what actually landed in the config.
    let shown = network::normalize_site(site)?;
    announce_edit(
        network::edit_site(&root.join(config::DEFAULT), add, &shown),
        add,
        &shown,
    )
}

/// "already exists" and "not in the list" are outcomes, not failures.
fn announce_edit(result: Result<()>, add: bool, name: &str) -> Result<()> {
    match result {
        Ok(()) => {
            out::status(if add { "✓" } else { "✓" }, &format!("{} {}", if add { "Added" } else { "Removed" }, name));
            Ok(())
        }
        Err(e) if network::is_noop(&e) => {
            out::status("→", &e);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Register or forget the current network for selective mode.
///
/// `dns` is accepted by `forget` to drop a resolver for a network you are no
/// longer on.
pub fn use_network(root: &Path, add: bool, dns: Option<&str>) -> Result<()> {
    let msg = if add {
        network::register(root)?
    } else {
        network::unregister(root, dns)?
    };
    out::status("✓", &msg);
    Ok(())
}

/// Each listener paired with the scheme it actually speaks.
///
/// Probing the HTTP port with `socks5://` fails the handshake (curl exit 97)
/// even when the proxy is perfectly healthy, which reads like a dead tunnel.
const PROBES: [(&str, u16); 2] = [("socks5", config::SOCKS_PORT), ("http", config::HTTP_PORT)];

/// Curl ifconfig.me through the running xray proxy to verify connectivity.
pub fn check_vpn(root: &Path) -> Result<()> {
    let (name, _) = profiles::resolve_active(root)?;
    out::kv("checking profile", &name);

    for (scheme, port) in PROBES {
        let proxy = format!("{scheme}://127.0.0.1:{port}");
        match run_curl(&proxy) {
            Ok(ip) => {
                out::kv("via proxy", &format!("{proxy} → {ip}"));
                return Ok(());
            }
            Err(e) => out::kv(&proxy, &e),
        }
    }
    Err("no running proxy found — start the VPN first".into())
}

fn run_curl(proxy: &str) -> Result<String> {
    let out = Command::new("curl")
        .args(["-s", "-x", proxy, "ifconfig.me"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;
    if out.status.success() {
        let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if ip.is_empty() {
            return Err("empty response".into());
        }
        Ok(ip)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("curl exited with {}", out.status)
        } else {
            err
        })
    }
}

fn ingress() -> Ingress {
    if network::supports_gateway() {
        Ingress::TProxy
    } else {
        Ingress::Tun
    }
}

fn sidecars() -> [(&'static str, Scope); 2] {
    [
        (config::ON, Scope::Global),
        (config::DEFAULT, Scope::Selective),
    ]
}

fn refresh_sidecars(root: &Path) -> Result<()> {
    let ingress = ingress();
    for (name, scope) in sidecars() {
        ensure_sidecar(root, name, scope, ingress)?;
    }
    Ok(())
}

fn ensure_sidecar(root: &Path, name: &str, scope: Scope, ingress: Ingress) -> Result<()> {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{name}: {e}"))?;
    }
    let action = match config::read_json(&path) {
        Ok(existing) => {
            let fresh = config::migrate_singbox(&existing, scope, ingress);
            if fresh == existing {
                config::check_singbox(&path).map_err(|e| format!("{name}: {e}"))?;
                out::status("→", &format!("{name}: already current"));
                return Ok(());
            }
            config::write_json(&path, &fresh)?;
            "migrated"
        }
        Err(_) => {
            config::write_json(&path, &config::singbox_config(scope, ingress))?;
            "created"
        }
    };
    config::check_singbox(&path).map_err(|e| format!("{name}: {e}"))?;
    out::status("✓", &format!("{name}: {action}, sing-box check OK"));
    Ok(())
}

/// Verify that the external binaries the CLI depends on are on PATH.
pub fn ensure_dependencies() -> Result<()> {
    for tool in ["xray", "sing-box"] {
        let found = Command::new("which")
            .arg(tool)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if !found {
            return Err(format!(
                "{tool} not found in PATH — run `./install.sh` to install it"
            ));
        }
    }
    Ok(())
}

pub fn check(root: &Path) -> Result<()> {
    if let Err(e) = ensure_dependencies() {
        out::warn(&e);
        out::info("run ./install.sh to install missing dependencies");
        return Ok(());
    }
    let mut ok = true;

    match profiles::resolve_active(root) {
        Ok((name, path)) => match config::check_xray(&path) {
            Ok(()) => out::status("✓", &format!("profile '{name}': OK")),
            Err(e) => {
                out::status("✗", &format!("profile '{name}': FAILED"));
                out::error(&e);
                ok = false;
            }
        },
        Err(e) => out::error(&e),
    }

    for (name, _) in sidecars() {
        let path = root.join(name);
        if !path.exists() {
            out::status("✗", &format!("{name}: MISSING (run `xvpn repair`)"));
            ok = false;
            continue;
        }
        match config::check_singbox(&path) {
            Ok(()) => out::status("✓", &format!("{name}: OK")),
            Err(e) => {
                out::status("✗", &format!("{name}: FAILED"));
                out::error(&e);
                ok = false;
            }
        }
    }

    if !ok {
        return Err(String::new());
    }
    Ok(())
}

pub fn repair(root: &Path) -> Result<()> {
    if let Err(e) = ensure_dependencies() {
        out::warn(&e);
        out::info("run ./install.sh to install missing dependencies");
        return Ok(());
    }
    profiles::migrate_legacy(root)?;
    refresh_sidecars(root)?;
    out::status("✓", "configs repaired. the supervisor will reload");
    Ok(())
}

pub fn reset(root: &Path) -> Result<()> {
    if let Err(e) = ensure_dependencies() {
        out::warn(&e);
        out::info("run ./install.sh to install missing dependencies");
        return Ok(());
    }
    profiles::migrate_legacy(root)?;

    out::section("removing configs:");
    for name in [config::XRAY, config::CONF, config::ACTIVE, config::ORDER] {
        match fs::remove_file(root.join(name)) {
            Ok(()) => out::status("✓", &format!("deleted {name}")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => out::status("→", &format!("{name}: already absent")),
            Err(e) => out::status("✗", &format!("could not delete {name}: {e}")),
        }
    }
    if config::profiles_dir(root).is_dir() {
        match fs::remove_dir_all(config::profiles_dir(root)) {
            Ok(()) => out::status("✓", &format!("deleted {}", config::PROFILES)),
            Err(e) => out::status("✗", &format!("could not delete {}: {e}", config::PROFILES)),
        }
    }

    let ingress = ingress();
    let mut failures: Vec<Error> = Vec::new();
    out::section("recreating configs:");
    for (name, scope) in sidecars() {
        let path = root.join(name);
        if let Err(e) = config::write_json(&path, &config::singbox_config(scope, ingress)) {
            failures.push(format!("{name}: {e}"));
            continue;
        }
        match config::check_singbox(&path) {
            Ok(()) => out::status("✓", &format!("reset {name} (sing-box check OK)")),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }

    for (name, content) in [
        (config::CONF, network::CONF_DEFAULT),
        (config::MODE, "default\n"),
    ] {
        match fs::write(root.join(name), content) {
            Ok(()) => out::status("✓", &format!("reset {name}")),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }

    if !failures.is_empty() {
        out::error("reset finished with errors:");
        for f in &failures {
            out::error(f);
        }
        return Err(String::new());
    }
    out::status("✓", "defaults restored: mode=default, all profiles removed");
    Ok(())
}

/// Run an executable directly, bypassing the proxy when index is 0.
///
/// `xvpn 0 app args...` runs the app with no proxy at all (guaranteed).
/// `xvpn N app args...` runs the app through profile N temporarily,
/// using an isolated xray instance and environment variables.
pub fn run_app(root: &Path, index: u16, trailing: &[String]) -> Result<()> {
    if trailing.is_empty() {
        return Err(
            "usage: xvpn <n> <app> [args...]\n  n=0 runs without proxy, n>0 runs through that profile temporarily"
                .into(),
        );
    }
    let app = &trailing[0];
    let app_args = &trailing[1..];

    let (name, cfg, socks_port, http_port) = if index == 0 {
        (
            "direct".to_string(),
            config::direct_xray_config(10812, 10813),
            10812,
            10813,
        )
    } else {
        let profiles = profiles::list(root);
        let profile = profiles.get(index as usize - 1).ok_or_else(|| {
            format!(
                "no profile {index} (have {}; see `xvpn profile list`)",
                profiles.len()
            )
        })?;
        let mut cfg = config::read_json(&profile.path)?;
        let socks_port = 10810;
        let http_port = 10811;

        if let Some(inbounds) = cfg.get_mut("inbounds").and_then(|i| i.as_array_mut()) {
            for inbound in inbounds {
                if inbound["protocol"] == "socks" {
                    inbound["port"] = serde_json::json!(socks_port);
                } else if inbound["protocol"] == "http" {
                    inbound["port"] = serde_json::json!(http_port);
                }
            }
        }
        (profile.name.clone(), cfg, socks_port, http_port)
    };

    let tmp_dir = std::env::temp_dir().join("xvpn");
    let _ = fs::create_dir_all(&tmp_dir);
    let tmp_id = format!("{}-{}", std::process::id(), index);
    let tmp_path = tmp_dir.join(format!("xray-isolated-{}.json", tmp_id));
    config::write_json(&tmp_path, &cfg)?;

    if index > 0 {
        out::status("✓", &format!("active profile: {name}"));
    }
    out::status(
        "→",
        &format!("starting isolated proxy on ports {socks_port}/{http_port}"),
    );

    let log_path = tmp_dir.join(format!("xray-isolated-{}.log", tmp_id));
    let log_file = fs::File::create(&log_path).map_err(|e| format!("creating log: {e}"))?;

    let mut xray = Command::new("xray")
        .args(["run", "-c"])
        .arg(&tmp_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        .spawn()
        .map_err(|e| format!("could not start xray: {e}"))?;

    // Wait for xray to be ready
    let mut ready = false;
    for _ in 0..50 {
        if TcpStream::connect(format!("127.0.0.1:{socks_port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(Some(status)) = xray.try_wait() {
            let log_content = fs::read_to_string(&log_path).unwrap_or_default();
            let _ = fs::remove_file(&tmp_path);
            let _ = fs::remove_file(&log_path);
            return Err(format!("xray exited with {status}\n{log_content}"));
        }
    }

    if !ready {
        let _ = xray.kill();
        let _ = fs::remove_file(&tmp_path);
        let _ = fs::remove_file(&log_path);
        return Err("timed out waiting for isolated xray to start".into());
    }

    out::status(
        "→",
        &format!(
            "running {app} via {}",
            if index == 0 {
                "direct bypass".to_string()
            } else {
                format!("profile {index}")
            }
        ),
    );

    let mut cmd = Command::new(app);
    cmd.args(app_args);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.env("ALL_PROXY", format!("socks5://127.0.0.1:{socks_port}"));
    cmd.env("all_proxy", format!("socks5://127.0.0.1:{socks_port}"));
    cmd.env("HTTP_PROXY", format!("http://127.0.0.1:{http_port}"));
    cmd.env("http_proxy", format!("http://127.0.0.1:{http_port}"));
    cmd.env("HTTPS_PROXY", format!("http://127.0.0.1:{http_port}"));
    cmd.env("https_proxy", format!("http://127.0.0.1:{http_port}"));

    let status = cmd
        .spawn()
        .map_err(|e| format!("could not run {app}: {e}"))?
        .wait()
        .map_err(|e| format!("{app} failed: {e}"))?;

    let _ = xray.kill();
    let _ = xray.wait();
    let _ = fs::remove_file(&tmp_path);
    let _ = fs::remove_file(&log_path);

    std::process::exit(status.code().unwrap_or(1));
}

pub fn agent(cmd: crate::cli::AgentCmd, _root: &Path) -> Result<()> {
    match cmd {
        crate::cli::AgentCmd::Install => {
            out::status("→", "installing and loading user agent");
            run_make("agent")
        }
        crate::cli::AgentCmd::Stop => {
            out::status("→", "stopping supervisor and proxies");
            run_make("stop")
        }
        crate::cli::AgentCmd::Restart => {
            out::status("→", "restarting supervisor");
            run_make("restart")
        }
    }
}

fn run_make(target: &str) -> Result<()> {
    let status = Command::new("make")
        .arg(target)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("make {target} failed: {e}"))?;

    if !status.success() {
        return Err(format!("make {target} exited with {status}").into());
    }
    Ok(())
}

/// Spawns `tail -f` to watch supervisor, xray and sing-box logs.
pub fn logs() -> Result<()> {
    let logs = [
        config::log_file("xray"),
        config::log_file("sing-box"),
        config::log_file("supervisor"),
    ];
    let mut args = vec!["-f".to_string()];
    for log in &logs {
        // Create files if they don't exist so tail doesn't complain
        if let Some(parent) = log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::OpenOptions::new().create(true).append(true).open(log);
        args.push(log.to_string_lossy().to_string());
    }
    let mut cmd = Command::new("tail");
    cmd.args(&args);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    let status = cmd
        .spawn()
        .map_err(|e| format!("could not tail logs: {e}"))?
        .wait()
        .map_err(|e| format!("tail logs failed: {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}
