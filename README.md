# 01fi — Universal Solana DeFi Toolkit for Agents

Three-layer architecture so any agent runtime — zeroclaw, Claude Agent SDK,
plain Python, raw CLI — can compose Solana DeFi yield strategies.

```
┌─────────────────────────────────────────────────────────────────┐
│  Agent runtimes                                                 │
│  ─ zeroclaw + zerox1-defi-plugin   (in zeroclaw fork, separate) │
│  ─ Claude Agent SDK script         (HTTP client to daemon)      │
│  ─ Python / TS / shell             (HTTP client to daemon)      │
└──────────────────────┬──────────────────────────────────────────┘
                       │ localhost HTTP
┌──────────────────────▼──────────────────────────────────────────┐
│  zerox1-defi-daemon (this workspace)                            │
│  HTTP service. Holds wallet. Builds → signs → broadcasts txs.   │
└──────────────────────┬──────────────────────────────────────────┘
                       │ Rust function calls
┌──────────────────────▼──────────────────────────────────────────┐
│  zerox1-defi-protocols (this workspace)                         │
│  Pure Rust library. Instruction builders for Kamino, JLP,       │
│  Adrena, Sanctum, Pyth. No runtime dependencies.                │
└─────────────────────────────────────────────────────────────────┘
```

## Crates

| Crate | Type | Purpose |
|---|---|---|
| `zerox1-defi-protocols` | lib | Pure Rust instruction builders. No runtime, no I/O. |
| `zerox1-defi-daemon` | bin | Localhost HTTP service. Wallet + RPC + broadcast. |
| `zerox1-defi-cli` | bin | Manual CLI for testing protocols against devnet/mainnet. |

The zeroclaw plugin (`zerox1-defi-plugin`) lives in the zeroclaw fork, not
here. It is a thin HTTP client to the daemon.

## Status

Scaffold. Kamino USDC supply + withdraw wired end-to-end with correct
account layouts; Anchor instruction discriminator marked TODO pending IDL
verification. See `crates/zerox1-defi-protocols/src/protocols/kamino.rs`.

## Quickstart (devnet test)

```bash
# Build
cargo build --release

# Set up wallet (devnet)
export SOLANA_RPC_URL=https://api.devnet.solana.com
export WALLET_KEYPAIR_PATH=$HOME/.config/solana/id.json

# Start daemon on localhost:9091
./target/release/zerox1-defi-daemon

# In another terminal, test via CLI
./target/release/zerox1-defi-cli kamino-supply --asset usdc --amount 1.0
```

## Roadmap (per PORTFOLIO_STRATEGY.md)

- [x] Workspace skeleton
- [ ] Kamino USDC supply / withdraw (in progress — scaffold complete)
- [ ] Sanctum INF stake / unstake
- [ ] Kamino Multiply (jitoSOL/SOL leveraged)
- [ ] Jupiter JLP mint / burn
- [ ] Adrena SOL short open / close
- [ ] Pyth price subscription
- [ ] Yield Router (cross-venue APR comparison)

## Design principles

- **Zero runtime lock-in**: protocols crate has no agent-runtime dependency
- **Wallet stays local**: daemon binds to localhost only; no remote access
- **One protocol per module**: `protocols/kamino.rs`, `protocols/jlp.rs`, etc.
- **Each instruction returns `Vec<Instruction>`**: caller bundles into a tx
- **Borsh + Anchor discriminators**: standard Solana program calling convention
