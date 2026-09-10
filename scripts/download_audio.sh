#!/bin/bash

DL="$HOME/Desktop/.dl_tmp_$(date +%s)"
STATUS_MSG="✗ Error"

mkdir -p "$DL"

cleanup() {
	find "$DL" -type f ! -name '*.mp3' -delete

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

SQ_THUMB_BIN="$1"
SQ_EXEC=""

if [ -n "$SQ_THUMB_BIN" ] && [ -f "$SQ_THUMB_BIN" ] && [ -x "$SQ_THUMB_BIN" ]; then
	SQ_EXEC="$SQ_THUMB_BIN"
elif command -v sq_thumbnail >/dev/null 2>&1; then
	SQ_EXEC="sq_thumbnail"
else
	STATUS_MSG="✗ sq_thumbnail not found"
	exit 0
fi

URL="$(pbpaste | tr -d '\r\n')"

if [ -z "$URL" ] || [[ ! "$URL" =~ ^https?://(www\.)?(youtube\.com|youtu\.be)/ ]]; then
	STATUS_MSG="✗ No link in clipboard"
	exit 0
fi

if [[ "$URL" == *"watch?v="* ]]; then
	PLAYLIST_FLAG="--no-playlist"
else
	PLAYLIST_FLAG="--yes-playlist"
fi

if ! yt-dlp \
	--cookies-from-browser chrome \
	"$PLAYLIST_FLAG" \
	-x \
	--audio-format mp3 \
	--write-thumbnail \
	--convert-thumbnails jpg \
	--write-info-json \
	-o "$DL/%(playlist_title|.)s/%(title)s.%(ext)s" \
	"$URL" >/dev/null 2>&1; then
	exit 0
fi

if ! find "$DL" -type f -name '*.mp3' -print -quit | grep -q .; then
	exit 0
fi

# 2. Use the dynamically located executable
process_image() {
	local img="$1" out="$2"
	"$SQ_EXEC" "$img" "$out" >/dev/null 2>&1
}

get_field() {
	python3 -c "import json,sys
try:
 d=json.load(open(sys.argv[1])); print(d.get(sys.argv[2]) or '')
except: print('')" "$1" "$2" 2>/dev/null
}

get_album_thumb_url() {
	yt-dlp --flat-playlist --dump-single-json \
		"https://www.youtube.com/playlist?list=$1" 2>/dev/null |
		python3 -c "import json,sys
try:
 d=json.load(sys.stdin); t=[x for x in d.get('thumbnails',[]) if x.get('url')]; t.sort(key=lambda x:(x.get('width') or 0)*(x.get('height') or 0),reverse=True); print(t[0]['url'] if t else '')
except: print('')" 2>/dev/null
}

find "$DL" -name '*.mp3' -print0 |
	xargs -0 -n1 dirname 2>/dev/null |
	sort -u |
	while IFS= read -r dir; do
		mp3s=("$dir"/*.mp3)
		albums=()

		for mp3 in "${mp3s[@]}"; do
			json="${mp3%.mp3}.info.json"
			[ -f "$json" ] && albums+=("$(get_field "$json" album)") || albums+=("")
		done

		first_album="${albums[0]}"
		same_album=true

		for a in "${albums[@]}"; do
			if [ -z "$a" ] || [ "$a" != "$first_album" ]; then
				same_album=false
				break
			fi
		done

		if [ "$same_album" = true ]; then
			first_json="${mp3s[0]%.mp3}.info.json"
			playlist_id=$(get_field "$first_json" playlist_id)
			album_thumb_url=""

			[ -n "$playlist_id" ] && album_thumb_url=$(get_album_thumb_url "$playlist_id")

			if [ -n "$album_thumb_url" ]; then
				curl -s -L "$album_thumb_url" -o "$dir/album_raw.jpg" >/dev/null 2>&1
				src_img="$dir/album_raw.jpg"
			else
				src_img="${mp3s[0]%.mp3}.jpg"
			fi

			[ -f "$src_img" ] && process_image "$src_img" "$dir/cover.jpg"

			for mp3 in "${mp3s[@]}"; do
				json="${mp3%.mp3}.info.json"
				album=$(get_field "$json" album)
				artist=$(get_field "$json" album_artist)
				[ -z "$artist" ] && artist=$(get_field "$json" artist)
				track=$(get_field "$json" track)

				meta_args=()
				[ -n "$album" ] && meta_args+=(-metadata "album=$album")
				[ -n "$artist" ] && meta_args+=(-metadata "artist=$artist")
				[ -n "$track" ] && meta_args+=(-metadata "title=$track")

				out="${mp3%.mp3}_tmp.mp3"

				ffmpeg -y \
					-i "$mp3" \
					-i "$dir/cover.jpg" \
					-map 0:0 \
					-map 1:0 \
					-c copy \
					-id3v2_version 3 \
					"${meta_args[@]}" \
					"$out" >/dev/null 2>&1 && mv "$out" "$mp3"
			done

			find "$dir" -maxdepth 1 -name '*.jpg' ! -name 'cover.jpg' -delete
			rm -f "$dir/cover.jpg"
		else
			for mp3 in "${mp3s[@]}"; do
				img="${mp3%.mp3}.jpg"
				[ -f "$img" ] || continue

				process_image "$img" "$dir/cropped.jpg"

				json="${mp3%.mp3}.info.json"
				channel=$(get_field "$json" channel)
				[ -z "$channel" ] && channel=$(get_field "$json" uploader)
				playlist_title=$(get_field "$json" playlist_title)
				title=$(get_field "$json" title)

				meta_args=()
				[ -n "$playlist_title" ] && meta_args+=(-metadata "album=$playlist_title")
				[ -n "$playlist_title" ] && meta_args+=(-metadata "album_artist=$channel")
				[ -n "$channel" ] && meta_args+=(-metadata "artist=$channel")
				[ -n "$title" ] && meta_args+=(-metadata "title=$title")

				out="${mp3%.mp3}_tmp.mp3"

				ffmpeg -y \
					-i "$mp3" \
					-i "$dir/cropped.jpg" \
					-map 0:0 \
					-map 1:0 \
					-c copy \
					-id3v2_version 3 \
					"${meta_args[@]}" \
					"$out" >/dev/null 2>&1 && mv "$out" "$mp3"

				rm -f "$dir/cropped.jpg" "$img"
			done
		fi
	done

STATUS_MSG="✓ Downloaded"
