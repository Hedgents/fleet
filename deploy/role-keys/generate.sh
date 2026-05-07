#!/usr/bin/env bash
# Generate dev role keys (32 random bytes each) for all five fleet daemons +
# the orchestrator stub. NOT for production — these are unencrypted at rest.
set -euo pipefail
cd "$(dirname "$0")"
for role in riskwatcher multiply hedgedjlp stablefloor researcher orchestrator; do
    mkdir -p "$role"
    openssl rand 32 > "$role/${role}-role.key"
    chmod 600 "$role/${role}-role.key"
done
echo "Generated dev role keys for: riskwatcher, multiply, hedgedjlp, stablefloor, researcher, orchestrator."
