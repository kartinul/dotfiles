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

use crate::config::{self, Ingress};
use crate::network;
use crate::profiles;
use crate::Result;

const POLL: Duration = Duration::from_secs(5);
const STARTUP_GRACE: Duration = Duration::from_secs(3);
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
            Want::Selective => Some(config::DEFAULT),
            _ => None,
        }
    }
}

struct Proxy {
    child: Child,
    started: Instant,
    /// Spawned through `sudo`, so it runs as root and our own SIGKILL is
    /// refused. Teardown has to go back through sudo to stop it.
    elevated: bool,
}

impl Proxy {
    fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    fn past_grace(&self) -> bool {
        self.started.elapsed() > STARTUP_GRACE
    }
}

pub struct Plan {
    pub want: Want,
    pub key: String,
}

pub fn plan(root: &Path, sudo_cache: &mut Option<(Instant, bool)>) -> Plan {
    let mode_path = root.join(config::MODE);
    let mode = fs::read_to_string(&mode_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "default".to_string());
    let mut key = format!("{mode}:{}", mtime(&mode_path));

    let active = match profiles::resolve_active(root) {
        Ok((name, path)) => {
            key.push_str(&format!(":{name}:{}", mtime(&path)));
            Some(name)
        }
        Err(e) => {
            // `off` genuinely needs no profile. Every other mode does, so the
            // reason gets reported instead of collapsing into a cheerful
            // `none` that looks like a working tunnel.
            if mode == "off" {
                None
            } else {
                return Plan {
                    want: Want::Failed,
                    key: format!("failed:{e}"),
                };
            }
        }
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
        if !path.is_file() {
            return Plan {
                want: Want::Failed,
                key: format!("failed:{sidecar} missing"),
            };
        }
        key.push_str(&format!(":{sidecar}:{}", mtime(&path)));

        // tun installs routes, which needs root. Probed here rather than left to
        // start-up so the state file explains the block instead of the sidecar
        // exiting every 3 seconds and being restarted forever.
        if ingress() == Ingress::Tun && !config::is_root() {
            let ready = cached_sudo(sudo_cache);
            key.push_str(&format!(":sudo:{ready}"));
            if !ready {
                return Plan {
                    want: Want::Failed,
                    key: "failed:this mode needs root for the tun interface — \
                      run `xvpn supervise` in foreground or configure sing-box in sudoers"
                        .to_string(),
                };
            }
        }
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
        if let Err(e) = fs::create_dir_all(parent) {
            log(format!("cannot create {}: {e}", parent.display()));
            return;
        }
    }
    // Reported rather than dropped: when this failed silently, `xvpn` status
    // showed a stale value from whatever last had write access.
    if let Err(e) = fs::write(&path, format!("{state}\n")) {
        log(format!("cannot write {}: {e}", path.display()));
    }
}

fn log(msg: impl std::fmt::Display) {
    eprintln!("[xvpn] {msg}");
}

/// Exclusive, non-blocking lock held for the supervisor's lifetime.
///
/// Two supervisors on one root are destructive: each `stop_all` kills the
/// other's proxies, and they contend for ports 10808/10809. Refusing to start
/// is the whole fix.
struct SingleInstance {
    _file: fs::File,
}

fn single_instance() -> Result<SingleInstance> {
    let path = config::lock_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: `file` owns a valid descriptor and outlives the call. The kernel
    // drops the lock when the process exits, so a crash cannot strand it.
    let rc = unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err(format!(
                "another xvpn supervisor is already running ({} is locked)\n\
                 stop it first: pkill -f 'xvpn --supervise'",
                path.display()
            ));
        }
        return Err(format!("locking {}: {err}", path.display()));
    }
    Ok(SingleInstance { _file: file })
}

/// Returns true if another process holds the supervisor lock file.
pub fn is_running() -> bool {
    let path = config::lock_file();
    let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) else {
        return false;
    };
    let rc = unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock
            || err.raw_os_error() == Some(libc::EWOULDBLOCK)
            || err.raw_os_error() == Some(libc::EAGAIN)
        {
            return true;
        }
    } else {
        unsafe {
            libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_UN);
        }
    }
    false
}

/// Whether sudo can be used right now without a password prompt.
///
/// The supervisor may be a background agent with no terminal to prompt on, so
/// it must never try: `sudo -n` fails immediately instead of hanging on a
/// prompt nobody can see or answer.
fn sudo_ready() -> bool {
    let check_true = Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if check_true {
        return true;
    }
    Command::new("sudo")
        .args(["-n", "sing-box", "version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Cached `sudo_ready()`, valid for the lifetime of a sudo timestamp.
///
/// The supervisor polls every 5 seconds, and `sudo -n` is a process spawn
/// that is only meaningful once per credential grant. Caching it keeps the
/// idle supervisor quiet instead of spawning a process every cycle.
fn cached_sudo(cache: &mut Option<(Instant, bool)>) -> bool {
    let now = Instant::now();
    if let Some((ts, ready)) = *cache {
        if now.duration_since(ts) < Duration::from_secs(300) {
            return ready;
        }
    }
    let ready = sudo_ready();
    *cache = Some((now, ready));
    ready
}

/// sing-box needs root for tun on macOS; xray only listens on localhost.
///
/// Both must be true for `on` mode to work at all, and neither is discoverable
/// until start-up fails with a bare permission error.
fn sidecar_needs_root() -> bool {
    ingress() == Ingress::Tun && !config::is_root()
}

fn spawn(program: &str, config_path: &Path, elevate: bool) -> Result<Proxy> {
    let log_path = config::log_file(program);
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Failure used to be swallowed into `Stdio::null()`, which sent every
    // proxy error to /dev/null and left the logs looking merely stale.
    let file = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => Some(f),
        Err(e) => {
            log(format!(
                "cannot open {}: {e} — {program} output will be discarded",
                log_path.display()
            ));
            None
        }
    };
    let stderr = file
        .as_ref()
        .map(|f| Stdio::from(f.try_clone().unwrap()))
        .unwrap_or_else(Stdio::null);

    let mut command = if elevate {
        let mut c = Command::new("sudo");
        c.args(["-n", program]);
        c
    } else {
        Command::new(program)
    };
    let child = command
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
        elevated: elevate,
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
    // Ensure external binaries are available before we start loops.
    if let Err(e) = crate::commands::ensure_dependencies() {
        eprintln!("[xvpn] dependency error: {}", e);
        eprintln!("[xvpn] aborting supervisor start – run ./install.sh to install missing tools.");
        return Err(e);
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    hook_shutdown_signals(&shutdown);

    // Taken before any child is spawned: two supervisors reconciling one root
    // kill each other's proxies on every config change.
    let _instance = single_instance()?;

    log(format!("supervising {}", root.display()));
    if let Some(notice) = config::stale_root_notice() {
        log(notice);
    }
    let mut running: BTreeMap<&'static str, Proxy> = BTreeMap::new();
    let mut applied: Option<String> = None;
    let mut last_validated = Instant::now() - REVALIDATE_EVERY;
    // `sudo -n` is a process spawn; caching it across polls keeps the idle
    // supervisor quiet instead of spawning a process every 5 seconds.
    let mut sudo_cache: Option<(Instant, bool)> = None;

    while !shutdown.load(Ordering::SeqCst) {
        let plan = plan(root, &mut sudo_cache);
        let key = plan.key.clone();

        // A crashed proxy is reason to restart.
        let stale: Vec<(&'static str, &'static str)> = running
            .iter_mut()
            .filter_map(|(name, p)| {
                if p.past_grace() && p.has_exited() {
                    Some((*name, "exited unexpectedly"))
                } else {
                    None
                }
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
    // Reset mode to off when supervisor stops to indicate no active routing
    let mode_path = root.join(config::MODE);
    let _ = fs::write(&mode_path, b"off\n");
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
    // Nothing to route: spawning xray here would leave a proxy running that
    // nothing feeds and no mode asked for.
    if want == Want::None {
        return;
    }
    let Ok((name, _)) = profiles::active_config(root) else {
        return;
    };
    log(format!("profile: {name}"));

    match spawn("xray", &root.join(config::XRAY), false) {
        Ok(p) => {
            running.insert("xray", p);
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
    let elevate = sidecar_needs_root();
    if elevate {
        // Already probed in plan(), so this is the cached-credential path.
        log("tun mode needs root — starting sing-box with cached sudo");
    }
    match spawn("sing-box", &root.join(sidecar), elevate) {
        Ok(p) => {
            running.insert("sing-box", p);
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
        if proxy.elevated {
            // `sudo` is a wrapper: killing its PID orphans the real
            // `sing-box`, which keeps hijacking DNS and routing as root.
            // pkill reaches the actual process instead.
            let _ = Command::new("sudo")
                .args(["-n", "pkill", "-x", "sing-box"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = proxy.child.kill();
        let _ = proxy.child.wait();
    }
    // No global `pkill` here. It was meant to catch orphans, but it also killed
    // any *other* supervisor's proxies, which is what turned two stale
    // supervisors into a tunnel that flapped on and off. `make stop` clears
    // leftovers explicitly instead.
    let _ = fs::remove_file(state_path());
}
