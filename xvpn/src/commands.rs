//! What each subcommand does.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::cli::{apps_args, sites_args, AppsAction, Mode, ProfileCmd, SitesAction};
use crate::config::{self, Ingress, Scope};
use crate::network;
use crate::profiles;
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
    println!("active profile: {name}");
    // Don't change the mode — whatever was active (on/default) stays.
    if mode == "default" {
        println!("mode stays default");
    } else {
        println!("mode stays {mode}");
    }
    refresh_sidecars(root)?;
    println!("supervisor will reload");
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
    println!("mode: {mode}  active: {active}");

    // Only when there is one: "profile: none" on an install with nothing picked
    // is noise, not information.
    match profiles::active_name(root) {
        Some(name) => {
            let server = profiles::list(root)
                .into_iter()
                .find(|p| p.name == name)
                .and_then(|p| p.server)
                .unwrap_or_else(|| "(unreadable)".into());
            println!("profile: {name} ({server})");
        }
        None => println!("profile: none (run `xvpn import`)"),
    }

    // The routing lists only mean something when selective routing is in effect.
    if mode == "default" {
        print_routing(root);
    }
    Ok(())
}

fn print_routing(root: &Path) {
    let path = root.join(config::SELECTIVE);
    let cfg = match config::read_json(&path) {
        Ok(c) => c,
        Err(_) => {
            println!("routed: unknown (run `xvpn repair`)");
            return;
        }
    };
    // Read from the live config, not the template: what is listed here is what
    // sing-box is actually routing.
    let apps = network::app_names(&cfg);
    let sites = network::site_names(&cfg);

    // Labelled as always-routed, not "via xray". It is a floor baked into every
    // selective config, so listing it beside the lists the user chose would
    // read as if they had chosen it too.
    println!("always proxied (built in):");
    println!("  {}", config::PROXY_TOOLS.join(", "));

    if !apps.is_empty() {
        println!("apps:");
        for app in &apps {
            println!("  {app}");
        }
    }
    if !sites.is_empty() {
        println!("sites (and subdomains):");
        for site in &sites {
            println!("  {site}");
        }
    }
    if apps.is_empty() && sites.is_empty() {
        println!("nothing else routed, so everything goes direct");
    } else {
        println!("everything else direct");
    }
}

pub fn set_mode(root: &Path, mode: Mode) -> Result<()> {
    let mode = mode.as_str();
    fs::write(root.join(config::MODE), mode).map_err(|e| format!("writing mode: {e}"))?;
    println!("mode: {mode}");
    // Give the supervisor time to notice the change and act on it.
    std::thread::sleep(Duration::from_secs(6));
    Ok(())
}

pub fn import(root: &Path, link: Option<&str>, force: bool) -> Result<()> {
    profiles::migrate_legacy(root)?;
    let link = read_link(link)?;
    let name = profiles::import(root, None, &link, force)?;
    println!("saved profile '{name}'");
    refresh_sidecars(root)?;
    println!("\nactive. the supervisor will reload");
    Ok(())
}

pub fn profile(root: &Path, cmd: ProfileCmd) -> Result<()> {
    profiles::migrate_legacy(root)?;
    match cmd {
        ProfileCmd::Add { name, link, force } => {
            let link = read_link(link.as_deref())?;
            let saved = profiles::import(root, Some(&name), &link, force)?;
            println!("saved profile '{saved}'");
            refresh_sidecars(root)?;
            println!("\nactive. the supervisor will reload");
            Ok(())
        }
        ProfileCmd::List => profiles::print_list(root),
        ProfileCmd::Use { name } => {
            let name = profiles::resolve(root, &name)?;
            profiles::validate(root, &name)?;
            profiles::activate(root, &name)?;
            println!("active profile: {name}");
            refresh_sidecars(root)?;
            println!("supervisor will reload");
            Ok(())
        }
        ProfileCmd::Remove { name } => {
            let name = profiles::resolve(root, &name)?;
            profiles::remove(root, &name)?;
            println!("removed profile '{name}'");
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
    let apps = network::listed_apps(&root.join(config::SELECTIVE))?;
    if apps.is_empty() {
        println!("apps: (none)");
    } else {
        println!("apps:");
        for app in apps {
            println!("  - {app}");
        }
    }
    Ok(())
}

pub fn add_app(root: &Path, app: &str) -> Result<()> {
    edit_app(root, true, app)
}

pub fn remove_app(root: &Path, app: &str) -> Result<()> {
    edit_app(root, false, app)
}

fn edit_app(root: &Path, add: bool, app: &str) -> Result<()> {
    announce_edit(
        network::edit_app(&root.join(config::SELECTIVE), add, app),
        add,
        app,
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
    let sites = network::listed_sites(&root.join(config::SELECTIVE))?;
    if sites.is_empty() {
        println!("sites: (none)");
    } else {
        println!("sites (and their subdomains):");
        for site in sites {
            println!("  - {site}");
        }
    }
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
        network::edit_site(&root.join(config::SELECTIVE), add, &shown),
        add,
        &shown,
    )
}

/// "already exists" and "not in the list" are outcomes, not failures.
fn announce_edit(result: Result<()>, add: bool, name: &str) -> Result<()> {
    match result {
        Ok(()) => {
            println!("{} {name}", if add { "Added" } else { "Removed" });
            Ok(())
        }
        Err(e) if network::is_noop(&e) => {
            println!("{e}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Register or forget the current network for selective mode.
///
/// Takes no argument: the DHCP resolver identifies the network, so there is
/// nothing to pick and nothing to mistype. `dns` is accepted by `forget` to drop
/// a resolver for a network you are no longer on.
pub fn use_network(root: &Path, add: bool, dns: Option<&str>) -> Result<()> {
    let msg = if add {
        network::register(root)?
    } else {
        network::unregister(root, dns)?
    };
    println!("{msg}");
    Ok(())
}

/// Curl ifconfig.me through the running xray proxy to verify connectivity.
pub fn check_vpn(root: &Path) -> Result<()> {
    let (name, _) = profiles::resolve_active(root)?;
    println!("checking profile: {name}");

    // Try SOCKS proxy first (port 10808), then HTTP proxy (port 10809).
    for port in [config::SOCKS_PORT, config::HTTP_PORT] {
        let proxy = format!("socks5://127.0.0.1:{port}");
        match run_curl(&proxy) {
            Ok(ip) => {
                println!("via {proxy}: {ip}");
                return Ok(());
            }
            Err(e) => println!("{proxy}: {e}"),
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
        (config::SELECTIVE, Scope::Selective),
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
    let action = match config::read_json(&path) {
        Ok(existing) => {
            let fresh = config::migrate_singbox(&existing, scope, ingress);
            if fresh == existing {
                config::check_singbox(&path).map_err(|e| format!("{name}: {e}"))?;
                println!("{name}: already current");
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
    println!("{name}: {action}, sing-box check OK");
    Ok(())
}

pub fn check(root: &Path) -> Result<()> {
    let mut ok = true;

    match profiles::resolve_active(root) {
        Ok((name, path)) => match config::check_xray(&path) {
            Ok(()) => println!("profile '{name}': OK"),
            Err(e) => {
                println!("profile '{name}': FAILED\n{e}");
                ok = false;
            }
        },
        Err(e) => println!("{e}"),
    }

    for (name, _) in sidecars() {
        let path = root.join(name);
        if !path.exists() {
            println!("{name}: MISSING (run `xvpn repair`)");
            ok = false;
            continue;
        }
        match config::check_singbox(&path) {
            Ok(()) => println!("{name}: OK"),
            Err(e) => {
                println!("{name}: FAILED\n{e}");
                ok = false;
            }
        }
    }

    // Empty error: failures are already printed, so no bare "Error:" prefix.
    if !ok {
        return Err(String::new());
    }
    Ok(())
}

pub fn repair(root: &Path) -> Result<()> {
    profiles::migrate_legacy(root)?;
    refresh_sidecars(root)?;
    println!("\nconfigs repaired. the supervisor will reload");
    Ok(())
}

// Leaves the running tunnel alone (killing root-owned processes needs sudo);
// `make reset` stops the supervisor first, then calls this.
pub fn reset(root: &Path) -> Result<()> {
    profiles::migrate_legacy(root)?;

    for name in [config::XRAY, config::CONF, config::ACTIVE, config::ORDER] {
        match fs::remove_file(root.join(name)) {
            Ok(()) => println!("deleted {name}"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => println!("{name}: already absent"),
            Err(e) => println!("could not delete {name}: {e}"),
        }
    }
    if config::profiles_dir(root).is_dir() {
        match fs::remove_dir_all(config::profiles_dir(root)) {
            Ok(()) => println!("deleted {}", config::PROFILES),
            Err(e) => println!("could not delete {}: {e}", config::PROFILES),
        }
    }

    let ingress = ingress();
    let mut failures: Vec<Error> = Vec::new();
    for (name, scope) in sidecars() {
        let path = root.join(name);
        if let Err(e) = config::write_json(&path, &config::singbox_config(scope, ingress)) {
            failures.push(format!("{name}: {e}"));
            continue;
        }
        match config::check_singbox(&path) {
            Ok(()) => println!("reset {name} (sing-box check OK)"),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }

    for (name, content) in [
        (config::CONF, network::CONF_DEFAULT),
        (config::MODE, "default\n"),
    ] {
        match fs::write(root.join(name), content) {
            Ok(()) => println!("reset {name}"),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }

    if !failures.is_empty() {
        return Err(format!(
            "reset finished with errors:\n  {}",
            failures.join("\n  ")
        ));
    }
    println!("\ndefaults restored: mode=default, all profiles removed");
    Ok(())
}

/// Run an executable directly, bypassing the proxy when index is 0.
///
/// `xvpn 0 app args...` runs the app with no proxy at all (guaranteed).
/// `xvpn N app args...` activates profile N and runs the app through it.
pub fn run_app(root: &Path, index: u16, trailing: &[String]) -> Result<()> {
    if trailing.is_empty() {
        return Err("usage: xvpn <n> <app> [args...]\n  n=0 runs without proxy, n>0 activates that profile".into());
    }
    let app = &trailing[0];
    let app_args = &trailing[1..];

    if index == 0 {
        println!("running {app} without proxy");
    } else {
        use_profile(root, index)?;
        println!("running {app} via profile {index}");
    }

    let mut cmd = Command::new(app);
    cmd.args(app_args);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    // Strip proxy env vars so the child never sees them.
    cmd.env_remove("ALL_PROXY");
    cmd.env_remove("all_proxy");
    cmd.env_remove("HTTP_PROXY");
    cmd.env_remove("http_proxy");
    cmd.env_remove("HTTPS_PROXY");
    cmd.env_remove("https_proxy");
    cmd.env_remove("SOCKS_SERVER");
    cmd.env_remove("socks_server");
    let status = cmd.spawn()
        .map_err(|e| format!("could not run {app}: {e}"))?
        .wait()
        .map_err(|e| format!("{app} failed: {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}
