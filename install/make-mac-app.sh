#!/usr/bin/env bash
# Build a double-clickable Speedcraft.app for macOS. Run on the Mac:
#   ./install/make-mac-app.sh            # builds target/release and dist/Speedcraft.app
# The app launches `speedcraft ui`, which opens http://127.0.0.1:7700 in your
# browser. Quit from the Terminal window it opens (Ctrl-C) or close the window.
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(uname -s)" = "Darwin" ] || { echo "run this on macOS"; exit 1; }
command -v cargo >/dev/null || { echo "install Rust first: https://rustup.rs"; exit 1; }
echo "==> building release binary"
cargo build --release 2>&1 | tail -2
APP=dist/Speedcraft.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Resources/config"
cp target/release/speedcraft "$APP/Contents/Resources/speedcraft"
cp config/example.toml "$APP/Contents/Resources/config/example.toml"
cat > "$APP/Contents/MacOS/Speedcraft" <<'LAUNCH'
#!/usr/bin/env bash
# Launcher: runs the UI with its data in ~/Library/Application Support/Speedcraft
set -e
HERE="$(cd "$(dirname "$0")/../Resources" && pwd)"
DATA="$HOME/Library/Application Support/Speedcraft"
mkdir -p "$DATA"
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
# Open a Terminal window so logs are visible and Ctrl-C stops everything.
SCRIPT="cd \"$DATA\" && set -a && . \"$DATA/env\" && set +a && export RUST_LOG=\${RUST_LOG:-info} && \"$HERE/speedcraft\" --config \"$DATA/config.toml\" ui"
osascript <<OSA
tell application "Terminal"
  activate
  do script "$SCRIPT"
end tell
OSA
LAUNCH
chmod +x "$APP/Contents/MacOS/Speedcraft"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Speedcraft</string>
  <key>CFBundleDisplayName</key><string>Speedcraft</string>
  <key>CFBundleIdentifier</key><string>dev.speedcraft.app</string>
  <key>CFBundleVersion</key><string>0.2.0</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleExecutable</key><string>Speedcraft</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
echo "==> built $APP"
echo "Move it to /Applications if you like. First launch: right-click → Open (unsigned app)."
echo "Data lives in ~/Library/Application Support/Speedcraft (config.toml, env, ledger.jsonl, agents.json)."
