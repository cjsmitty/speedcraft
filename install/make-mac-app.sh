#!/usr/bin/env bash
# Build a double-clickable Speedcraft.app for macOS. Run on the Mac:
#   ./install/make-mac-app.sh            # builds target/release and dist/Speedcraft.app
# The app launches `speedcraft ui`, which opens http://127.0.0.1:7700 in your
# browser. Quit from the Terminal window it opens (Ctrl-C) or close the window.
# Set SPEEDCRAFT_BIN to package a prebuilt binary (CI builds a universal one)
# instead of compiling here. Set SIGN_IDENTITY to sign with a Developer ID;
# otherwise the app is ad-hoc signed (works, but Gatekeeper asks on first open).
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(uname -s)" = "Darwin" ] || { echo "run this on macOS"; exit 1; }
if [ -n "${SPEEDCRAFT_BIN:-}" ]; then
  BIN="$SPEEDCRAFT_BIN"
else
  command -v cargo >/dev/null || { echo "install Rust first: https://rustup.rs"; exit 1; }
  echo "==> building release binary"
  cargo build --release 2>&1 | tail -2
  BIN=target/release/speedcraft
fi
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
APP=dist/Speedcraft.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Resources/config"
cp "$BIN" "$APP/Contents/Resources/speedcraft"
chmod +x "$APP/Contents/Resources/speedcraft"
cp config/example.toml "$APP/Contents/Resources/config/example.toml"
cat > "$APP/Contents/MacOS/Speedcraft" <<'LAUNCH'
#!/bin/bash
# Speedcraft launcher. Runs the control panel server directly (no Terminal,
# no AppleScript: macOS blocks downloaded apps from driving Terminal, which
# made the app bounce once and vanish). Logs go to ~/Library/Logs/Speedcraft.
set -u
HERE="$(cd "$(dirname "$0")/../Resources" && pwd)"
APP_ROOT="$(cd "$HERE/../.." && pwd)"
DATA="$HOME/Library/Application Support/Speedcraft"
LOGS="$HOME/Library/Logs/Speedcraft"
mkdir -p "$DATA" "$LOGS"
LOG="$LOGS/speedcraft.log"
URL="http://127.0.0.1:7700"

# Downloaded bundles carry a quarantine flag that can stop the nested binary
# from running. Clearing it on our own files is safe and best-effort.
xattr -dr com.apple.quarantine "$APP_ROOT" 2>/dev/null || true
chmod +x "$HERE/speedcraft" 2>/dev/null || true

[ -f "$DATA/config.toml" ] || cp "$HERE/config/example.toml" "$DATA/config.toml"
[ -f "$DATA/env" ] || cat > "$DATA/env" <<'ENV'
# Exchange API keys for LIVE mode on this Mac. Paper mode needs nothing here.
# Also required for live: live = true in config.toml and the line below uncommented.
#SPEEDCRAFT_I_ACCEPT_LOSSES=yes
#COINBASE_API_KEY_NAME=
#COINBASE_API_PRIVATE_KEY=
#KRAKEN_API_KEY=
#KRAKEN_API_SECRET=
ENV
chmod 600 "$DATA/env"

dialog() { osascript -e "display dialog \"$1\" with title \"Speedcraft\" buttons {\"OK\"} default button 1 with icon caution" >/dev/null 2>&1 || true; }

# Already running? Just bring up the panel.
if curl -s -m 2 -o /dev/null "$URL/healthz"; then
  open "$URL"
  exit 0
fi

cd "$DATA" || exit 1
set -a; . "$DATA/env"; set +a
export RUST_LOG="${RUST_LOG:-info}"
{
  echo "=== Speedcraft launched $(date) ==="
  echo "config: $DATA/config.toml"
} >> "$LOG"

# Run the server as this app's process. It opens the browser itself once
# the port is bound. Quit from the panel's "Quit" button.
"$HERE/speedcraft" --config "$DATA/config.toml" ui >> "$LOG" 2>&1
STATUS=$?
if [ $STATUS -ne 0 ]; then
  TAIL=$(tail -n 5 "$LOG" | sed 's/"/\\"/g' | tr '\n' ' ')
  dialog "Speedcraft could not start (exit $STATUS). Last log lines: $TAIL  Full log: $LOG"
fi
exit $STATUS
LAUNCH
chmod +x "$APP/Contents/MacOS/Speedcraft"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Speedcraft</string>
  <key>CFBundleDisplayName</key><string>Speedcraft</string>
  <key>CFBundleIdentifier</key><string>dev.speedcraft.app</string>
  <key>CFBundleVersion</key><string>__VERSION__</string>
  <key>CFBundleShortVersionString</key><string>__VERSION__</string>
  <key>CFBundleExecutable</key><string>Speedcraft</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
</dict></plist>
PLIST
sed -i '' "s/__VERSION__/$VERSION/g" "$APP/Contents/Info.plist"
echo "==> signing"
codesign --force --deep --sign "${SIGN_IDENTITY:--}" ${SIGN_IDENTITY:+--options runtime --timestamp} "$APP"
codesign --verify --deep --strict "$APP" && echo "signature ok (${SIGN_IDENTITY:-ad-hoc})"
echo "==> built $APP (v$VERSION)"
echo "Move it to /Applications if you like. First launch: right-click → Open (not notarized)."
echo "The app runs in the background with no Dock icon; the panel opens in your browser at http://127.0.0.1:7700."
echo "Quit from the panel's Quit button. Logs: ~/Library/Logs/Speedcraft/speedcraft.log"
echo "Data lives in ~/Library/Application Support/Speedcraft (config.toml, env, ledger.jsonl, agents.json)."
