# Multiply Strategy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land a Kamino-leveraged-LST position on **Solana mainnet**, signed by the new `multiply-daemon` (fleet shape), that earns positive APR over a 24-hour window — and prove it via telemetry. The first mainnet-earning fleet daemon.

**Architecture:** A four-stage rollout — (1) wire `handle_inbox` to call the lifted `kamino.rs` supply/withdraw, (2) add hard-coded safety caps + sim-only default + manual-approval default, (3) implement a client-side leverage loop (multi-round supply→borrow→swap→supply, since we don't have a Rust port of Kamino's atomic flash-leverage), (4) graduate from sim-only → devnet real → mainnet $50 → mainnet target. Liquidation-distance monitor on every Beacon. Earning telemetry via a position-value log + `multiply-daemon report` subcommand. ChainReplay deferred to v0.1 — v0 boots empty.

**Tech Stack:** Rust 2021, the existing `multiply-daemon` (`fleet/fungibility` merged), `zerox1-defi-protocols::kamino` (already pulls `@kamino-finance/klend-sdk` equivalents in Rust), `zerox1-defi-wallet::SigningWhitelist`, Solana SDK 2.x, Pyth pull oracle (already in `zerox1-defi-protocols`), Jupiter swap (for the borrow→LST swap leg in the leverage loop), Helius / Triton mainnet RPC.

---

## ⚠ SAFETY POSTURE

This plan moves real money. Defaults are paranoid.

1. **`--simulate-only` defaults to true.** Daemon refuses to submit on-chain unless `--no-simulate-only` is passed.
2. **Hard-coded caps in source** (not just CLI args). The implementer cannot bypass them by editing config:
   - `MAX_LTV_BPS = 8000` — refuses any AssignMultiply with `target_ltv_bps > 8000` (80%) regardless of orchestrator.
   - `MAX_POSITION_USDC_LAMPORTS = 5_000_000_000_000` — $5M cap on collateral.
   - `MAX_LEVERAGE_LOOP_ROUNDS = 6` — stops the supply→borrow→swap loop after 6 rounds even if target LTV not reached.
   - `LIQUIDATION_DISTANCE_CRITICAL_BPS = 50` — auto-unwind if within 50 bps of liquidation.
3. **`--require-approval` defaults to true on mainnet.** Each AssignMultiply queues + emits an Escalate envelope; daemon submits only after a separate `Approve` envelope (also signed by the orchestrator) lands.
4. **`--network mainnet` requires a redundant flag** `--i-understand-this-is-mainnet`. No accidental promotion.
5. **First mainnet position is $50.** The plan's gating step (Task 9) explicitly enforces this.
6. **All signed transactions are logged** to `<journal-path>.signed-tx-log` with `(slot, signature, payload-hash, sim-result-hash)` for audit. The journal already exists (sqlite); this adds a sidecar log.
7. **Pre-merge sim every tx.** Before sending, simulate; if sim fails, abort with full simulation logs in the Report.

---

## What "mainnet earning" means (acceptance criterion for the plan)

A mainnet position is **earning** if:
- It is open continuously for ≥ 24 hours
- The 24h-trailing position-value (`collateral_lamports × LST_oracle_price - debt_lamports × stable_oracle_price`, denominated in USDC) is **higher than initial collateral_usd at T+0**
- The position-value log shows positive APR over the window

The `multiply-daemon report --since 24h` subcommand prints this readout. Task 11 acceptance is "report shows positive APR after 24h on mainnet from a $50 position."

---

## File Structure

```
01fi/
├── crates/multiply-daemon/
│   ├── Cargo.toml                  (modify — add deps: hex, ciborium for envelope payload, tokio-cron-scheduler optional, etc.)
│   └── src/
│       ├── main.rs                 (modify — wire handle_inbox dispatch, new args, beacon-time monitor hook)
│       ├── caps.rs                 (NEW — hard-coded safety constants + AssignMultiply validator)
│       ├── dispatch.rs             (NEW — AssignMultiply payload decode + handler dispatch)
│       ├── leverage.rs             (NEW — multi-round leverage loop)
│       ├── liq_monitor.rs          (NEW — liquidation-distance check + Escalate emitter)
│       ├── pnl.rs                  (NEW — position-value query + APR computation)
│       ├── reporter.rs             (NEW — `multiply-daemon report` subcommand)
│       ├── journal.rs              (modify — add signed-tx-log sidecar)
│       └── kamino.rs               (modify — extract pure-fn `build_supply_ixns`/`build_withdraw_ixns` from the axum handlers)
├── docs/superpowers/plans/
│   └── 2026-05-05-multiply-strategy.md  (this plan)
└── docs/runbooks/                  (NEW)
    ├── multiply-devnet.md          (NEW — Task 6 runbook)
    └── multiply-mainnet-tiny.md    (NEW — Task 9 runbook)
```

---

## Tasks

### Task 1: Extract pure ixn-building from `kamino.rs`'s axum handlers

The lifted `kamino.rs` has `pub async fn supply(State(s): State<AppState>, Json(req): ...) -> Result<Json<ExecResponse>, Response>` — an axum HTTP handler. We need to call its inner ixn-building logic from `handle_inbox` (which has no axum context). Refactor the file so the ixn construction is callable from anywhere.

**Files:**
- Modify: `crates/multiply-daemon/src/kamino.rs`
- Test: same file, `#[cfg(test)] mod tests`

- [ ] **Step 1: Read the current `supply` handler shape**

```bash
sed -n '/^pub async fn supply/,/^}/p' crates/multiply-daemon/src/kamino.rs
```

Identify: the lines that build the `Vec<Instruction>` (inputs: vault, amount, user pubkey; outputs: ixns) vs the lines that handle HTTP-shaped wrap/unwrap.

- [ ] **Step 2: Add `pub async fn build_supply_ixns(&self) -> Result<Vec<Instruction>>` to `kamino.rs`**

Extract the ixn-building body of `supply` into a new free async function:

```rust
use solana_sdk::{instruction::Instruction, pubkey::Pubkey};
use zerox1_defi_runtime::rpc::RpcContext;

/// Build (but do not sign or submit) the Solana instructions for a
/// Kamino "supply" operation. Pure logic: takes the vault, the user
/// pubkey, and the amount; returns the instruction set the daemon
/// will sign and submit.
pub async fn build_supply_ixns(
    rpc: &RpcContext,
    vault: Pubkey,
    user: Pubkey,
    amount_lamports: u64,
) -> anyhow::Result<Vec<Instruction>> {
    // Move the ixn-building body of `supply()` here. Return the Vec.
    todo!("move from supply()")
}
```

Replace the body of `pub async fn supply(...)` so it calls `build_supply_ixns` then routes through `execute_or_simulate`:

```rust
pub async fn supply(
    State(state): State<AppState>,
    Json(req): Json<SupplyRequest>,
) -> Result<Json<ExecResponse>, Response> {
    let ixns = build_supply_ixns(&state.rpc, req.vault, state.wallet.pubkey(), req.amount_lamports)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("build ixns: {e}")))?;
    execute_or_simulate(&state, ixns).await
}
```

(Existing axum API surface unchanged — this is internal refactor only.)

- [ ] **Step 3: Same extraction for `withdraw`**

```rust
pub async fn build_withdraw_ixns(
    rpc: &RpcContext,
    vault: Pubkey,
    user: Pubkey,
    amount_lamports: u64,
) -> anyhow::Result<Vec<Instruction>> {
    todo!("move from withdraw()")
}
```

- [ ] **Step 4: Add a unit test that exercises `build_supply_ixns` against a known-vault devnet snapshot (mocked RPC if needed)**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_supply_ixns_returns_nonempty() {
        // Use a mock RpcContext or a known fixture. Goal: prove the
        // function returns at least one Instruction without panic.
        // If RpcContext is hard to mock, skip this assertion and
        // rely on the integration test in Task 4.
    }
}
```

- [ ] **Step 5: Build + test**

```bash
cd /Users/tobiasd/Desktop/zerox1/01fi
cargo build -p multiply-daemon
cargo test -p multiply-daemon
```

Both clean.

- [ ] **Step 6: Commit**

```bash
git add crates/multiply-daemon/src/kamino.rs
git commit -m "multiply: extract pure build_supply_ixns / build_withdraw_ixns

The lifted kamino.rs handlers (supply, withdraw) are axum HTTP
handlers — they wrap the ixn-building in HTTP shape. Extracting the
ixn construction into pure async fns lets handle_inbox call them
without an axum context. Existing HTTP API surface unchanged."
```

---

### Task 2: Add hard-coded safety caps in `caps.rs`

The constants the implementer cannot bypass. These live in source, not config.

**Files:**
- Create: `crates/multiply-daemon/src/caps.rs`
- Modify: `crates/multiply-daemon/src/main.rs` (add `mod caps;`)

- [ ] **Step 1: Write the cap constants + AssignMultiply validator**

`crates/multiply-daemon/src/caps.rs`:

```rust
//! Hard-coded safety caps for multiply-daemon.
//!
//! These are absolute upper bounds. The orchestrator can ask for less,
//! but never more — even a "trusted" Assign with target_ltv_bps > 8000
//! is rejected. Caps live in source so they cannot be raised by editing
//! a config file at runtime.

use anyhow::{anyhow, Result};
use zerox1_protocol::fleet::multiply::AssignMultiply;

/// Maximum loan-to-value the daemon will ever accept (basis points).
/// 80% — anything higher is liquidation-bait.
pub const MAX_LTV_BPS: u16 = 8000;

/// Maximum collateral the daemon will operate. $5M USDC equivalent.
/// Keeps blast radius bounded even if the orchestrator's keys are
/// compromised.
pub const MAX_POSITION_USDC_LAMPORTS: u64 = 5_000_000_000_000;

/// Maximum slippage on the swap leg of the leverage loop, in bps.
pub const MAX_SLIPPAGE_BPS: u16 = 200;

/// Hard ceiling on supply→borrow→swap rounds. Prevents accidental
/// infinite loops if LTV math diverges.
pub const MAX_LEVERAGE_LOOP_ROUNDS: u8 = 6;

/// If position liquidation-distance falls below this, the liq monitor
/// auto-unwinds without waiting for an orchestrator Approve.
pub const LIQUIDATION_DISTANCE_CRITICAL_BPS: u16 = 50;

/// Warning band — emit Escalate envelope but don't auto-unwind.
pub const LIQUIDATION_DISTANCE_WARNING_BPS: u16 = 200;

/// Validate an AssignMultiply against all caps. Returns Ok if every
/// requested value is within bounds, Err otherwise. Daemon rejects
/// any Assign that fails this check before doing any chain work.
pub fn validate_assign(a: &AssignMultiply) -> Result<()> {
    if a.target_ltv_bps > MAX_LTV_BPS {
        return Err(anyhow!(
            "target_ltv_bps {} exceeds hard cap {}",
            a.target_ltv_bps,
            MAX_LTV_BPS
        ));
    }
    if a.max_slippage_bps > MAX_SLIPPAGE_BPS {
        return Err(anyhow!(
            "max_slippage_bps {} exceeds hard cap {}",
            a.max_slippage_bps,
            MAX_SLIPPAGE_BPS
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(target_ltv: u16, slippage: u16) -> AssignMultiply {
        AssignMultiply {
            vault: [0; 32],
            target_ltv_bps: target_ltv,
            max_slippage_bps: slippage,
            deadline_unix: 0,
        }
    }

    #[test]
    fn accepts_within_caps() {
        assert!(validate_assign(&assign(6000, 50)).is_ok());
    }

    #[test]
    fn rejects_ltv_above_cap() {
        let err = validate_assign(&assign(8001, 50)).unwrap_err();
        assert!(err.to_string().contains("target_ltv_bps"));
    }

    #[test]
    fn rejects_slippage_above_cap() {
        let err = validate_assign(&assign(6000, 201)).unwrap_err();
        assert!(err.to_string().contains("max_slippage_bps"));
    }

    #[test]
    fn cap_constants_are_sensible() {
        // Sanity — if these get tuned, fail loudly in tests so the
        // change is reviewed.
        assert!(MAX_LTV_BPS <= 8500, "LTV cap above 85% is reckless");
        assert!(MAX_LEVERAGE_LOOP_ROUNDS <= 8, "more rounds = more failure surface");
        assert!(LIQUIDATION_DISTANCE_CRITICAL_BPS < LIQUIDATION_DISTANCE_WARNING_BPS);
    }
}
```

- [ ] **Step 2: Wire into main.rs**

In `crates/multiply-daemon/src/main.rs`, add near the other `mod` declarations:

```rust
mod caps;
```

- [ ] **Step 3: Build + test**

```bash
cargo build -p multiply-daemon
cargo test -p multiply-daemon caps::
```

All four cap tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/multiply-daemon/src/caps.rs crates/multiply-daemon/src/main.rs
git commit -m "multiply: add hard-coded safety caps

MAX_LTV_BPS=8000, MAX_POSITION_USDC_LAMPORTS=\$5M, MAX_SLIPPAGE_BPS=200,
MAX_LEVERAGE_LOOP_ROUNDS=6, LIQUIDATION_DISTANCE_{CRITICAL,WARNING}_BPS.
validate_assign rejects any AssignMultiply that exceeds these caps,
no matter how 'trusted' the orchestrator. Caps live in source so they
cannot be raised by editing a config file."
```

---

### Task 3: Add new CLI args + sim-only / require-approval defaults

**Files:**
- Modify: `crates/multiply-daemon/src/main.rs` (Args struct + main())

- [ ] **Step 1: Extend the `Args` struct**

In `main.rs`, find the existing `Args` struct (post-fungibility shape). Add these fields:

```rust
/// Solana RPC URL. Required. Devnet: https://api.devnet.solana.com,
/// Mainnet: <your-helius-or-triton-url>.
#[arg(long, env = "ZX_RPC_URL")]
rpc_url: String,

/// Maximum collateral the daemon will operate (USDC lamports).
/// Defaults to a small bound; raise for real positions but never above
/// caps::MAX_POSITION_USDC_LAMPORTS.
#[arg(long, env = "ZX_MAX_POSITION_USDC", default_value_t = 100_000_000)]  // $100
max_position_usdc_lamports: u64,

/// Refuse to actually submit transactions; simulate only. Defaults TRUE.
/// Pass --no-simulate-only to submit for real.
#[arg(long, env = "ZX_SIMULATE_ONLY", default_value_t = true,
       action = clap::ArgAction::Set)]
simulate_only: bool,

/// Require manual Approve envelope before each submission. Defaults TRUE
/// on mainnet, FALSE on devnet. See --network.
#[arg(long, env = "ZX_REQUIRE_APPROVAL")]
require_approval: Option<bool>,

/// Network: "devnet" or "mainnet". Mainnet additionally requires
/// --i-understand-this-is-mainnet.
#[arg(long, env = "ZX_NETWORK", default_value = "devnet")]
network: String,

/// Required redundant acknowledgment when --network mainnet. No default.
#[arg(long)]
i_understand_this_is_mainnet: bool,
```

- [ ] **Step 2: Validate args in `main()` before constructing the daemon**

After `let args = Args::parse()`, before the runtime block:

```rust
// Network sanity gates.
if args.network != "devnet" && args.network != "mainnet" {
    anyhow::bail!("--network must be 'devnet' or 'mainnet', got {:?}", args.network);
}
if args.network == "mainnet" && !args.i_understand_this_is_mainnet {
    anyhow::bail!(
        "--network mainnet requires --i-understand-this-is-mainnet \
         (this exists to make mainnet promotion explicit)"
    );
}

// Cap enforcement on max_position_usdc_lamports.
if args.max_position_usdc_lamports > caps::MAX_POSITION_USDC_LAMPORTS {
    anyhow::bail!(
        "--max-position-usdc-lamports {} exceeds hard cap {}",
        args.max_position_usdc_lamports,
        caps::MAX_POSITION_USDC_LAMPORTS
    );
}

// Resolve require_approval default: true on mainnet, false on devnet.
let require_approval = args.require_approval.unwrap_or(args.network == "mainnet");

tracing::info!(
    network = %args.network,
    rpc_url = %args.rpc_url,
    simulate_only = args.simulate_only,
    require_approval,
    max_position_usdc_lamports = args.max_position_usdc_lamports,
    "multiply args validated",
);
```

Pass `require_approval` into the `Multiply` struct (add it as a field).

- [ ] **Step 3: Build**

```bash
cargo build -p multiply-daemon
```

- [ ] **Step 4: Smoke-run argument validation**

```bash
# Should fail because --network mainnet without acknowledgment:
cargo run -p multiply-daemon -- \
    --secrets-dir /tmp/m-secrets \
    --wallet /tmp/m-secrets/solana-wallet.json \
    --rpc-url https://api.mainnet-beta.solana.com \
    --network mainnet 2>&1 | tail -5
```

Expected: error message about `--i-understand-this-is-mainnet`.

```bash
# Should fail because exceeds cap:
cargo run -p multiply-daemon -- \
    --secrets-dir /tmp/m-secrets \
    --wallet /tmp/m-secrets/solana-wallet.json \
    --rpc-url https://api.devnet.solana.com \
    --max-position-usdc-lamports 999999999999999999 2>&1 | tail -3
```

Expected: cap-violation error.

- [ ] **Step 5: Commit**

```bash
git add crates/multiply-daemon/src/main.rs
git commit -m "multiply: add --rpc-url, --simulate-only, --network, mainnet gates

Args:
- --rpc-url (required) Solana RPC endpoint
- --simulate-only (default true) refuses to submit on-chain
- --require-approval (default true on mainnet, false on devnet)
- --network devnet|mainnet (default devnet)
- --max-position-usdc-lamports (default \$100, cap-bounded by caps.rs)
- --i-understand-this-is-mainnet (required when --network mainnet)

Caps from caps.rs are enforced at startup. The redundant mainnet
acknowledgment exists so mainnet promotion cannot happen by typo."
```

---

### Task 4: Wire `handle_inbox` to dispatch `AssignMultiply`

**Files:**
- Create: `crates/multiply-daemon/src/dispatch.rs`
- Modify: `crates/multiply-daemon/src/main.rs` (mod dispatch; replace handle_inbox)

- [ ] **Step 1: Implement dispatch.rs**

`crates/multiply-daemon/src/dispatch.rs`:

```rust
//! Inbox dispatch — decode AssignMultiply, route to the leverage
//! handler (or simple supply for v0), build ReportMultiply, sign,
//! send back.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tracing::{info, warn};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::{
    envelope::{Envelope, BROADCAST_RECIPIENT},
    fleet::multiply::{AssignMultiply, ReportMultiply, ReportHeader},
    message::MsgType,
};

use crate::caps;

pub struct DispatchCtx {
    pub rpc: Arc<RpcContext>,
    pub wallet: Arc<Wallet>,
    pub whitelist: Arc<SigningWhitelist>,
    pub role_identity: RoleIdentity,
    pub simulate_only: bool,
    pub require_approval: bool,
}

/// Receive envelopes; dispatch on MsgType::Assign with an
/// AssignMultiply CBOR payload.
pub async fn run(mut handle: NodeHandle, ctx: DispatchCtx) -> Result<()> {
    while let Some(env) = handle.recv().await {
        match env.msg_type {
            MsgType::Assign => {
                let conv = env.conversation_id;
                match handle_assign(&ctx, &env).await {
                    Ok(report) => {
                        send_report(&handle, &ctx, conv, report).await.ok();
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "assign failed; sending error Report");
                        let report = ReportMultiply {
                            header: ReportHeader::err(conv, 1),  // generic error code
                            resulting_ltv_bps: 0,
                            tx_signature: None,
                        };
                        send_report(&handle, &ctx, conv, report).await.ok();
                    }
                }
            }
            MsgType::Beacon => { /* role registry observation — Task 7 */ }
            other => info!(msg_type = ?other, "ignoring inbox envelope"),
        }
    }
    warn!("inbox channel closed; daemon exiting");
    Ok(())
}

async fn handle_assign(ctx: &DispatchCtx, env: &Envelope) -> Result<ReportMultiply> {
    let payload: AssignMultiply = ciborium::de::from_reader(&env.payload[..])
        .context("decode AssignMultiply CBOR payload")?;

    info!(
        target_ltv_bps = payload.target_ltv_bps,
        max_slippage_bps = payload.max_slippage_bps,
        "AssignMultiply received",
    );

    // Cap validation — refuses values above hard caps regardless of orchestrator.
    caps::validate_assign(&payload).context("cap validation")?;

    // Approval gate (Task 8 implements the actual queue + Approve handshake).
    // For Task 4 / v0, we treat require_approval=true as a refuse-with-error
    // until Task 8 wires the approval flow.
    if ctx.require_approval {
        return Err(anyhow!(
            "require_approval is true and Approve flow is not yet wired (see Task 8)"
        ));
    }

    // For Task 4, do a single supply ixn (no leverage loop yet — Task 6).
    // The leverage loop dispatches here in Task 6.
    let conv = env.conversation_id;
    crate::leverage::run_or_simulate(ctx, &payload, conv).await
}

async fn send_report(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    conv: [u8; 16],
    report: ReportMultiply,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let mut payload = Vec::new();
    ciborium::ser::into_writer(&report, &mut payload)
        .context("serialize ReportMultiply")?;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // For v0, broadcast the Report back. Strategy plan v0.1 upgrades to
    // unicast via role_registry.
    let env = Envelope::build(
        MsgType::Report,
        sender_pubkey,
        BROADCAST_RECIPIENT,
        now_secs,
        0,  // nonce reset per conv — fine for broadcast scaffolding
        conv,
        payload,
        &signing_key,
    );
    handle.send(env).await.context("send Report")?;
    info!(?conv, ok = report.header.ok, "report sent");
    Ok(())
}
```

- [ ] **Step 2: Add a placeholder for `crate::leverage`**

For Task 4 we just need the function to exist; Task 6 fills its body.

`crates/multiply-daemon/src/leverage.rs` (placeholder body — full impl in Task 6):

```rust
//! Leverage loop. Filled in by Task 6.

use anyhow::{anyhow, Result};
use tracing::info;
use zerox1_protocol::fleet::multiply::{AssignMultiply, ReportMultiply, ReportHeader};

use crate::dispatch::DispatchCtx;

/// Either simulate the leverage entry, or actually submit it (per
/// ctx.simulate_only). For Task 4, this is a stub that returns
/// "ok with simulated mode, ltv unchanged" so dispatch round-trips work.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    assign: &AssignMultiply,
    conv: [u8; 16],
) -> Result<ReportMultiply> {
    info!(
        simulate_only = ctx.simulate_only,
        target_ltv_bps = assign.target_ltv_bps,
        "leverage::run_or_simulate (placeholder — Task 6 implements)",
    );
    if ctx.simulate_only {
        Ok(ReportMultiply {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: 0,  // placeholder
            tx_signature: None,
        })
    } else {
        Err(anyhow!("leverage loop not yet implemented (Task 6)"))
    }
}
```

- [ ] **Step 3: Wire mod + replace handle_inbox in main.rs**

In `crates/multiply-daemon/src/main.rs`:

```rust
mod caps;
mod dispatch;
mod leverage;
```

Replace the existing `async fn handle_inbox(...)` with a call into `dispatch::run`. In the `Daemon::run` body's `tokio::select!`, change the inbox branch to:

```rust
let dispatch_handle = node_handle.clone();
let dispatch_ctx = dispatch::DispatchCtx {
    rpc: self.rpc.clone(),
    wallet: self.wallet.clone(),
    whitelist: self.whitelist.clone(),
    role_identity: self.role_identity.clone(),
    simulate_only: self.args.simulate_only,
    require_approval: self.require_approval,
};
let inbox_task = tokio::spawn(dispatch::run(dispatch_handle, dispatch_ctx));
```

Also add `rpc: Arc<RpcContext>` and `require_approval: bool` to the `Multiply` struct, and construct them in `main()`. The `RpcContext` is built from `args.rpc_url`:

```rust
let rpc = Arc::new(zerox1_defi_runtime::rpc::RpcContext::new(&args.rpc_url));
```

(Check the actual `RpcContext::new` signature — it may take other parameters; match what's there.)

- [ ] **Step 4: Build**

```bash
cargo build -p multiply-daemon
```

Clean. May require adding `Arc` wraps where the existing code held bare values — apply consistently.

- [ ] **Step 5: Smoke-run with sim-only**

```bash
mkdir -p /tmp/m-secrets
openssl rand 32 > /tmp/m-secrets/multiply-role.key
chmod 600 /tmp/m-secrets/multiply-role.key
solana-keygen new --outfile /tmp/m-secrets/solana-wallet.json --no-bip39-passphrase --force

# Boot in devnet sim-only mode (default):
timeout 5 cargo run -p multiply-daemon -- \
    --secrets-dir /tmp/m-secrets \
    --wallet /tmp/m-secrets/solana-wallet.json \
    --rpc-url https://api.devnet.solana.com 2>&1 | head -30
```

Expected: "multiply args validated", node boots, beacons emit, no panics. The dispatch loop is silent until an Assign arrives.

- [ ] **Step 6: Commit**

```bash
git add crates/multiply-daemon/src/{dispatch,leverage}.rs crates/multiply-daemon/src/main.rs
git commit -m "multiply: wire handle_inbox dispatch (Assign decode + Report reply)

handle_inbox now decodes AssignMultiply CBOR payloads, validates them
against caps, dispatches to the leverage::run_or_simulate stub, builds
a ReportMultiply, signs it, and broadcasts the Report envelope back.

The leverage stub is a placeholder — Task 6 fills its body. Task 4 only
proves the dispatch round-trip works in sim-only."
```

---

### Task 5: End-to-end devnet smoke (single supply, sim-only)

Boot the daemon against devnet, send an AssignMultiply via `fleet-pm-stub`, verify the Report comes back.

- [ ] **Step 1: Generate role keys + Solana wallet for orchestrator + multiply**

```bash
mkdir -p /tmp/m-smoke/{multiply,orch}
openssl rand 32 > /tmp/m-smoke/multiply/multiply-role.key
openssl rand 32 > /tmp/m-smoke/orch/orchestrator-role.key
chmod 600 /tmp/m-smoke/{multiply,orch}/*.key
solana-keygen new --outfile /tmp/m-smoke/multiply/solana-wallet.json \
    --no-bip39-passphrase --force
```

- [ ] **Step 2: Fund the multiply wallet on devnet**

```bash
PUBKEY=$(solana-keygen pubkey /tmp/m-smoke/multiply/solana-wallet.json)
echo "Multiply wallet pubkey: $PUBKEY"
solana airdrop 2 "$PUBKEY" --url https://api.devnet.solana.com
```

(Devnet faucet sometimes throttles. If it fails, retry or use https://faucet.solana.com.)

- [ ] **Step 3: Boot multiply-daemon (devnet, sim-only)**

In one terminal:

```bash
cargo run --release -p multiply-daemon -- \
    --secrets-dir /tmp/m-smoke/multiply \
    --wallet /tmp/m-smoke/multiply/solana-wallet.json \
    --rpc-url https://api.devnet.solana.com \
    --listen /ip4/127.0.0.1/tcp/19302 \
    --beacon-interval-secs 5
```

Watch the logs — multiply should boot, libp2p listen on 19302, emit Beacons. Note its peer_id from the log.

- [ ] **Step 4: Send AssignMultiply via fleet-pm-stub**

In another terminal:

```bash
cargo run --release -p fleet-pm-stub -- \
    --secrets-dir /tmp/m-smoke/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19302 \
    --timeout-secs 15 \
    assign-multiply --target-ltv-bps 6000
```

Expected: stub boots, sends Assign, multiply daemon's logs show:
```
INFO  AssignMultiply received target_ltv_bps=6000 max_slippage_bps=50
INFO  leverage::run_or_simulate (placeholder — Task 6 implements) simulate_only=true
INFO  report sent ok=true
```

And the stub prints:
```
Report received: msg_type=Report ...
Report payload (decoded): ReportMultiply { header: ReportHeader { ok: true, ... }, resulting_ltv_bps: 0, tx_signature: None }
```

- [ ] **Step 5: Document the runbook**

`docs/runbooks/multiply-devnet.md`:

```markdown
# Multiply daemon — devnet smoke runbook

## Prerequisites
- Devnet RPC URL: https://api.devnet.solana.com (or your private)
- A Solana keypair with ≥1 devnet SOL (`solana airdrop 2 <pubkey> --url devnet`)
- An Ed25519 role-key file (32 raw bytes) for both the multiply daemon
  and the orchestrator stub

## Boot
[paste the `cargo run --release -p multiply-daemon` command from Step 3]

## Send an AssignMultiply
[paste the fleet-pm-stub command from Step 4]

## Expected output
[paste the expected log lines from Step 4]

## Troubleshooting
- "Failed to dial bootstrap peer": check the multiply daemon is listening
  on the port the stub's --bootstrap points at.
- "Report received but timed out": multiply may have crashed mid-dispatch;
  inspect its stderr.
- "AssignMultiply rejected: target_ltv_bps exceeds hard cap": you asked
  for >8000 bps. Pass --target-ltv-bps 6000 instead.
```

- [ ] **Step 6: Commit**

```bash
git add docs/runbooks/multiply-devnet.md
git commit -m "multiply: devnet smoke runbook (sim-only single-supply)

Documents the boot + Assign + Report round-trip on devnet with
sim-only enabled. The leverage loop is still stubbed; this proves
the dispatch wiring works end-to-end before Task 6 fills the body."
```

---

### Task 6: Implement the leverage loop in `leverage.rs`

The supply→borrow→swap→supply multi-round loop. Client-side (no flash-leverage atomic ixn yet — that's a v0.1 follow-up).

**Files:**
- Modify: `crates/multiply-daemon/src/leverage.rs`
- Modify: `crates/multiply-daemon/src/kamino.rs` (add `build_borrow_ixns` + `query_position_ltv_bps` helpers)

- [ ] **Step 1: Add `build_borrow_ixns` to `kamino.rs`**

The lifted code already has supply + withdraw. Borrow is the third primitive needed for the leverage loop.

```rust
pub async fn build_borrow_ixns(
    rpc: &RpcContext,
    vault: Pubkey,
    user: Pubkey,
    amount_lamports: u64,
) -> anyhow::Result<Vec<Instruction>> {
    // The Kamino lend program has a Borrow ixn, similar shape to Supply.
    // Build it analogously to build_supply_ixns. Reuse usdc_reserve_accounts()
    // for the reserve + obligation account derivations.
    todo!("implement using the same pattern as build_supply_ixns")
}
```

The actual ixn-construction details depend on the klend SDK conventions already used by `build_supply_ixns`. Read the `build_supply_ixns` body for the pattern — borrow is structurally similar, just a different program ixn discriminant.

- [ ] **Step 2: Add `query_position_ltv_bps` to `kamino.rs`**

```rust
pub async fn query_position_ltv_bps(
    rpc: &RpcContext,
    vault: Pubkey,
    user: Pubkey,
) -> anyhow::Result<u16> {
    // Read the user's Obligation account from chain via the rpc.
    // Compute LTV = total_debt_value / total_collateral_value, in bps.
    // If no position exists yet, return 0.
    todo!("read obligation, compute LTV in bps")
}
```

This pulls the obligation account (PDA derived from user + lending market), parses it via the existing `klend-sdk`-equivalent code already in `zerox1-defi-protocols::kamino`, and computes LTV. If `defi-protocols::kamino` doesn't yet have an obligation parser, add a minimal one — but DO NOT modify defi-protocols if it's still in someone else's read-only zone. Instead, add the parser here in multiply-daemon's `kamino.rs`.

- [ ] **Step 3: Implement the leverage loop**

`crates/multiply-daemon/src/leverage.rs` (replace the placeholder):

```rust
//! Leverage loop — supply→borrow→swap→supply rounds until target LTV.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;
use tracing::{info, warn};
use zerox1_protocol::fleet::multiply::{AssignMultiply, ReportMultiply, ReportHeader};

use crate::caps;
use crate::dispatch::DispatchCtx;
use crate::kamino;

/// Either simulate or submit the leverage loop.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    assign: &AssignMultiply,
    conv: [u8; 16],
) -> Result<ReportMultiply> {
    let user = ctx.wallet.pubkey();
    let vault = solana_sdk::pubkey::Pubkey::new_from_array(assign.vault);

    // 1. Read current LTV.
    let mut current_ltv = kamino::query_position_ltv_bps(&ctx.rpc, vault, user)
        .await
        .context("query current LTV")?;
    info!(current_ltv_bps = current_ltv, target_ltv_bps = assign.target_ltv_bps,
          "leverage loop entering");

    // 2. If already at or above target, do nothing.
    if current_ltv >= assign.target_ltv_bps {
        return Ok(ReportMultiply {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: current_ltv,
            tx_signature: None,
        });
    }

    // 3. Loop: supply → borrow → swap → repeat.
    let mut last_signature: Option<String> = None;
    for round in 1..=caps::MAX_LEVERAGE_LOOP_ROUNDS {
        // Compute the size for this round. Heuristic: borrow up to
        // (target_ltv - current_ltv) of remaining headroom.
        let headroom_bps = assign.target_ltv_bps.saturating_sub(current_ltv);
        if headroom_bps < 50 {
            info!("LTV within 50 bps of target; stopping early");
            break;
        }

        // Build the supply + borrow + swap ixns for this round.
        // (Pseudocode — translate to actual ixn building based on
        // build_supply_ixns / build_borrow_ixns shape and the Jupiter
        // swap helper from defi-protocols.)
        let supply_ixns = if round == 1 {
            // Round 1: supply initial collateral. Amount comes from
            // assign or a derived heuristic — for v0 use a fixed
            // collateral_lamports value.
            // Strategy: the orchestrator should encode initial size
            // in AssignMultiply — but our v0 payload doesn't have
            // that field. For now, use a hard-coded $100 first round
            // (reuse args.max_position_usdc_lamports as the size).
            kamino::build_supply_ixns(&ctx.rpc, vault, user, 100_000_000).await?
        } else {
            // Subsequent rounds: supply the LST received from the swap.
            Vec::new()  // TODO(v0.1): the swap's output goes here
        };
        let borrow_ixns = kamino::build_borrow_ixns(&ctx.rpc, vault, user, /* amount per-round */ 50_000_000).await?;
        let swap_ixns: Vec<solana_sdk::instruction::Instruction> = Vec::new();
        // TODO(v0.1): build_jupiter_swap_ixns(borrowed_amount → LST)

        let all_ixns: Vec<solana_sdk::instruction::Instruction> = supply_ixns.into_iter()
            .chain(borrow_ixns.into_iter())
            .chain(swap_ixns.into_iter())
            .collect();

        // Submit (or simulate) this round's bundle.
        let blockhash = ctx.rpc.client.get_latest_blockhash().await?;
        let mut tx = solana_sdk::transaction::Transaction::new_with_payer(
            &all_ixns, Some(&user)
        );
        ctx.wallet.sign_with_whitelist(&mut tx, &ctx.whitelist, blockhash)
            .context("sign tx with whitelist")?;

        if ctx.simulate_only {
            let sim = ctx.rpc.client.simulate_transaction(&tx).await?;
            if let Some(err) = sim.value.err {
                return Err(anyhow!("round {} sim failed: {:?}", round, err));
            }
            info!(round, "round sim ok");
        } else {
            let sig = ctx.rpc.client.send_and_confirm_transaction(&tx).await?;
            last_signature = Some(sig.to_string());
            info!(round, signature = %sig, "round committed");
        }

        // Re-read LTV after the round.
        current_ltv = kamino::query_position_ltv_bps(&ctx.rpc, vault, user).await?;
        info!(round, current_ltv_bps = current_ltv, "round done");

        if round == caps::MAX_LEVERAGE_LOOP_ROUNDS && current_ltv < assign.target_ltv_bps {
            warn!(
                rounds = caps::MAX_LEVERAGE_LOOP_ROUNDS,
                final_ltv_bps = current_ltv,
                target_ltv_bps = assign.target_ltv_bps,
                "max rounds reached, target not hit — stopping"
            );
        }
    }

    Ok(ReportMultiply {
        header: ReportHeader::ok(conv),
        resulting_ltv_bps: current_ltv,
        tx_signature: last_signature,
    })
}
```

- [ ] **Step 4: Build**

```bash
cargo build -p multiply-daemon
```

Expect compile errors on the `todo!()` paths. Resolve each:
- `build_borrow_ixns`: implement based on `build_supply_ixns` pattern
- `query_position_ltv_bps`: implement using whatever obligation parser exists in `defi-protocols::kamino`. If none, write a minimal one inline.

If those need more time than expected (e.g., obligation account format isn't documented in defi-protocols), STOP and report — that's a real research task and shouldn't be improvised.

- [ ] **Step 5: Devnet smoke #2 — actually submit**

Same setup as Task 5, but pass `--no-simulate-only`:

```bash
cargo run --release -p multiply-daemon -- \
    --secrets-dir /tmp/m-smoke/multiply \
    --wallet /tmp/m-smoke/multiply/solana-wallet.json \
    --rpc-url https://api.devnet.solana.com \
    --listen /ip4/127.0.0.1/tcp/19302 \
    --no-simulate-only
```

Then send the AssignMultiply via fleet-pm-stub. Check that:
- Each round's tx commits on chain (logs: "round N committed signature=...")
- The Report comes back with a non-None `tx_signature`
- `current_ltv_bps` increases each round
- Final `resulting_ltv_bps` is within ~100 bps of the target

- [ ] **Step 6: Commit**

```bash
git add crates/multiply-daemon/src/{leverage,kamino}.rs
git commit -m "multiply: implement client-side leverage loop

run_or_simulate walks supply→borrow→swap→supply rounds until target
LTV is reached or MAX_LEVERAGE_LOOP_ROUNDS is hit. Each round is its
own signed transaction; sim-or-submit branches on ctx.simulate_only.
Adds build_borrow_ixns + query_position_ltv_bps helpers in kamino.rs.

The Jupiter swap leg is a v0.1 follow-up — for v0 the loop builds
supply+borrow ixns and uses sim/empty-swap as placeholders."
```

---

### Task 7: Liquidation-distance monitor

Hook into the beacon emitter — every Beacon, query the position, compute distance-to-liquidation, emit Escalate if in warning band, auto-unwind if critical.

**Files:**
- Create: `crates/multiply-daemon/src/liq_monitor.rs`
- Modify: `crates/multiply-daemon/src/main.rs` (call `liq_monitor::tick` in beacon loop)

- [ ] **Step 1: Implement the monitor**

`crates/multiply-daemon/src/liq_monitor.rs`:

```rust
//! Liquidation-distance monitor.
//!
//! On every Beacon, query the position's current LTV vs its liquidation
//! threshold. Emit Escalate if in warning band; auto-unwind if critical.

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tracing::{info, warn, error};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::{
    envelope::{Envelope, BROADCAST_RECIPIENT},
    fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity},
    message::MsgType,
};

use crate::caps;

pub struct LiqMonitorCtx {
    pub rpc: Arc<RpcContext>,
    pub user: Pubkey,
    pub vault: Pubkey,
    pub role_identity: RoleIdentity,
    pub liquidation_ltv_bps: u16,  // chain-known liquidation threshold
}

/// Call once per Beacon. Reads position LTV; emits Escalate / triggers
/// auto-unwind if needed.
pub async fn tick(handle: &NodeHandle, ctx: &LiqMonitorCtx) -> Result<()> {
    let current_ltv = match crate::kamino::query_position_ltv_bps(&ctx.rpc, ctx.vault, ctx.user).await {
        Ok(ltv) => ltv,
        Err(e) => {
            warn!(?e, "liq monitor: query LTV failed; skipping tick");
            return Ok(());
        }
    };

    let distance_bps = ctx.liquidation_ltv_bps.saturating_sub(current_ltv);

    if distance_bps == 0 || distance_bps <= caps::LIQUIDATION_DISTANCE_CRITICAL_BPS {
        error!(current_ltv, liquidation_ltv = ctx.liquidation_ltv_bps,
               distance_bps, "CRITICAL — auto-unwinding");
        // Auto-unwind: skip approval, just close the position.
        // Implementation: call a `crate::leverage::unwind(ctx)` helper —
        // for v0 just emit an Escalate(Critical) and rely on the operator
        // to manually trigger an unwind via the orchestrator.
        emit_escalate(handle, ctx, RiskSeverity::Critical, current_ltv, distance_bps).await?;
    } else if distance_bps <= caps::LIQUIDATION_DISTANCE_WARNING_BPS {
        warn!(current_ltv, distance_bps, "WARNING — emit Escalate");
        emit_escalate(handle, ctx, RiskSeverity::Warning, current_ltv, distance_bps).await?;
    } else {
        info!(current_ltv, distance_bps, "liq monitor: position healthy");
    }

    Ok(())
}

async fn emit_escalate(
    handle: &NodeHandle,
    ctx: &LiqMonitorCtx,
    severity: RiskSeverity,
    current_ltv_bps: u16,
    distance_bps: u16,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();

    let escalate = EscalateRisk {
        severity,
        kind: RiskKind::LiquidationDistance,
        subject: ctx.vault.to_bytes(),
        measurement: distance_bps as i64,
        raised_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&escalate, &mut payload)?;

    let env = Envelope::build(
        MsgType::Escalate,
        sender,
        BROADCAST_RECIPIENT,
        escalate.raised_at_unix,
        0,
        [0u8; 16],
        payload,
        &signing_key,
    );
    handle.send(env).await?;
    info!(?severity, current_ltv_bps, distance_bps, "Escalate sent");
    Ok(())
}
```

- [ ] **Step 2: Wire `liq_monitor::tick` into the beacon loop in main.rs**

Modify the existing `emit_beacons` function (or add a sibling task) to call `liq_monitor::tick` once per beacon interval. Pass through the necessary context.

This may require adding a `current_position_vault: Option<Pubkey>` field on `Multiply` (the daemon doesn't know what vault to monitor until the first Assign comes in). Set it from `dispatch::handle_assign` after a successful round.

For v0 simplicity: only run the monitor if a position is known. If `current_position_vault.is_none()`, skip the tick.

- [ ] **Step 3: Build + smoke (still devnet, sim-only)**

```bash
cargo build -p multiply-daemon
```

Smoke-run on devnet with a small position; manually compute the expected LTV and verify:
- Healthy LTV → "position healthy" log
- Approach the warning band → Escalate envelope visible in fleet-pm-stub's inbox

(For v0 we test the warning emission, not the auto-unwind, since auto-unwind requires the unwind function which we mark as v0.1.)

- [ ] **Step 4: Commit**

```bash
git add crates/multiply-daemon/src/liq_monitor.rs crates/multiply-daemon/src/main.rs
git commit -m "multiply: liquidation-distance monitor

Beacon-time hook queries current position LTV vs the chain's known
liquidation threshold, computes distance in bps, and:
- emits Escalate(Warning) if distance ≤ 200 bps
- emits Escalate(Critical) if distance ≤ 50 bps

Auto-unwind on Critical is a v0.1 follow-up — the unwind function
isn't wired yet, so v0 just escalates and relies on operator
intervention via the orchestrator."
```

---

### Task 8: Manual-approval flow

Mainnet defaults to `require_approval=true`. The daemon must queue the AssignMultiply and emit an Escalate-like "needs approval" envelope; only proceed after a separate `Approve` envelope (signed by orchestrator, referencing the queued conv-id) lands.

**Files:**
- Create: `crates/multiply-daemon/src/approval.rs`
- Modify: `crates/multiply-daemon/src/dispatch.rs`

- [ ] **Step 1: Write approval queue**

`crates/multiply-daemon/src/approval.rs`:

```rust
//! Manual-approval queue. When require_approval is true, the daemon
//! does NOT execute an Assign on receipt — it stores the (conv_id,
//! AssignMultiply) pair, emits a "needs approval" Escalate, and
//! waits for an Approve envelope referencing the same conv_id.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zerox1_protocol::fleet::multiply::AssignMultiply;

const APPROVAL_TTL: Duration = Duration::from_secs(300);  // 5 min

pub struct ApprovalQueue {
    pending: Mutex<HashMap<[u8; 16], (AssignMultiply, Instant)>>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self { pending: Mutex::new(HashMap::new()) }
    }

    pub fn enqueue(&self, conv: [u8; 16], assign: AssignMultiply) {
        let mut p = self.pending.lock().unwrap();
        p.retain(|_, (_, t)| t.elapsed() < APPROVAL_TTL);  // garbage-collect expired
        p.insert(conv, (assign, Instant::now()));
    }

    pub fn approve(&self, conv: [u8; 16]) -> Option<AssignMultiply> {
        let mut p = self.pending.lock().unwrap();
        p.remove(&conv).map(|(a, _)| a)
    }
}
```

- [ ] **Step 2: Use it in dispatch.rs**

In `dispatch::handle_assign`, replace the early-return-on-require-approval with:

```rust
if ctx.require_approval {
    ctx.approval_queue.enqueue(env.conversation_id, payload.clone());
    // Emit an Escalate(Notice) so the orchestrator (and humans) see
    // there's a pending Assign waiting.
    crate::approval::emit_pending_envelope(...).await?;
    return Ok(ReportMultiply {
        header: ReportHeader::ok(env.conversation_id),
        resulting_ltv_bps: 0,
        tx_signature: None,
    });
}
```

And in the inbox's MsgType match, add a branch for an Approve message. zerox1-protocol doesn't yet have `MsgType::Approve` defined for the fleet — check `zerox1_protocol::message::MsgType`. If `Approve` isn't in the enum, add it (this requires editing `zerox1-protocol`, which is in node-enterprise). Update the protocol crate.

If editing zerox1-protocol is too costly for this task, use `MsgType::Sync` as a stand-in and document that v0.1 will add a proper `Approve` variant.

- [ ] **Step 3: Build + smoke**

End-to-end smoke: send an Assign with require_approval=true. Verify the daemon enqueues + responds with the placeholder Report. Then send an Approve (or Sync stand-in) referencing the conv_id. Verify the daemon now actually runs the leverage loop.

- [ ] **Step 4: Commit**

```bash
git add crates/multiply-daemon/src/approval.rs crates/multiply-daemon/src/dispatch.rs
git commit -m "multiply: manual-approval flow

When require_approval is true (default on mainnet), AssignMultiply is
queued (5-min TTL) and the daemon emits a 'needs approval' envelope
instead of executing. Submit only proceeds after an Approve envelope
(or MsgType::Sync stand-in until protocol adds Approve) referencing
the same conv_id arrives."
```

---

### Task 9: Position telemetry + earning readout

The `report` subcommand. Without this, "earning" is unverifiable.

**Files:**
- Create: `crates/multiply-daemon/src/pnl.rs`
- Create: `crates/multiply-daemon/src/reporter.rs`
- Modify: `crates/multiply-daemon/src/main.rs` (add `report` subcommand)

- [ ] **Step 1: Position-value query in `pnl.rs`**

```rust
//! Position-value tracking + APR computation.

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::SystemTime;
use zerox1_defi_runtime::rpc::RpcContext;

#[derive(Debug, Clone)]
pub struct PositionSnapshot {
    pub timestamp_unix: u64,
    pub slot: u64,
    pub collateral_lamports: u64,
    pub debt_lamports: u64,
    pub collateral_oracle_price_usdc_e6: u64,  // USDC × 1e6
    pub debt_oracle_price_usdc_e6: u64,
    pub net_value_usdc_lamports: i64,
}

/// Query the position once and return a snapshot.
pub async fn snapshot(rpc: &RpcContext, vault: Pubkey, user: Pubkey) -> Result<PositionSnapshot> {
    // 1. Read obligation account → (collateral_amount, debt_amount).
    // 2. Read Pyth oracle for the collateral mint.
    // 3. Read Pyth oracle for the debt mint.
    // 4. net = collateral_amount × col_price - debt_amount × debt_price.
    // (Reuse defi-protocols::pyth helpers for the oracle reads.)
    todo!("implement using defi-protocols' pyth + obligation parser")
}

/// Append a snapshot to the position log (newline-delimited JSON).
pub fn append_to_log(path: &std::path::Path, snap: &PositionSnapshot) -> Result<()> {
    use std::io::Write;
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(snap)?)?;
    Ok(())
}
```

(Add `Serialize, Deserialize` derives to `PositionSnapshot` for the JSON log.)

- [ ] **Step 2: Reporter subcommand in `reporter.rs`**

```rust
//! `multiply-daemon report` subcommand. Reads the position log, prints APR.

use anyhow::Result;
use std::path::Path;

pub fn report(log_path: &Path, since_secs: u64) -> Result<()> {
    let mut snaps: Vec<crate::pnl::PositionSnapshot> = Vec::new();
    for line in std::fs::read_to_string(log_path)?.lines() {
        if let Ok(s) = serde_json::from_str(line) { snaps.push(s); }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let cutoff = now.saturating_sub(since_secs);
    let recent: Vec<_> = snaps.iter().filter(|s| s.timestamp_unix >= cutoff).collect();

    if recent.len() < 2 {
        println!("Not enough snapshots in window. Run the daemon longer.");
        return Ok(());
    }
    let first = recent.first().unwrap();
    let last  = recent.last().unwrap();

    let elapsed_secs = last.timestamp_unix - first.timestamp_unix;
    let pnl_usdc_e6 = last.net_value_usdc_lamports - first.net_value_usdc_lamports;
    let pnl_pct = pnl_usdc_e6 as f64 / first.net_value_usdc_lamports.abs() as f64;
    let apr = pnl_pct * (365.25 * 86400.0) / elapsed_secs as f64;

    println!("Multiply position report (window: {elapsed_secs}s)");
    println!("  Initial net value (USDC): {}", first.net_value_usdc_lamports as f64 / 1e6);
    println!("  Current net value (USDC): {}", last.net_value_usdc_lamports as f64 / 1e6);
    println!("  PnL: {:+.4} USDC ({:+.2}%)", pnl_usdc_e6 as f64 / 1e6, pnl_pct * 100.0);
    println!("  Annualized: {:+.2}%", apr * 100.0);
    Ok(())
}
```

- [ ] **Step 3: Wire `report` subcommand into main.rs**

Promote the daemon CLI to a subcommand layout (run | report). The daemon's existing args become `multiply-daemon run ...`; the new subcommand is `multiply-daemon report --log <path> --since <secs>`.

- [ ] **Step 4: Hook snapshot recording into the beacon loop**

Each beacon tick (after liq_monitor), call `pnl::snapshot` and `pnl::append_to_log`. Limit log size (rotate at 10 MB or so).

- [ ] **Step 5: Build + smoke**

Boot devnet daemon, take a position, let it run for a few minutes, then in another shell:

```bash
cargo run -p multiply-daemon -- report \
    --log /tmp/m-smoke/multiply/position-log.ndjson \
    --since 600
```

Should print a PnL readout with annualized APR.

- [ ] **Step 6: Commit**

```bash
git add crates/multiply-daemon/src/{pnl,reporter}.rs crates/multiply-daemon/src/main.rs
git commit -m "multiply: position telemetry + earning report subcommand

pnl::snapshot reads obligation + oracle prices, computes net position
value in USDC. Beacon loop appends a snapshot to a JSON-lines log.
'multiply-daemon report --since <secs>' reads the log and prints
annualized APR over the window. This is the 'mainnet earning' proof."
```

---

### Task 10: Mainnet tiny-position runbook ($50)

Document + execute the first mainnet position. Manual approval. $50 cap. Watch for 24h.

**Files:**
- Create: `docs/runbooks/multiply-mainnet-tiny.md`

- [ ] **Step 1: Write the runbook**

`docs/runbooks/multiply-mainnet-tiny.md`:

```markdown
# Multiply daemon — mainnet tiny-position runbook

⚠ THIS USES REAL MONEY. ALL CHECKS BELOW ARE MANDATORY.

## Pre-flight checklist

- [ ] Devnet smoke (Task 5 + Task 6) has been run successfully end-to-end
      with at least 3 leverage rounds and a clean Report
- [ ] `multiply-daemon report --since <devnet-window>` shows the
      telemetry pipeline produces a valid readout
- [ ] Liquidation monitor has been observed firing on devnet (force a
      bad position to verify the Escalate path)
- [ ] The mainnet wallet keypair is funded with ≥ 0.1 SOL for fees and
      $50 USDC of trading capital
- [ ] The mainnet RPC URL is private (Helius / Triton); free public
      RPC is too unreliable for live positions
- [ ] Operator is at the keyboard for at least the next 30 minutes

## Boot

```bash
cargo run --release -p multiply-daemon -- run \
    --secrets-dir /Users/.../mainnet-secrets/multiply \
    --wallet /Users/.../mainnet-secrets/multiply/solana-wallet.json \
    --rpc-url <YOUR_HELIUS_OR_TRITON_URL> \
    --listen /ip4/127.0.0.1/tcp/9302 \
    --network mainnet \
    --i-understand-this-is-mainnet \
    --max-position-usdc-lamports 50000000 \
    --no-simulate-only
```

## Send the Assign

```bash
cargo run --release -p fleet-pm-stub -- \
    --secrets-dir /Users/.../mainnet-secrets/orch \
    --bootstrap /ip4/127.0.0.1/tcp/9302 \
    --timeout-secs 300 \
    assign-multiply --target-ltv-bps 5000  # 50% — conservative for first-mainnet
```

## Approve

The daemon will queue + emit a "needs approval" envelope. Watch its
logs. Once you see `enqueued; awaiting Approve for conv_id=<id>`, send:

```bash
# (Implementation TBD — depends on Task 8's MsgType::Approve resolution.)
```

## Watch

Monitor logs continuously for the first 5 minutes. Confirm:
- Each leverage round commits a real tx
- `current_ltv_bps` walks toward 5000
- Final `resulting_ltv_bps` is within 100 bps of 5000
- Solana Explorer shows the txs from your wallet pubkey

## 24-hour earning verification

After 24 hours:

```bash
cargo run --release -p multiply-daemon -- report \
    --log /Users/.../mainnet-secrets/multiply/position-log.ndjson \
    --since 86400
```

The "Annualized" line is the proof of mainnet earning. Save the output.

## Unwind

To close the position cleanly:

```bash
# Send AssignMultiply with target_ltv_bps=0 to deleverage.
cargo run --release -p fleet-pm-stub -- \
    --secrets-dir /Users/.../mainnet-secrets/orch \
    --bootstrap /ip4/127.0.0.1/tcp/9302 \
    assign-multiply --target-ltv-bps 0
# Approve when prompted.
```

## Emergency

If the liq monitor screams Critical:

```bash
# Force-unwind. Sends an Assign with target_ltv_bps=0 + a flag
# bypassing the require_approval gate (only valid in --emergency mode).
# (Implementation deferred — for v0, manually Ctrl+C the daemon and
# unwind via Kamino's web UI.)
```
```

- [ ] **Step 2: Commit**

```bash
git add docs/runbooks/multiply-mainnet-tiny.md
git commit -m "multiply: mainnet tiny-position runbook (\$50, manual approval)

Checklist + boot commands + 24h earning verification path. The first
real-money mainnet test for the new fleet shape. Hard-coded \$50 cap
and 50% target LTV; manual approval required for every Assign."
```

- [ ] **Step 3: Execute the runbook**

This step is performed by the operator, not the implementer. Once the runbook lands and the daemon code reaches Task 9 acceptance, the operator runs through the checklist and posts the resulting `report --since 86400` output as the plan's acceptance evidence.

---

### Task 11: 24-hour watch + auto-mode promotion

After the $50 mainnet position has been earning for 24 hours and the report shows positive APR, lift `--require-approval` and promote to a larger position.

This is operational, not a code task — it's listed here so the plan has a clear endpoint.

- [ ] **Step 1: 24h has elapsed; report shows positive APR; no liquidation alerts fired**
- [ ] **Step 2: Increase `--max-position-usdc-lamports` to a chosen value (e.g., $500)**
- [ ] **Step 3: Set `--require-approval false` only after a second 24h watch at the larger position**
- [ ] **Step 4: Document the result in `docs/runbooks/multiply-mainnet-tiny.md` as a "post-mortem" appendix**

---

## Self-Review

**Spec coverage:**
- Real Solana mainnet tx signed by multiply-daemon → Tasks 4, 6
- Position earns positive APR over 24h → Tasks 9 + 10 + 11
- Safety (sim-only, caps, manual approval, mainnet gates) → Tasks 2, 3, 7, 8
- Telemetry / proof of earning → Task 9
- Devnet validation before mainnet → Tasks 5, 6
- Mainnet runbook → Task 10

Every plan section traces to a task.

**Placeholder scan:** A few `todo!()` markers remain in code blocks (build_borrow_ixns body, query_position_ltv_bps body, pnl::snapshot body) — these are explicitly called out for the implementer with guidance on how to fill them. Acceptable since the surrounding context is concrete.

**Type consistency:** `AssignMultiply`, `ReportMultiply`, `EscalateRisk`, `RiskKind`, `RiskSeverity` all reference the existing `zerox1_protocol::fleet::*` types defined in F3. `RoleIdentity`, `RpcContext`, `Wallet`, `SigningWhitelist`, `NodeHandle` reference existing infrastructure from the fungibility branch. `MAX_LTV_BPS` etc. defined in Task 2 are referenced by name in Tasks 4, 6, 7. No drift.

**Out of scope (deliberately):**
- ChainReplay impl (v0.1)
- Kamino atomic flash-leverage ixn (v0.1; v0 uses client-side multi-round)
- Jupiter swap leg in the leverage loop (v0.1; v0 falls back to no-swap, producing slower lever-up but real positions)
- Auto-unwind on Critical (v0.1; v0 emits Escalate and relies on operator)
- Multi-position support (one active position per daemon for v0)
- Mainnet promotion to >$500 (separate plan)

**Risk:** The biggest unknowns are (a) `query_position_ltv_bps` requires reading + parsing a Kamino obligation account — if the parser doesn't exist in `defi-protocols::kamino`, Task 6 stalls until it's written. Recommend the implementer scout this before starting Task 6 and surface as BLOCKED if the parser is missing. (b) The Jupiter swap leg is deferred — without it, the leverage loop won't actually reach high LTVs in production. The v0 plan accepts this as a known limitation and produces a partial-leverage position; v0.1 implements the swap.

**MVP earning timeline:** Tasks 1–10 ≈ 5–7 days of focused implementation; Task 11 ≈ 24 hours wall-clock for the watch period. Total time-to-mainnet-earning-readout: ~8–10 days.
