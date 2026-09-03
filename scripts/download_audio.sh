#!/bin/bash

DL=~/Desktop/dl_$(date +%s)
mkdir -p "$DL"

cleanup() {
  find "$DL" -name '*.info.json' -delete
  items=("$DL"/*)
  if [ -e "${items[0]}" ]; then
    if [ "${#items[@]}" -eq 1 ]; then
      mv "${items[0]}" ~/Desktop/
    else
      out=~/Desktop/dl_result_$(date +%s)
      mkdir -p "$out"
      mv "$DL"/* "$out"/
    fi
  fi
  rm -rf "$DL"
}
trap cleanup EXIT

URL="$(pbpaste)"

if [[ "$URL" == *"watch?v="* ]]; then
  PLAYLIST_FLAG="--no-playlist"
else
  PLAYLIST_FLAG="--yes-playlist"
fi

yt-dlp --cookies-from-browser chrome $PLAYLIST_FLAG -x --audio-format mp3 \
  --write-thumbnail --convert-thumbnails jpg --write-info-json --ignore-errors \
  -o "$DL"/'%(playlist_title|.)s/%(title)s.%(ext)s' "$URL"

detect_border() {
  python3 -c "
import sys
from PIL import Image

img = Image.open(sys.argv[1]).convert('RGB')
w, h = img.size
px = img.load()
threshold = 35

def color_dist(c1, c2):
    return sum((a-b)**2 for a,b in zip(c1,c2)) ** 0.5

border_color = px[0, h//2]

def col_is_border(x):
    samples = [px[x, y] for y in range(0, h, max(1, h//30))]
    avg = sum(color_dist(c, border_color) for c in samples) / len(samples)
    return avg < threshold

def row_is_border(y):
    samples = [px[x, y] for x in range(0, w, max(1, w//30))]
    avg = sum(color_dist(c, border_color) for c in samples) / len(samples)
    return avg < threshold

left = 0
for x in range(w):
    if not col_is_border(x):
        left = x
        break

right = w
for x in range(w-1, -1, -1):
    if not col_is_border(x):
        right = x+1
        break

top = 0
for y in range(h):
    if not row_is_border(y):
        top = y
        break

bottom = h
for y in range(h-1, -1, -1):
    if not row_is_border(y):
        bottom = y+1
        break

cw, ch = right-left, bottom-top
if cw <= 0 or ch <= 0:
    print(f'crop={w}:{h}:0:0')
else:
    print(f'crop={cw}:{ch}:{left}:{top}')
" "$1"
}

process_image() {
  local img="$1" out="$2"
  local crop
  crop=$(detect_border "$img")
  local pre=""
  [ -n "$crop" ] && pre="${crop},"
  ffmpeg -y -i "$img" -filter_complex \
    "[0:v]${pre}split=2[bg][fg];[bg]scale=1000:1000:force_original_aspect_ratio=increase,crop=1000:1000,gblur=sigma=20[bg2];[fg]scale=1000:1000:force_original_aspect_ratio=decrease[fg2];[bg2][fg2]overlay=(W-w)/2:(H-h)/2" \
    -q:v 1 "$out"
}

get_field() {
  python3 -c "import json,sys
try:
  d=json.load(open(sys.argv[1]))
  print(d.get(sys.argv[2]) or '')
except Exception:
  print('')" "$1" "$2"
}

get_album_thumb_url() {
  local playlist_id="$1"
  yt-dlp --flat-playlist --dump-single-json "https://www.youtube.com/playlist?list=$playlist_id" 2>/dev/null | python3 -c "
import json,sys
try:
  d=json.load(sys.stdin)
  thumbs=[t for t in d.get('thumbnails',[]) if t.get('url')]
  thumbs.sort(key=lambda t:(t.get('width') or 0)*(t.get('height') or 0), reverse=True)
  print(thumbs[0]['url'] if thumbs else '')
except Exception:
  print('')"
}

find "$DL" -name '*.mp3' -print0 | xargs -0 -n1 dirname | sort -u | while IFS= read -r dir; do
  mp3s=("$dir"/*.mp3)
  albums=()
  for mp3 in "${mp3s[@]}"; do
    json="${mp3%.mp3}.info.json"
    [ -f "$json" ] && albums+=("$(get_field "$json" album)") || albums+=("")
  done

  first_album="${albums[0]}"
  same_album=true
  for a in "${albums[@]}"; do
    [ -z "$a" ] && same_album=false && break
    [ "$a" != "$first_album" ] && same_album=false && break
  done

  if [ "$same_album" = true ]; then
    first_json="${mp3s[0]%.mp3}.info.json"
    playlist_id=$(get_field "$first_json" playlist_id)
    album_thumb_url=""
    [ -n "$playlist_id" ] && album_thumb_url=$(get_album_thumb_url "$playlist_id")

    if [ -n "$album_thumb_url" ]; then
      curl -s -L "$album_thumb_url" -o "$dir/album_raw.jpg"
      src_img="$dir/album_raw.jpg"
    else
      src_img="${mp3s[0]%.mp3}.jpg"
    fi
    process_image "$src_img" "$dir/cover.jpg"
    rm -f "$dir/album_raw.jpg"

    for mp3 in "${mp3s[@]}"; do
      json="${mp3%.mp3}.info.json"
      album=$(get_field "$json" album)
      artist=$(get_field "$json" album_artist)
      [ -z "$artist" ] && artist=$(get_field "$json" artist)
      track=$(get_field "$json" track)
      meta_args=()
      [ -n "$album" ]  && meta_args+=(-metadata "album=$album")
      [ -n "$artist" ] && meta_args+=(-metadata "artist=$artist")
      [ -n "$track" ]  && meta_args+=(-metadata "title=$track")
      out="${mp3%.mp3}_tmp.mp3"
      ffmpeg -y -i "$mp3" -i "$dir/cover.jpg" -map 0:0 -map 1:0 -c copy -id3v2_version 3 \
        "${meta_args[@]}" "$out"
      mv "$out" "$mp3"
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
      meta_args=()
      [ -n "$playlist_title" ] && meta_args+=(-metadata "album=$playlist_title")
      [ -n "$channel" ] && meta_args+=(-metadata "artist=$channel")
      out="${mp3%.mp3}_tmp.mp3"
      ffmpeg -y -i "$mp3" -i "$dir/cropped.jpg" -map 0:0 -map 1:0 -c copy -id3v2_version 3 \
        "${meta_args[@]}" "$out"
      mv "$out" "$mp3"
      rm -f "$dir/cropped.jpg" "$img"
    done
  fi
done