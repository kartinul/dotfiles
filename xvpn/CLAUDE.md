# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

NEVER RUN `xvpn on` because it's gonna disconnect CLAUDE

## Project Overview

**xvpn** — A user-space VPN manager for xray/sing-box. It manages multiple vless:// profiles, runs a supervisor that reconciles config state, and exposes three routing modes: `on` (all traffic), `off` (nothing), and `default` (selective per-app/per-site).

The supervisor runs as a **user agent** (LaunchAgent on macOS, user systemd unit on Linux) — no root daemon. `on` mode needs root only for sing-box's tun interface on macOS, reached via cached `sudo -n`.

## Key Design Decisions

- **User-space only**: State lives in `~/.config/xvpn` (config) and `~/.local/state/xvpn` (runtime). Root is never required except for `on` mode's tun interface on macOS.
- **Supervisor reconciliation**: The supervisor polls every 5s, computes a `Plan` from current mode/profile/network, and starts/stops xray + sing-box to match. It publishes observed state (not intended) to `~/.local/state/xvpn/current`.
- **Single-instance lock**: `flock()` on `~/.local/state/xvpn/supervisor.lock` prevents two supervisors on one root.
- **Config migration**: `migrate_singbox()` carries user-added rules (`process_path_regex`, `domain_suffix`) across schema rewrites.
- **`sudo` caching**: `on` mode probes `sudo -n` in `plan()` and caches the result for 5 minutes to avoid process spawns every poll.

## Commands

```bash
# Build & install
make              # build + install to ~/.local/bin (default)
make build        # compile release binary to ./bin
make install      # install to ~/.local/bin (no sudo)

# Supervisor (user agent)
make agent        # install & load LaunchAgent (macOS) or systemd user unit (Linux)
make unagent      # unload & remove
make restart      # restart in place
make stop         # stop supervisor + all proxies
make supervise    # run supervisor in foreground (Ctrl-C to stop)

# Modes
make on           # route all traffic (needs `sudo -v` on macOS)
make off          # route nothing
make selective    # route listed apps/sites only (default mode)

# Inspection
make status       # show mode, active profile, routed apps/sites
make validate     # validate all configs against installed xray/sing-box
make repair       # rewrite sing-box configs onto current schema
make migrate      # move state from legacy locations
make logs         # tail all logs
make logs-xray    # tail xray log
make logs-singbox # tail sing-box log
make logs-supervisor # tail supervisor log

# Development
make check        # type-check
make check-linux  # type-check for Linux target
make fmt          # format sources
make clippy       # lint
make lint         # fmt check + clippy (warnings denied) + Linux check
make clean        # remove build artifacts
```

## Running Tests

```bash
cargo test                    # all tests
cargo test test_parse_link    # single test
cargo test --lib              # library tests only
```

## Platform-Specific Code

`network.rs` uses `#[cfg(target_os = "macos")]` and `#[cfg(target_os = "linux")]` modules (`imp`) for:
- Wi-Fi interface detection (`networksetup` / `/sys/class/net`)
- DNS server reading (`ipconfig getoption` / `/etc/resolv.conf`)
- Default gateway (`ipconfig getoption` / `/proc/net/route`)

## Configuration Files

- `~/.config/xvpn/mode` — current mode: `on` | `default` | `off`
- `~/.config/xvpn/xray.json` — active profile's xray config (copied from profile)
- `~/.config/xvpn/profiles/*.json` — named vless profiles (owner-only 0600)
- `~/.config/xvpn/order` — profile list order (one name per line)
- `~/.config/xvpn/active` — active profile name
- `~/.config/xvpn/modes/on.json` — sing-box config for `on` mode (global routing)
- `~/.config/xvpn/modes/default.json` — sing-box config for `default` mode (selective)
- `~/.config/xvpn/xvpn.conf` — registered networks (`SELECTIVE_DNS=(...)`)
- `~/.local/state/xvpn/current` — supervisor published state
- `~/.local/state/xvpn/supervisor.lock` — single-instance flock
- `~/.local/state/xvpn/*.log` — per-process logs

## External Dependencies

- `xray` and `sing-box` binaries (installed via `install.sh` / package manager)
- Rust deps: `clap`, `libc`, `signal-hook`, `serde`, `serde_json`
