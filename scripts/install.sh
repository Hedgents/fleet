#!/usr/bin/env sh
# 01fi worker daemon installer.
#
# Usage:
#   curl -fsSL https://01fi.dev/install.sh | sh -s -- \
#     --fleet-id <hex> --role <role> --fleet-token <hex>
#
#   curl -fsSL https://01fi.dev/install.sh | sh -s -- \
#     --uninstall --role <role>
#
# Roles: multiply | hedgedJlp | stableFloor | riskWatcher | researcher
#
# What it does (install path):
#   1. Detect OS/arch
#   2. Download the matching prebuilt binary from the latest GitHub release
#   3. Verify SHA256 against the release manifest
#   4. Install binaries to ~/.zerox1-defi/bin/
#   5. Generate a fresh Solana keypair for this role
#   6. Write fleet token to ~/.zerox1-defi/fleet.token (mode 0600)
#   7. Install + start a launchd LaunchAgent (macOS) or systemd user unit (Linux)
#   8. Print the wallet pubkey + funding instructions
#
# Defense-in-depth choices:
#   - Pure POSIX sh; no bash-isms
#   - All paths quoted
#   - SHA256 verification of every binary download
#   - Token file is 0600
#   - Wallet file is 0600
#   - Refuses to overwrite an existing role's wallet without --force
#   - User-scoped install (no sudo, no system-wide pollution)

set -eu

# ── Defaults ────────────────────────────────────────────────────────────────

REPO="${ZEROX1_REPO:-chumyin/01fi}"
INSTALL_DIR="${ZEROX1_HOME:-$HOME/.zerox1-defi}"
BIN_DIR="$INSTALL_DIR/bin"
GH_API="https://api.github.com/repos/$REPO/releases/latest"

DAEMON_PORT_BASE=9091  # multiply=9091, hedgedJlp=9092, ...

ACTION=install
FLEET_ID=""
FLEET_TOKEN=""
ROLE=""
FORCE=0
VERSION=""  # optional pin (default: latest)

# ── Argument parsing ────────────────────────────────────────────────────────

usage() {
    cat <<EOF
01fi worker daemon installer

USAGE:
  install.sh --fleet-id <hex> --role <role> --fleet-token <hex>
  install.sh --uninstall --role <role>

OPTIONS:
  --fleet-id        16-hex fleet identifier
  --fleet-token     64-hex fleet shared secret
  --role            multiply | hedgedJlp | stableFloor | riskWatcher | researcher
  --version         install a specific tag (default: latest)
  --force           overwrite existing wallet for this role
  --uninstall       remove the service and binary for the given role
  -h, --help        show this help

ENV:
  ZEROX1_HOME       install directory (default: \$HOME/.zerox1-defi)
  ZEROX1_REPO       GitHub repo (default: chumyin/01fi)
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --fleet-id)    FLEET_ID="${2:-}";    shift 2 ;;
        --fleet-token) FLEET_TOKEN="${2:-}"; shift 2 ;;
        --role)        ROLE="${2:-}";        shift 2 ;;
        --version)     VERSION="${2:-}";     shift 2 ;;
        --force)       FORCE=1;              shift ;;
        --uninstall)   ACTION=uninstall;     shift ;;
        -h|--help)     usage; exit 0 ;;
        *)             echo "ERROR: unknown arg: $1" >&2; usage; exit 2 ;;
    esac
done

# ── Helpers ─────────────────────────────────────────────────────────────────

log()  { printf '\033[1;36m%s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m⚠\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

require() {
    command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1"
}

valid_role() {
    case "$1" in
        multiply|hedgedJlp|stableFloor|riskWatcher|researcher) return 0 ;;
        *) return 1 ;;
    esac
}

port_for_role() {
    case "$1" in
        multiply)    echo $((DAEMON_PORT_BASE + 0)) ;;
        hedgedJlp)   echo $((DAEMON_PORT_BASE + 1)) ;;
        stableFloor) echo $((DAEMON_PORT_BASE + 2)) ;;
        riskWatcher) echo $((DAEMON_PORT_BASE + 3)) ;;
        researcher)  echo $((DAEMON_PORT_BASE + 4)) ;;
    esac
}

detect_target() {
    OS=$(uname -s)
    ARCH=$(uname -m)
    case "$OS-$ARCH" in
        Darwin-arm64)  echo "darwin-arm64" ;;
        Darwin-x86_64) echo "darwin-x64"   ;;
        Linux-x86_64)  echo "linux-x64"    ;;
        Linux-aarch64) echo "linux-arm64"  ;;
        *) err "unsupported OS/arch: $OS-$ARCH (build from source)" ;;
    esac
}

sha256_of() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# ── Validate inputs ─────────────────────────────────────────────────────────

[ -n "$ROLE" ] || { usage; err "--role is required"; }
valid_role "$ROLE" || err "invalid role: $ROLE"

if [ "$ACTION" = install ]; then
    [ -n "$FLEET_ID" ]    || err "--fleet-id is required for install"
    [ -n "$FLEET_TOKEN" ] || err "--fleet-token is required for install"
    # Cheap sanity-check format
    case "$FLEET_ID" in
        [0-9a-fA-F]*) [ "${#FLEET_ID}" -eq 16 ] || err "fleet-id must be 16 hex chars (got ${#FLEET_ID})" ;;
        *) err "fleet-id must be hex" ;;
    esac
    case "$FLEET_TOKEN" in
        [0-9a-fA-F]*) [ "${#FLEET_TOKEN}" -eq 64 ] || err "fleet-token must be 64 hex chars (got ${#FLEET_TOKEN})" ;;
        *) err "fleet-token must be hex" ;;
    esac
fi

require curl
require uname
require tar
require mkdir
require chmod

# ── Uninstall path ──────────────────────────────────────────────────────────

uninstall_role() {
    OS=$(uname -s)
    case "$OS" in
        Darwin)
            PLIST="$HOME/Library/LaunchAgents/world.zerox1.defi.$ROLE.plist"
            if [ -f "$PLIST" ]; then
                launchctl unload "$PLIST" 2>/dev/null || true
                rm -f "$PLIST"
                ok "removed launchd: world.zerox1.defi.$ROLE"
            else
                warn "no launchd unit for $ROLE"
            fi
            ;;
        Linux)
            UNIT="$HOME/.config/systemd/user/zerox1-defi-$ROLE.service"
            if [ -f "$UNIT" ]; then
                systemctl --user stop    "zerox1-defi-$ROLE.service" 2>/dev/null || true
                systemctl --user disable "zerox1-defi-$ROLE.service" 2>/dev/null || true
                rm -f "$UNIT"
                systemctl --user daemon-reload
                ok "removed systemd: zerox1-defi-$ROLE.service"
            else
                warn "no systemd unit for $ROLE"
            fi
            ;;
    esac
    log "Wallet preserved at $INSTALL_DIR/$ROLE-wallet.json"
    log "To fully wipe: rm -rf '$INSTALL_DIR'"
}

if [ "$ACTION" = uninstall ]; then
    uninstall_role
    exit 0
fi

# ── Install path ────────────────────────────────────────────────────────────

TARGET=$(detect_target)
log "target: $TARGET"

# Resolve manifest URL
if [ -n "$VERSION" ]; then
    MANIFEST_URL="https://github.com/$REPO/releases/download/$VERSION/manifest.json"
else
    log "fetching latest release info from GitHub…"
    require_python_or_jq() {
        if command -v python3 >/dev/null; then echo python3
        elif command -v python  >/dev/null; then echo python
        elif command -v jq      >/dev/null; then echo jq
        else err "need python3 or jq to parse GitHub API response"
        fi
    }
    PARSER=$(require_python_or_jq)
    REL_JSON=$(curl -fsSL -H "Accept: application/vnd.github+json" "$GH_API")
    if [ "$PARSER" = "jq" ]; then
        TAG=$(printf '%s' "$REL_JSON" | jq -r '.tag_name')
    else
        TAG=$(printf '%s' "$REL_JSON" | "$PARSER" -c "import sys,json;print(json.load(sys.stdin)['tag_name'])")
    fi
    [ -n "$TAG" ] && [ "$TAG" != "null" ] || err "could not resolve latest tag"
    MANIFEST_URL="https://github.com/$REPO/releases/download/$TAG/manifest.json"
fi
log "manifest: $MANIFEST_URL"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

curl -fsSL -o "$TMP/manifest.json" "$MANIFEST_URL"

# Pull URL + sha256 for our target
if command -v jq >/dev/null; then
    ASSET_URL=$(jq -r ".assets[\"$TARGET\"].url"    < "$TMP/manifest.json")
    ASSET_SHA=$(jq -r ".assets[\"$TARGET\"].sha256" < "$TMP/manifest.json")
else
    PY=$(command -v python3 || command -v python)
    [ -n "$PY" ] || err "need python3 or jq to parse manifest"
    ASSET_URL=$("$PY" -c "import sys,json;d=json.load(open('$TMP/manifest.json'));print(d['assets']['$TARGET']['url'])")
    ASSET_SHA=$("$PY" -c "import sys,json;d=json.load(open('$TMP/manifest.json'));print(d['assets']['$TARGET']['sha256'])")
fi
[ -n "$ASSET_URL" ] && [ "$ASSET_URL" != "null" ] || err "no asset for $TARGET in manifest"

log "downloading $ASSET_URL"
curl -fsSL -o "$TMP/binary.tar.gz" "$ASSET_URL"

ACTUAL_SHA=$(sha256_of "$TMP/binary.tar.gz")
if [ "$ACTUAL_SHA" != "$ASSET_SHA" ]; then
    err "SHA256 mismatch — refusing to install. expected $ASSET_SHA, got $ACTUAL_SHA"
fi
ok "verified SHA256: $ACTUAL_SHA"

mkdir -p "$BIN_DIR"
tar -C "$TMP" -xzf "$TMP/binary.tar.gz"
EXTRACTED=$(find "$TMP" -maxdepth 1 -type d -name 'zerox1-defi-*' | head -1)
[ -n "$EXTRACTED" ] || err "tarball did not contain zerox1-defi-* directory"
cp "$EXTRACTED/zerox1-defi-daemon" "$BIN_DIR/"
cp "$EXTRACTED/zerox1-defi-cli"    "$BIN_DIR/"
chmod 0755 "$BIN_DIR/zerox1-defi-daemon" "$BIN_DIR/zerox1-defi-cli"
ok "installed binaries to $BIN_DIR"

# ── Wallet ──────────────────────────────────────────────────────────────────

WALLET="$INSTALL_DIR/$ROLE-wallet.json"
if [ -f "$WALLET" ] && [ "$FORCE" -ne 1 ]; then
    warn "wallet already exists at $WALLET (use --force to overwrite)"
    log "(re-using existing wallet)"
else
    if command -v solana-keygen >/dev/null; then
        solana-keygen new --no-bip39-passphrase --force --silent -o "$WALLET" >/dev/null
    else
        # Fallback: 64 random bytes (Solana keypair format)
        # ed25519-dalek-compatible: first 32 = secret seed, last 32 = pubkey.
        # Without solana-keygen we can't compute the pubkey — error rather than write garbage.
        err "solana-keygen not found. Install Solana CLI first: https://docs.solana.com/cli/install-solana-cli-tools"
    fi
    chmod 0600 "$WALLET"
    ok "generated wallet: $WALLET"
fi

PUBKEY=$(solana-keygen pubkey "$WALLET" 2>/dev/null || echo "<run: solana-keygen pubkey $WALLET>")

# ── Token file ──────────────────────────────────────────────────────────────

TOKEN_FILE="$INSTALL_DIR/fleet.token"
printf '%s\n' "$FLEET_TOKEN" > "$TOKEN_FILE"
chmod 0600 "$TOKEN_FILE"
ok "wrote fleet token to $TOKEN_FILE (mode 0600)"

# ── Service install ─────────────────────────────────────────────────────────

PORT=$(port_for_role "$ROLE")
SERVICE_NAME="world.zerox1.defi.$ROLE"
DATA_DIR="$INSTALL_DIR/data-$ROLE"
mkdir -p "$DATA_DIR"

OS=$(uname -s)
case "$OS" in
    Darwin)
        AGENTS_DIR="$HOME/Library/LaunchAgents"
        mkdir -p "$AGENTS_DIR"
        PLIST="$AGENTS_DIR/$SERVICE_NAME.plist"
        cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>$SERVICE_NAME</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN_DIR/zerox1-defi-daemon</string>
    <string>--fleet-id</string>        <string>$FLEET_ID</string>
    <string>--fleet-token-file</string><string>$TOKEN_FILE</string>
    <string>--role</string>            <string>$ROLE</string>
    <string>--wallet-keypair-path</string><string>$WALLET</string>
    <string>--data-dir</string>        <string>$DATA_DIR</string>
    <string>--bind-port</string>       <string>$PORT</string>
  </array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <true/>
  <key>StandardOutPath</key>  <string>$INSTALL_DIR/$ROLE.log</string>
  <key>StandardErrorPath</key><string>$INSTALL_DIR/$ROLE.err</string>
</dict>
</plist>
EOF
        launchctl unload "$PLIST" 2>/dev/null || true
        launchctl load   "$PLIST"
        ok "installed launchd: $SERVICE_NAME (port $PORT)"
        ;;
    Linux)
        UNIT_DIR="$HOME/.config/systemd/user"
        mkdir -p "$UNIT_DIR"
        UNIT="$UNIT_DIR/zerox1-defi-$ROLE.service"
        cat > "$UNIT" <<EOF
[Unit]
Description=01fi $ROLE daemon
After=network-online.target

[Service]
Type=simple
ExecStart=$BIN_DIR/zerox1-defi-daemon \\
  --fleet-id $FLEET_ID \\
  --fleet-token-file $TOKEN_FILE \\
  --role $ROLE \\
  --wallet-keypair-path $WALLET \\
  --data-dir $DATA_DIR \\
  --bind-port $PORT
Restart=on-failure
RestartSec=5
StandardOutput=append:$INSTALL_DIR/$ROLE.log
StandardError=append:$INSTALL_DIR/$ROLE.err

[Install]
WantedBy=default.target
EOF
        systemctl --user daemon-reload
        systemctl --user enable --now "zerox1-defi-$ROLE.service"
        ok "installed systemd: zerox1-defi-$ROLE.service (port $PORT)"
        ;;
    *)
        err "unsupported OS for service install: $OS"
        ;;
esac

# ── Done ────────────────────────────────────────────────────────────────────

cat <<EOF

────────────────────────────────────────────────────────────────────
✓ 01fi $ROLE worker installed.

  Pubkey:   $PUBKEY
  Port:     $PORT (loopback only)
  Logs:     $INSTALL_DIR/$ROLE.log

  ⚠ Fund this wallet with SOL before mobile sends work.
    Send to: $PUBKEY (Solana mainnet)

  Next:     open mobile → Fleet tab → approve the join-request
            from this worker (should appear within seconds).

  Uninstall: install.sh --uninstall --role $ROLE
────────────────────────────────────────────────────────────────────
EOF
