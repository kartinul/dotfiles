#!/bin/bash

open -a "Crisp"
trap "killall Crisp 2>/dev/null" EXIT INT TERM
sleep 5


ID=$(swift -e '
import CoreGraphics
var count: UInt32 = 0
CGGetOnlineDisplayList(0, nil, &count)
var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
CGGetOnlineDisplayList(count, &ids, &count)
if let lastID = ids.last { print(lastID) }
	')

if [ -z "$ID" ] || [ "$ID" -eq 1 ]; then
	echo "Crisp failed to create virtual display. Quitting..."
	exit 0
fi

sed -i '' "s/^output_name[[:space:]]*=.*/output_name = $ID/" ~/.config/sunshine/sunshine.conf

sunshine
