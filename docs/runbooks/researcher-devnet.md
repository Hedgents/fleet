# Researcher Daemon — Devnet Runbook

Researcher is fleet's read-only signal publisher. It watches market state across six dimensions and broadcasts MarketSignal envelopes to subscriber daemons. No chain authority — structurally cannot sign txs.

## Pre-flight

- Role key: 32 raw random bytes at `<secrets>/researcher-role.key`, mode 0600
- NO Solana wallet required (read-only)
- Solana RPC URL — public devnet works; mainnet recommended for real signal value
- Subscriber list: hex pubkeys of the daemons that should receive signals (multiply, stable-yield, hedgedjlp, speculator agent_ids — these are the role pubkeys, not Solana wallet pubkeys)

## Boot command (devnet, all watchers, real Pyth)

```bash
RUST_LOG=info,libp2p=warn cargo run --release -p researcher-daemon -- \
    --secrets-dir ./secrets/researcher-devnet \
    --rpc-url https://api.devnet.solana.com \
    --network devnet \
    --listen /ip4/0.0.0.0/tcp/19316 \
    --beacon-interval-secs 5 \
    --price-feed 'sol-usd:H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG:SOL' \
    --price-poll-interval-secs 30 \
    --peg-feed 'usdc-usd:Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD:USDC' \
    --peg-poll-interval-secs 60 \
    --telemetry-log ./researcher-signals.jsonl \
    --tally-interval-secs 60 \
    --subscriber <multiply_agent_id_hex> \
    --subscriber <stable_yield_agent_id_hex>
```

Note: lending, funding, JLP, and token-activity watchers omitted from the boot example since their underlying decoders are 0-stubbed in v0. Adding them is harmless (they tick + log) but won't emit until the decoders land.

## Verifying it's running

```bash
# In another terminal:
tail -f researcher-signals.jsonl
```

Each emitted signal appears as a JSONL line. On a stable price + on-peg USDC, no signals fire — that's correct behavior. To force a signal:

1. Tail the log for >=1h while devnet price moves naturally, OR
2. Boot with `--price-poll-interval-secs 5` and watch a price move >=2% in a 5min window (rare on devnet), OR
3. Switch `--rpc-url` to mainnet and watch real price action

## What success looks like

- `researcher args validated` (boot)
- `rpc network verified network=devnet` (genesis check)
- `Loaded identity` (role pubkey + agent_id printed)
- `listening listen=/ip4/...` (TCP up)
- `BEACON emitted role=researcher` every 5s
- `<watcher_name> watcher starting` for each watcher
- `<watcher_name> first observation seeded` for stateful watchers (price, jlp_yield, perp_funding)
- Hourly: `researcher signal tally (rolling window) total=N info=A notice=B important=C`
- On a real signal: `<kind> signal broadcast sent_count=N recipient_count=M` + a JSONL line

## Failure modes

| Symptom | Likely cause | Fix |
|---|---|---|
| `Error: --network=mainnet requires --i-understand-this-is-mainnet flag` | Forgot ack | Add the flag |
| `Error: RPC URL ... returned genesis hash X but --network mainnet expects Y` | RPC/network mismatch | Use a mainnet RPC URL or change --network |
| Watchers boot but no signals fire | All watchers below threshold (normal) OR underlying decoder 0-stubbed (M3 lending APR, M4 drift funding, M7 JLP yield, M8 Bags.fm) | Check the M-numbered comments in each watcher; M5 price + M6 stable_peg are the only fully-real watchers in v0 |
| `pyth poll failed` warns | Devnet Pyth feed not keeper-updated | Use the documented mainnet Pyth feed pubkeys instead, or accept that devnet has limited Pyth coverage |
| JSONL file empty after 1h | Either: no signals fired (normal on stable markets), or telemetry handle not threaded through. Check the watcher's log lines — `signal generated but no subscribers` means no recipients are configured (`--subscriber` empty). |
| `Subscribers not BEACON-ing` | Recipient daemons aren't running with their own role keys | Boot multiply/stable-yield with their own `--secrets-dir` and pass their agent_ids via `--subscriber` |

## Mainnet promotion

Same flags but:
- `--network mainnet --i-understand-this-is-mainnet`
- Mainnet Pyth feeds (look up at pyth.network)
- Mainnet Kamino lending market for `--lending-reserve` (if/when M3 decoder lands)
- Mainnet Drift perp markets for `--funding-market` (if/when M4 decoder lands)
- Mainnet JLP pool: `5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq` (if/when M7 decoder lands)
- Bags.fm program ID for `--bags-program-id` (if/when M8 subscriber lands)

## Adding a new watcher

Pattern (from M3-M8):
1. Add a file under `crates/researcher-daemon/src/watchers/<name>.rs`
2. Mirror the structure of existing watchers (poll loop, classify, dedup, broadcast)
3. Add `pub mod <name>;` to `watchers/mod.rs`
4. Add CLI flags in `main.rs`
5. Spawn the watcher's `run(...)` in `main`'s `tokio::select!`, threading `telemetry_handle.clone()` and `subscribers.clone()`

## What's NOT in v0

- Real APR computation in lending_rate (M3)
- Real funding-rate decode in perp_funding (M4 — needs drift.rs)
- Real JLP yield + composition decode in jlp_yield (M7 — jlp.rs lacks pool decoder)
- Real Bags.fm log subscription in token_activity (M8 — needs WS pubsub + decoder)

These can be filled in iteratively. The signal-emission infra, dedup, telemetry, and runbook all work today; specific decoders are post-v0 polish.
