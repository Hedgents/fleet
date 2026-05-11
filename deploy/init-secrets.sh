#!/usr/bin/env bash
# Generate role keys + Solana wallet into /secrets if not already present.
# Idempotent — does nothing on subsequent runs.

set -euo pipefail

SECRETS=/secrets
mkdir -p "$SECRETS"

ROLES=(multiply stable-yield hedgedjlp riskwatcher researcher orchestrator)

generated=0
for role in "${ROLES[@]}"; do
    f="$SECRETS/${role}-role.key"
    if [[ -f "$f" ]]; then continue; fi
    python3 -c "import os; open('$f','wb').write(os.urandom(32))"
    chmod 600 "$f"
    generated=1
    echo "generated $f"
done

WALLET="$SECRETS/solana-wallet.json"
if [[ ! -f "$WALLET" ]]; then
    python3 <<PY > "$WALLET"
import json
from nacl.signing import SigningKey
sk = SigningKey.generate()
combined = list(bytes(sk)) + list(bytes(sk.verify_key))
print(json.dumps(combined))
PY
    chmod 600 "$WALLET"
    echo "generated $WALLET (DEMO WALLET — do not fund for mainnet without rotating)"
    generated=1
fi

if [[ "$generated" -eq 0 ]]; then
    echo "secrets already present — skipping init"
fi

# Print the wallet pubkey so the operator can see it
python3 <<PY
import json
from nacl.signing import SigningKey
data = json.load(open("$WALLET"))
sk = SigningKey(bytes(data[:32]))
import base64
pk_bytes = bytes(sk.verify_key)
# base58 from raw bytes
ALPHA = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def b58(b):
    n = int.from_bytes(b, "big")
    out = ""
    while n > 0:
        n, r = divmod(n, 58)
        out = ALPHA[r] + out
    for byte in b:
        if byte == 0:
            out = "1" + out
        else:
            break
    return out
print(f"wallet pubkey: {b58(pk_bytes)}")
PY
