# 01fi Fleet — Six-Daemon Split Implementation Plan

> **2026-05-06 update:** This plan referenced Drift Protocol as a perp DEX. Drift was hacked April 2026 ($285M). Fleet pivoted to Jupiter Perps for hedgedjlp. See cleanup commit and the revised hedgedjlp plan.

> **2026-05-06 update:** Speculator removed from fleet. Six rounds of strategy research (Drift, Jupiter Perps, stablecoin forex, LST spread, stable-stable arb, RWA) found no viable always-on strategy for the slot on Solana in 2026. Fleet committed to 5-daemon architecture: multiply, stable-yield, hedgedjlp (pending), riskwatcher, researcher. Speculator may return when Drift relaunches Q3+ 2026 or new alpha source emerges. See `cleanup/remove-speculator` branch for the deletion commit.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the monolithic `zerox1-defi-daemon` (one binary, `--role` flag) into six purpose-built binaries — one per hedge-fund desk — each with its own runtime profile, key-access boundary, and IO shape, sharing a common `zerox1-defi-runtime` crate.

**Architecture:** A trading desk metaphor: the mobile app is the **PM** (orchestrator, holds custody). Five hosted daemons are specialized desks: **Risk** (read-only streaming watcher), **Yield-Lev** (Multiply / Kamino leveraged LST), **Yield-Neutral** (HedgedJlp / JLP+Adrena delta-neutral), **Yield-Reserve** (StableFloor / Sanctum INF gas reserve), **Quant Research** (backtests, on-chain history), **Execution Trader** (Speculator / latency-sensitive directional). Capability separation is by binary, not by flag — daemons that don't sign do not link the wallet crate. Each binary picks its own Tokio flavor, RPC pool, and persistence strategy. All inter-daemon coordination flows through HMAC-signed mesh envelopes (existing `pairing.rs` code, lifted into the shared runtime crate).

**Tech Stack:** Rust 2021, Tokio, libp2p (via `node/` mesh), `zerox1-defi-protocols` (existing), Solana SDK 2.0, Helius/Triton RPC, Yellowstone gRPC (riskwatcher only), sqlite (multiply/hedgedjlp only), Jito tip channel (hedgedjlp/speculator only).

**Scope boundary:** This plan delivers the **workspace split, shared runtime crate, and six runnable binary scaffolds** — each binary boots, identifies itself to the fleet, accepts mesh envelopes, and exposes a health endpoint. The migration of existing handler logic (`kamino.rs`, `sanctum.rs`, `pyth.rs`) into the appropriate daemon is included. Strategy logic beyond what already exists is **out of scope** — each daemon's strategy depth gets its own follow-up plan.

---

## ⚠ COORDINATION NOTE — strictly additive

**Another agent is actively working in `crates/zerox1-defi-daemon` AND `crates/zerox1-defi-protocols`.** This plan is therefore **strictly additive** with hard read-only zones.

**Read-only zones (do NOT modify any file under these paths):**
- `crates/zerox1-defi-daemon/` — the monolith
- `crates/zerox1-defi-protocols/` — the shared protocol clients

**Rules:**
- New crates that need code from the monolith get **COPIES** (`cp`), never moves. The duplication is intentional and temporary.
- New daemons depend on `zerox1-defi-protocols` via path (read-only consumer). They must compile against whatever shape `protocols` has on this worktree's HEAD, **without editing it**.
- The old monolith and `protocols` continue to build unchanged. The new fleet runs in parallel.
- Task 2 (lift→copy), Task 3 (no edits to old daemon), Task 10 (CLI/docs), Task 11 (decommission) are all adjusted or **deferred** — see notes inline.
- **Cleanup pass** happens in a follow-up plan once the other agent signals done on both daemon and protocols.

**Working directory:** All work happens in the git worktree at `/Users/tobiasd/Desktop/zerox1-01fi-fleet/` on branch `fleet/six-daemons`. The other agent's uncommitted work lives in the parallel checkout at `/Users/tobiasd/Desktop/zerox1/01fi/` on `main`. Subagents must always operate on the worktree path.

**If a subagent finds itself about to edit a file under `crates/zerox1-defi-daemon/` or `crates/zerox1-defi-protocols/`, stop immediately and surface it — it is a plan violation.**

**Specific warning for Task 4:** the original plan said "if `cargo tree` shows wallet contamination via protocols, split protocols into `-types` and `-signing`." That fix is **forbidden in this plan run** — it would edit `zerox1-defi-protocols`. If the cargo tree check finds the wallet crate in a no-key daemon's dep graph, **stop and surface to the user** instead of attempting the split.

**Path note:** All file paths in this plan that begin with `01fi/...` should be read as relative-to-worktree-root in this run, i.e. `01fi/Cargo.toml` → `Cargo.toml` at `/Users/tobiasd/Desktop/zerox1-01fi-fleet/Cargo.toml`. The worktree root **is** the 01fi project root.

---

## The Team — Mandate per Desk

Each daemon has a one-line mandate, a hard authority boundary, and a runtime profile. The mandates are the contract every binary's `main.rs` enforces.

### PM — Mobile Orchestrator (logical, not a server binary)

- **Mandate:** Holds custody of all funds. Decides what to delegate, to whom, with what budget, on what deadline.
- **Authority:** Full signing. Initiates every PROPOSE.
- **Runtime:** React Native + Kotlin/Swift native modules. Lives in `mobile/`. **Not built by this plan** — included in the team table only so role boundaries are explicit.
- **Identity:** `Role::Orchestrator` in fleet pairing.

### Risk — Risk Watcher

- **Mandate:** Continuously monitor oracle prices, lending health factors, depeg signals, perp funding, and liquidation distance for every position the fleet holds. Emit alerts. Never trade.
- **Authority:** **No keys.** Wallet crate is not linked. Cannot sign, by construction.
- **Latency target:** Sub-second from oracle update to alert fan-out.
- **Runtime:** Tokio multi-thread (4 workers). Persistent Pyth pull + Yellowstone gRPC subscriptions. Backpressured `tokio::sync::broadcast` channels for alert fan-out.
- **Persistence:** None hot-path. Append-only alert log to disk for forensics.
- **Deploy:** Active-active across two hosts; alerts deduped by content hash on the orchestrator side.
- **Binary:** `riskwatcher-daemon`.

### Yield-Lev — Multiply (Kamino leveraged LST)

- **Mandate:** Maintain a target-LTV leveraged LST position on Kamino. Rebalance on drift bands. Unwind on risk signal from Risk.
- **Authority:** Signs only Kamino program ixns. Signing whitelist enforced at the wallet layer.
- **Latency target:** Minutes. The hard requirement is **idempotent recovery** after crashes mid-flight.
- **Runtime:** Tokio current-thread, single-flight executor per position.
- **Persistence:** SQLite WAL journal of every ixn through `pending → submitted → confirmed`. On boot, replay the journal — never resubmit a confirmed ixn, always close an orphan.
- **Deploy:** Single instance, supervised. No replication.
- **Binary:** `multiply-daemon`.

### Yield-Neutral — HedgedJlp (Jupiter JLP + Adrena short)

- **Mandate:** Maintain a delta-neutral basket (long JLP, short SOL via Adrena) sized to a target notional. Rebalance the hedge when |delta| crosses a band.
- **Authority:** Signs only Jupiter mint/redeem and Adrena perp ixns. Whitelist enforced.
- **Latency target:** Both legs land within the same epoch. The execution risk is **leg mismatch** (one fills, one doesn't) → naked delta.
- **Runtime:** Tokio multi-thread (2 workers, one per leg), joined under a deadline. Two independent Jito-aware senders so a leg failure on one path doesn't block the other.
- **Persistence:** Append-only "leg-pair" ledger. Boot recovery's only job: detect orphan legs and close them.
- **Deploy:** Single instance, supervised. Co-locate with a low-jitter RPC.
- **Binary:** `hedgedjlp-daemon`.

### Yield-Reserve — StableFloor (Sanctum INF)

- **Mandate:** Hold the gas reserve in Sanctum INF (multi-LST liquid staking). Top up / drain on orchestrator request.
- **Authority:** Signs only Sanctum mint/redeem ixns.
- **Latency target:** Hours-to-days. Never the bottleneck.
- **Runtime:** Cron-driven, **single-shot subprocess** — invoked by the orchestrator over the mesh, executes one operation, exits. No resident process.
- **Persistence:** Last-success timestamp file only.
- **Deploy:** No long-running unit. Triggered by orchestrator.
- **Binary:** `stablefloor-daemon`.

### Quant Research — Researcher

- **Mandate:** Run backtests, scenario sims, and on-chain history scans on demand. Produce structured research artefacts (JSON / Parquet) consumed by the PM.
- **Authority:** **No keys.** Read-only RPC + archival access.
- **Latency target:** None. Throughput-bound.
- **Runtime:** Tokio multi-thread + Rayon for CPU-parallel sims. Mesh connection only while a job is running.
- **Persistence:** Per-job artefact directory. No daemon state.
- **Deploy:** Ephemeral worker on a fat host (or cloud burst). Idle = not running.
- **Binary:** `researcher-daemon`.

### Execution Trader — Speculator

- **Mandate:** Execute directional trades the PM hands down. Best-effort low-latency routing through Jupiter swap + Jito.
- **Authority:** Signs swap/transfer ixns. Isolated key — distinct from yield-desk keys, blast radius bounded.
- **Latency target:** Tail latency matters; quote freshness TTL in millis.
- **Runtime:** Tokio current-thread pinned to a core. Pre-warmed RPC + Jito tip channel. Ring-buffer logging, async flush.
- **Persistence:** Ring buffer → disk async. Not a recovery primitive — last trade recovery is the PM's job.
- **Deploy:** Single instance, supervised, tight memory limit.
- **Binary:** `speculator-daemon`.

### Authority matrix (one place to look)

| Daemon | Signs? | RPC | Streams | Persistence | Deploy |
|---|---|---|---|---|---|
| riskwatcher | No | Standard | Pyth + Yellowstone | Append-only forensic log | Active-active |
| multiply | Kamino-only | Standard | None | SQLite WAL journal | Single, supervised |
| hedgedjlp | Jupiter+Adrena | Two senders | None | Leg-pair ledger | Single, supervised |
| stablefloor | Sanctum-only | Standard | None | Timestamp file | Single-shot cron |
| researcher | No | Archival | None | Job artefacts | Ephemeral |
| speculator | Swap+transfer | Jito-aware | None | Ring buffer | Single, supervised |

---

## File Structure

```
01fi/
├── Cargo.toml                                  (modify — workspace members)
├── crates/
│   ├── zerox1-defi-protocols/                  (unchanged)
│   ├── zerox1-defi-runtime/                    (NEW — shared)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                          (Daemon trait, RuntimeProfile)
│   │       ├── pairing.rs                      (lifted from old daemon)
│   │       ├── mesh.rs                         (HMAC envelopes, send/recv)
│   │       ├── health.rs                       (axum /health route)
│   │       ├── rpc.rs                          (lifted from old daemon)
│   │       └── persistence.rs                  (lifted; opt-in per daemon)
│   ├── zerox1-defi-wallet/                     (NEW — only signing daemons depend)
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                          (lifted from old daemon wallet.rs + program-id whitelist)
│   ├── riskwatcher-daemon/                     (NEW)
│   │   ├── Cargo.toml                          (does NOT depend on zerox1-defi-wallet)
│   │   └── src/
│   │       ├── main.rs
│   │       ├── streams.rs                      (Pyth + Yellowstone subscriptions)
│   │       └── alerts.rs                       (lifted Pyth cache logic from handlers/pyth.rs)
│   ├── multiply-daemon/                        (NEW)
│   │   ├── Cargo.toml                          (depends on zerox1-defi-wallet, sqlite)
│   │   └── src/
│   │       ├── main.rs
│   │       ├── journal.rs                      (sqlite WAL ixn journal)
│   │       └── kamino.rs                       (lifted from handlers/kamino.rs + kamino_loader.rs)
│   ├── hedgedjlp-daemon/                       (NEW)
│   │   ├── Cargo.toml                          (depends on zerox1-defi-wallet)
│   │   └── src/
│   │       ├── main.rs
│   │       ├── ledger.rs                       (leg-pair append-only ledger)
│   │       └── legs.rs                         (jupiter+adrena two-leg executor)
│   ├── stablefloor-daemon/                     (NEW)
│   │   ├── Cargo.toml                          (depends on zerox1-defi-wallet)
│   │   └── src/
│   │       ├── main.rs                         (single-shot oneshot::run)
│   │       └── sanctum.rs                      (lifted from handlers/sanctum.rs)
│   ├── researcher-daemon/                      (NEW)
│   │   ├── Cargo.toml                          (does NOT depend on zerox1-defi-wallet; rayon, polars)
│   │   └── src/
│   │       ├── main.rs
│   │       └── jobs.rs
│   ├── speculator-daemon/                      (NEW)
│   │   ├── Cargo.toml                          (depends on zerox1-defi-wallet)
│   │   └── src/
│   │       ├── main.rs
│   │       └── exec.rs
│   ├── zerox1-defi-daemon/                     (DEPRECATE → DELETE in last task)
│   └── zerox1-defi-cli/                        (modify — add per-daemon subcommands)
├── docs/superpowers/plans/                     (this plan lives here)
├── FLEET_CONFIG_ENTERPRISE.md                  (modify — replace single-binary refs)
├── FLEET_PAIRING_PERSONAL.md                   (modify — same)
└── PLAN.md                                     (modify — note the split)
```

**Why these splits:**
- `zerox1-defi-runtime` exists so the six binaries don't each copy 600 lines of mesh/pairing/health code.
- `zerox1-defi-wallet` exists *separately from runtime* so the no-key daemons (`riskwatcher`, `researcher`) literally cannot link signing code. This is the one structural property that catches authority-boundary mistakes at compile time.
- Each daemon is its own crate (not a binary inside a shared crate) so deps are scoped — `multiply-daemon` pulls in `rusqlite`, `riskwatcher-daemon` pulls in `yellowstone-grpc-proto`, neither leaks into the other.

---

## Tasks

### Task 1: Add the shared runtime crate

**Files:**
- Create: `01fi/crates/zerox1-defi-runtime/Cargo.toml`
- Create: `01fi/crates/zerox1-defi-runtime/src/lib.rs`
- Modify: `01fi/Cargo.toml` (add to `[workspace] members`)

- [ ] **Step 1: Create the crate manifest**

`01fi/crates/zerox1-defi-runtime/Cargo.toml`:

```toml
[package]
name = "zerox1-defi-runtime"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
thiserror.workspace = true
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
tokio.workspace = true
axum.workspace = true
tower-http.workspace = true
hmac.workspace = true
sha2.workspace = true
rand.workspace = true
hex.workspace = true
```

- [ ] **Step 2: Define the Daemon trait and RuntimeProfile**

`01fi/crates/zerox1-defi-runtime/src/lib.rs`:

```rust
//! Shared runtime primitives for every 01fi daemon.
//!
//! Each daemon binary picks a `RuntimeProfile`, implements `Daemon`, and
//! calls `run(profile, daemon)` from `main`. The profile drives Tokio
//! flavor, worker count, and whether a health server binds.

pub mod health;
pub mod mesh;
pub mod pairing;
pub mod persistence;
pub mod rpc;

use anyhow::Result;

/// How a daemon's Tokio runtime is configured.
#[derive(Debug, Clone, Copy)]
pub enum RuntimeProfile {
    /// Single-thread, current-thread runtime. Use for serial executors
    /// (multiply) and pinned latency loops (speculator).
    SingleThread,
    /// Multi-thread runtime with a fixed worker count. Use for streaming
    /// (riskwatcher: 4) and two-leg execution (hedgedjlp: 2).
    MultiThread { workers: usize },
    /// One-shot: do the work, exit. Use for stablefloor.
    OneShot,
    /// Throughput-bound batch runtime with Rayon-friendly worker count.
    Batch { workers: usize },
}

#[async_trait::async_trait]
pub trait Daemon: Send + Sync + 'static {
    /// Stable name shown in logs and fleet introspection.
    fn name(&self) -> &'static str;

    /// Whether this daemon can produce signed Solana transactions.
    /// Enforced at compile time by whether the binary depends on
    /// `zerox1-defi-wallet` — this method is documentation only.
    fn signs_transactions(&self) -> bool;

    /// Daemon main loop. Returns when shutdown is requested.
    async fn run(self: Box<Self>) -> Result<()>;
}

pub fn build_runtime(profile: RuntimeProfile) -> Result<tokio::runtime::Runtime> {
    let mut builder = match profile {
        RuntimeProfile::SingleThread | RuntimeProfile::OneShot => {
            tokio::runtime::Builder::new_current_thread()
        }
        RuntimeProfile::MultiThread { workers } | RuntimeProfile::Batch { workers } => {
            let mut b = tokio::runtime::Builder::new_multi_thread();
            b.worker_threads(workers);
            b
        }
    };
    builder.enable_all().build().map_err(Into::into)
}
```

- [ ] **Step 3: Stub the submodules**

Create empty stubs that will be filled by Task 2:

`01fi/crates/zerox1-defi-runtime/src/health.rs`:

```rust
//! Lifted in Task 2.
```

Same one-line stub in `mesh.rs`, `pairing.rs`, `persistence.rs`, `rpc.rs`.

- [ ] **Step 4: Add the crate to the workspace**

In `01fi/Cargo.toml`, add to `[workspace] members`:

```toml
members = [
    "crates/zerox1-defi-protocols",
    "crates/zerox1-defi-daemon",
    "crates/zerox1-defi-cli",
    "crates/zerox1-defi-runtime",
]
```

Also add to `[workspace.dependencies]`:

```toml
async-trait = "0.1"
```

And add to the new crate's `[dependencies]`:

```toml
async-trait.workspace = true
```

- [ ] **Step 5: Verify it builds**

Run: `cd 01fi && cargo check -p zerox1-defi-runtime`
Expected: compiles with no errors.

- [ ] **Step 6: Commit**

```bash
cd /Users/tobiasd/Desktop/zerox1/01fi
git add Cargo.toml crates/zerox1-defi-runtime/
git commit -m "01fi: add zerox1-defi-runtime shared crate (Daemon trait + RuntimeProfile)"
```

---

### Task 2: Copy pairing, mesh, rpc, persistence, health into the runtime crate

**⚠ Additive only — do not modify any file under `zerox1-defi-daemon/`.** The old monolith continues to use its own copies; the new runtime crate gets independent copies. Cleanup (deleting the duplicates from the monolith, repointing it at the runtime crate) happens in a follow-up plan after the other agent signals done.

**Files:**
- Copy: `01fi/crates/zerox1-defi-daemon/src/pairing.rs` → `01fi/crates/zerox1-defi-runtime/src/pairing.rs`
- Copy: `01fi/crates/zerox1-defi-daemon/src/rpc.rs` → `01fi/crates/zerox1-defi-runtime/src/rpc.rs`
- Copy: `01fi/crates/zerox1-defi-daemon/src/persistence.rs` → `01fi/crates/zerox1-defi-runtime/src/persistence.rs`
- Create: `01fi/crates/zerox1-defi-runtime/src/mesh.rs` (derive from reading old `server.rs` + `pairing.rs` — read-only on the old files)
- Create: `01fi/crates/zerox1-defi-runtime/src/health.rs` (derive from reading old `server.rs` — read-only on the old file)
- **No** modifications to `zerox1-defi-daemon/src/main.rs` or its `Cargo.toml`.

- [ ] **Step 1: Copy `pairing.rs` verbatim into the new crate**

```bash
cp 01fi/crates/zerox1-defi-daemon/src/pairing.rs 01fi/crates/zerox1-defi-runtime/src/pairing.rs
```

In the **new** `pairing.rs` (under `zerox1-defi-runtime/`), fix any `use crate::...` paths to refer to the new crate's modules. Do not touch the old file.

- [ ] **Step 2: Copy `rpc.rs` and `persistence.rs` the same way**

```bash
cp 01fi/crates/zerox1-defi-daemon/src/rpc.rs         01fi/crates/zerox1-defi-runtime/src/rpc.rs
cp 01fi/crates/zerox1-defi-daemon/src/persistence.rs 01fi/crates/zerox1-defi-runtime/src/persistence.rs
```

Run `cargo check -p zerox1-defi-runtime` and fix path errors **only in the new copies** until it compiles. Do not touch the old files.

- [ ] **Step 3: Extract the mesh envelope code into `mesh.rs`**

The old daemon's `server.rs` does HMAC verification inline on every fleet POST. Extract that into a `verify_envelope(state: &PairingState, body: &[u8], hmac_header: &str) -> Result<FleetMessage>` function in `mesh.rs`. Symmetric `sign_envelope` for outbound.

```rust
// 01fi/crates/zerox1-defi-runtime/src/mesh.rs
use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::pairing::{FleetMessage, PairingState};

type HmacSha256 = Hmac<Sha256>;

pub fn sign_envelope(state: &PairingState, body: &[u8]) -> Result<String> {
    let token = state.fleet_token().ok_or_else(|| anyhow!("not paired"))?;
    let mut mac = HmacSha256::new_from_slice(token.as_bytes())?;
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_envelope(state: &PairingState, body: &[u8], header: &str) -> Result<FleetMessage> {
    let token = state.fleet_token().ok_or_else(|| anyhow!("not paired"))?;
    let mut mac = HmacSha256::new_from_slice(token.as_bytes())?;
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    if !constant_time_eq(expected.as_bytes(), header.as_bytes()) {
        return Err(anyhow!("envelope hmac mismatch"));
    }
    serde_json::from_slice::<FleetMessage>(body).map_err(Into::into)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

(If `PairingState` does not yet expose `fleet_token()` as an accessor, add it during this step.)

- [ ] **Step 4: Extract `/health` into `health.rs`**

```rust
// 01fi/crates/zerox1-defi-runtime/src/health.rs
use axum::{routing::get, Json, Router};
use serde_json::json;

pub fn router(daemon_name: &'static str) -> Router {
    Router::new().route(
        "/health",
        get(move || async move {
            Json(json!({ "ok": true, "daemon": daemon_name }))
        }),
    )
}
```

- [ ] **Step 5: (Skipped — would touch the monolith, deferred to cleanup plan.)**

The original step here repointed the old daemon at the new runtime crate and deleted its duplicate files. That edits files inside `zerox1-defi-daemon/`, which the coordination note forbids. **Do nothing in this step.** The duplication is intentional and temporary; cleanup happens later.

- [ ] **Step 6: Verify the full workspace still builds**

Run: `cd 01fi && cargo check --workspace`
Expected: every crate compiles, **including the unchanged `zerox1-defi-daemon` monolith and the new `zerox1-defi-runtime` crate side-by-side**.

- [ ] **Step 7: Commit**

```bash
git add 01fi/
git commit -m "01fi: lift pairing/mesh/rpc/persistence/health into zerox1-defi-runtime"
```

---

### Task 3: Add the wallet crate (signing-only daemons depend on it)

**⚠ Additive only — leave `zerox1-defi-daemon/src/wallet.rs` alone.** The new daemons get the new crate; the monolith keeps its own `wallet.rs` until the cleanup plan.

**Files:**
- Create: `01fi/crates/zerox1-defi-wallet/Cargo.toml`
- Create: `01fi/crates/zerox1-defi-wallet/src/lib.rs` (**copied** from old `wallet.rs`, plus the new `SigningWhitelist`)
- Modify: `01fi/Cargo.toml` (add to workspace)
- **No** modifications to `zerox1-defi-daemon/Cargo.toml`. **No** deletion of `zerox1-defi-daemon/src/wallet.rs`.

- [ ] **Step 1: Create the wallet crate**

`01fi/crates/zerox1-defi-wallet/Cargo.toml`:

```toml
[package]
name = "zerox1-defi-wallet"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
solana-sdk.workspace = true
tracing.workspace = true
```

- [ ] **Step 2: Copy `wallet.rs` verbatim and add a program-id whitelist**

```bash
cp 01fi/crates/zerox1-defi-daemon/src/wallet.rs 01fi/crates/zerox1-defi-wallet/src/lib.rs
```

(Do not delete or modify the original under `zerox1-defi-daemon/src/wallet.rs`.) At the bottom of the **new** `lib.rs`, append:

```rust
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;

/// A daemon's mandate — which programs it is allowed to sign for.
/// Each daemon constructs a `SigningWhitelist` once at boot and runs every
/// outbound transaction through `verify_tx` before signing.
pub struct SigningWhitelist {
    allowed: Vec<Pubkey>,
}

impl SigningWhitelist {
    pub fn new(allowed: Vec<Pubkey>) -> Self {
        Self { allowed }
    }

    pub fn verify_tx(&self, tx: &Transaction) -> anyhow::Result<()> {
        for ix in tx.message.instructions.iter() {
            let program_id = tx.message.account_keys.get(ix.program_id_index as usize)
                .ok_or_else(|| anyhow::anyhow!("malformed instruction: program_id_index out of bounds"))?;
            if !self.allowed.contains(program_id) {
                return Err(anyhow::anyhow!(
                    "signing whitelist violation: program {} not allowed for this daemon",
                    program_id
                ));
            }
        }
        Ok(())
    }
}

impl Wallet {
    /// Sign a transaction only if every instruction targets a whitelisted program.
    pub fn sign_with_whitelist(
        &self,
        tx: &mut Transaction,
        whitelist: &SigningWhitelist,
        recent_blockhash: solana_sdk::hash::Hash,
    ) -> anyhow::Result<()> {
        whitelist.verify_tx(tx)?;
        tx.try_sign(&[&self.keypair], recent_blockhash)?;
        Ok(())
    }
}
```

- [ ] **Step 3: Add to workspace (only)**

In `01fi/Cargo.toml`, append to `members`:

```toml
    "crates/zerox1-defi-wallet",
```

**Do not** modify `zerox1-defi-daemon/Cargo.toml` and **do not** delete `zerox1-defi-daemon/src/wallet.rs`. The monolith keeps its own copy until the cleanup plan; the duplication is intentional.

- [ ] **Step 4: Add a unit test for the whitelist**

`01fi/crates/zerox1-defi-wallet/src/lib.rs`, append:

```rust
#[cfg(test)]
mod whitelist_tests {
    use super::*;
    use solana_sdk::{
        instruction::{AccountMeta, Instruction},
        message::Message,
        signature::{Keypair, Signer},
    };

    #[test]
    fn rejects_unwhitelisted_program() {
        let payer = Keypair::new();
        let allowed = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let ix = Instruction::new_with_bytes(other, &[], vec![AccountMeta::new(payer.pubkey(), true)]);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let wl = SigningWhitelist::new(vec![allowed]);
        assert!(wl.verify_tx(&tx).is_err());
    }

    #[test]
    fn accepts_whitelisted_program() {
        let payer = Keypair::new();
        let allowed = Pubkey::new_unique();
        let ix = Instruction::new_with_bytes(allowed, &[], vec![AccountMeta::new(payer.pubkey(), true)]);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let wl = SigningWhitelist::new(vec![allowed]);
        assert!(wl.verify_tx(&tx).is_ok());
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cd 01fi && cargo test -p zerox1-defi-wallet`
Expected: 2 tests pass.

- [ ] **Step 6: Commit**

```bash
git add 01fi/
git commit -m "01fi: extract zerox1-defi-wallet crate with signing whitelist"
```

---

### Task 4: Scaffold riskwatcher-daemon (no wallet, multi-thread streaming)

**Files:**
- Create: `01fi/crates/riskwatcher-daemon/Cargo.toml`
- Create: `01fi/crates/riskwatcher-daemon/src/main.rs`
- Create: `01fi/crates/riskwatcher-daemon/src/streams.rs`
- Create: `01fi/crates/riskwatcher-daemon/src/alerts.rs` (lift from old `handlers/pyth.rs`)
- Modify: `01fi/Cargo.toml` (add to workspace)

- [ ] **Step 1: Create the manifest — no wallet dep**

`01fi/crates/riskwatcher-daemon/Cargo.toml`:

```toml
[package]
name = "riskwatcher-daemon"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "riskwatcher-daemon"
path = "src/main.rs"

[dependencies]
zerox1-defi-runtime = { path = "../zerox1-defi-runtime" }
zerox1-defi-protocols = { path = "../zerox1-defi-protocols" }
# DO NOT add zerox1-defi-wallet — riskwatcher must not be able to sign.
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
axum.workspace = true
```

- [ ] **Step 2: Write the daemon main loop**

`01fi/crates/riskwatcher-daemon/src/main.rs`:

```rust
//! Risk Watcher daemon — read-only oracle and health monitor.
//! Mandate: emit alerts; never trade. The wallet crate is intentionally
//! not in the dependency graph.

mod alerts;
mod streams;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]
    fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")]
    fleet_token: String,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9301")]
    health_bind: String,
}

struct RiskWatcher {
    args: Args,
}

#[async_trait]
impl Daemon for RiskWatcher {
    fn name(&self) -> &'static str { "riskwatcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, "riskwatcher starting");
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        let server = tokio::spawn(async move { axum::serve(listener, health).await });
        let streams = tokio::spawn(streams::run());
        tokio::select! {
            r = server => r??,
            r = streams => r??,
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 4 })?;
    rt.block_on(Box::new(RiskWatcher { args }).run())
}
```

- [ ] **Step 3: Stub `streams.rs` with a placeholder loop**

`01fi/crates/riskwatcher-daemon/src/streams.rs`:

```rust
use anyhow::Result;
use std::time::Duration;
use tracing::debug;

/// Placeholder for Pyth pull + Yellowstone gRPC subscriptions.
/// Filled in by the riskwatcher-strategy follow-up plan.
pub async fn run() -> Result<()> {
    loop {
        debug!("riskwatcher tick");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
```

- [ ] **Step 4: Lift the Pyth cache logic into `alerts.rs`**

```bash
cp 01fi/crates/zerox1-defi-daemon/src/handlers/pyth.rs 01fi/crates/riskwatcher-daemon/src/alerts.rs
```

Fix any `use crate::handlers::...` paths. Anything that signs gets deleted (this is read-only) — if `pyth.rs` is already read-only, it should compile after path fixes.

- [ ] **Step 5: Add to workspace**

In `01fi/Cargo.toml`, append to `members`: `"crates/riskwatcher-daemon",`.

- [ ] **Step 6: Verify the binary builds and the wallet crate is NOT in its dep graph**

Run: `cd 01fi && cargo build -p riskwatcher-daemon`
Expected: builds.

Run: `cd 01fi && cargo tree -p riskwatcher-daemon | grep zerox1-defi-wallet`
Expected: **no output** (riskwatcher must not link the wallet crate).

If `zerox1-defi-wallet` shows up, the dep graph is contaminated — find which crate pulled it in (most likely `zerox1-defi-protocols`) and split that crate into a `-types` (key-free) and `-signing` half. Stop here and resolve before continuing.

- [ ] **Step 7: Smoke-run the binary**

Run: `cd 01fi && cargo run -p riskwatcher-daemon -- --fleet-id test --fleet-token aaaa`
In another shell: `curl -s http://127.0.0.1:9301/health`
Expected: `{"daemon":"riskwatcher","ok":true}`. Kill with ctrl-C.

- [ ] **Step 8: Commit**

```bash
git add 01fi/
git commit -m "01fi: scaffold riskwatcher-daemon (no-wallet, multi-thread streaming)"
```

---

### Task 5: Scaffold multiply-daemon (Kamino, signing, sqlite journal)

**Files:**
- Create: `01fi/crates/multiply-daemon/Cargo.toml`
- Create: `01fi/crates/multiply-daemon/src/main.rs`
- Create: `01fi/crates/multiply-daemon/src/journal.rs`
- Create: `01fi/crates/multiply-daemon/src/kamino.rs` (lift from old `handlers/kamino.rs` + `kamino_loader.rs`)
- Modify: `01fi/Cargo.toml` (workspace + add `rusqlite` to workspace deps)

- [ ] **Step 1: Add rusqlite to workspace deps**

In `01fi/Cargo.toml` `[workspace.dependencies]`:

```toml
rusqlite = { version = "0.31", features = ["bundled"] }
```

- [ ] **Step 2: Create the manifest**

`01fi/crates/multiply-daemon/Cargo.toml`:

```toml
[package]
name = "multiply-daemon"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "multiply-daemon"
path = "src/main.rs"

[dependencies]
zerox1-defi-runtime = { path = "../zerox1-defi-runtime" }
zerox1-defi-protocols = { path = "../zerox1-defi-protocols" }
zerox1-defi-wallet    = { path = "../zerox1-defi-wallet" }
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
axum.workspace = true
rusqlite.workspace = true
solana-sdk.workspace = true
```

- [ ] **Step 3: Write the daemon main loop**

`01fi/crates/multiply-daemon/src/main.rs`:

```rust
//! Multiply daemon — Kamino leveraged LST. Single-flight, sqlite-journaled.

mod journal;
mod kamino;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]            fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")]         fleet_token: String,
    #[arg(long, env = "ZX_WALLET")]              wallet: PathBuf,
    #[arg(long, env = "ZX_JOURNAL", default_value = "multiply-journal.sqlite")]
                                                  journal: PathBuf,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9302")]
                                                  health_bind: String,
}

struct Multiply {
    args: Args,
    wallet: Wallet,
    whitelist: SigningWhitelist,
    journal: journal::Journal,
}

#[async_trait]
impl Daemon for Multiply {
    fn name(&self) -> &'static str { "multiply" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, "multiply starting");
        self.journal.replay().await?;
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        axum::serve(listener, health).await?;
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(kamino::program_ids());
    let journal = journal::Journal::open(&args.journal)?;
    let rt = build_runtime(RuntimeProfile::SingleThread)?;
    rt.block_on(Box::new(Multiply { args, wallet, whitelist, journal }).run())
}
```

- [ ] **Step 4: Implement the journal skeleton**

`01fi/crates/multiply-daemon/src/journal.rs`:

```rust
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;
use tracing::info;

pub struct Journal {
    conn: Mutex<Connection>,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ixns (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                position  TEXT NOT NULL,
                payload   BLOB NOT NULL,
                state     TEXT NOT NULL CHECK(state IN ('pending','submitted','confirmed','failed')),
                signature TEXT,
                created   INTEGER NOT NULL,
                updated   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS ixns_state_idx ON ixns(state);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// On boot, log every non-confirmed ixn so a human (or the strategy
    /// follow-up plan) can decide what to do.
    pub async fn replay(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, position, state FROM ixns WHERE state != 'confirmed'")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        for (id, pos, state) in rows {
            info!(id, position = %pos, state = %state, "journal replay: orphan ixn");
        }
        Ok(())
    }
}
```

- [ ] **Step 5: Lift Kamino logic and expose `program_ids()`**

```bash
cp 01fi/crates/zerox1-defi-daemon/src/handlers/kamino.rs 01fi/crates/multiply-daemon/src/kamino.rs
cat 01fi/crates/zerox1-defi-daemon/src/kamino_loader.rs >> 01fi/crates/multiply-daemon/src/kamino.rs
```

At the top of `multiply-daemon/src/kamino.rs`, add:

```rust
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Program IDs the Multiply daemon is allowed to sign for. Anything else
/// is rejected by the wallet whitelist before signing.
pub fn program_ids() -> Vec<Pubkey> {
    vec![
        // Kamino Lend
        Pubkey::from_str("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD").unwrap(),
        // Kamino Farms (used by Multiply harvest path)
        Pubkey::from_str("FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr").unwrap(),
    ]
}
```

(Confirm IDs against current `zerox1-defi-protocols::kamino` constants — if they live there as constants, import them instead.)

Fix any `use crate::handlers::...` paths.

- [ ] **Step 6: Add to workspace and build**

In `01fi/Cargo.toml` `members`: `"crates/multiply-daemon",`.

Run: `cd 01fi && cargo build -p multiply-daemon`
Expected: compiles.

- [ ] **Step 7: Commit**

```bash
git add 01fi/
git commit -m "01fi: scaffold multiply-daemon (single-thread, sqlite WAL journal, Kamino-only signing)"
```

---

### Task 6: Scaffold hedgedjlp-daemon (two-leg, dual sender, leg-pair ledger)

**Files:**
- Create: `01fi/crates/hedgedjlp-daemon/Cargo.toml`
- Create: `01fi/crates/hedgedjlp-daemon/src/main.rs`
- Create: `01fi/crates/hedgedjlp-daemon/src/legs.rs`
- Create: `01fi/crates/hedgedjlp-daemon/src/ledger.rs`
- Modify: `01fi/Cargo.toml`

- [ ] **Step 1: Manifest**

`01fi/crates/hedgedjlp-daemon/Cargo.toml`:

```toml
[package]
name = "hedgedjlp-daemon"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "hedgedjlp-daemon"
path = "src/main.rs"

[dependencies]
zerox1-defi-runtime = { path = "../zerox1-defi-runtime" }
zerox1-defi-protocols = { path = "../zerox1-defi-protocols" }
zerox1-defi-wallet    = { path = "../zerox1-defi-wallet" }
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
axum.workspace = true
solana-sdk.workspace = true
```

- [ ] **Step 2: Main loop with two-worker runtime**

`01fi/crates/hedgedjlp-daemon/src/main.rs`:

```rust
//! HedgedJLP daemon — long JLP, short SOL on Adrena. Two legs, one deadline.

mod legs;
mod ledger;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]      fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")]   fleet_token: String,
    #[arg(long, env = "ZX_WALLET")]        wallet: PathBuf,
    #[arg(long, env = "ZX_LEDGER", default_value = "hedgedjlp-ledger.log")]
                                            ledger: PathBuf,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9303")]
                                            health_bind: String,
}

struct HedgedJlp {
    args: Args,
    wallet: Wallet,
    whitelist: SigningWhitelist,
    ledger: ledger::Ledger,
}

#[async_trait]
impl Daemon for HedgedJlp {
    fn name(&self) -> &'static str { "hedgedjlp" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, "hedgedjlp starting");
        self.ledger.recover_orphans().await?;
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        axum::serve(listener, health).await?;
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(legs::program_ids());
    let ledger = ledger::Ledger::open(&args.ledger)?;
    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 2 })?;
    rt.block_on(Box::new(HedgedJlp { args, wallet, whitelist, ledger }).run())
}
```

- [ ] **Step 3: Append-only leg-pair ledger**

`01fi/crates/hedgedjlp-daemon/src/ledger.rs`:

```rust
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::warn;

#[derive(Serialize, Deserialize, Debug)]
pub struct LegPair {
    pub pair_id: String,
    pub long_sig:  Option<String>,
    pub short_sig: Option<String>,
    pub state:     LegState,
    pub ts:        i64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum LegState { Pending, BothFilled, OrphanLong, OrphanShort, Closed }

pub struct Ledger {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).read(true).open(path)?;
        Ok(Self { path: path.to_path_buf(), file: Mutex::new(file) })
    }

    pub fn append(&self, entry: &LegPair) -> Result<()> {
        let mut f = self.file.lock().unwrap();
        writeln!(f, "{}", serde_json::to_string(entry)?)?;
        f.flush()?;
        Ok(())
    }

    /// On boot, scan the ledger and report any pair stuck in OrphanLong /
    /// OrphanShort — the close path is implemented in the strategy
    /// follow-up plan; this just surfaces them.
    pub async fn recover_orphans(&self) -> Result<()> {
        let f = std::fs::File::open(&self.path)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            let entry: LegPair = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if matches!(entry.state, LegState::OrphanLong | LegState::OrphanShort) {
                warn!(pair = %entry.pair_id, state = ?entry.state, "ledger orphan needs close");
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Stub `legs.rs` with program IDs and a placeholder execute fn**

```rust
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub fn program_ids() -> Vec<Pubkey> {
    vec![
        // Jupiter Perps / JLP
        Pubkey::from_str("PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu").unwrap(),
        // Adrena
        Pubkey::from_str("13gDzEXCdocbj8iAiqrScGo47NiSuYENGsRqi3SEAwet").unwrap(),
    ]
}
```

(Confirm both program IDs against current mainnet — placeholder values must be corrected in the strategy follow-up.)

- [ ] **Step 5: Workspace + build**

`members`: append `"crates/hedgedjlp-daemon",`.
Run: `cd 01fi && cargo build -p hedgedjlp-daemon`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add 01fi/
git commit -m "01fi: scaffold hedgedjlp-daemon (two-worker runtime, leg-pair ledger)"
```

---

### Task 7: Scaffold stablefloor-daemon (one-shot, Sanctum INF)

**Files:**
- Create: `01fi/crates/stablefloor-daemon/Cargo.toml`
- Create: `01fi/crates/stablefloor-daemon/src/main.rs`
- Create: `01fi/crates/stablefloor-daemon/src/sanctum.rs` (lift)
- Modify: `01fi/Cargo.toml`

- [ ] **Step 1: Manifest**

`01fi/crates/stablefloor-daemon/Cargo.toml`:

```toml
[package]
name = "stablefloor-daemon"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "stablefloor-daemon"
path = "src/main.rs"

[dependencies]
zerox1-defi-runtime = { path = "../zerox1-defi-runtime" }
zerox1-defi-protocols = { path = "../zerox1-defi-protocols" }
zerox1-defi-wallet    = { path = "../zerox1-defi-wallet" }
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
serde.workspace = true
serde_json.workspace = true
solana-sdk.workspace = true
```

- [ ] **Step 2: Main with subcommands `mint` / `redeem`**

`01fi/crates/stablefloor-daemon/src/main.rs`:

```rust
//! Stable-floor daemon — single-shot: mint or redeem Sanctum INF, exit.

mod sanctum;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_WALLET")]    wallet: PathBuf,
    #[arg(long, env = "ZX_STAMP", default_value = "stablefloor.last")]
                                        stamp: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Mint   { #[arg(long)] sol_amount: f64 },
    Redeem { #[arg(long)] inf_amount: f64 },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(sanctum::program_ids());
    let rt = build_runtime(RuntimeProfile::OneShot)?;
    rt.block_on(async move {
        match args.cmd {
            Cmd::Mint   { sol_amount } => sanctum::mint(&wallet, &whitelist, sol_amount).await?,
            Cmd::Redeem { inf_amount } => sanctum::redeem(&wallet, &whitelist, inf_amount).await?,
        }
        std::fs::write(&args.stamp, chrono::Utc::now().to_rfc3339()).ok();
        info!("stablefloor done");
        Ok::<_, anyhow::Error>(())
    })
}
```

(If `chrono` is not yet in the workspace, replace the stamp write with `std::time::SystemTime::now()` formatting — chrono is a nice-to-have, not load-bearing.)

- [ ] **Step 3: Lift Sanctum logic**

```bash
cp 01fi/crates/zerox1-defi-daemon/src/handlers/sanctum.rs 01fi/crates/stablefloor-daemon/src/sanctum.rs
```

Reshape the API to expose `program_ids()`, `mint(...)`, `redeem(...)`. If the lifted file is HTTP-handler-shaped, extract the inner logic and drop the HTTP layer.

- [ ] **Step 4: Workspace + build**

`members`: append `"crates/stablefloor-daemon",`.
Run: `cd 01fi && cargo build -p stablefloor-daemon`
Expected: compiles.

- [ ] **Step 5: Smoke-run**

Run: `cd 01fi && cargo run -p stablefloor-daemon -- --wallet /tmp/test.json mint --sol-amount 0.0`
Expected: errors out on wallet load (no real wallet) — that's fine, it proves the binary parses args and reaches the mint path.

- [ ] **Step 6: Commit**

```bash
git add 01fi/
git commit -m "01fi: scaffold stablefloor-daemon (one-shot Sanctum INF mint/redeem)"
```

---

### Task 8: Scaffold researcher-daemon (no wallet, batch runtime)

**Files:**
- Create: `01fi/crates/researcher-daemon/Cargo.toml`
- Create: `01fi/crates/researcher-daemon/src/main.rs`
- Create: `01fi/crates/researcher-daemon/src/jobs.rs`
- Modify: `01fi/Cargo.toml`

- [ ] **Step 1: Manifest — no wallet, rayon for CPU parallelism**

```toml
[package]
name = "researcher-daemon"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "researcher-daemon"
path = "src/main.rs"

[dependencies]
zerox1-defi-runtime = { path = "../zerox1-defi-runtime" }
zerox1-defi-protocols = { path = "../zerox1-defi-protocols" }
# DO NOT add zerox1-defi-wallet — researcher must not be able to sign.
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
axum.workspace = true
rayon = "1.10"
```

- [ ] **Step 2: Main loop**

`01fi/crates/researcher-daemon/src/main.rs`:

```rust
//! Researcher daemon — read-only batch worker. No keys, no streams.
//! Pulls jobs from the mesh, produces artefacts, exits when idle.

mod jobs;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]    fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")] fleet_token: String,
    #[arg(long, env = "ZX_ARTEFACTS", default_value = "./researcher-artefacts")]
                                          artefacts: PathBuf,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9304")]
                                          health_bind: String,
    #[arg(long, env = "ZX_WORKERS", default_value_t = num_cpus())]
                                          workers: usize,
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

struct Researcher { args: Args }

#[async_trait]
impl Daemon for Researcher {
    fn name(&self) -> &'static str { "researcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        std::fs::create_dir_all(&self.args.artefacts)?;
        info!(fleet = %self.args.fleet_id, workers = self.args.workers, "researcher starting");
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        let server = tokio::spawn(async move { axum::serve(listener, health).await });
        let runner = tokio::spawn(jobs::run());
        tokio::select! { r = server => r??, r = runner => r??, }
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let workers = args.workers;
    let rt = build_runtime(RuntimeProfile::Batch { workers })?;
    rt.block_on(Box::new(Researcher { args }).run())
}
```

- [ ] **Step 3: Stub `jobs.rs`**

```rust
use anyhow::Result;
use std::time::Duration;
use tracing::debug;

pub async fn run() -> Result<()> {
    loop {
        debug!("researcher idle");
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
```

- [ ] **Step 4: Workspace + build + verify no wallet dep**

`members`: append `"crates/researcher-daemon",`.
Run: `cd 01fi && cargo build -p researcher-daemon`
Run: `cd 01fi && cargo tree -p researcher-daemon | grep zerox1-defi-wallet`
Expected: no output.

- [ ] **Step 5: Commit**

```bash
git add 01fi/
git commit -m "01fi: scaffold researcher-daemon (no-wallet, batch runtime)"
```

---

### Task 9: Scaffold speculator-daemon (pinned single-thread, Jito-aware)

**Files:**
- Create: `01fi/crates/speculator-daemon/Cargo.toml`
- Create: `01fi/crates/speculator-daemon/src/main.rs`
- Create: `01fi/crates/speculator-daemon/src/exec.rs`
- Modify: `01fi/Cargo.toml`

- [ ] **Step 1: Manifest**

```toml
[package]
name = "speculator-daemon"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "speculator-daemon"
path = "src/main.rs"

[dependencies]
zerox1-defi-runtime = { path = "../zerox1-defi-runtime" }
zerox1-defi-protocols = { path = "../zerox1-defi-protocols" }
zerox1-defi-wallet    = { path = "../zerox1-defi-wallet" }
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
axum.workspace = true
solana-sdk.workspace = true
```

- [ ] **Step 2: Main loop — pinned thread, current-thread runtime**

`01fi/crates/speculator-daemon/src/main.rs`:

```rust
//! Speculator daemon — directional execution. Latency-tail-sensitive.
//! Pinned to a single core; current-thread Tokio.

mod exec;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::{info, warn};
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]    fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")] fleet_token: String,
    #[arg(long, env = "ZX_WALLET")]      wallet: PathBuf,
    #[arg(long, env = "ZX_PIN_CORE")]    pin_core: Option<usize>,
    #[arg(long, env = "ZX_QUOTE_TTL_MS", default_value_t = 750)]
                                          quote_ttl_ms: u64,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9305")]
                                          health_bind: String,
}

struct Speculator {
    args: Args,
    wallet: Wallet,
    whitelist: SigningWhitelist,
}

#[async_trait]
impl Daemon for Speculator {
    fn name(&self) -> &'static str { "speculator" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, ttl = self.args.quote_ttl_ms, "speculator starting");
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        axum::serve(listener, health).await?;
        Ok(())
    }
}

fn pin_to_core(core: usize) {
    // Best-effort. On Linux, use sched_setaffinity. On macOS this is a no-op
    // and we just log. The plan does not introduce a third dep just for pinning;
    // if you want hard pinning, add `core_affinity = "0.8"` later.
    warn!(core, "core pinning requested but not implemented in this scaffold");
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    if let Some(c) = args.pin_core { pin_to_core(c); }
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(exec::program_ids());
    let rt = build_runtime(RuntimeProfile::SingleThread)?;
    rt.block_on(Box::new(Speculator { args, wallet, whitelist }).run())
}
```

- [ ] **Step 3: Stub `exec.rs`**

```rust
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub fn program_ids() -> Vec<Pubkey> {
    vec![
        // Jupiter Aggregator v6
        Pubkey::from_str("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4").unwrap(),
        // SPL Token (transfers)
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap(),
    ]
}
```

- [ ] **Step 4: Workspace + build**

`members`: append `"crates/speculator-daemon",`.
Run: `cd 01fi && cargo build -p speculator-daemon`
Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add 01fi/
git commit -m "01fi: scaffold speculator-daemon (pinned single-thread, Jupiter+SPL signing only)"
```

---

### Task 10: Update the CLI and fleet config docs — **DEFERRED**

**⚠ Skipped in this plan run.** The CLI lives in `zerox1-defi-cli` (separate from the monolith) but its install-script generator is the contract the monolith ships with — repointing it now would force the other agent's daemon work to land alongside binaries that don't exist on user hosts yet. Defer to the cleanup plan (`2026-05-XX-fleet-cleanup.md`) once the other agent signals done and all six new binaries are confirmed shipping.

**Do not execute Steps 1–5 below in this plan run.** They are kept for reference so the cleanup plan can lift them verbatim.

**Files (for the future cleanup plan, not this one):**
- Modify: `01fi/crates/zerox1-defi-cli/src/main.rs` (replace single-binary launch with per-daemon subcommands)
- Modify: `01fi/FLEET_PAIRING_PERSONAL.md`
- Modify: `01fi/FLEET_CONFIG_ENTERPRISE.md`
- Modify: `01fi/PLAN.md`
- Modify: `01fi/README.md`

- [ ] **Step 1: CLI subcommand surface**

In `01fi/crates/zerox1-defi-cli/src/main.rs`, the install/launch helpers currently emit `zerox1-defi-daemon --role X ...`. Replace with a mapping table:

```rust
fn binary_for(role: &str) -> Option<&'static str> {
    match role {
        "riskwatcher" | "risk-watcher" | "risk_watcher" => Some("riskwatcher-daemon"),
        "multiply"                                       => Some("multiply-daemon"),
        "hedgedjlp" | "hedged-jlp" | "hedged_jlp"        => Some("hedgedjlp-daemon"),
        "stablefloor" | "stable-floor" | "stable_floor"  => Some("stablefloor-daemon"),
        "researcher"                                     => Some("researcher-daemon"),
        "speculator"                                     => Some("speculator-daemon"),
        "orchestrator"                                   => None, // mobile only
        _ => None,
    }
}
```

Wire `binary_for` into the install-script generator that the mobile app pastes — the script now downloads/runs the role-specific binary instead of `zerox1-defi-daemon`.

- [ ] **Step 2: Update fleet docs**

In `FLEET_PAIRING_PERSONAL.md` and `FLEET_CONFIG_ENTERPRISE.md`, find every `zerox1-defi-daemon --role X` reference and replace with the role-specific binary name from the table above. Add a paragraph at the top of each doc:

> **Note (May 2026):** The fleet now ships as six purpose-built binaries
> instead of one `--role`-flagged daemon. See `docs/superpowers/plans/2026-05-04-fleet-six-daemons.md`
> for the rationale and the authority/runtime matrix.

- [ ] **Step 3: Update `PLAN.md` and `README.md`**

In `PLAN.md`, in the "Two Products, Two Trust Models" section, append a sentence:

> Both products ship the same six daemon binaries; the coordination layer
> (pairing-driven vs. config-driven) differs.

In `README.md`, replace any "the daemon" references with "the fleet" and link to the runtime/authority matrix in this plan.

- [ ] **Step 4: Verify the workspace still builds**

Run: `cd 01fi && cargo build --workspace`
Expected: every binary compiles.

- [ ] **Step 5: Commit**

```bash
git add 01fi/
git commit -m "01fi: update CLI and fleet docs for the six-binary split"
```

---

### Task 11: Decommission the old monolith — **DEFERRED**

**⚠ Skipped in this plan run.** Another agent is actively working in `zerox1-defi-daemon`. Deleting the directory would obliterate their in-flight work. This task moves to the cleanup plan and only runs after **two preconditions both hold**: (a) the other agent signals their daemon work is done and merged, and (b) all six new daemons have confirmed functional parity per Step 1 below.

**Do not execute Steps 1–5 below in this plan run.** They are kept for reference.

**Files (for the future cleanup plan, not this one):**
- Delete: `01fi/crates/zerox1-defi-daemon/` (entire directory)
- Modify: `01fi/Cargo.toml` (remove from members)

- [ ] **Step 1: Confirm functional parity**

Run each new daemon against a local devnet test fleet and confirm:
- riskwatcher-daemon: emits the same Pyth-derived health updates the old monolith did under `--role riskwatcher`.
- multiply-daemon: rebalance dry-run produces the same ixn set as the old monolith.
- hedgedjlp-daemon: leg-pair planning produces the same ixn set.
- stablefloor-daemon: mint/redeem dry-run produces the same ixn set.
- speculator-daemon: swap quote + signing-whitelist check passes on a Jupiter ixn.

If any check fails, **stop**. Do not delete the old crate.

- [ ] **Step 2: Remove from workspace**

In `01fi/Cargo.toml`, delete the `"crates/zerox1-defi-daemon"` line from `members`.

- [ ] **Step 3: Delete the directory**

```bash
rm -rf 01fi/crates/zerox1-defi-daemon
```

- [ ] **Step 4: Verify**

Run: `cd 01fi && cargo build --workspace`
Expected: every remaining crate compiles. No reference to `zerox1-defi-daemon` left.

Run: `cd 01fi && grep -r "zerox1-defi-daemon" --include="*.rs" --include="*.toml" --include="*.md"`
Expected: no results (or only historical references inside this plan file, which is fine).

- [ ] **Step 5: Commit**

```bash
git add 01fi/
git commit -m "01fi: remove monolithic zerox1-defi-daemon (replaced by the six-binary fleet)"
```

---

## Self-Review Notes

**Spec coverage:** Every role in `pairing.rs::Role` has a binary except `Orchestrator` (mobile, intentionally out of scope) — covered.

**Authority boundary:** Two daemons (`riskwatcher`, `researcher`) have `cargo tree` checks in their tasks proving the wallet crate is not in their dep graph. This is the structural property the user asked for.

**Runtime profile coverage:** Every daemon names a `RuntimeProfile` variant in its `main.rs`, and every variant in the enum is used by at least one daemon — no dead profiles, no daemons defaulting to something they shouldn't.

**Migration path:** Existing handler code (`kamino.rs`, `sanctum.rs`, `pyth.rs`, `kamino_loader.rs`, `wallet.rs`, `pairing.rs`, `rpc.rs`, `persistence.rs`) is each accounted for — either lifted into `zerox1-defi-runtime`, lifted into `zerox1-defi-wallet`, or lifted into the daemon that owns its mandate.

**Out of scope (deliberately):** Strategy depth — none of these daemons does the actual rebalance / leg-pair execution / streaming subscription / backtest in this plan. Each gets a follow-up plan named `2026-05-XX-<daemon-name>-strategy.md`.

**Risk:** If `zerox1-defi-protocols` itself depends on something that pulls in signing primitives, the wallet-isolation property breaks. Task 4 has an explicit `cargo tree` check that catches this; the fix (split protocols into `-types` + `-signing`) is called out.
