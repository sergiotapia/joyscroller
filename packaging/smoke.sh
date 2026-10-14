#!/bin/bash
# Checks a packaged joyscroller on the OS it's running on: its version, that
# its tools download and run here, and that the app starts, serves its UI,
# keeps its data where it should, hands a second launch to the running copy,
# and turns off from the UI without leaving anything running. Runs in CI on
# every package (bash; Git Bash on Windows).
#
#   packaging/smoke.sh <executable> <version>
#
# For a macOS app, <executable> is Joyscroller.app/Contents/MacOS/joyscroller;
# the app itself is launched the way Finder would, with `open`.
set -euo pipefail

exe=$1 version=$2
url=http://127.0.0.1:4774
case "$(uname -s)" in
  Darwin) os=macos ;;
  MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *) os=linux ;;
esac
if [ "$os" = macos ]; then
  app=$(cd "$(dirname "$exe")/../.." && pwd)
  data="$HOME/Library/Application Support/Joyscroller"
  media="$HOME/Movies/Joyscroller"
  log="$data/joyscroller.log"
else
  data=$(cd "$(dirname "$exe")" && pwd)
  media="$data/downloads"
  log="${RUNNER_TEMP:-/tmp}/joyscroller-smoke.log"
fi
show_log() { echo "== the app's log"; cat "$log" 2> /dev/null || true; }
trap show_log ERR

running() {
  if [ "$os" = windows ]; then
    tasklist //FI "IMAGENAME eq joyscroller.exe" | grep -qi joyscroller.exe
  else
    pgrep -x joyscroller > /dev/null
  fi
}

echo "== --version"
got=$("$exe" --version)
echo "$got"
[[ "$got" == "joyscroller $version"* ]]

echo "== tools"
# A macOS app logs to its file, so its output is there rather than here.
"$exe" tools
[ "$os" != macos ] || tail -n 8 "$log"

echo "== launch"
if [ "$os" = macos ]; then
  open "$app"
else
  "$exe" > "$log" 2>&1 &
fi
page=
for _ in $(seq 60); do
  page=$(curl -fsS "$url/" 2> /dev/null) && break
  sleep 2
done
grep -q "– joyscroller</title>" <<< "$page"
grep -q 'action="/quit"' <<< "$page"
echo "the UI answers at $url"
[ -f "$data/joyscroller.db" ]
[ -d "$media" ]
echo "the database is in $data and downloads go to $media"

echo "== second launch"
# Hands over to the running copy (opening its UI) and exits successfully.
"$exe"

echo "== turn off"
curl -fsS -X POST -H "Origin: $url" "$url/quit" > /dev/null
for _ in $(seq 30); do
  running || break
  sleep 1
done
if running; then
  echo "joyscroller is still running after Turn off" >&2
  false
fi
if curl -fsS "$url/" > /dev/null 2>&1; then
  echo "the UI still answers after Turn off" >&2
  false
fi
show_log
echo "smoke test passed"
