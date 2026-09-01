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

process_image() {
  local img="$1" out="$2"
  local crop
  crop=$(ffmpeg -i "$img" -vf "cropdetect=limit=24:round=2:skip=0" -frames:v 1 -f null - 2>&1 | grep -o 'crop=[0-9:]*' | tail -1)
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
    src_img="${mp3s[0]%.mp3}.jpg"
    process_image "$src_img" "$dir/cover.jpg"
    for mp3 in "${mp3s[@]}"; do
      json="${mp3%.mp3}.info.json"
      album=$(get_field "$json" album)
      artist=$(get_field "$json" album_artist)
      [ -z "$artist" ] && artist=$(get_field "$json" artist)
      track=$(get_field "$json" track)
      out="${mp3%.mp3}_tmp.mp3"
      ffmpeg -y -i "$mp3" -i "$dir/cover.jpg" -map 0:0 -map 1:0 -c copy -id3v2_version 3 \
        -metadata album="$album" -metadata artist="$artist" -metadata title="$track" \
        "$out"
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
      artist=$(get_field "$json" artist)
      [ -z "$artist" ] && artist="$channel"
      out="${mp3%.mp3}_tmp.mp3"
      if [ -n "$playlist_title" ]; then
        ffmpeg -y -i "$mp3" -i "$dir/cropped.jpg" -map 0:0 -map 1:0 -c copy -id3v2_version 3 \
          -metadata album="$channel" -metadata artist="$artist" \
          "$out"
      else
        ffmpeg -y -i "$mp3" -i "$dir/cropped.jpg" -map 0:0 -map 1:0 -c copy -id3v2_version 3 \
          -metadata artist="$artist" \
          "$out"
      fi
      mv "$out" "$mp3"
      rm -f "$dir/cropped.jpg" "$img"
    done
  fi
done
