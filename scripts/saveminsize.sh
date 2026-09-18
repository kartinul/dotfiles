#!/bin/bash
# minsize            -> save current frontmost app's window 1 size
# minsize "AppName"  -> save given app's window 1 size
# minsize --rm "AppName" -> remove entry

set -euo pipefail
JSON="$HOME/.config/yabai/minsizes.json"
mkdir -p "$(dirname "$JSON")"
[ -f "$JSON" ] || echo '{}' > "$JSON"

if [ "${1:-}" = "--rm" ]; then
    tmp=$(mktemp)
    jq --arg app "$2" 'del(.[$app])' "$JSON" > "$tmp" && mv "$tmp" "$JSON"
    exit 0
fi

if [ -n "${1:-}" ]; then
    app="$1"
else
    app=$(osascript -e 'tell application "System Events" to get name of first process whose frontmost is true')
fi

size=$(osascript -e "tell application \"System Events\" to get size of window 1 of process \"$app\"")
w=$(echo "$size" | cut -d',' -f1 | tr -d ' ')
h=$(echo "$size" | cut -d',' -f2 | tr -d ' ')

tmp=$(mktemp)
jq --arg app "$app" --argjson w "$w" --argjson h "$h" \
   '.[$app] = {"width": $w, "height": $h}' "$JSON" > "$tmp" && mv "$tmp" "$JSON"

echo "$app: ${w}x${h}"