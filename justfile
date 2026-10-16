# Run everything before committing: `just precommit`.
precommit: fmt deps lint deny slop test

fmt:
    cargo fmt --all

# Dependencies listed in Cargo.toml that the code never uses.
deps:
    cargo machete

# Compiler warnings + clippy (lint config lives in Cargo.toml and clippy.toml).
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Advisories, licenses, banned and duplicate crates (see deny.toml).
deny:
    cargo deny check

# Placeholder comments, hedging, deferrals and stubs left in the code.
slop:
    antislop src

# No doctests: this is a binary crate, and nextest doesn't run them anyway.
test:
    cargo nextest run --all-features

# Rebuild assets/videojs.js, the Video.js 10 elements the player uses, from
# the versions pinned in assets/videojs/package-lock.json. Needs Node.
videojs:
    cd assets/videojs && npm ci && npm run build

# The SHA-256 of each platform's Chrome headless shell zip at `version`, for
# ASSETS and CHROME_VERSION in src/tools.rs. Downloads about 600 MB.
chrome-hashes version:
    for platform in linux64 linux-arm64 mac-arm64 mac-x64 win64; do \
      echo "$platform $(curl -fsSL https://storage.googleapis.com/chrome-for-testing-public/{{version}}/$platform/chrome-headless-shell-$platform.zip | sha256sum | cut -d' ' -f1)"; \
    done

# Render the side panel's clock radio and its hover animation into assets/
# with Blender (see assets/blender/radio.py and radio_motion.py): the still,
# the layers that move, and radio.json, which says where they go. Fetches
# the two fonts the scene sets type in. About 15 minutes on a recent GPU.
radio samples="512":
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(mktemp -d)
    trap 'rm -rf "$out"' EXIT
    mkdir "$out/fonts"
    curl -fsSL -o "$out/fonts/LuckiestGuy.ttf" https://github.com/google/fonts/raw/main/apache/luckiestguy/LuckiestGuy-Regular.ttf
    curl -fsSL -o "$out/fonts/Fredoka.ttf" "https://github.com/google/fonts/raw/main/ofl/fredoka/Fredoka%5Bwdth,wght%5D.ttf"
    blender -b -P assets/blender/radio.py -- "$out/radio.png" "$out/fonts" {{samples}} "$out/motion" > "$out/blender.log" 2>&1 \
      || { tail -30 "$out/blender.log"; exit 1; }
    cwebp -quiet -q 90 -alpha_q 100 -m 6 "$out/radio.png" -o assets/radio.webp
    for layer in lit dome dome-lit knob; do
      cwebp -quiet -q 90 -alpha_q 100 -m 6 "$out/motion/$layer.png" -o "assets/radio-$layer.webp"
    done
    cp "$out/motion/motion.json" assets/radio.json

# Render /tv's television set into assets/ with Blender (see
# assets/blender/tv.py): the set, its glass's reflections, the sprites of
# its buttons, knob and power light, and tv.json, which says where they go.
# Fetches the two fonts the scene sets type in. A few minutes on a recent GPU.
tv samples="256":
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(mktemp -d)
    trap 'rm -rf "$out"' EXIT
    mkdir "$out/fonts"
    curl -fsSL -o "$out/fonts/LuckiestGuy.ttf" https://github.com/google/fonts/raw/main/apache/luckiestguy/LuckiestGuy-Regular.ttf
    curl -fsSL -o "$out/fonts/Fredoka.ttf" "https://github.com/google/fonts/raw/main/ofl/fredoka/Fredoka%5Bwdth,wght%5D.ttf"
    blender -b -P assets/blender/tv.py -- "$out/tv" "$out/fonts" {{samples}} > "$out/blender.log" 2>&1 \
      || { tail -30 "$out/blender.log"; exit 1; }
    for part in tv tv-glass tv-power tv-up tv-down tv-knob tv-led; do
      cwebp -quiet -q 90 -alpha_q 100 -m 6 "$out/tv/$part.png" -o "assets/$part.webp"
    done
    cp "$out/tv/tv.json" assets/tv.json

# Render /tv's bumpers with Blender (see assets/blender/bumpers.py) and
# encode each into assets/bumper-<name>.mp4: H.264 at the 640 x 480 it's
# drawn at, its sting compressed as the day's bumpers were and brought to
# -15 LUFS (two-pass loudnorm, linear), and bumpers.json, which tv.rs reads.
# About ten minutes on a recent GPU.
bumpers samples="48":
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(mktemp -d)
    trap 'rm -rf "$out"' EXIT
    mkdir "$out/fonts"
    curl -fsSL -o "$out/fonts/LuckiestGuy.ttf" https://github.com/google/fonts/raw/main/apache/luckiestguy/LuckiestGuy-Regular.ttf
    curl -fsSL -o "$out/fonts/Fredoka.ttf" "https://github.com/google/fonts/raw/main/ofl/fredoka/Fredoka%5Bwdth,wght%5D.ttf"
    blender -b -P assets/blender/bumpers.py -- "$out/b" "$out/fonts" {{samples}} > "$out/blender.log" 2>&1 \
      || { tail -30 "$out/blender.log"; exit 1; }
    for wav in "$out"/b/*.wav; do
      name=$(basename "$wav" .wav)
      squeeze="acompressor=threshold=-26dB:ratio=5:attack=3:release=150:makeup=8dB"
      measured=$(ffmpeg -hide_banner -nostats -i "$wav" -af "$squeeze,loudnorm=I=-15:TP=-1.5:LRA=3:print_format=json" -f null - 2>&1 | sed -n '/^{/,/^}/p')
      get() { python3 -c "import json,sys; print(json.loads(sys.stdin.read())['$1'])" <<< "$measured"; }
      level="loudnorm=I=-15:TP=-1.5:LRA=3:linear=true:measured_I=$(get input_i):measured_TP=$(get input_tp):measured_LRA=$(get input_lra):measured_thresh=$(get input_thresh):offset=$(get target_offset)"
      ffmpeg -v error -y -framerate 30 -i "$out/b/$name/%04d.png" -i "$wav" \
        -af "$squeeze,$level" -ar 48000 -ac 1 -c:a aac -b:a 160k \
        -c:v libx264 -preset slow -crf 16 -tune animation -pix_fmt yuv420p -g 30 \
        -shortest -movflags +faststart "assets/bumper-$name.mp4"
    done
    cp "$out/b/bumpers.json" assets/bumpers.json
