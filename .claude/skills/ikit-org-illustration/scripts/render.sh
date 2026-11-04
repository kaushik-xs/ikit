#!/usr/bin/env bash
# Render an illustration page to light and dark PNGs at 2x with headless Chrome.
# Usage: render.sh <page.html> [height-css-px]
# Writes <name>.png (light) and <name>-dark.png next to the page.
set -euo pipefail

page="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
height="${2:-990}"
base="${page%.html}"

chrome="${CHROME:-}"
if [ -z "$chrome" ]; then
  for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
           "$(command -v google-chrome || true)" "$(command -v chromium || true)"; do
    [ -n "$c" ] && [ -x "$c" ] && { chrome="$c"; break; }
  done
fi
[ -n "$chrome" ] || { echo "Chrome not found; set CHROME=/path/to/chrome" >&2; exit 1; }

shot() { # $1 = preferredColorScheme (1 light, 0 dark), $2 = output
  "$chrome" --headless=new --disable-gpu --hide-scrollbars \
    --force-device-scale-factor=2 --window-size="1440,$height" \
    --virtual-time-budget=6000 --blink-settings="preferredColorScheme=$1" \
    --screenshot="$2" "file://$page" >/dev/null 2>&1
  echo "$2"
}
shot 1 "$base.png"
shot 0 "$base-dark.png"
