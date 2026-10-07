//! Decides what should be running, and keeps it running.
//!
//! Configs are validated before they are applied: a rejected config keeps the
//! tunnel down and publishes `failed` rather than claiming "on".

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{self, Ingress, Scope};
use crate::network;
use crate::profiles;
use crate::Result;

const POLL: Duration = Duration::from_secs(5);
const STARTUP_GRACE: Duration = Duration::from_secs(3);
const HUNG_AFTER: Duration = Duration::from_secs(30);
// Catches a proxy upgraded underneath a running install.
const REVALIDATE_EVERY: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    None,
    On,
    Selective,
    Failed,
}

impl Want {
    pub fn as_str(self) -> &'static str {
        match self {
            Want::None => "none",
            Want::On => "on",
            Want::Selective => "selective",
            Want::Failed => "failed",
        }
    }

    fn sidecar(self) -> Option<&'static str> {
        match self {
            Want::On => Some(config::ON),
            Want::Selective => Some(config::SELECTIVE),
            _ => None,
        }
    }
}

struct Proxy {
    child: Child,
    started: Instant,
    active: Instant,
}

impl Proxy {
    fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    fn past_grace(&self) -> bool {
        self.started.elapsed() > STARTUP_GRACE
    }

    fn is_hung(&self) -> bool {
        self.active.elapsed() > HUNG_AFTER
    }
}

pub struct Plan {
    pub want: Want,
    pub key: String,
}

pub fn plan(root: &Path) -> Plan {
    let mode = fs::read_to_string(root.join(config::MODE))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "default".to_string());
    let mut key = mode.clone();

    let active = match profiles::resolve_active(root) {
        Ok((name, path)) => {
            if let Err(e) = config::check_xray(&path) {
                return Plan {
                    want: Want::Failed,
                    key: format!("failed:{e}"),
                };
            }
            key.push_str(&format!(":{name}:{}", mtime(&path)));
            Some(name)
        }
        Err(_) => None,
    };

    let want = if active.is_none() {
        Want::None
    } else if mode == "on" {
        Want::On
    } else if mode == "off" {
        Want::None
    } else if network::read_networks(root).matches(&network::dns_servers()) {
        Want::Selective
    } else {
        Want::None
    };

    if let Some(sidecar) = want.sidecar() {
        let path = root.join(sidecar);
        let ingress = ingress();
        let scope = if want == Want::Selective {
            Scope::Selective
        } else {
            Scope::Global
        };
        let _ = scope;
        if !path.is_file() {
            return Plan {
                want: Want::Failed,
                key: format!("failed:{sidecar} missing"),
            };
        }
        if let Err(e) = config::check_singbox(&path) {
            return Plan {
                want: Want::Failed,
                key: format!("failed:{sidecar}: {e}"),
            };
        }
        key.push_str(&format!(":{sidecar}:{}", mtime(&path)));
        let _ = ingress;
    }

    Plan { want, key }
}

fn ingress() -> Ingress {
    if network::supports_gateway() {
        Ingress::TProxy
    } else {
        Ingress::Tun
    }
}

fn mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_path() -> PathBuf {
    config::state_file()
}

fn publish(state: &str) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, format!("{state}\n"));
}

fn log(msg: impl std::fmt::Display) {
    eprintln!("[xvpn] {msg}");
}

fn spawn(program: &str, config_path: &Path) -> Result<Proxy> {
    let log_path = if program == "xray" {
        PathBuf::from("/var/log/xvpn-xray.log")
    } else {
        PathBuf::from("/var/log/xvpn-singbox.log")
    };
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let stderr = file
        .as_ref()
        .map(|f| Stdio::from(f.try_clone().unwrap()))
        .unwrap_or_else(Stdio::null);

    let child = Command::new(program)
        .args(["run", "-c"])
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;

    Ok(Proxy {
        child,
        started: Instant::now(),
        active: Instant::now(),
    })
}

// Stops children on SIGTERM. Without this the supervisor dies on launchd's or
// systemd's stop signal and leaves the proxies running unattended.
#[cfg(unix)]
fn hook_shutdown_signals(flag: &Arc<AtomicBool>) {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    for signal in [SIGTERM, SIGINT, SIGHUP] {
        let _ = signal_hook::flag::register(signal, Arc::clone(flag));
    }
}

#[cfg(not(unix))]
fn hook_shutdown_signals(_flag: &Arc<AtomicBool>) {}

pub fn supervise(root: &Path) -> Result<()> {
    let shutdown = Arc::new(AtomicBool::new(false));
    hook_shutdown_signals(&shutdown);

    log(format!("supervising {}", root.display()));
    let mut running: BTreeMap<&'static str, Proxy> = BTreeMap::new();
    let mut applied: Option<String> = None;
    let mut last_validated = Instant::now() - REVALIDATE_EVERY;

    while !shutdown.load(Ordering::SeqCst) {
        let plan = plan(root);
        let key = plan.key.clone();

        // A crashed or hung proxy is as much a reason to restart as a config change.
        let stale: Vec<(&'static str, &'static str)> = running
            .iter_mut()
            .filter_map(|(name, p)| {
                if p.past_grace() && p.has_exited() {
                    return Some((*name, "exited unexpectedly"));
                }
                if p.past_grace() && p.is_hung() {
                    // try_wait() returns Ok(None) when the process is alive
                    // but stuck — force-kill and restart it.
                    let _ = p.child.kill();
                    return Some((*name, "hung; restarted"));
                }
                None
            })
            .collect();
        for (name, reason) in &stale {
            log(format!("{name}: {reason}"));
            running.remove(name);
            applied = None;
        }

        let changed = applied.as_deref() != Some(key.as_str());
        let revalidate = last_validated.elapsed() >= REVALIDATE_EVERY;

        if changed || revalidate {
            if plan.want == Want::Failed {
                if changed {
                    log(format!(
                        "config rejected, staying down: {}",
                        key.trim_start_matches("failed:")
                    ));
                }
                stop_all(&mut running);
                publish("failed");
            } else {
                if changed {
                    log(format!("applying {}", plan.want.as_str()));
                    stop_all(&mut running);
                    start_for(root, plan.want, &mut running);
                }
                publish(plan.want.as_str());
            }
            applied = Some(key);
            last_validated = Instant::now();
        }

        interruptible_sleep(POLL, &shutdown);
    }

    log("shutting down");
    stop_all(&mut running);
    let _ = fs::remove_file(state_path());
    log("stopped");
    Ok(())
}

fn interruptible_sleep(total: Duration, shutdown: &AtomicBool) {
    const STEP: Duration = Duration::from_millis(200);
    let mut waited = Duration::ZERO;
    while waited < total {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(STEP);
        waited += STEP;
    }
}

fn start_for(root: &Path, want: Want, running: &mut BTreeMap<&'static str, Proxy>) {
    let Ok((name, _)) = profiles::active_config(root) else {
        return;
    };
    log(format!("profile: {name}"));

    match spawn("xray", &root.join(config::XRAY)) {
        Ok(p) => {
            running.insert("xray", Proxy { active: Instant::now(), ..p });
        }
        Err(e) => {
            log(e);
            publish("failed");
            return;
        }
    }

    let Some(sidecar) = want.sidecar() else {
        return;
    };
    match spawn("sing-box", &root.join(sidecar)) {
        Ok(p) => {
            running.insert("sing-box", Proxy { active: Instant::now(), ..p });
        }
        Err(e) => {
            log(e);
            // xray with nothing feeding it is pointless.
            stop_all(running);
            publish("failed");
        }
    }
}

fn stop_all(running: &mut BTreeMap<&'static str, Proxy>) {
    for (_, mut proxy) in std::mem::take(running) {
        let _ = proxy.child.kill();
        let _ = proxy.child.wait();
    }
    // Catch any zombie that outlived the supervisor.
    let _ = Command::new("pkill").arg("-x").arg("xray").output();
    let _ = Command::new("pkill").arg("-x").arg("sing-box").output();
    let _ = fs::remove_file(state_path());
}
