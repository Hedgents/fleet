# 01fi Local-Dev Fleet Harness

Boots the five fleet daemons in containers on a shared docker network.

## First-time setup

Build context for docker-compose is the parent of this worktree, so it
needs to live next to a `node-enterprise/` worktree. If you cloned via
the standard `git worktree add`, that's already the case
(`/Users/tobiasd/Desktop/zerox1-01fi-fleet/` and
`/Users/tobiasd/Desktop/node-enterprise/`).

Generate role keys and (for signing daemons) Solana wallets:

```bash
./role-keys/generate.sh

# Per signing daemon, drop a Solana keypair JSON in its secrets dir:
for role in multiply hedgedjlp stablefloor; do
    solana-keygen new --outfile role-keys/$role/solana-wallet.json --no-bip39-passphrase --force
done
```

## Run

From this directory:

```bash
docker compose -f docker-compose.fleet.yml up --build
```

First build is slow (compiles all five binaries inside a Rust container,
~5–10 min on a fast machine). Subsequent builds are cached unless source
changes.

Watch the logs — riskwatcher comes up first; the others bootstrap to it
and start emitting Beacons every 5 seconds. Each Beacon adds the
emitting daemon to the others' role registries.

## Tear down

```bash
docker compose -f docker-compose.fleet.yml down
```

(Volumes are read-only mounts of `role-keys/<role>/`, so nothing
persistent to clean up.)

## Topology

```
            ┌──────────────┐
            │ riskwatcher  │  (bootstrap node, listens on tcp/9301)
            └──────┬───────┘
                   │ /dns/riskwatcher/tcp/9301
       ┌───────────┼───────────────┐
       ▼           ▼               ▼
   ┌─────────┐  ┌──────────┐  ┌────────────┐
   │ multiply │  │ hedgedjlp│  │ researcher │
   └─────────┘  └──────────┘  └────────────┘
                                  tcp/9302..4
```

stablefloor is one-shot and excluded from the resident topology. Run it
on demand:

```bash
docker compose -f docker-compose.fleet.yml run --rm stablefloor stablefloor-daemon \
    --secrets-dir /secrets \
    --wallet /secrets/solana-wallet.json \
    mint --sol-amount 0.001
```
