#!/bin/bash
# Installs ftu-daemon (LaunchDaemon running as you) and the HAL plugin, then restarts
# coreaudiod. Run with sudo from the repository: sudo packaging/install.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
USER_NAME="${SUDO_USER:?run with sudo}"
USER_HOME="$(dscl . -read "/Users/$USER_NAME" NFSHomeDirectory | awk '{print $2}')"
SUPPORT="/Library/Application Support/FastTrackUltra"
PLIST="/Library/LaunchDaemons/local.ftu.daemon.plist"
DRIVER="/Library/Audio/Plug-Ins/HAL/FastTrackUltra.driver"

[ -x "$ROOT/daemon/target/release/ftu-daemon" ] || { echo "build the daemon first: cargo build --release (in daemon/)"; exit 1; }
[ -d "$ROOT/plugin/build/FastTrackUltra.driver" ] || { echo "build the plugin first: cmake --build plugin/build"; exit 1; }

echo "Installing daemon to $SUPPORT"
install -d -o root -g wheel "$SUPPORT"
install -m 755 -o root -g wheel "$ROOT/daemon/target/release/ftu-daemon" "$SUPPORT/ftu-daemon"

echo "Installing $PLIST (runs as $USER_NAME)"
sed -e "s|@USER@|$USER_NAME|g" -e "s|@LOG@|$USER_HOME/Library/Logs/ftu-daemon.log|g" \
  "$ROOT/packaging/local.ftu.daemon.plist.in" > "$PLIST"
chown root:wheel "$PLIST"
chmod 644 "$PLIST"
launchctl bootout system/local.ftu.daemon 2>/dev/null || true
# bootout returns before the old job is gone; bootstrap fails with error 5 until then.
for _ in $(seq 1 50); do
  launchctl print system/local.ftu.daemon >/dev/null 2>&1 || break
  sleep 0.1
done
for attempt in 1 2 3 4 5; do
  launchctl bootstrap system "$PLIST" && break
  [ "$attempt" = 5 ] && { echo "launchctl bootstrap failed"; exit 1; }
  sleep 1
done

echo "Installing $DRIVER"
install -d -o root -g wheel /Library/Audio/Plug-Ins/HAL
rm -rf "$DRIVER"
cp -R "$ROOT/plugin/build/FastTrackUltra.driver" "$DRIVER"
chown -R root:wheel "$DRIVER"

echo "Restarting coreaudiod (audio drops for a moment)"
killall coreaudiod || true

echo "Done. Daemon log: $USER_HOME/Library/Logs/ftu-daemon.log"
