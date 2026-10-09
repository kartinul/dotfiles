#!/bin/bash
# Install xvpn for the current platform.
#
#   macOS   brew + a user LaunchAgent
#   Linux   distro package manager + a user systemd unit
#
# Nothing here needs sudo except installing distro packages. The supervisor runs
# as you: there is no root daemon, because a root supervisor wrote its state to
# root-owned paths that no unprivileged xvpn command could then read or rewrite.
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OS="$(uname -s)"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found in PATH"; }

# --- dependencies ----------------------------------------------------------
# xray and sing-box are the data and ingress planes; neither is bundled.
install_deps() {
  case "$OS" in
    Darwin)
      need brew
      log "installing xray + sing-box (brew)"
      brew install xray sing-box
      ;;
    Linux)
      if command -v pacman >/dev/null 2>&1; then
        log "installing xray + sing-box (pacman)"
        sudo pacman -S --needed --noconfirm xray sing-box
      elif command -v apt-get >/dev/null 2>&1; then
        log "installing sing-box (apt); xray often needs its own repo"
        sudo apt-get install -y sing-box || \
          die "sing-box unavailable via apt — see https://sing-box.sagernet.org/installation/"
        command -v xray >/dev/null 2>&1 || \
          die "xray not found — install from https://github.com/XTLS/Xray-core#installation"
      elif command -v dnf >/dev/null 2>&1; then
        log "installing sing-box (dnf)"
        sudo dnf install -y sing-box || die "sing-box unavailable via dnf"
        command -v xray >/dev/null 2>&1 || die "xray not found in PATH"
      else
        die "no supported package manager found (pacman/apt-get/dnf)"
      fi
      ;;
    *) die "unsupported OS: $OS" ;;
  esac
}

# --- supervisor ------------------------------------------------------------

install_macos_agent() {
  log "installing the user LaunchAgent"
  make -C "$SRC" agent
}

install_linux_agent() {
  log "installing the user systemd unit"
  make -C "$SRC" agent
}

# --- main ------------------------------------------------------------------

install_deps

log "building and installing"
make -C "$SRC" build

BINDIR="$HOME/.local/bin"
STATE="$HOME/.local/state/xvpn"
mkdir -p "$BINDIR" "$STATE"
rm -f "$BINDIR/xvpn"
cp -f "$SRC/bin/xvpn" "$BINDIR/xvpn"
chmod +x "$BINDIR/xvpn"
if [ "$OS" = "Darwin" ]; then
  codesign -s - --force "$BINDIR/xvpn" 2>/dev/null || true
fi

"$BINDIR/xvpn" repair

case "$OS" in
  Darwin) install_macos_agent ;;
  Linux)  install_linux_agent ;;
esac

cat <<EOF

installed on $OS, as the current user (no root daemon).

  ~/.config/xvpn            profiles, mode, generated configs
  ~/.local/state/xvpn       runtime state and logs

  xvpn import 'vless://...'    import a link
  xvpn set default             selective routing (per app)
  xvpn apps set Telegram       route one app
  xvpn sites set netflix.com   route one site (and its subdomains)
  xvpn on                      route ALL traffic (needs sudo -v on macOS)
  xvpn                         current mode + what goes via xray
  xvpn logs                    tail the supervisor and proxy logs
  xvpn --help                  everything else

Full-system routing in 'on' mode needs root for sing-box's tun interface. Run
'sudo -v' once; the supervisor then reuses that cached grant instead of
prompting, since a background agent cannot answer a password prompt.
EOF