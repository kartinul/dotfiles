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

detect_border() {
	python3 -c "
import sys
from PIL import Image
img=Image.open(sys.argv[1]).convert('RGB')
w,h=img.size
px=img.load()
threshold=35
def d(a,b): return sum((x-y)**2 for x,y in zip(a,b))**0.5
bc=px[0,h//2]
def cb(x):
    ys=range(0,h,max(1,h//30))
    return sum(d(px[x,y],bc) for y in ys)/len(list(ys))<threshold
def rb(y):
    xs=range(0,w,max(1,w//30))
    return sum(d(px[x,y],bc) for x in xs)/len(list(xs))<threshold
l=next((x for x in range(w) if not cb(x)),0)
r=next((x+1 for x in range(w-1,-1,-1) if not cb(x)),w)
t=next((y for y in range(h) if not rb(y)),0)
b=next((y+1 for y in range(h-1,-1,-1) if not rb(y)),h)
print(f'crop={r-l}:{b-t}:{l}:{t}')
" "$1" 2>/dev/null
}

process_image() {
	local img="$1" out="$2" crop
	crop=$(detect_border "$img")
	ffmpeg -y -i "$img" -filter_complex \
		"[0:v]${crop},split=2[bg][fg];[bg]scale=1000:1000:force_original_aspect_ratio=increase,crop=1000:1000,gblur=sigma=20[bg2];[fg]scale=1000:1000:force_original_aspect_ratio=decrease[fg2];[bg2][fg2]overlay=(W-w)/2:(H-h)/2" \
		-q:v 1 "$out" >/dev/null 2>&1
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
