#!/usr/bin/env bash

if [ ! command -v ffmpeg ]; then
    exit 1
fi

if [ -d .tmp ]; then
    rm -rf .tmp
fi

mkdir -p .tmp

function video_color {
    color=$1
    length=$2

    ffmpeg -f lavfi -i color=color=$color -t $length .tmp/$color.mp4
}

per_video_seconds=5
colors=(red green blue)
total_seconds=$(echo "$per_video_seconds * ${#colors[@]}" | bc)

for color in "${colors[@]}"; do
    video_color $color $per_video_seconds
done

echo 'colors.mp4:' >> .tmp/colors.stitchspec
echo '    red.mp4' >> .tmp/colors.stitchspec
echo '    green.mp4' >> .tmp/colors.stitchspec
echo '    blue.mp4' >> .tmp/colors.stitchspec

RUST_LOG=debug cargo r --release -- .tmp/colors.stitchspec -i ./.tmp -o ./.tmp -v

echo "There should be a $total_seconds second video in .tmp/colors.mp4 with:"

for color in "${colors[@]}"; do
    echo "  $per_video_seconds seconds of $color"
done

echo "on macos, run: \`open .tmp/colors.mp4\`"
