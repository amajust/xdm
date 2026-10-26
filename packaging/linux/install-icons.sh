#!/usr/bin/env bash
# Regenerates every icon the app needs from the single source of truth:
#   crates/xdm-app/assets/xdm-rust-logo.png   <- replace this file with your logo
#
# Produces:
#   - a trimmed, centered 256 px copy the binaries embed (xdm-logo-256.png)
#   - the multi-size Windows .ico (embedded into the exe at build time)
#   - the hicolor PNGs + .desktop entry for the Linux taskbar/dock
#
# Requires ImageMagick (magick) for resizing. No root required.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
master="$repo_root/crates/xdm-app/assets/xdm-rust-logo.png"
logo="$repo_root/crates/xdm-app/assets/xdm-logo-256.png"

[ -f "$master" ] || { echo "missing $master" >&2; exit 1; }
command -v magick >/dev/null 2>&1 || { echo "ImageMagick (magick) is required" >&2; exit 1; }

# 0. Trimmed, centered square base at 512 px. The fuzz cut removes the
#    soft glow/shadow padding around the artwork (a zero-fuzz trim only
#    removes fully transparent pixels, which made the icon render small);
#    the 480/512 resize keeps a small breathing margin.
base="$(mktemp --suffix=.png)"
magick "$master" -alpha set -fuzz 2% -trim +repage -resize 480x480 \
    -background none -gravity center -extent 512x512 "$base"

# 1. Windows .ico (explorer/taskbar icon embedded in the exe).
magick "$base" -define icon:auto-resize=256,128,64,48,32,24,16 \
    "$repo_root/crates/xdm-app/assets/xdm-app.ico"

# 2. 256 px copy the binaries embed (keeps the exe small and startup fast).
magick "$base" -resize 256x256 "$logo"

# 3. Linux: hicolor PNGs so the `xdm-app` app_id resolves in the taskbar.
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
icon_dir="$data_home/icons/hicolor"
apps_dir="$data_home/applications"

for size in 16 24 32 48 64 128 256; do
    dir="$icon_dir/${size}x${size}/apps"
    mkdir -p "$dir"
    magick "$base" -resize "${size}x${size}" "$dir/xdm-app.png"
done
mkdir -p "$icon_dir/scalable/apps"
cp "$base" "$icon_dir/scalable/apps/xdm-app.png"
rm -f "$base"

mkdir -p "$apps_dir"
cp "$repo_root/packaging/linux/xdm-app.desktop" "$apps_dir/"

# Refresh caches so the change is visible immediately.
command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -f -t "$icon_dir" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$apps_dir" 2>/dev/null || true

echo "Regenerated .ico + hicolor icons from $master and installed to $data_home"
echo "Restart the app (and re-login if the taskbar is stubborn) to see it."
