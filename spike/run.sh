#!/usr/bin/env bash
# Runs the #793 spike in a throwaway headless Chrome for Testing profile; cleans up on exit.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=${SPIKE_WORK:-$HOME/.cache/agent-scratch/fastbrowse-ext-spike/run}
CHROME=${SPIKE_CHROME:-$(ls -d $HOME/.cache/ms-playwright/chromium-1248/chrome-linux64)/chrome}
mkdir -p "$WORK"; PROFILE=$(mktemp -d "$WORK/profile.XXXX")
export SPIKE_SOCK="$WORK/host.sock"
[ -e "$SPIKE_SOCK" ] && command rm -- "$SPIKE_SOCK"  # a killed host leaves its socket file behind
mkdir -p "$PROFILE/NativeMessagingHosts"
cat > "$WORK/host-wrapper" <<W
#!/bin/sh
SPIKE_SOCK="$SPIKE_SOCK" exec python3 "$HERE/host.py" "\$@"
W
chmod +x "$WORK/host-wrapper"
cat > "$PROFILE/NativeMessagingHosts/dev.fastbrowse.spike.json" <<M
{"name":"dev.fastbrowse.spike","description":"fastbrowse spike host","path":"$WORK/host-wrapper","type":"stdio",
 "allowed_origins":["chrome-extension://ldancfdkkogpfnhdmghknefcpblendmo/"]}
M
python3 -m http.server 8793 --bind 127.0.0.1 -d "$HERE/fixture" >/dev/null 2>&1 & HTTP=$!
"$CHROME" --headless=new --user-data-dir="$PROFILE" --remote-debugging-port=0 --no-first-run --no-default-browser-check \
  --disable-features=DisableLoadExtensionCommandLineSwitch --load-extension="$HERE/ext" about:blank >"$WORK/chrome.log" 2>&1 & CH=$!
cleanup(){ kill $CH $HTTP 2>/dev/null || true; wait $CH $HTTP 2>/dev/null || true; pkill -f "$HERE/host.py" 2>/dev/null || true; }
trap cleanup EXIT INT TERM
for _ in $(seq 50); do [ -S "$SPIKE_SOCK" ] && [ -f "$PROFILE/DevToolsActivePort" ] && break; sleep 0.2; done
[ -S "$SPIKE_SOCK" ] || { echo "host never started"; tail -5 "$WORK/chrome.log"; cat "$SPIKE_SOCK.log" 2>/dev/null; exit 1; }
uv run --no-project --with websockets python "$HERE/client.py" "$SPIKE_SOCK" "$PROFILE" "http://127.0.0.1:8793/"
echo "host log:"; cat "$SPIKE_SOCK.log"
