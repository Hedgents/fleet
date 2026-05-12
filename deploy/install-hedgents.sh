#!/usr/bin/env bash
# Hedgents fleet installer for Linux (x64 + arm64).
#
# Idiomatic install path: a system user `hedgents` owns /var/lib/hedgents
# (data) and runs binaries from /opt/hedgents/bin. systemd units in
# /etc/systemd/system/. Configuration in /etc/hedgents/hedgents.env.
#
# Usage (Oracle ARM, fresh box):
#
#   curl -sSL \
#     https://github.com/Hedgents/fleet/releases/latest/download/install-hedgents.sh \
#     | sudo bash
#
# Or pin a specific release:
#
#   sudo TAG=fleet-v0.1.0 ./install-hedgents.sh
#
# After install, edit /etc/hedgents/hedgents.env to set RPC_URL (Helius
# recommended) and optional ELEVENLABS_API_KEY, then:
#
#   sudo systemctl daemon-reload
#   sudo systemctl enable --now hedgents.target

set -euo pipefail

# ─── Config ────────────────────────────────────────────────────────────────
REPO="${REPO:-Hedgents/fleet}"
TAG="${TAG:-latest}"
PREFIX="${PREFIX:-/opt/hedgents}"
DATADIR="${DATADIR:-/var/lib/hedgents}"
ETCDIR="${ETCDIR:-/etc/hedgents}"
UNITDIR="${UNITDIR:-/etc/systemd/system}"
USERNAME="${USERNAME:-hedgents}"
RPC_DEFAULT="${RPC_URL:-https://api.mainnet-beta.solana.com}"

# ─── Helpers ───────────────────────────────────────────────────────────────
log()  { printf "\n▸ %s\n" "$*"; }
ok()   { printf "  ✓ %s\n" "$*"; }
warn() { printf "  ! %s\n" "$*" >&2; }
bail() { printf "\n✖ %s\n" "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || bail "must run as root (sudo)"

# ─── Detect arch ───────────────────────────────────────────────────────────
case "$(uname -m)" in
    x86_64|amd64) TARGET=linux-x64 ;;
    aarch64|arm64) TARGET=linux-arm64 ;;
    *) bail "unsupported architecture: $(uname -m)" ;;
esac
ok "detected arch: $TARGET"

# ─── Fetch manifest ────────────────────────────────────────────────────────
if [[ "$TAG" == "latest" ]]; then
    MANIFEST_URL="https://github.com/${REPO}/releases/latest/download/manifest.json"
else
    MANIFEST_URL="https://github.com/${REPO}/releases/download/${TAG}/manifest.json"
fi
log "fetching manifest from $MANIFEST_URL"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -sSL "$MANIFEST_URL" -o "$TMP/manifest.json" || bail "failed to fetch manifest"

VERSION=$(python3 -c "import json; print(json.load(open('$TMP/manifest.json'))['version'])")
RESOLVED_TAG=$(python3 -c "import json; print(json.load(open('$TMP/manifest.json'))['tag'])")
ok "release: $RESOLVED_TAG (version $VERSION)"

FLEET_URL=$(python3 -c "import json; print(json.load(open('$TMP/manifest.json'))['fleet']['$TARGET']['url'])")
FLEET_SHA=$(python3 -c "import json; print(json.load(open('$TMP/manifest.json'))['fleet']['$TARGET']['sha256'])")
FRONTEND_URL=$(python3 -c "import json; m=json.load(open('$TMP/manifest.json')); print(m['frontend']['url'] if m.get('frontend') else '')")

# ─── Download + verify ─────────────────────────────────────────────────────
log "downloading fleet tarball"
curl -sSL "$FLEET_URL" -o "$TMP/fleet.tar.gz"
echo "$FLEET_SHA  $TMP/fleet.tar.gz" | sha256sum -c - >/dev/null \
    || bail "fleet tarball sha256 mismatch"
ok "fleet checksum verified"

# ─── Create user + dirs ────────────────────────────────────────────────────
log "creating system user $USERNAME and directories"
if ! id "$USERNAME" >/dev/null 2>&1; then
    useradd --system --home "$DATADIR" --shell /usr/sbin/nologin "$USERNAME"
    ok "created user $USERNAME"
else
    ok "user $USERNAME already exists"
fi
mkdir -p "$PREFIX/bin" "$DATADIR/secrets" "$DATADIR/logs" "$ETCDIR"
chown -R "$USERNAME:$USERNAME" "$DATADIR"
chmod 700 "$DATADIR/secrets"

# ─── Install binaries + systemd units ──────────────────────────────────────
log "installing binaries and systemd units"
tar -xzf "$TMP/fleet.tar.gz" -C "$TMP"
SRC=$(find "$TMP" -maxdepth 1 -type d -name "hedgents-fleet-*" | head -1)
[[ -d "$SRC" ]] || bail "tarball layout unexpected"

install -m 0755 "$SRC/bin/"*-daemon "$PREFIX/bin/"
install -m 0755 "$SRC/bin/fleet-dashboard-server" "$SRC/bin/fleet-pm-stub" "$PREFIX/bin/"
install -m 0755 "$SRC/bin/paper-trade-loop.sh" "$PREFIX/bin/"
install -m 0644 "$SRC/systemd/"*.service "$SRC/systemd/"*.target "$UNITDIR/"
ok "installed binaries to $PREFIX/bin"
ok "installed systemd units to $UNITDIR"

# ─── First-run secret generation ───────────────────────────────────────────
if [[ ! -f "$DATADIR/secrets/multiply-role.key" ]]; then
    log "generating role keys + Solana wallet (first run)"
    command -v python3 >/dev/null || bail "python3 required for key gen"
    python3 -m pip --version >/dev/null 2>&1 || apt-get install -y python3-pip
    python3 -c "import nacl" 2>/dev/null || pip3 install pynacl --break-system-packages || pip3 install pynacl

    for role in multiply stable-yield hedgedjlp riskwatcher researcher orchestrator; do
        F="$DATADIR/secrets/${role}-role.key"
        python3 -c "import os; open('$F','wb').write(os.urandom(32))"
        chmod 600 "$F"
        chown "$USERNAME:$USERNAME" "$F"
    done

    WALLET="$DATADIR/secrets/solana-wallet.json"
    python3 <<PY > "$WALLET"
import json
from nacl.signing import SigningKey
sk = SigningKey.generate()
combined = list(bytes(sk)) + list(bytes(sk.verify_key))
print(json.dumps(combined))
PY
    chmod 600 "$WALLET"
    chown "$USERNAME:$USERNAME" "$WALLET"
    ok "generated secrets in $DATADIR/secrets (DEMO WALLET — do not fund without rotating)"
fi

# ─── Derive pubkeys for systemd env file ───────────────────────────────────
log "deriving role pubkeys for env file"
derive_pubkey() {
    python3 -c "
from nacl.signing import SigningKey
seed = open('$DATADIR/secrets/$1-role.key', 'rb').read()
print(bytes(SigningKey(seed).verify_key).hex())
"
}
ORCH=$(derive_pubkey orchestrator)
MUL=$(derive_pubkey multiply)
SY=$(derive_pubkey stable-yield)
HJ=$(derive_pubkey hedgedjlp)
RW=$(derive_pubkey riskwatcher)

# ─── Write env file (idempotent — preserve existing values) ────────────────
ENVFILE="$ETCDIR/hedgents.env"
if [[ ! -f "$ENVFILE" ]]; then
    cat > "$ENVFILE" <<EOF
# Hedgents fleet runtime config.
# Edit RPC_URL to point at your Helius (or other) mainnet RPC.
# Set ELEVENLABS_API_KEY to enable hourly voice briefings.

RPC_URL=${RPC_DEFAULT}
ELEVENLABS_API_KEY=

# Required by the daemons so they emit JSON tracing that the dashboard's
# log-tailer ingests. Plain text logs are not parsed.
RUST_LOG_FORMAT=json
RUST_LOG=info,libp2p=warn

# Derived role pubkeys — do not edit.
ORCHESTRATOR_PUBKEY=${ORCH}
MULTIPLY_PUBKEY=${MUL}
STABLE_YIELD_PUBKEY=${SY}
HEDGEDJLP_PUBKEY=${HJ}
RISKWATCHER_PUBKEY=${RW}
EOF
    chmod 640 "$ENVFILE"
    chown root:"$USERNAME" "$ENVFILE"
    ok "created $ENVFILE"
else
    # Update only the derived pubkey lines, preserve RPC_URL + ELEVENLABS_API_KEY
    sed -i \
        -e "s|^ORCHESTRATOR_PUBKEY=.*|ORCHESTRATOR_PUBKEY=${ORCH}|" \
        -e "s|^MULTIPLY_PUBKEY=.*|MULTIPLY_PUBKEY=${MUL}|" \
        -e "s|^STABLE_YIELD_PUBKEY=.*|STABLE_YIELD_PUBKEY=${SY}|" \
        -e "s|^HEDGEDJLP_PUBKEY=.*|HEDGEDJLP_PUBKEY=${HJ}|" \
        -e "s|^RISKWATCHER_PUBKEY=.*|RISKWATCHER_PUBKEY=${RW}|" \
        "$ENVFILE"
    ok "updated derived pubkeys in $ENVFILE (preserved RPC_URL + ELEVENLABS_API_KEY)"
fi

systemctl daemon-reload

# ─── Print summary ─────────────────────────────────────────────────────────
WALLET_PK=$(python3 <<PY
import json
from nacl.signing import SigningKey
data = json.load(open("$DATADIR/secrets/solana-wallet.json"))
pk = bytes(SigningKey(bytes(data[:32])).verify_key)
alpha = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
n = int.from_bytes(pk, "big"); out = ""
while n > 0:
    n, r = divmod(n, 58); out = alpha[r] + out
for b in pk:
    if b == 0: out = "1" + out
    else: break
print(out)
PY
)

cat <<EOF

────────────────────────────────────────────────────────────
  Hedgents fleet installed.

  version:        $VERSION
  binaries:       $PREFIX/bin
  systemd units:  $UNITDIR/hedgents-*.{service,target}
  config:         $ENVFILE
  data:           $DATADIR
  wallet pubkey:  $WALLET_PK

  Next steps:
    1. Edit $ENVFILE — set RPC_URL to a private RPC if you have one.
    2. Start the fleet:
         sudo systemctl enable --now hedgents.target
    3. Check status:
         systemctl status 'hedgents-*'
         curl http://127.0.0.1:7700/daemons

  Default mode is simulate-only — no transactions are broadcast.
────────────────────────────────────────────────────────────
EOF
