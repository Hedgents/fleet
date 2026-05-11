# Hedgents fleet — Docker quickstart

One-command deployment of the full fleet for hackathon judges,
institutional evaluators, or local development.

## What you get

From the fleet repo root:

```
docker compose up --build
```

brings up:

- **5 fleet daemons** (multiply, stable-yield, hedgedjlp, riskwatcher,
  researcher) running in `--simulate-only=true` mode against Solana
  mainnet
- **paper-trade-loop** firing Assigns every 5 minutes so the dashboard
  timeline shows continuous activity
- **fleet-dashboard-server** at <http://localhost:7700> (REST + WS)
- **Next.js dashboard UI** at <http://localhost:3000>

Initial build takes 5–10 minutes (compiles all Rust binaries and the
Next.js frontend). Subsequent boots are seconds thanks to layer caching.

## Prerequisites

- Docker Engine 24+ with Compose v2 (`docker compose`, not `docker-compose`)
- ~4 GB of free RAM during the first build
- Internet (pulls Rust + Debian base images, clones p2p_architecture and
  the frontend repo)

## What's running by default

| Service          | Port | Purpose |
|------------------|------|---------|
| frontend         | 3000 | Dashboard UI (Next.js)                          |
| dashboard        | 7700 | REST + WS for fleet state                       |
| multiply         | —    | Bootstrap node, leveraged jitoSOL strategy      |
| stable-yield     | —    | Kamino USDC supply strategy                     |
| hedgedjlp        | —    | Delta-neutral Jupiter LP strategy               |
| riskwatcher      | 9091 | Position monitor + Prometheus metrics           |
| researcher       | —    | Rate/price feeds, MarketSignal emitter          |
| paper-trade-loop | —    | Drives periodic Assigns (5 min interval)        |
| init             | —    | One-shot key generation (exits after running)   |

All daemons share two Docker volumes:
- `secrets` — role keys + Solana wallet
- `logs` — JSONL telemetry the dashboard ingests

## Use a private RPC (recommended)

The default uses the public mainnet RPC, which is rate-limited and
sometimes serves stale state. For sustained operation:

```bash
export RPC_URL="https://mainnet.helius-rpc.com/?api-key=YOUR_KEY"
docker compose up --build
```

## Going live (real funds)

The default profile is **simulate-only** — no transactions are ever
broadcast. To execute a real on-chain deposit:

1. Fund the wallet whose pubkey is printed by the `init` service on
   first run.
2. Follow the manual runbook: `docs/runbooks/stable-yield-mainnet-tiny.md`.
3. Use `scripts/mainnet-demo-stable-yield.sh` from the host (not in
   Docker) — it expects direct RPC access and handles the sim → confirm
   → live submit flow.

We deliberately do not ship a "flip to live" docker-compose profile.
Real-money execution should be a conscious, manual step.

## Stop / tear down

```bash
docker compose down            # stop containers, keep volumes
docker compose down --volumes  # nuke secrets + logs + db (full reset)
```

## Troubleshooting

**First build is slow.** Expected. Rust compilation of the full
workspace inside Docker is 5–10 minutes on a modern machine. The
build uses `cargo-chef` so a dependency-only layer is cached;
subsequent rebuilds only recompile your changes.

**Dashboard shows daemons red after boot.** Give it 30–60 seconds. The
libp2p mesh forms after multiply (the bootstrap node) starts listening
and the other daemons dial in.

**Frontend can't reach dashboard.** Check `docker compose ps` — both
should be `running`. The frontend talks to `http://localhost:7700`
from the browser, which means the dashboard's `ports: 7700:7700`
publish must be intact.

**Build fails with "no space left on device".** Docker Desktop on macOS
defaults to 60 GB and the Rust build cache can blow past it. Increase
disk allocation in Docker Desktop → Settings → Resources.

## Cleaning up an old setup

The previous `deploy/role-keys/` directory pattern is no longer used —
secrets now live in a Docker volume populated by the `init` service.
The old directory and `generate.sh` script can be deleted.
