#!/usr/bin/env bash
# Install Speedcraft as a headless agent on a Linux box (Debian/Ubuntu/Fedora,
# systemd). Run as a user with sudo. Idempotent: re-run to upgrade.
#
#   curl -fsSL https://raw.githubusercontent.com/cjsmitty/speedcraft/main/install/install-agent.sh | bash
#   # or, from a checkout:   ./install/install-agent.sh
#
# Environment overrides:
#   SPEEDCRAFT_REPO      git URL to build from (default: this repo)
#   SPEEDCRAFT_REF       branch/tag to build (default: main)
#   SPEEDCRAFT_BIN       path to a prebuilt binary to install instead of building
#   SPEEDCRAFT_LISTEN    bind address (default 127.0.0.1:7777; use 0.0.0.0:7777 only on a private network)
#   SPEEDCRAFT_NAME      agent display name (default: hostname)
#   SPEEDCRAFT_AUTOSTART paper | live | none (default paper)
set -euo pipefail

REPO="${SPEEDCRAFT_REPO:-https://github.com/cjsmitty/speedcraft.git}"
REF="${SPEEDCRAFT_REF:-main}"
LISTEN="${SPEEDCRAFT_LISTEN:-127.0.0.1:7777}"
NAME="${SPEEDCRAFT_NAME:-$(hostname -s 2>/dev/null || hostname)}"
AUTOSTART="${SPEEDCRAFT_AUTOSTART:-paper}"
SVC_USER=speedcraft
ETC=/etc/speedcraft
VAR=/var/lib/speedcraft
BIN=/usr/local/bin/speedcraft

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v systemctl >/dev/null || die "systemd is required"
[ "$(id -u)" -eq 0 ] || SUDO=sudo
SUDO="${SUDO:-}"

if [ -n "${SPEEDCRAFT_BIN:-}" ]; then
  say "installing prebuilt binary from $SPEEDCRAFT_BIN"
  $SUDO install -m 0755 "$SPEEDCRAFT_BIN" "$BIN"
else
  if ! command -v cargo >/dev/null; then
    say "installing Rust toolchain (rustup)"
    command -v curl >/dev/null || { $SUDO apt-get update -qq && $SUDO apt-get install -y -qq curl; }
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if ! command -v cc >/dev/null; then
    say "installing C compiler (needed by the Rust linker)"
    if command -v apt-get >/dev/null; then $SUDO apt-get update -qq && $SUDO apt-get install -y -qq build-essential git pkg-config;
    elif command -v dnf >/dev/null; then $SUDO dnf install -y gcc git;
    else die "install a C compiler and git, then re-run"; fi
  fi
  command -v git >/dev/null || die "git is required"
  SRC=$(mktemp -d)
  trap 'rm -rf "$SRC"' EXIT
  say "cloning $REPO ($REF)"
  git clone --depth 1 --branch "$REF" "$REPO" "$SRC/speedcraft"
  say "building release binary (this takes a few minutes the first time)"
  (cd "$SRC/speedcraft" && cargo build --release --locked 2>&1 | tail -3)
  $SUDO install -m 0755 "$SRC/speedcraft/target/release/speedcraft" "$BIN"
fi
say "installed $($BIN --version)"

if ! id "$SVC_USER" >/dev/null 2>&1; then
  say "creating system user $SVC_USER"
  $SUDO useradd --system --home "$VAR" --shell /usr/sbin/nologin "$SVC_USER" 2>/dev/null || $SUDO useradd --system --home "$VAR" --shell /sbin/nologin "$SVC_USER"
fi
$SUDO mkdir -p "$ETC" "$VAR"

if [ ! -f "$ETC/config.toml" ]; then
  say "writing default config to $ETC/config.toml (paper mode)"
  $BIN example-config | $SUDO tee "$ETC/config.toml" >/dev/null
fi
if [ ! -f "$ETC/agent.env" ]; then
  say "writing $ETC/agent.env (put exchange API keys here; chmod 600)"
  $SUDO tee "$ETC/agent.env" >/dev/null <<'ENV'
# Exchange credentials for LIVE mode. Leave empty for paper mode.
# Also required for live: live = true in config.toml AND SPEEDCRAFT_I_ACCEPT_LOSSES=yes below.
#SPEEDCRAFT_I_ACCEPT_LOSSES=yes
#COINBASE_API_KEY_NAME=organizations/.../apiKeys/...
#COINBASE_API_PRIVATE_KEY=-----BEGIN EC PRIVATE KEY-----\n...\n-----END EC PRIVATE KEY-----
#KRAKEN_API_KEY=
#KRAKEN_API_SECRET=
#BINANCE_US_API_KEY=
#BINANCE_US_API_SECRET=
RUST_LOG=info
ENV
fi
if [ ! -f "$ETC/agent.token" ]; then
  say "generating agent token"
  head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n' | $SUDO tee "$ETC/agent.token" >/dev/null
  echo | $SUDO tee -a "$ETC/agent.token" >/dev/null
fi
$SUDO chown -R "$SVC_USER:$SVC_USER" "$ETC" "$VAR"
$SUDO chmod 700 "$ETC" "$VAR"
$SUDO chmod 600 "$ETC/agent.env" "$ETC/agent.token"

AUTO_ARG=""
[ "$AUTOSTART" != "none" ] && AUTO_ARG="--autostart $AUTOSTART"
say "writing systemd unit"
$SUDO tee /etc/systemd/system/speedcraft-agent.service >/dev/null <<UNIT
[Unit]
Description=Speedcraft trading agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SVC_USER
Group=$SVC_USER
WorkingDirectory=$VAR
EnvironmentFile=$ETC/agent.env
ExecStart=$BIN --config $ETC/config.toml agent --listen $LISTEN --name "$NAME" --token-file $ETC/agent.token $AUTO_ARG
Restart=always
RestartSec=5
# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=$VAR $ETC
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT

$SUDO systemctl daemon-reload
$SUDO systemctl enable --now speedcraft-agent.service
sleep 2
if $SUDO systemctl is-active --quiet speedcraft-agent.service; then
  say "agent is running"
else
  $SUDO journalctl -u speedcraft-agent.service -n 30 --no-pager
  die "agent failed to start; see log above"
fi

TOKEN=$($SUDO cat "$ETC/agent.token")
cat <<DONE

Speedcraft agent installed.

  Name:     $NAME
  Listen:   $LISTEN
  Config:   $ETC/config.toml      (edit, then: sudo systemctl restart speedcraft-agent)
  Secrets:  $ETC/agent.env
  Ledger:   $VAR/ledger.jsonl
  Logs:     sudo journalctl -u speedcraft-agent -f
  Token:    $TOKEN

Add it in the Mac app (Remote agents → Add agent):
  - If LISTEN is 127.0.0.1, first open a tunnel from your Mac:
        ssh -N -L 7777:127.0.0.1:7777 $(whoami)@$(hostname -I 2>/dev/null | awk '{print $1}')
    then use URL http://127.0.0.1:7777
  - If this box is on a private network / Tailscale, use http://<this-box-ip>:7777
  - Paste the token above.
DONE
