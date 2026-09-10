#!/bin/bash

DL="$HOME/Desktop/.dl_tmp_$(date +%s)"
STATUS_MSG="✗ Error"

mkdir -p "$DL"

cleanup() {
	find "$DL" -type f ! -name '*.mp4' -delete

	items=("$DL"/*)

	if [ -e "${items[0]}" ]; then
		if [ "${#items[@]}" -eq 1 ]; then
			mv "${items[0]}" "$HOME/Desktop/"
		else
			for item in "$DL"/*; do
				mv "$item" "$HOME/Desktop/"
			done
		fi
	fi

	rm -rf "$DL"
	echo "$STATUS_MSG"
}

trap cleanup EXIT

URL="$(pbpaste | tr -d '\r\n')"

if [ -z "$URL" ] || [[ ! "$URL" =~ ^https?://(www\.)?(youtube\.com|youtu\.be)/ ]]; then
	STATUS_MSG="✗ No link in clipboard"
	exit 1
fi

if [[ "$URL" == *"watch?v="* ]]; then
	PLAYLIST_FLAG="--no-playlist"
else
	PLAYLIST_FLAG="--yes-playlist"
fi

if ! yt-dlp \
	--cookies-from-browser chrome \
	"$PLAYLIST_FLAG" \
	-f "bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]" \
	--merge-output-format mp4 \
	-o "$DL/%(playlist_title|.)s/%(playlist_index|.)s - %(title)s.%(ext)s" \
	"$URL" >/dev/null 2>&1; then
	exit 1
fi

if ! find "$DL" -type f -name '*.mp4' -print -quit | grep -q .; then
	exit 1
fi

STATUS_MSG="✓ Downloaded"
