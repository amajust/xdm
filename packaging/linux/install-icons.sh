#!/usr/bin/env bash
# Installs the XDM app icon into the user's hicolor icon theme and the
# .desktop file, so the Linux taskbar/dock can resolve the `xdm-app`
# window's icon by app_id (the Wayland fallback when the compositor does
# not support xdg-toplevel-icon). No root required.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
src="$repo_root/xdm-app/src/main/resources/images"

icon_dir="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
apps_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

for size in 16 24 32 48 64 128 256; do
    dir="$icon_dir/${size}x${size}/apps"
    mkdir -p "$dir"
    cp "$src/xdm-logo-${size}.png" "$dir/xdm-app.png"
done
# Non-square-cache fallback size used by some panels.
mkdir -p "$icon_dir/scalable/apps"
cp "$src/xdm-logo-256.png" "$icon_dir/scalable/apps/xdm-app.png"

mkdir -p "$apps_dir"
cp "$repo_root/packaging/linux/xdm-app.desktop" "$apps_dir/"

# Refresh the icon/desktop caches so the change is visible immediately.
command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -f -t "$icon_dir" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$apps_dir" 2>/dev/null || true

echo "Installed xdm-app icon + desktop entry to ${XDG_DATA_HOME:-$HOME/.local/share}"
