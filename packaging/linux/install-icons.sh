#!/usr/bin/env bash
# Regenerates every icon the app needs from the single source of truth:
#   crates/xdm-app/assets/xdm-logo-256.png   <- replace this file with your logo
#
# Produces:
#   - the multi-size Windows .ico (embedded into the exe at build time)
#   - the hicolor PNGs + .desktop entry for the Linux taskbar/dock
#
# Requires ImageMagick (magick) for resizing. No root required.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
logo="$repo_root/crates/xdm-app/assets/xdm-rust.png"

[ -f "$logo" ] || { echo "missing $logo" >&2; exit 1; }
command -v magick >/dev/null 2>&1 || { echo "ImageMagick (magick) is required" >&2; exit 1; }

# 1. Windows .ico (explorer/taskbar icon embedded in the exe).
magick "$logo" -define icon:auto-resize=256,128,64,48,32,24,16 \
    "$repo_root/crates/xdm-app/assets/xdm-app.ico"

# 2. Linux: hicolor PNGs so the `xdm-app` app_id resolves in the taskbar.
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
icon_dir="$data_home/icons/hicolor"
apps_dir="$data_home/applications"

for size in 16 24 32 48 64 128 256; do
    dir="$icon_dir/${size}x${size}/apps"
    mkdir -p "$dir"
    magick "$logo" -resize "${size}x${size}" "$dir/xdm-app.png"
done
mkdir -p "$icon_dir/scalable/apps"
cp "$logo" "$icon_dir/scalable/apps/xdm-app.png"

mkdir -p "$apps_dir"
cp "$repo_root/packaging/linux/xdm-app.desktop" "$apps_dir/"

# Refresh caches so the change is visible immediately.
command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -f -t "$icon_dir" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$apps_dir" 2>/dev/null || true

echo "Regenerated .ico from $logo and installed Linux icons to $data_home"
echo "Restart the app (and re-login if the taskbar is stubborn) to see it."
