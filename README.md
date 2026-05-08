# Hedgents fleet

**Institutional DeFi on Solana, run by a fleet of role-isolated autonomous agents.**

Hedgents is a self-hosted treasury management product for institutional operators. The fleet is five Rust binaries communicating over a peer-to-peer mesh; each binary owns one role and one cryptographic identity.

## The five daemons

| Daemon | Role | Strategy | Signs txs |
|---|---|---|---|
| `multiply-daemon` | Leveraged staking trader | Kamino LST farming with leverage | Yes |
| `stable-yield-daemon` | Passive lender | Kamino USDC supply | Yes |
| `hedgedjlp-daemon` | Delta-neutral basis trader | Long JLP, short SOL/ETH/BTC perps on Jupiter Perps | Yes |
| `riskwatcher-daemon` | Risk officer | Observes positions + emits soft-veto Escalates | **No (compile-time isolated)** |
| `researcher-daemon` | Signal publisher | 5 watchers (Kamino rates, Pyth prices, JLP yield, peg drift, etc.) | **No (compile-time isolated)** |

## Verified differentiators

1. **Compile-time authority isolation.** `riskwatcher-daemon` and `researcher-daemon` `Cargo.toml` deliberately omit the wallet crate. `cargo tree -p riskwatcher-daemon | grep wallet` returns empty. A compromised binary cannot reach the signing code because it isn't linked.
2. **Per-instruction whitelist.** Every signing daemon validates each instruction's `program_id` against a hard-coded allowlist before signing (`SigningWhitelist::verify_ixns`). Defense in depth on top of authority isolation.
3. **Soft-veto protocol.** `multiply` respects `EscalateRisk(Critical, LiquidationDistance)` from a configured `--riskwatcher` pubkey: pauses 300s, rejects new Assigns. Closed-by-default — without a configured riskwatcher pubkey, all Escalates are dropped.
4. **Role identity decoupled from host identity.** Each daemon loads a long-lived Ed25519 role key (e.g. `riskwatcher-role.key`) from `secrets-dir`; the libp2p peer-id is ephemeral. Role key moves to a backup host → new peer-id, same cryptographic identity.
5. **On-premise by default.** `cargo build --workspace` produces all binaries. No SaaS server, no managed keys.

## Strategy mainnet-readiness

| Daemon | Mainnet ready | First-position size |
|---|---|---|
| multiply | ✓ ($50 runbook landed) | $50 USDC |
| stable-yield | ✓ ($50 runbook landed) | $50 USDC |
| hedgedjlp | YELLOW (sim-only until live custody loader lands) | $200 USDC (sim-only) |

Riskwatcher + researcher are infrastructure; they don't take positions.

## Build

```bash
cargo build --workspace
cargo test --workspace
```

Requires sibling clone of [Hedgents/p2p_architecture](https://github.com/Hedgents/p2p_architecture) at `../p2p_architecture/` (path-dep).

## Quick start (devnet)

The `scripts/run-fleet-with-dashboard.sh` boot script starts the full fleet + dashboard server in one shell:

```bash
./scripts/run-fleet-with-dashboard.sh devnet
```

First run generates per-role Ed25519 keys + a Solana keypair under `~/01fi-soak/secrets/`. Each daemon writes JSON-formatted tracing logs to `~/01fi-soak/logs/<role>.log`; the dashboard server tails them all and serves a REST + WebSocket API on `127.0.0.1:7700`.

Pair with the local frontend at [Hedgents/frontend](https://github.com/Hedgents/frontend) (`localhost:3000`) for the operator dashboard view. To issue a sim-only Assign:

```bash
./target/release/fleet-pm-stub \
    --secrets-dir ~/01fi-soak/secrets \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19302 \
    --recipient-agent-id <stable-yield-agent-id-from-log> \
    --timeout-secs 60 \
    assign-stable-lend --usdc-lamports 10000000
```

For mainnet: `./scripts/run-fleet-with-dashboard.sh mainnet` (operator must fund the wallet first).

## Repository layout

```
crates/
├── multiply-daemon/         — Kamino leveraged LST
├── stable-yield-daemon/     — Kamino USDC supply
├── hedgedjlp-daemon/        — JLP + Jupiter Perps shorts (delta-hedged)
├── riskwatcher-daemon/      — read-only risk officer
├── researcher-daemon/       — read-only signal publisher
├── zerox1-defi-runtime/     — daemon framework (RpcContext, RoleIdentity, SigningWhitelist)
├── zerox1-defi-protocols/   — Solana DEX integrations (Kamino, Jupiter Perps, Pyth, Sanctum)
└── zerox1-defi-wallet/      — signing infrastructure (only linked by signing daemons)

tools/
├── fleet-pm-stub/           — orchestrator stand-in for testing (CLI)
└── fleet-dashboard-server/  — local dashboard backend (TBD)

docs/
├── runbooks/                — mainnet operator runbooks per daemon
└── superpowers/plans/       — implementation plans (M1-Mn per daemon)
```

## Roadmap

- **Now**: 5-daemon fleet operational. Multiply + stable-yield mainnet-ready. Hedgedjlp sim-only pending live custody loader.
- **+1 week**: Local operator dashboard with live mesh feed (see `docs/superpowers/plans/2026-05-06-demo-sprint.md`).
- **+1 month**: Investor capital onboarding.
- **+Q3**: Institutional deployment — same software, $50M-scale treasuries.

## License

TBD — institutional preview.
