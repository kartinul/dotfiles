#!/bin/bash
set -e
SRC="$HOME/dotfiles/xvpn"

brew install xray sing-box

chmod +x "$SRC"/bin/*

sudo mkdir -p /usr/local/etc /usr/local/bin
sudo ln -sfn "$SRC" /usr/local/etc/xvpn
for f in xvpn xvpn-daemon; do
  sudo ln -sf "$SRC/bin/$f" "/usr/local/bin/$f"
done

[ -f "$SRC/xvpn.conf" ] || cp "$SRC/xvpn.conf.example" "$SRC/xvpn.conf"
[ -f "$SRC/mode" ] || echo default > "$SRC/mode"

# launchd requires a root-owned real file, not a symlink
sudo cp "$SRC/com.xvpn.plist" /Library/LaunchDaemons/com.xvpn.plist
sudo chown root:wheel /Library/LaunchDaemons/com.xvpn.plist
sudo chmod 644 /Library/LaunchDaemons/com.xvpn.plist
sudo launchctl bootout system/com.xvpn 2>/dev/null || true
sudo rm -f /private/var/db/com.xvpn.launchd/com.xvpn.plist 2>/dev/null || true
sudo launchctl bootstrap system /Library/LaunchDaemons/com.xvpn.plist

echo "Installed. Next: xvpn import, then connect to your wifi and run: xvpn use"
