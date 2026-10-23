#!/bin/bash
# Removes ftu-daemon and the HAL plugin. Run with sudo: sudo packaging/uninstall.sh
set -euo pipefail
launchctl bootout system/local.ftu.daemon 2>/dev/null || true
rm -f /Library/LaunchDaemons/local.ftu.daemon.plist
rm -rf "/Library/Application Support/FastTrackUltra"
rm -rf /Library/Audio/Plug-Ins/HAL/FastTrackUltra.driver
killall coreaudiod || true
echo "Removed."
