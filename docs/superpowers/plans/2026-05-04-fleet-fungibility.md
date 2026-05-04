# 01fi Fleet — Fungibility & Embedded Mesh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make the six fleet daemons **hot-swappable across machines**. Each daemon embeds a node-enterprise libp2p peer, holds a long-lived **role identity** (Ed25519 key in a secrets vault) that survives any single instance, replays its in-memory state from on-chain history on boot, and talks to the mesh via tight binary envelopes. Result: kill any daemon process, boot a replacement on a different host with the same role key, and the fleet recovers without human intervention.

**Architecture:** Six self-contained agents. Each agent = `runtime` (Tokio + Daemon trait) + business logic + embedded `zerox1-node-enterprise` library + role identity loaded from secrets. Inter-agent communication happens over libp2p directly (gossipsub for fleet-wide, request-response for bilateral). The mobile PM is one more peer. No HTTP-over-loopback in the hot path. No human-readable middleware in agent↔agent traffic. Liveness via mesh `Beacon`; replacement via vault key restore + chain replay.

**The principle:** Agents talk with each other through *intent* (numeric `MsgType`) + *content* (typed CBOR payload). Humans see English only at the edge — the mobile PM converts user chat to bytes, daemons convert REPORTs back to English for display. The middle is bytes.

**Tech Stack:** Rust 2021, Tokio, libp2p 0.54 (via embedded node-enterprise), `zerox1-protocol` (CBOR Envelope + MsgType), Solana SDK, ed25519-dalek (role identity), CBOR (`ciborium`), optional Hashicorp Vault / SOPS for secrets.

---

## ⚠ COORDINATION NOTES

**Read-only zones (still):**
- `crates/zerox1-defi-daemon/` — owned by the other agent
- `crates/zerox1-defi-protocols/` — owned by the other agent

**Newly modifiable zones (per user direction):**
- `/Users/tobiasd/Desktop/zerox1/node-enterprise/` — separate Cargo workspace, but we're authorized to refactor it for fleet needs. Treat its `crates/zerox1-protocol/` as the canonical wire format that we'll consume from 01fi via path-dep.

**Two-workspace setup:** `01fi/` and `node-enterprise/` are separate Cargo workspaces in two parallel directories. We bridge them with a relative path-dep:
```toml
# in 01fi/Cargo.toml workspace.dependencies:
zerox1-protocol = { path = "../node-enterprise/crates/zerox1-protocol" }
zerox1-node-enterprise = { path = "../node-enterprise/crates/zerox1-node-enterprise" }
```
This means **the worktree path matters**. The 01fi worktree at `/Users/tobiasd/Desktop/zerox1-01fi-fleet/` has `../node-enterprise/` resolving to `/Users/tobiasd/Desktop/node-enterprise/`, which **does not exist**. We need either (a) a node-enterprise worktree at that path, (b) an absolute path-dep, or (c) merge the workspaces. **Resolved as Task 1 of this plan.**

**Branch strategy:**
- Continue on the existing `fleet/six-daemons` branch in worktree `/Users/tobiasd/Desktop/zerox1-01fi-fleet/`. This plan stacks on top of the prior 13 commits.
- Create a parallel branch in `node-enterprise` (e.g., `fleet/library-api`) for the lib refactor in Task 2. Two coordinated branches, two repos, both eventually merge to their respective mains.

**Out of scope for this plan (separate follow-ups):**
- Real strategy logic per daemon (Pyth subscriptions, Kamino rebalance, two-leg execution, etc.) — six per-daemon strategy plans
- Aggregator repurposing for fleet observability
- Mobile PM UI work
- Decommissioning the monolith (deferred Tasks 10/11 from the six-daemons plan)
- The other agent's protocols migration (Adrena → Jupiter, Sanctum → Jito)

**Dropped from the prior plan (replaced by this one):**
- `runtime::mesh::{sign_envelope, verify_envelope}` — HMAC was a placeholder; per-message Ed25519 (or a localnet-simpler primitive — Task 8 decides) inside the embedded node replaces it
- `--fleet-token` CLI flag on every daemon — replaced by `--role-key` + `--secrets-source`
- `/health` axum route on each daemon — replaced by mesh-level `Beacon` heartbeat. (Optional ops-monitoring sidecar can keep `/health` if SRE wants it; not the primary channel.)

---

## File Structure

```
/Users/tobiasd/Desktop/
├── zerox1-01fi-fleet/                       (01fi worktree, branch fleet/six-daemons)
│   ├── Cargo.toml                            (modify — add path-deps)
│   ├── crates/
│   │   ├── zerox1-defi-runtime/              (extend significantly)
│   │   │   └── src/
│   │   │       ├── lib.rs                    (modify — add new module decls)
│   │   │       ├── identity.rs               (NEW — RoleIdentity)
│   │   │       ├── secrets.rs                (NEW — secret-source abstraction)
│   │   │       ├── role_registry.rs          (NEW — mesh-level claim/resolve)
│   │   │       ├── replay.rs                 (NEW — ChainReplay trait + boot hook)
│   │   │       ├── node_client.rs            (NEW — re-export of NodeService API)
│   │   │       ├── pairing.rs                (UNCHANGED — still useful for legacy paths)
│   │   │       ├── rpc.rs                    (unchanged)
│   │   │       ├── persistence.rs            (unchanged)
│   │   │       ├── mesh.rs                   (DELETE in Task 9)
│   │   │       └── health.rs                 (DELETE in Task 9; optional sidecar later)
│   │   ├── zerox1-defi-wallet/               (unchanged)
│   │   ├── riskwatcher-daemon/               (modify — embed node, drop /health, drop fleet-token)
│   │   ├── multiply-daemon/                  (modify — same)
│   │   ├── hedgedjlp-daemon/                 (modify — same)
│   │   ├── stablefloor-daemon/               (modify — embed lite, see Task 8 note)
│   │   ├── researcher-daemon/                (modify — same)
│   │   └── speculator-daemon/                (modify — bump to MultiThread{2})
│   ├── deploy/
│   │   ├── docker-compose.fleet.yml          (NEW — Task 10)
│   │   ├── Dockerfile                        (NEW — shared per-daemon Dockerfile)
│   │   └── role-keys/                        (NEW — gitignored; six per-daemon ed25519 keys for dev)
│   └── tools/
│       └── fleet-pm-stub/                    (NEW — small CLI that sends Assign envelopes)
│           ├── Cargo.toml
│           └── src/main.rs
└── node-enterprise/                          (separate workspace, branch fleet/library-api)
    └── crates/
        ├── zerox1-protocol/
        │   └── src/
        │       └── fleet/                    (NEW — Task 5; per-desk payload types)
        │           ├── mod.rs
        │           ├── multiply.rs           (AssignMultiply, ReportMultiply)
        │           ├── hedgedjlp.rs
        │           ├── stablefloor.rs
        │           ├── riskwatcher.rs
        │           ├── researcher.rs
        │           └── speculator.rs
        └── zerox1-node-enterprise/
            ├── src/
            │   ├── lib.rs                    (NEW — Task 2; library API surface)
            │   ├── service.rs                (NEW — Task 2; NodeService struct)
            │   ├── handle.rs                 (NEW — Task 2; NodeHandle for callers)
            │   └── main.rs                   (modify — Task 2; thin binary wrapper)
            └── Cargo.toml                    (modify — add `[lib]` section)
```

---

## The Vision in Concrete Terms

**Day-to-day**: A daemon fails, its container exits, an orchestrator (Kubernetes / nomad / a shell script) starts a fresh container on a different host. The new container reads the same role key from the vault, libp2p Swarm starts fresh with a new instance PeerId, the daemon emits `Claim(Multiply)` on the mesh, replays the last N hours of on-chain ixns matching its role key into in-memory state, and starts processing inbound `Assign` envelopes. **No state on local disk is load-bearing.** Other daemons see the BEACON resume from a new PeerId, update their role→PeerId cache, send/receive normally. Total downtime: seconds.

**Fungibility test**: Run two instances with the same role key on different hosts simultaneously. The fleet protocol detects the collision (two `Claim(Multiply)` BEACONs from different PeerIds), and one of them must yield (lower-PeerId-wins or last-claim-wins — Task 6 decides). The losing instance shuts down or refuses to act. No double-spends.

**Identity model:**
| Identity | Lifetime | Storage | Used for |
|---|---|---|---|
| Role identity (Ed25519) | Forever (rotated only on key compromise) | Vault / SOPS / sealed-secret | Signing wire envelopes; on-chain ixns when relevant |
| Instance PeerId (Ed25519) | One process lifetime | Generated at boot, ephemeral | libp2p transport-layer routing |
| Solana wallet (Ed25519) | Forever (rotated rarely) | Vault / SOPS / sealed-secret | Signing Solana txns |

The role identity and the Solana wallet are both long-lived but distinct concerns. A daemon may sign a `Report` envelope with its role key while signing the Kamino tx inside that report's payload with the Solana wallet. Two keys, two purposes.

---

## Tasks

### Task 1: Workspace plumbing — share `zerox1-protocol` between repos

**Files:**
- Create: `/Users/tobiasd/Desktop/node-enterprise` (new git worktree of node-enterprise on a new branch)
- Modify: `Cargo.toml` (workspace) — add path-deps and workspace deps
- (Read-only verification on `node-enterprise/Cargo.toml`)

This task resolves the directory-layout mismatch between the 01fi worktree (`/Users/tobiasd/Desktop/zerox1-01fi-fleet/`) and the parent's node-enterprise location (`/Users/tobiasd/Desktop/zerox1/node-enterprise/`). After this task, `../node-enterprise/` from inside the 01fi worktree resolves correctly.

- [ ] **Step 1: Create a node-enterprise worktree at the sibling path**

```bash
cd /Users/tobiasd/Desktop/zerox1/node-enterprise
git worktree add -b fleet/library-api /Users/tobiasd/Desktop/node-enterprise
ls /Users/tobiasd/Desktop/node-enterprise/crates/   # should show: zerox1-aggregator, zerox1-node-enterprise, zerox1-protocol
```

- [ ] **Step 2: Verify the path-dep resolution from inside the 01fi worktree**

```bash
cd /Users/tobiasd/Desktop/zerox1-01fi-fleet
ls ../node-enterprise/crates/zerox1-protocol/   # should show src/, Cargo.toml
```

- [ ] **Step 3: Add `zerox1-protocol` as a workspace path-dep in 01fi**

In `/Users/tobiasd/Desktop/zerox1-01fi-fleet/Cargo.toml`, append to `[workspace.dependencies]`:

```toml
zerox1-protocol = { path = "../node-enterprise/crates/zerox1-protocol" }
zerox1-node-enterprise = { path = "../node-enterprise/crates/zerox1-node-enterprise" }
```

- [ ] **Step 4: Smoke build**

```bash
cd /Users/tobiasd/Desktop/zerox1-01fi-fleet
cargo metadata --no-deps -q | head -2  # verify metadata resolves
cargo check --workspace
```

Expected: clean build. The new path-deps are declared in workspace deps but not yet *used* by any crate — so this just validates the path is reachable.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml
git commit -m "01fi: add path-deps on node-enterprise's zerox1-protocol and node binary

Bridges the two Cargo workspaces. Worktree at ../node-enterprise resolves
to a sibling git worktree of the node-enterprise repo on branch
fleet/library-api."
```

---

### Task 2: Refactor `zerox1-node-enterprise` from binary to library + thin binary

**Repo:** node-enterprise worktree at `/Users/tobiasd/Desktop/node-enterprise/` on branch `fleet/library-api`.

**Files:**
- Create: `crates/zerox1-node-enterprise/src/lib.rs`
- Create: `crates/zerox1-node-enterprise/src/service.rs`
- Create: `crates/zerox1-node-enterprise/src/handle.rs`
- Modify: `crates/zerox1-node-enterprise/Cargo.toml` (add `[lib]` section)
- Modify: `crates/zerox1-node-enterprise/src/main.rs` (collapse to thin wrapper)

The current `node-enterprise` is a binary — `main.rs` builds config, identity, swarm, API server, and runs the loop. We extract a library API so embedders can do the same from their own binaries.

- [ ] **Step 1: Add `[lib]` declaration to manifest**

In `crates/zerox1-node-enterprise/Cargo.toml`, add:

```toml
[lib]
name = "zerox1_node_enterprise"
path = "src/lib.rs"

[[bin]]
name = "zerox1-node-enterprise"
path = "src/main.rs"
```

(If `[[bin]]` already exists, leave it; otherwise add it explicitly so cargo knows both targets exist.)

- [ ] **Step 2: Define the library surface in `lib.rs`**

`crates/zerox1-node-enterprise/src/lib.rs`:

```rust
//! Library API for the 0x01 enterprise node.
//!
//! Embedders construct a `NodeService` from a `NodeConfig`, drive it via
//! `NodeService::run()`, and interact with it through `NodeHandle`. The
//! `main.rs` binary wraps this into a standalone executable.

pub mod api;
pub mod batch;
pub mod config;
pub mod constants;
pub mod handle;
pub mod identity;
pub mod logger;
pub mod network;
pub mod node;
pub mod peer_state;
pub mod reputation;
pub mod service;

pub use config::Config as NodeConfig;
pub use handle::NodeHandle;
pub use service::NodeService;
```

- [ ] **Step 3: Extract the `NodeService` builder**

Create `crates/zerox1-node-enterprise/src/service.rs`. Move the current `main.rs` boot logic (config parsing minus clap, identity load, swarm build, API server bind) into:

```rust
use anyhow::Result;

use crate::config::Config;
use crate::handle::NodeHandle;

pub struct NodeService {
    /* moved from main.rs: swarm, api_state, identity, etc. */
}

impl NodeService {
    /// Build a node from configuration. Does not start the event loop —
    /// caller drives via `run()`.
    pub fn build(cfg: Config) -> Result<Self> { /* ... */ }

    /// Get a clone-able handle for sending envelopes / subscribing to events.
    /// Cheap to clone; multiple handles share the same underlying queues.
    pub fn handle(&self) -> NodeHandle { /* ... */ }

    /// Run the Swarm event loop and the API server. Returns when shutdown
    /// is signalled or an unrecoverable error occurs.
    pub async fn run(self) -> Result<()> { /* ... */ }
}
```

The exact field set comes from inspecting `main.rs` — every local variable that lives across the swarm event-loop iterations becomes a field. Don't change semantics; just move the code.

- [ ] **Step 4: Define `NodeHandle`**

Create `crates/zerox1-node-enterprise/src/handle.rs`:

```rust
use anyhow::Result;
use tokio::sync::mpsc;

use zerox1_protocol::Envelope;

#[derive(Clone)]
pub struct NodeHandle {
    outbound: mpsc::Sender<Envelope>,
    inbound:  flume::Receiver<Envelope>,
    /* possibly: subscriptions handle, peer-list snapshot getter */
}

impl NodeHandle {
    /// Send an envelope onto the mesh. The node signs (with its identity)
    /// and routes; this returns when the envelope is queued for transmission.
    pub async fn send(&self, env: Envelope) -> Result<()> {
        self.outbound.send(env).await.map_err(|_| anyhow::anyhow!("node shut down"))
    }

    /// Receive an envelope addressed to this node. Returns None when the
    /// underlying channel closes (node shutting down).
    pub async fn recv(&self) -> Option<Envelope> {
        self.inbound.recv_async().await.ok()
    }
}
```

(`flume` is chosen here because it's cheap-clone-able for multi-consumer; if the workspace already standardizes on `tokio::sync::broadcast` for fan-out, use that instead. Decision: read what `node.rs` and `api.rs` already do for inbound delivery and match the existing channel type.)

- [ ] **Step 5: Collapse `main.rs` to a thin wrapper**

`crates/zerox1-node-enterprise/src/main.rs`:

```rust
use anyhow::Result;
use clap::Parser;
use zerox1_node_enterprise::{NodeConfig, NodeService};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = NodeConfig::parse();
    NodeService::build(cfg)?.run().await
}
```

Anything else currently in `main.rs` should have moved to `service.rs` / `handle.rs` / existing modules.

- [ ] **Step 6: Verify both library and binary build**

```bash
cd /Users/tobiasd/Desktop/node-enterprise
cargo build -p zerox1-node-enterprise --lib
cargo build -p zerox1-node-enterprise --bin zerox1-node-enterprise
cargo test -p zerox1-node-enterprise   # if any tests exist
```

Both must succeed.

- [ ] **Step 7: Verify embedder smoke build (from 01fi)**

```bash
cd /Users/tobiasd/Desktop/zerox1-01fi-fleet
cargo check --workspace
```

01fi crates don't yet *use* node-enterprise's library API, but the path-dep declaration from Task 1 must still resolve.

- [ ] **Step 8: Commit (in node-enterprise worktree)**

```bash
cd /Users/tobiasd/Desktop/node-enterprise
git add crates/zerox1-node-enterprise/
git commit -m "node-enterprise: extract NodeService library API alongside binary

Embedders can now build a node via NodeService::build(cfg).run() and
interact with it through a clone-able NodeHandle. The standalone binary
becomes a thin clap parser around the same library."
```

---

### Task 3: Define `zerox1-protocol::fleet` payload taxonomy

**Repo:** node-enterprise worktree, `crates/zerox1-protocol/`.

**Files:**
- Create: `crates/zerox1-protocol/src/fleet/mod.rs`
- Create: `crates/zerox1-protocol/src/fleet/{multiply,hedgedjlp,stablefloor,riskwatcher,researcher,speculator}.rs`
- Modify: `crates/zerox1-protocol/src/lib.rs` (declare `pub mod fleet`)

Per-desk request/response payload types. Pure schema, no logic. CBOR-serializable. **No string fields except where strictly necessary** (e.g., a transaction signature in base58).

- [ ] **Step 1: Module declaration**

In `crates/zerox1-protocol/src/lib.rs`, add:

```rust
pub mod fleet;
```

- [ ] **Step 2: Module root with shared types**

`crates/zerox1-protocol/src/fleet/mod.rs`:

```rust
//! Fleet-specific payload types.
//!
//! Each desk has its own pair of (request, response) types. They serialize
//! via CBOR and ride inside the `Envelope::payload` field of the existing
//! 0x01 protocol. The MsgType high nibble (0x1_) identifies these as
//! Collaboration messages — intra-org coordination, no payment leg.

pub mod hedgedjlp;
pub mod multiply;
pub mod researcher;
pub mod riskwatcher;
pub mod sanctum;       // alias name kept; module is "sanctum" since stablefloor is the role
pub mod speculator;
pub mod stablefloor;

use serde::{Deserialize, Serialize};

/// Every fleet response carries this header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportHeader {
    /// Echoed from the originating Assign's conversation_id (already in Envelope, but useful inline for log replay).
    pub conversation_id: [u8; 16],
    /// Did the requested action succeed?
    pub ok: bool,
    /// On failure, machine-readable error code (no English).
    pub error_code: Option<u32>,
}
```

- [ ] **Step 3: Per-desk payload modules**

Each desk gets two structs (request + response). Examples — fill in the actual fields per the daemon's mandate:

`crates/zerox1-protocol/src/fleet/multiply.rs`:

```rust
//! Multiply Desk — Kamino leveraged LST positions.

use serde::{Deserialize, Serialize};
use super::ReportHeader;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignMultiply {
    /// Position identifier (Kamino vault key bytes).
    pub vault: [u8; 32],
    /// Target loan-to-value, in basis points (e.g. 6000 = 60%).
    pub target_ltv_bps: u16,
    /// Maximum acceptable slippage on rebalance, in basis points.
    pub max_slippage_bps: u16,
    /// Hard deadline (unix seconds).
    pub deadline_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportMultiply {
    pub header: ReportHeader,
    /// Resulting LTV after the rebalance (bps).
    pub resulting_ltv_bps: u16,
    /// Solana transaction signature, base58 (only string field — required by the chain).
    pub tx_signature: Option<String>,
}
```

Define analogous shapes for the other five desks. Use the `01fi/PROTOCOLS.md` and the per-daemon plan section as the source of truth for what fields matter. Keep payloads compact — every field is a numeric type or fixed-length byte array unless impossible.

- [ ] **Step 4: Test round-trip CBOR encoding**

`crates/zerox1-protocol/src/fleet/mod.rs` add at bottom:

```rust
#[cfg(test)]
mod tests {
    use super::multiply::*;
    use ciborium::{de::from_reader, ser::into_writer};

    #[test]
    fn assign_multiply_roundtrips() {
        let original = AssignMultiply {
            vault: [1u8; 32],
            target_ltv_bps: 6000,
            max_slippage_bps: 50,
            deadline_unix: 1714800000,
        };
        let mut buf = Vec::new();
        into_writer(&original, &mut buf).unwrap();
        let decoded: AssignMultiply = from_reader(&buf[..]).unwrap();
        assert_eq!(decoded.vault, original.vault);
        assert_eq!(decoded.target_ltv_bps, original.target_ltv_bps);
    }
}
```

Add one round-trip test per desk.

- [ ] **Step 5: Build and test**

```bash
cd /Users/tobiasd/Desktop/node-enterprise
cargo test -p zerox1-protocol
```

All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/zerox1-protocol/
git commit -m "node-enterprise: add zerox1-protocol::fleet per-desk payloads

Compact CBOR-serializable request/response types for each of the six 01fi
desks (multiply, hedgedjlp, stablefloor, riskwatcher, researcher, speculator).
No string fields on the hot path — humans see English only at the edge."
```

---

### Task 4: Add `runtime::identity::RoleIdentity`

**Repo:** 01fi worktree, `crates/zerox1-defi-runtime/`.

**Files:**
- Create: `crates/zerox1-defi-runtime/src/identity.rs`
- Modify: `crates/zerox1-defi-runtime/src/lib.rs` (declare `pub mod identity`)
- Modify: `crates/zerox1-defi-runtime/Cargo.toml` (add `ed25519-dalek` workspace dep, declare in workspace if missing)

- [ ] **Step 1: Add `ed25519-dalek` to workspace deps**

In root `Cargo.toml` `[workspace.dependencies]`:

```toml
ed25519-dalek = { version = "2", features = ["rand_core", "serde"] }
```

(Already present in node-enterprise; mirror the version. If 01fi's solana-sdk pulls a conflicting version, pin both to whatever solana-sdk uses.)

- [ ] **Step 2: Define `Role` and `RoleIdentity`**

`crates/zerox1-defi-runtime/src/identity.rs`:

```rust
//! Long-lived agent identity, keyed by role rather than instance.
//!
//! A `RoleIdentity` survives any single daemon process. When a daemon
//! crashes and a replacement boots elsewhere, the replacement reloads
//! the same role key from a secrets backend and inherits the role's
//! place on the mesh.

use anyhow::{anyhow, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Multiply,
    HedgedJlp,
    StableFloor,
    RiskWatcher,
    Researcher,
    Speculator,
    Orchestrator,  // the mobile PM
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Multiply     => "multiply",
            Role::HedgedJlp    => "hedgedjlp",
            Role::StableFloor  => "stablefloor",
            Role::RiskWatcher  => "riskwatcher",
            Role::Researcher   => "researcher",
            Role::Speculator   => "speculator",
            Role::Orchestrator => "orchestrator",
        }
    }
}

pub struct RoleIdentity {
    role: Role,
    signing_key: SigningKey,
}

impl RoleIdentity {
    pub fn new(role: Role, signing_key: SigningKey) -> Self {
        Self { role, signing_key }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}
```

- [ ] **Step 3: Wire into lib.rs**

In `crates/zerox1-defi-runtime/src/lib.rs`, add:

```rust
pub mod identity;
```

- [ ] **Step 4: Add a unit test for round-trip stability**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn role_str_stable() {
        assert_eq!(Role::Multiply.as_str(), "multiply");
        assert_eq!(Role::HedgedJlp.as_str(), "hedgedjlp");
    }

    #[test]
    fn identity_round_trip() {
        let key = SigningKey::generate(&mut OsRng);
        let id = RoleIdentity::new(Role::Multiply, key);
        assert_eq!(id.role(), Role::Multiply);
        // Verifying key is deterministic from signing key
        assert_eq!(id.verifying_key().as_bytes().len(), 32);
    }
}
```

- [ ] **Step 5: Build and test**

```bash
cargo test -p zerox1-defi-runtime
```

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/zerox1-defi-runtime/Cargo.toml crates/zerox1-defi-runtime/src/identity.rs crates/zerox1-defi-runtime/src/lib.rs
git commit -m "01fi: add runtime::identity (RoleIdentity + Role enum)"
```

---

### Task 5: Add `runtime::secrets` abstraction

**Files:**
- Create: `crates/zerox1-defi-runtime/src/secrets.rs`
- Modify: `crates/zerox1-defi-runtime/src/lib.rs`
- Modify: `crates/zerox1-defi-runtime/Cargo.toml` (no new deps required for file/env backends)

A pluggable secret-source trait. File and env backends ship now; Vault/SOPS are stubs to be filled later.

- [ ] **Step 1: Define the trait + two backends**

`crates/zerox1-defi-runtime/src/secrets.rs`:

```rust
//! Secret-source abstraction.
//!
//! Daemons load their role identity (and Solana wallet) from a
//! `SecretSource` at boot. The actual storage backend is configurable —
//! file path for development, env var for containers, Vault for prod.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

#[async_trait::async_trait]
pub trait SecretSource: Send + Sync {
    /// Fetch a secret by logical name. Returns the raw bytes; callers parse.
    async fn get(&self, name: &str) -> Result<Vec<u8>>;
}

/// Reads secrets from files in a directory. Each secret is a separate file
/// named after the secret key (e.g. `multiply-role.key`, `multiply-wallet.json`).
pub struct FileSource {
    base: PathBuf,
}

impl FileSource {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }
}

#[async_trait::async_trait]
impl SecretSource for FileSource {
    async fn get(&self, name: &str) -> Result<Vec<u8>> {
        let path = self.base.join(name);
        tokio::fs::read(&path).await
            .with_context(|| format!("read secret {} at {}", name, path.display()))
    }
}

/// Reads secrets from environment variables. Useful for container deployments
/// where the secret is injected by the orchestrator.
pub struct EnvSource;

#[async_trait::async_trait]
impl SecretSource for EnvSource {
    async fn get(&self, name: &str) -> Result<Vec<u8>> {
        let env_name = name.to_uppercase().replace('-', "_");
        let value = std::env::var(&env_name)
            .with_context(|| format!("env var {} not set", env_name))?;
        Ok(value.into_bytes())
    }
}
```

- [ ] **Step 2: Helper for loading a `RoleIdentity` via a `SecretSource`**

Append to `secrets.rs`:

```rust
use crate::identity::{Role, RoleIdentity};
use ed25519_dalek::SigningKey;

/// Load a role identity from a secret source. The secret must be exactly
/// 32 raw bytes (Ed25519 signing key seed). For dev, generate with
/// `openssl rand 32 > .role.key`.
pub async fn load_role_identity(
    source: &dyn SecretSource,
    role: Role,
    secret_name: &str,
) -> Result<RoleIdentity> {
    let raw = source.get(secret_name).await?;
    if raw.len() != 32 {
        return Err(anyhow!("role key must be exactly 32 bytes, got {}", raw.len()));
    }
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&raw);
    let signing_key = SigningKey::from_bytes(&bytes);
    Ok(RoleIdentity::new(role, signing_key))
}
```

- [ ] **Step 3: Wire into lib.rs and add `tempfile` to dev-deps for tests**

In `lib.rs`:

```rust
pub mod secrets;
```

In runtime crate `Cargo.toml`:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 4: Tests**

Append to `secrets.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Role;
    use tempfile::TempDir;

    #[tokio::test]
    async fn file_source_round_trip() {
        let tmp = TempDir::new().unwrap();
        let secret_path = tmp.path().join("multiply-role.key");
        let key_bytes: [u8; 32] = [42; 32];
        tokio::fs::write(&secret_path, key_bytes).await.unwrap();

        let src = FileSource::new(tmp.path());
        let id = load_role_identity(&src, Role::Multiply, "multiply-role.key").await.unwrap();
        assert_eq!(id.role(), Role::Multiply);
    }

    #[tokio::test]
    async fn rejects_wrong_size() {
        let tmp = TempDir::new().unwrap();
        let secret_path = tmp.path().join("bad.key");
        tokio::fs::write(&secret_path, b"too short").await.unwrap();

        let src = FileSource::new(tmp.path());
        let res = load_role_identity(&src, Role::Multiply, "bad.key").await;
        assert!(res.is_err());
    }
}
```

- [ ] **Step 5: Build and test**

```bash
cargo test -p zerox1-defi-runtime
```

- [ ] **Step 6: Commit**

```bash
git add crates/zerox1-defi-runtime/
git commit -m "01fi: add runtime::secrets (FileSource, EnvSource, role-identity loader)"
```

---

### Task 6: Add `runtime::role_registry` (mesh-level claim/resolve)

**Files:**
- Create: `crates/zerox1-defi-runtime/src/role_registry.rs`
- Modify: `crates/zerox1-defi-runtime/src/lib.rs`

A registry that listens for `Beacon` envelopes from every peer, parses out role assignments, and resolves "Multiply Desk → currently which PeerId?" lookups for outbound sends.

- [ ] **Step 1: Define the registry + claim/conflict semantics**

`crates/zerox1-defi-runtime/src/role_registry.rs`:

```rust
//! Mesh-level role-to-peer resolution.
//!
//! Every daemon broadcasts a Beacon every N seconds containing its role
//! and instance pubkey. The registry maintains a role -> (peer_id,
//! last_seen) map. Outbound sends look up "who is currently the
//! Multiply Desk?" via this map.
//!
//! Conflict resolution: if two BEACONs claim the same role with different
//! instance keys within the staleness window, the lexicographically lower
//! pubkey wins (deterministic, no coordination needed). The losing daemon
//! detects the conflict on its own via its own registry and is expected
//! to shut down (or retry after the winner stops broadcasting).

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;

use crate::identity::Role;

#[derive(Debug, Clone, Copy)]
pub struct RoleAssignment {
    pub instance_pubkey: VerifyingKey,
    pub last_seen: Instant,
}

pub struct RoleRegistry {
    map: RwLock<HashMap<Role, RoleAssignment>>,
    stale_after: Duration,
}

impl RoleRegistry {
    pub fn new(stale_after: Duration) -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
            stale_after,
        }
    }

    /// Update the registry from a heard Beacon. Returns `Some(loser_pubkey)`
    /// if a conflict was detected and we should expect the loser to drop.
    pub fn observe(&self, role: Role, instance_pubkey: VerifyingKey) -> Option<VerifyingKey> {
        let mut map = self.map.write().unwrap();
        let now = Instant::now();
        match map.get(&role) {
            None => {
                map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                None
            }
            Some(existing) if existing.instance_pubkey == instance_pubkey => {
                map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                None
            }
            Some(existing) if existing.last_seen.elapsed() > self.stale_after => {
                // The previous holder went silent — new instance takes over.
                map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                None
            }
            Some(existing) => {
                // Live conflict. Lower pubkey wins.
                if instance_pubkey.as_bytes() < existing.instance_pubkey.as_bytes() {
                    let loser = existing.instance_pubkey;
                    map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                    Some(loser)
                } else {
                    Some(instance_pubkey)
                }
            }
        }
    }

    pub fn resolve(&self, role: Role) -> Option<VerifyingKey> {
        let map = self.map.read().unwrap();
        map.get(&role)
            .filter(|a| a.last_seen.elapsed() <= self.stale_after)
            .map(|a| a.instance_pubkey)
    }
}
```

- [ ] **Step 2: Tests**

Append:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn key() -> VerifyingKey {
        SigningKey::generate(&mut OsRng).verifying_key()
    }

    #[test]
    fn first_claim_wins() {
        let reg = RoleRegistry::new(Duration::from_secs(60));
        let a = key();
        assert!(reg.observe(Role::Multiply, a).is_none());
        assert_eq!(reg.resolve(Role::Multiply), Some(a));
    }

    #[test]
    fn live_conflict_lower_pubkey_wins() {
        let reg = RoleRegistry::new(Duration::from_secs(60));
        let mut a = key();
        let mut b = key();
        if a.as_bytes() > b.as_bytes() { std::mem::swap(&mut a, &mut b); }
        // a < b
        assert!(reg.observe(Role::Multiply, b).is_none());
        let loser = reg.observe(Role::Multiply, a);
        assert_eq!(loser, Some(b));
        assert_eq!(reg.resolve(Role::Multiply), Some(a));
    }
}
```

- [ ] **Step 3: Wire into lib.rs**

```rust
pub mod role_registry;
```

- [ ] **Step 4: Build and test**

```bash
cargo test -p zerox1-defi-runtime
```

- [ ] **Step 5: Commit**

```bash
git add crates/zerox1-defi-runtime/
git commit -m "01fi: add runtime::role_registry (claim/resolve with conflict semantics)"
```

---

### Task 7: Add `runtime::replay::ChainReplay` trait

**Files:**
- Create: `crates/zerox1-defi-runtime/src/replay.rs`
- Modify: `crates/zerox1-defi-runtime/src/lib.rs`

A boot-time hook for daemons to rebuild in-memory state from on-chain history. The trait is generic — each daemon implements its own version. The runtime crate just defines the contract and a stub helper.

- [ ] **Step 1: Define the trait**

`crates/zerox1-defi-runtime/src/replay.rs`:

```rust
//! Boot-time chain replay.
//!
//! Each daemon implements `ChainReplay::replay(...)` to reconstruct in-
//! memory state from on-chain ixns matching its role's signing key. This
//! is what makes daemons stateless: kill any process, replay rebuilds the
//! state in seconds on the replacement.

use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::signature::Signature;

use crate::identity::Role;
use crate::rpc::RpcContext;

/// A daemon-specific state object built from chain history.
#[async_trait]
pub trait ChainReplay: Sized {
    /// Restore state by reading chain history filtered by the role's
    /// signing pubkey. Implementors typically scan the last N slots,
    /// fetch the role's signed ixns, and fold them into the daemon's
    /// canonical in-memory shape.
    async fn replay(rpc: &RpcContext, role: Role) -> Result<Self>;

    /// Most recently observed signature, for incremental update after
    /// boot. None means "no history yet" (fresh fleet).
    fn last_signature(&self) -> Option<Signature>;
}
```

- [ ] **Step 2: Wire into lib.rs**

```rust
pub mod replay;
```

- [ ] **Step 3: Smoke build (no tests yet — implementations come per-daemon in their strategy plans)**

```bash
cargo check -p zerox1-defi-runtime
```

- [ ] **Step 4: Commit**

```bash
git add crates/zerox1-defi-runtime/
git commit -m "01fi: add runtime::replay::ChainReplay trait

Boot-time chain-replay primitive. Each daemon implements its own
ChainReplay so a freshly-booted replacement can reconstruct in-memory
state from on-chain ixns matching its role's signing key. Concrete
impls land in per-daemon strategy plans."
```

---

### Task 8: Embed `NodeService` into each fleet daemon

**Files:** For **each** of the six daemons:
- Modify: `crates/<daemon>-daemon/Cargo.toml` (add `zerox1-protocol`, `zerox1-node-enterprise`, `ed25519-dalek` deps; remove `axum` if /health is dropped)
- Modify: `crates/<daemon>-daemon/src/main.rs` (heavy rewrite — see template)

This is a single task because the changes follow a uniform template. Process the daemons in this order to surface integration issues early:

1. `riskwatcher-daemon` (no-wallet, multi-thread — easy first pass)
2. `multiply-daemon` (signing, single-thread + embedded node)
3. `hedgedjlp-daemon`, `researcher-daemon`, `speculator-daemon` (mechanical follow-ups)
4. `stablefloor-daemon` (one-shot — sees a *thinner* embedding; see note)

**Per-daemon template (pseudocode for the new `main.rs`):**

```rust
use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{
    build_runtime, Daemon, RuntimeProfile,
    identity::{Role, RoleIdentity},
    role_registry::RoleRegistry,
    secrets::{FileSource, load_role_identity},
};
use zerox1_node_enterprise::{NodeConfig, NodeService};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_SECRETS_DIR")]
    secrets_dir: PathBuf,
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/0")]
    listen: String,
    #[arg(long, env = "ZX_BOOTSTRAP", value_delimiter = ',')]
    bootstrap: Vec<String>,
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,
    // ... daemon-specific args (e.g. multiply's --kamino-vault, speculator's --quote-ttl-ms)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let rt = build_runtime(RuntimeProfile::SingleThread)?;  // per-daemon profile
    rt.block_on(async move {
        // Load role identity from secrets
        let secrets = FileSource::new(&args.secrets_dir);
        let role_id = load_role_identity(&secrets, Role::Multiply, "multiply-role.key").await?;

        // Build embedded node-enterprise instance
        let node_cfg = NodeConfig::for_fleet(role_id.signing_key().clone(), &args.listen, &args.bootstrap);
        let node = NodeService::build(node_cfg)?;
        let handle = node.handle();

        // Spawn the node's event loop
        let node_task = tokio::spawn(node.run());

        // Daemon-specific startup: chain replay, beacon emitter, inbox dispatcher
        let registry = RoleRegistry::new(std::time::Duration::from_secs(args.beacon_interval_secs * 3));
        let beacon_task = tokio::spawn(emit_beacons(handle.clone(), role_id.role(), args.beacon_interval_secs));
        let inbox_task = tokio::spawn(handle_inbox(handle.clone(), /* daemon state */));

        tokio::select! {
            r = node_task    => r??,
            r = beacon_task  => r??,
            r = inbox_task   => r??,
        }
        Ok(())
    })
}

async fn emit_beacons(handle: NodeHandle, role: Role, interval_secs: u64) -> Result<()> {
    use std::time::Duration;
    use zerox1_protocol::{Envelope, MsgType};
    let interval = Duration::from_secs(interval_secs);
    loop {
        // Construct a Beacon envelope (MsgType::Beacon = 0x03), broadcast to mesh
        // The role + instance pubkey are encoded in the payload
        // ...
        tokio::time::sleep(interval).await;
    }
}

async fn handle_inbox(handle: NodeHandle, /* state */) -> Result<()> {
    while let Some(env) = handle.recv().await {
        match env.msg_type {
            MsgType::Assign => { /* dispatch */ }
            MsgType::Beacon => { /* update role registry */ }
            MsgType::TaskCancel => { /* abort in-flight */ }
            _ => { /* ignore or log */ }
        }
    }
    Ok(())
}
```

**Stablefloor exception:** Stablefloor is one-shot — `RuntimeProfile::OneShot`. Embedding the full Swarm just to mint INF and exit is overkill (200-500ms swarm bring-up vs ~50ms for the actual operation). For stablefloor, the embedded node is **not built**. Instead, the binary opens a libp2p-less HTTP+WS connection to a co-located node (configurable via `--peer-node-api http://127.0.0.1:8080`) and sends one envelope through that. This is the only daemon that talks to a *separate* node binary.

**Implementation order** (one sub-task per daemon, separate commits):

- [ ] **8.1: riskwatcher-daemon** — embed NodeService, drop /health (keep ops sidecar optional), drop --fleet-token, add --secrets-dir + --listen + --bootstrap, emit Beacons
- [ ] **8.2: multiply-daemon** — same template, plus chain-replay-on-boot stub (calls `runtime::replay::ChainReplay::replay(...)` — actual impl is `unimplemented!()` for now; strategy plan fills it)
- [ ] **8.3: hedgedjlp-daemon** — same as 8.2; bump runtime to MultiThread{2} if not already
- [ ] **8.4: researcher-daemon** — same; Batch runtime stays
- [ ] **8.5: speculator-daemon** — bump to MultiThread{2} (was SingleThread); embed NodeService; pin only the trade-execution task, not the whole runtime
- [ ] **8.6: stablefloor-daemon** — different shape: HTTP-only client of a co-located node binary; no embedded Swarm; otherwise same auth/identity model

Each sub-task ends with its own commit. Total: 6 commits across this task.

After 8.6: run `cargo build --workspace`, confirm clean. `cargo tree -p riskwatcher-daemon | grep zerox1-defi-wallet` still empty (authority isolation preserved).

---

### Task 9: Drop obsolete bits

**Files:**
- Delete: `crates/zerox1-defi-runtime/src/mesh.rs`
- Delete: `crates/zerox1-defi-runtime/src/health.rs`
- Modify: `crates/zerox1-defi-runtime/src/lib.rs` (remove `pub mod mesh;` and `pub mod health;`)
- Modify: each daemon's main.rs that previously imported these (should already be done in Task 8)

- [ ] **Step 1: Delete the now-unused modules**

```bash
rm crates/zerox1-defi-runtime/src/mesh.rs
rm crates/zerox1-defi-runtime/src/health.rs
```

- [ ] **Step 2: Remove module declarations from lib.rs**

Edit `crates/zerox1-defi-runtime/src/lib.rs`, delete the `pub mod mesh;` and `pub mod health;` lines.

- [ ] **Step 3: Verify nothing else references them**

```bash
grep -rn "runtime::mesh\|runtime::health\|use zerox1_defi_runtime::mesh\|use zerox1_defi_runtime::health" crates/ tools/
```

Expected: no results.

- [ ] **Step 4: Build clean**

```bash
cargo build --workspace
cargo check --workspace
```

Expected: success.

- [ ] **Step 5: Commit**

```bash
git add crates/zerox1-defi-runtime/
git commit -m "01fi: remove runtime::mesh and runtime::health (replaced by embedded node)

The HMAC mesh module was a placeholder. Per-message envelope signing
now happens inside the embedded zerox1-node-enterprise instance via
its libp2p stack and the canonical Ed25519 envelope signing in
zerox1-protocol. The /health route is replaced by mesh-level Beacons.
Ops monitoring can attach to the node-enterprise API if needed."
```

---

### Task 10: Build the 6-container local-dev fleet harness

**Files:**
- Create: `deploy/Dockerfile` (one shared Dockerfile, takes `DAEMON` build arg)
- Create: `deploy/docker-compose.fleet.yml`
- Create: `deploy/role-keys/.gitignore` (ignore `*.key`)
- Create: `deploy/role-keys/generate.sh` (helper to generate dev keys)
- Create: `deploy/README.md`

The harness boots six containers, each with one daemon and one role key, sharing a private docker network. Mesh discovery happens via mDNS (works in docker bridge networks) or explicit bootstrap multiaddrs.

- [ ] **Step 1: Shared Dockerfile**

`deploy/Dockerfile`:

```dockerfile
FROM rust:1.83-slim AS build
ARG DAEMON
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /work
COPY . .
COPY ../node-enterprise /work/node-enterprise
RUN cargo build --release -p ${DAEMON}-daemon

FROM debian:bookworm-slim
ARG DAEMON
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /work/target/release/${DAEMON}-daemon /usr/local/bin/daemon
ENTRYPOINT ["/usr/local/bin/daemon"]
```

(The COPY of `../node-enterprise` requires building the docker image with build context at the parent level. Document this in the README.)

- [ ] **Step 2: docker-compose**

`deploy/docker-compose.fleet.yml`:

```yaml
version: "3.9"

services:
  riskwatcher:
    build:
      context: ..
      dockerfile: zerox1-01fi-fleet/deploy/Dockerfile
      args:
        DAEMON: riskwatcher
    environment:
      ZX_SECRETS_DIR: /secrets
      ZX_LISTEN: /ip4/0.0.0.0/tcp/9001
      RUST_LOG: info
    volumes:
      - ./role-keys/riskwatcher:/secrets:ro
    networks: [fleet]

  multiply:
    build:
      context: ..
      dockerfile: zerox1-01fi-fleet/deploy/Dockerfile
      args:
        DAEMON: multiply
    environment:
      ZX_SECRETS_DIR: /secrets
      ZX_LISTEN: /ip4/0.0.0.0/tcp/9002
      ZX_BOOTSTRAP: /dns/riskwatcher/tcp/9001
      RUST_LOG: info
    volumes:
      - ./role-keys/multiply:/secrets:ro
    networks: [fleet]
    depends_on: [riskwatcher]

  # ... hedgedjlp, stablefloor, researcher, speculator analogously ...

networks:
  fleet:
    driver: bridge
```

- [ ] **Step 3: Key-generation helper**

`deploy/role-keys/generate.sh`:

```bash
#!/usr/bin/env bash
# Generates dev role keys (32 random bytes each) for all six desks.
# DO NOT use in production — these are unencrypted at rest.
set -euo pipefail
cd "$(dirname "$0")"
for role in riskwatcher multiply hedgedjlp stablefloor researcher speculator; do
    mkdir -p "$role"
    openssl rand 32 > "$role/${role}-role.key"
    chmod 600 "$role/${role}-role.key"
done
echo "Generated dev role keys. Add 'role-keys/' to git ignore (already done)."
```

`deploy/role-keys/.gitignore`:

```
*.key
```

- [ ] **Step 4: README**

`deploy/README.md`:

```markdown
# 01fi Local-Dev Fleet Harness

Boots all six fleet daemons in containers on a shared docker network.

## First-time setup

```bash
./deploy/role-keys/generate.sh
docker compose -f deploy/docker-compose.fleet.yml build
```

## Run

```bash
docker compose -f deploy/docker-compose.fleet.yml up
```

Each container runs one daemon. Mesh discovery uses bootstrap multiaddrs;
riskwatcher is the first to come up so the others bootstrap to it.

## Tear down

```bash
docker compose -f deploy/docker-compose.fleet.yml down -v
```
```

- [ ] **Step 5: Smoke run**

```bash
./deploy/role-keys/generate.sh
docker compose -f deploy/docker-compose.fleet.yml up --build
```

Expected: six containers come up, each emits its boot log + Beacons every 30s. Containers stay running. Press Ctrl-C and verify clean shutdown.

- [ ] **Step 6: Commit**

```bash
git add deploy/
git commit -m "01fi: add 6-container local-dev fleet harness (docker-compose)

Boots all six daemons in their own containers, simulating the
multi-machine production shape on a developer laptop. Role keys are
per-container, gitignored, generated by deploy/role-keys/generate.sh."
```

---

### Task 11: PM-stub CLI for end-to-end smoke

**Files:**
- Create: `tools/fleet-pm-stub/Cargo.toml`
- Create: `tools/fleet-pm-stub/src/main.rs`
- Modify: workspace `Cargo.toml` (add `tools/fleet-pm-stub` to members)

A small CLI that connects to the fleet mesh, sends a single `Assign` envelope to a target role, waits for the `Report` reply, and prints it. This is the integration test substitute until the mobile PM is wired up.

- [ ] **Step 1: Manifest**

`tools/fleet-pm-stub/Cargo.toml`:

```toml
[package]
name = "fleet-pm-stub"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "fleet-pm-stub"
path = "src/main.rs"

[dependencies]
zerox1-defi-runtime = { path = "../../crates/zerox1-defi-runtime" }
zerox1-protocol.workspace = true
zerox1-node-enterprise.workspace = true
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
ciborium = "0.2"  # for serializing fleet payloads
```

- [ ] **Step 2: Implementation**

`tools/fleet-pm-stub/src/main.rs`:

```rust
//! Fleet PM stub — sends one Assign and prints the Report.
//!
//! Until the mobile app's PM client is wired up, this is the way to
//! drive the fleet end-to-end during development.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use zerox1_defi_runtime::{
    identity::{Role, RoleIdentity},
    secrets::{FileSource, load_role_identity},
};
use zerox1_node_enterprise::{NodeConfig, NodeService};
use zerox1_protocol::{Envelope, MsgType, fleet::multiply::AssignMultiply};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    secrets_dir: PathBuf,
    #[arg(long, default_value = "/ip4/127.0.0.1/tcp/0")]
    listen: String,
    #[arg(long, value_delimiter = ',')]
    bootstrap: Vec<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Send an Assign to the Multiply Desk and wait for the Report.
    AssignMultiply {
        #[arg(long)]
        target_ltv_bps: u16,
    },
    // ... add Cmd variants for the other desks as their payloads land ...
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let secrets = FileSource::new(&args.secrets_dir);
    let role_id = load_role_identity(&secrets, Role::Orchestrator, "orchestrator-role.key").await?;
    // Build a mini node so we can speak the mesh.
    let cfg = NodeConfig::for_fleet(role_id.signing_key().clone(), &args.listen, &args.bootstrap);
    let node = NodeService::build(cfg)?;
    let handle = node.handle();
    let _node_task = tokio::spawn(node.run());

    match args.cmd {
        Cmd::AssignMultiply { target_ltv_bps } => {
            let payload = AssignMultiply {
                vault: [0u8; 32],
                target_ltv_bps,
                max_slippage_bps: 50,
                deadline_unix: chrono_now() + 300,
            };
            // ... encode payload, build Envelope with MsgType::Assign, send via handle ...
            let timeout = Duration::from_secs(30);
            let report = wait_for_report(&handle, timeout).await?;
            println!("Report: {:?}", report);
        }
    }
    Ok(())
}

fn chrono_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn wait_for_report(handle: &zerox1_node_enterprise::NodeHandle, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, async {
        while let Some(env) = handle.recv().await {
            if env.msg_type == MsgType::Report {
                return Ok(());
            }
        }
        anyhow::bail!("inbox closed");
    }).await??;
    Ok(())
}
```

(Replace pseudocode for envelope construction with the actual Envelope::new(...) call once node-enterprise's library API exposes it.)

- [ ] **Step 3: Add to workspace**

In root `Cargo.toml`, append to `[workspace] members`:

```toml
"tools/fleet-pm-stub",
```

- [ ] **Step 4: Build**

```bash
cargo build -p fleet-pm-stub
```

- [ ] **Step 5: End-to-end smoke run**

In one terminal:

```bash
docker compose -f deploy/docker-compose.fleet.yml up
```

In another:

```bash
cargo run -p fleet-pm-stub -- \
    --secrets-dir deploy/role-keys/orchestrator \
    --bootstrap /ip4/127.0.0.1/tcp/9002 \
    assign-multiply --target-ltv-bps 6000
```

Expected: the stub sends an Assign envelope, the Multiply container in the fleet receives it, dispatches (logs the request — actual rebalance is `unimplemented!()` until the strategy plan), and returns a Report. The stub prints the Report and exits.

If the daemon's handler is `unimplemented!()`, expect a panic on the daemon side and an error-coded Report — that's still fine; it proves the round-trip works.

- [ ] **Step 6: Commit**

```bash
git add tools/ Cargo.toml
git commit -m "01fi: add fleet-pm-stub CLI for end-to-end smoke testing

Sends an Assign envelope to a target role and prints the Report. Until
the mobile PM is wired in, this is the development driver."
```

---

### Task 12: Final cross-crate review

Dispatch a final code-reviewer subagent:

- Confirm the structural property holds: `cargo tree -p riskwatcher-daemon | grep zerox1-defi-wallet` still empty after all changes.
- Confirm `cargo build --workspace` clean in 01fi worktree.
- Confirm `cargo build --workspace` clean in node-enterprise worktree.
- Confirm `docker compose -f deploy/docker-compose.fleet.yml up` boots six containers without crashes (stays up at least 2 minutes; each container emits at least 4 BEACONs).
- Confirm `fleet-pm-stub` end-to-end smoke produces a valid Report (or expected unimplemented-panic error code; either is acceptable for this plan).
- Confirm read-only zones (`crates/zerox1-defi-daemon/`, `crates/zerox1-defi-protocols/`) untouched: `git diff ac7b911..HEAD -- crates/zerox1-defi-daemon/ crates/zerox1-defi-protocols/` empty.
- Confirm no half-finished work — every daemon embeds the node, every daemon has a role-keyed identity, no `--fleet-token` references remain.

Report: READY TO MERGE / READY WITH POLISH / FIXES REQUIRED.

---

## Self-Review Notes

**Spec coverage:**
- Hot-swap across machines: Tasks 4, 5, 8 (role identity + secrets + embedded node)
- Stateless / replayable: Task 7 (replay trait), Task 8 (boot-time replay hook in each daemon)
- Binary wire format, no human middleware: Task 3 (fleet payloads, no string fields)
- Role-keyed identity: Tasks 4, 6 (RoleIdentity + role registry with conflict resolution)
- node-enterprise modifiable: Task 2 (library extraction)
- Six machines: Task 10 (docker-compose harness simulating six hosts)
- End-to-end testable: Task 11 (fleet-pm-stub)

Every requirement traces to at least one task.

**Placeholder scan:** No "TBD" / "implement later" — every step has either complete code, a precise list of what to extract, or a clearly-bounded stub (e.g., `unimplemented!()` in chain replay, to be filled by per-daemon strategy plans).

**Type consistency:** `Role` enum is defined in Task 4 and consumed verbatim in Tasks 5–8. `RoleIdentity` is constructed once (Task 4), loaded via `load_role_identity` (Task 5), used by daemons (Task 8). `RoleRegistry` API stable across Task 6 / Task 8. `ChainReplay` trait stable Task 7 / Task 8.

**What's deliberately deferred:**
- Concrete chain-replay implementations per daemon → per-daemon strategy plans
- Mobile PM client (uses fleet-pm-stub as a placeholder for now)
- Aggregator repurposing (separate plan)
- Vault / SOPS secret backends beyond `FileSource` and `EnvSource` → operational hardening plan
- Public bootstrap fleet for cross-org → out of scope for fleet-internal mesh

**Risk:** Task 2 (library extraction) is the largest task. If the current `main.rs` has tighter coupling between Swarm setup and CLI parsing than expected, the refactor surfaces friction late. Mitigation: Task 2 is gated on `cargo build` succeeding *both* the library and the binary — if the binary breaks, the refactor stalls and we fix it before downstream tasks can proceed.
