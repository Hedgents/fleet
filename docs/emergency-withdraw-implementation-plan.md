# Emergency Withdraw — implementation plan (research only)

Status: research / design only. No code changes yet. v0.2.9 baseline.

This document captures the file-by-file diff plan for adding a one-click
"emergency withdraw to a whitelisted destination" path to the Hedgents fleet.
It is the artefact produced by the read described in
`docs/emergency-withdraw-implementation-plan.md`'s own task spec.

Feature in one sentence: operator hits a red button on
`dashboard.hedgents.com`, every live execution daemon (stable-yield, multiply,
hedgedjlp) unwinds its position and sweeps remaining USDC + SOL to an address
loaded at install time from the env var `EMERGENCY_WITHDRAW_DESTINATION`.

The compile-time authority boundary is preserved: the dashboard server still
holds no signing key and signs nothing. It only broadcasts an `EmergencyWithdraw`
envelope. The daemons own the keys and own the sweep.

---

## Section 1 — Authoritative ground truth

### 1.1 Mesh protocol

The protocol crate is **outside** the fleet workspace: it lives in the sibling
p2p_architecture worktree and is pulled in by path in `Cargo.toml`.

- `Cargo.toml` lines 53-54:

  ```
  zerox1-protocol       = { path = "../p2p_architecture/crates/zerox1-protocol" }
  zerox1-node-enterprise = { path = "../p2p_architecture/crates/zerox1-node-enterprise" }
  ```

- Absolute path: `/Users/tobiasd/Desktop/Hedgents/p2p_architecture/crates/zerox1-protocol/src/`

Key files inside the protocol crate:

- `src/message.rs`          — `MsgType` enum, the `repr(u16)` wire code.
- `src/envelope.rs`         — `Envelope` (signed wrapper). `Envelope::build(msg_type, sender, recipient, ts, nonce, conv_id, payload, sk)`.
- `src/fleet/mod.rs`        — `ReportHeader { conversation_id, ok, error_code }`.
- `src/fleet/stable_lend.rs`  — AssignStableLend / WithdrawStableLend / ReportStableLend / ReportStableWithdraw.
- `src/fleet/multiply.rs`     — AssignMultiply / ReportMultiply only (no Withdraw type).
- `src/fleet/hedgedjlp.rs`    — AssignHedgedJlp / WithdrawHedgedJlp / ReportHedgedJlp / ReportHedgedJlpWithdraw.
- `src/fleet/riskwatcher.rs`  — `EscalateRisk { severity, kind, subject, measurement, raised_at_unix }`.

`MsgType` (src/message.rs lines 22-81). Allocated values:

```
Infrastructure 0x01..=0x05  (Advertise=0x01, Discover=0x02, Beacon=0x03,
                              Feedback=0x04, MarketSignal=0x05)
Collaboration  0x10..=0x18  (Assign, Ack, Clarify, Report, Approve,
                              TaskCancel, Escalate, Sync, Withdraw=0x18)
Negotiation    0x20..=0x26
```

**Free wire values in the Collaboration nibble: `0x19`, `0x1A`, `0x1B`,
`0x1C`, `0x1D`, `0x1E`, `0x1F`.** We will claim `0x19 = EmergencyWithdraw`.

Routing is **bilateral**: dispatchers in each daemon match on `env.msg_type`
and decode `env.payload` via `ciborium`. The dashboard server's
`tools/fleet-dashboard-server/src/ingest/envelope_decoder.rs` does the same
to *display* events but never to inject them.

### 1.2 Per-daemon dispatchers + withdraw paths

| Daemon          | Has Withdraw type? | Has full-close path? | File holding it |
|-----------------|--------------------|----------------------|-----------------|
| stable-yield    | yes                | yes (`u64::MAX` sentinel) | `crates/stable-yield-daemon/src/lend.rs` `run_withdraw_or_simulate` |
| hedgedjlp       | yes                | yes (`u64::MAX` sentinel) | `crates/hedgedjlp-daemon/src/unwind.rs` `run_or_simulate` |
| multiply        | **NO**             | **NO**                    | n/a — would need to be written |

Specific line numbers (all paths absolute, all in the fleet repo):

- `crates/stable-yield-daemon/src/dispatch.rs:91-156`  — `run(...)` inbox loop.
- `crates/stable-yield-daemon/src/dispatch.rs:372-416` — `handle_withdraw(...)`.
- `crates/stable-yield-daemon/src/lend.rs:409-533`     — `run_withdraw_or_simulate(...)`. Already accepts `u64::MAX = withdraw-all`.
- `crates/stable-yield-daemon/src/lend.rs:540-594`     — `build_withdraw_ixns(...)`.
- `crates/stable-yield-daemon/src/caps.rs:49-56`       — `validate_withdraw(...)`.
- `crates/stable-yield-daemon/src/kamino.rs:29-42`     — `whitelist_program_ids()` (5 program ids).

- `crates/hedgedjlp-daemon/src/dispatch.rs:117-187`    — `run(...)`.
- `crates/hedgedjlp-daemon/src/dispatch.rs:358-402`    — `handle_withdraw(...)`.
- `crates/hedgedjlp-daemon/src/unwind.rs:99-338`       — `run_or_simulate(...)`. Closes perp shorts + Jupiter-swap JLP→USDC. `u64::MAX` semantics already handled by `compute_jlp_to_burn` at line 343.
- `crates/hedgedjlp-daemon/src/caps.rs:82-...`         — `validate_withdraw(...)`.
- `crates/hedgedjlp-daemon/src/whitelist.rs:34`        — `whitelist_program_ids()`.

- `crates/multiply-daemon/src/dispatch.rs:161-324`     — `run(...)`. Only handles Assign/Approve/Escalate/Beacon. **No Withdraw arm.**
- `crates/multiply-daemon/src/leverage.rs`             — only the `lever-up` direction. No `lever-down` / repay / unwind code exists.
- `crates/multiply-daemon/src/caps.rs:19-46`           — caps. **No `validate_withdraw`.**
- `crates/multiply-daemon/src/kamino.rs:63`            — `whitelist_program_ids()`.

Implication: stable-yield and hedgedjlp can short-circuit to their existing
full-close paths; **multiply needs an entirely new unwind path written from
scratch** OR we explicitly scope multiply's "emergency withdraw" as a *halt
on new leverage* + *let riskwatcher / manual ops drain it later*. See §9.

### 1.3 Compile-time authority isolation

Each daemon's wallet is gated by `SigningWhitelist` (constructed once at
boot from a static program-id list):

- `crates/zerox1-defi-wallet/src/lib.rs:42-...` — `SigningWhitelist` impl + `verify_ixns()` boundary.
- Wired into every signing call:
  - stable-yield: `crates/stable-yield-daemon/src/lend.rs:110-112`, `:443-445`.
  - hedgedjlp:   `crates/hedgedjlp-daemon/src/unwind.rs:172`, plus jlp_hedge.rs.
  - multiply:    `crates/multiply-daemon/src/leverage.rs` (every iteration).

Current whitelists:

- stable-yield (`kamino.rs:29-42`): Kamino Lend, SPL Token, ATA, System, Compute Budget. Five programs.
- hedgedjlp:    Jupiter Perps, SPL Token, ATA, System, Compute Budget. Five programs.
- multiply:     Kamino Lend, SPL Stake Pool (Jito), SPL Token, ATA, System, Compute Budget.

**Sweep authority impact** — the USDC sweep is a single
`spl_token::transfer_checked` ixn from the daemon's USDC ATA. That program
id (`TOKEN_PROGRAM_ID`) is **already in every daemon's whitelist**. The SOL
sweep is a `system_program::transfer` ixn — `SYSTEM_PROGRAM_ID` is also
**already in every daemon's whitelist**. **No `caps.rs` / `kamino.rs` /
`whitelist.rs` program-id additions are required** for the sweep itself.

That said, we will add a new compile-time guard — a destination-pubkey
allow-list check inside the daemon's emergency-withdraw handler that
compares the incoming envelope's destination against the
CLI-flag-configured one and refuses if mismatched. That guard lives in
the dispatcher, not in `SigningWhitelist`, because `SigningWhitelist` is
program-id-keyed not account-keyed.

### 1.4 Dashboard server

- `tools/fleet-dashboard-server/Cargo.toml:34` — currently depends on `zerox1-defi-protocols` only. **No** `zerox1-protocol`, **no** `zerox1-node-enterprise`. The server cannot build or send envelopes today.
- `tools/fleet-dashboard-server/src/main.rs:57-137` — `main()` wires log_tailer + pnl_jsonl + apr_sampler + axum API. No mesh node, no signing key, no socket-out.
- `tools/fleet-dashboard-server/src/api/mod.rs:30-46` — Axum `router()`. CORS is set to GET+OPTIONS only — adding `POST` requires changing line 37.
- `tools/fleet-dashboard-server/src/api/state.rs:28-39` — `/aum`, `/pnl`, `/paper`, `/positions`, `/daemons`, `/wallet`, `/rates`, `/strategies`, `/apr/history` — all `get(...)`.
- `tools/fleet-dashboard-server/src/api/events.rs:41-46` — `/events`, `/events/activity`, `/events/live` (WS), `/onchain/activity`.

No existing POST endpoint. No existing envelope-broadcast primitive.

### 1.5 fleet-pm-stub

- `tools/fleet-pm-stub/src/main.rs:529-665` — `build_envelope_from_cmd(cmd) -> (MsgType, conv_id, payload_bytes, label)`. This is the existing envelope builder, with subcommands per Assign / Withdraw / Approve.
- `tools/fleet-pm-stub/src/main.rs:202-...` — `build_node_config(...)` spins up an embedded `NodeService` with the orchestrator role key.
- `tools/fleet-pm-stub/src/main.rs:667-...` — `main()` boots the node, sends one BEACON, waits for the recipient peer, then dispatches the envelope.

fleet-pm-stub already does **everything** the dashboard would need — boot a
libp2p node with the orchestrator role key, build an envelope, send it
bilaterally, wait for a Report. The cleanest implementation reuses this
machinery via a new subcommand (e.g. `emergency-withdraw-all`) and the
dashboard's `/emergency/withdraw-all` POST handler **shells out** to that
subprocess. The alternative (extracting the envelope builder + node-spawn
code into a library) is more correct long-term but much heavier work.

### 1.6 install-hedgents.sh

- `deploy/install-hedgents.sh:143-176` — derives pubkeys from per-role keypair files, plus the Solana wallet pubkey via inline python.
- `deploy/install-hedgents.sh:178-221` — writes `/etc/hedgents/hedgents.env` (idempotent: preserves `RPC_URL`, overwrites derived pubkey lines). Pattern for adding `EMERGENCY_WITHDRAW_DESTINATION` is to mirror the `SOLANA_WALLET_PUBKEY` append-if-missing block at lines 217-219.

### 1.7 Systemd live units

All three live units follow the same shape:

- `deploy/systemd/hedgents-multiply-live.service`
- `deploy/systemd/hedgents-stable-yield-live.service`
- `deploy/systemd/hedgents-hedgedjlp-live.service`

`EnvironmentFile=/etc/hedgents/hedgents.env` is already loaded; adding
`--emergency-destination=${EMERGENCY_WITHDRAW_DESTINATION}` to each
`ExecStart=` is a one-line change per file.

### 1.8 Frontend

- Cloned read-only to `/tmp/hf-explore/frontend/`.
- `app/page.tsx:8-29` — single page, top-down: `<NumbersPanel />`, `<StrategyCardsRow />`, `<BenchmarkComparisonBar />`, `<MeshFeed />`, `<OnchainActivityRail />`, `<AprHistoryChart />`.
- `components/NumbersPanel.tsx:124-215` — `NumbersPanel` already shows wallet pubkey + balances. The emergency button lives most naturally as a 5th card in this 4-up grid (`grid-cols-1 md:grid-cols-4`), or — better — as a slim red strip *above* the panel since "Emergency" should not look like routine telemetry.
- `lib/api.ts:1-237` — only `fetch*` helpers (GET) + `openEventStream` (WS). No POST. CORS on the server only allows GET+OPTIONS.
- No existing hold-to-confirm UX in the codebase; would be a green-field component.

---

## Section 2 — Protocol changes

### 2.1 New message type

File: `../p2p_architecture/crates/zerox1-protocol/src/message.rs`

Add to the enum (line 62, after `Withdraw = 0x18`):

```rust
    /// Operator one-click emergency: each daemon fully unwinds its
    /// position and sweeps liquid USDC + SOL to a pre-configured
    /// destination. Bilateral, sender = orchestrator, recipient = daemon.
    EmergencyWithdraw = 0x19,
```

Add the corresponding `from_u16` arm at line 99-100:

```rust
    0x19 => Ok(Self::EmergencyWithdraw),
```

And the `Display` arm at line 162:

```rust
    Self::EmergencyWithdraw => "EMERGENCY_WITHDRAW",
```

### 2.2 Shared payload type

File: `../p2p_architecture/crates/zerox1-protocol/src/fleet/mod.rs`

Add a new submodule line:

```rust
pub mod emergency;
```

Create `../p2p_architecture/crates/zerox1-protocol/src/fleet/emergency.rs`:

```rust
//! EmergencyWithdraw — operator-triggered global unwind.
//!
//! Sent by the orchestrator (via dashboard click) to each execution
//! daemon. The recipient unwinds its strategy fully then sweeps any
//! remaining liquid USDC and SOL to `destination`.
//!
//! Destination is enforced at the daemon side: the daemon refuses any
//! envelope whose `destination` doesn't match its own CLI-flag-configured
//! pubkey. The envelope carries it only so the orchestrator can prove
//! intent and the daemon can detect a config drift between dashboard
//! and daemon.

use super::ReportHeader;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmergencyWithdraw {
    /// 32-byte Solana pubkey of the sweep destination. Daemon enforces
    /// match against `--emergency-destination` CLI flag and rejects on
    /// mismatch (error_code = 7).
    pub destination: [u8; 32],
    /// 0 = no deadline. Otherwise UNIX-seconds — daemon refuses if
    /// `now > deadline_unix`.
    pub deadline_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportEmergencyWithdraw {
    pub header: ReportHeader,
    /// Strategy-specific full-close result. None on `ok=false`.
    pub unwind_summary: Option<UnwindSummary>,
    /// USDC swept (lamports, 6 decimals). 0 on dust-only / nothing to sweep.
    pub usdc_swept_lamports: u64,
    /// SOL swept (lamports). 0 if balance below the rent-reserve threshold.
    pub sol_swept_lamports: u64,
    /// All tx signatures from this run, in order: unwind ixns then sweep ixns.
    pub tx_signatures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnwindSummary {
    /// Echoed strategy label, "stable_yield" | "multiply" | "hedgedjlp".
    /// Plain ASCII per the no-strings invariant exception clause in
    /// `fleet/mod.rs` (this is a self-tag for routing, not a free-form
    /// string).
    pub strategy: String,
    /// Best-effort post-unwind on-chain receivable in USDC base units.
    /// For stable_yield: deposited USDC reclaimed. For hedgedjlp: USDC
    /// returned from JLP redeem. For multiply: liquid USDC after the
    /// full-close sequence completes (may be 0 if multiply has no
    /// unwind path implemented — see Section 9 risk #1).
    pub usdc_reclaimed_lamports: u64,
}
```

Round-trip CBOR tests follow the pattern in `stable_lend.rs:60-136` —
include one test per struct.

### 2.3 No collisions

Run `grep -n "0x19" ../p2p_architecture/crates/zerox1-protocol/src/` first.
None found in the current tree.

---

## Section 3 — Per-daemon changes

For each daemon the shape is identical, only the unwind body differs.

### 3.1 stable-yield-daemon (easiest — 4 commits' worth)

(a) **What exists**
- `lend.rs::run_withdraw_or_simulate(ctx, payload, conv)` already does the full klend withdraw (`u64::MAX` sentinel). Builds 3-ixn bundle (ATA-create + refresh_reserve + withdraw_obligation_collateral_and_redeem_reserve_collateral). Already whitelist-verified.
- `dispatch.rs::run(...)` already matches on `MsgType::Withdraw` and `MsgType::Approve`.
- `caps.rs::validate_withdraw(...)` exists and accepts `u64::MAX`.

(b) **What gets added**
- New CLI flag in `main.rs` Args: `--emergency-destination <base58>` (line ~107, mirror `--orchestrator-agent-id`). Optional; required iff `--network=mainnet` (mirror the C1 main.rs ack pattern at line 187-198). Parse to `Option<[u8;32]>`.
- New field on `DispatchCtx` in `dispatch.rs:22-48`: `pub emergency_destination: Option<[u8;32]>`.
- Wire it through `main.rs:256-262`.
- New dispatch arm in `dispatch.rs::run(...)` at `MsgType::EmergencyWithdraw` (insert at line 145, before Beacon):

  ```rust
  MsgType::EmergencyWithdraw => {
      let conv = env.conversation_id;
      let recipient = env.sender;
      if !sender_is_authorised(ctx.orchestrator_agent_id, env.sender, "EmergencyWithdraw") {
          continue;
      }
      match handle_emergency(&handle, &ctx, &env).await {
          Ok(report) => { let _ = send_report_emergency(&handle, &ctx, recipient, conv, report).await; }
          Err(e) => { warn!(?e, ?conv, "emergency-withdraw failed"); /* error report with code 1 */ }
      }
  }
  ```

- New `handle_emergency(handle, ctx, env)` function that:
  1. CBOR-decodes `EmergencyWithdraw`.
  2. Validates `ctx.emergency_destination == Some(payload.destination)` (code 7 on mismatch).
  3. Bypasses approval queue. **Emergency is intentionally not approval-gated** — the hold-to-confirm UX in the frontend is the human gate, queueing it for orchestrator Approve would defeat the purpose. This is a deliberate divergence from `WithdrawStableLend`.
  4. Calls `crate::lend::run_withdraw_or_simulate(ctx, &fake_withdraw_with_u64_max, conv)` to drain the obligation.
  5. After the withdraw succeeds (or even if it errors — sweep what you can), call a new `crate::sweep::sweep_usdc_and_sol(ctx, destination)`.

- New file: `crates/stable-yield-daemon/src/sweep.rs` — pure utility, builds and submits `spl_token::transfer_checked` + `system_program::transfer` for whatever's currently liquid. Reads ATA + native SOL balance via `ctx.rpc.client.get_account(...)`. Leaves a configurable SOL rent-reserve (suggest 0.01 SOL constant).
- Add `pub mod sweep;` to `lib.rs`.
- New `send_report_emergency(...)` mirrored on `send_report_withdraw(...)`.

(c) **What gets modified**
- `dispatch.rs::payload_is_for_this_daemon(...)` at line 77-87 — add `MsgType::EmergencyWithdraw => ciborium::de::from_reader::<EmergencyWithdraw, _>(...)`.
- `caps.rs` — add `validate_emergency_withdraw(payload) -> Result<()>` (no real caps; just sanity-check `destination != [0u8;32]`).

Approx LOC delta for stable-yield: **~250 LOC + tests**.

### 3.2 hedgedjlp-daemon

(a) **What exists**
- `unwind.rs::run_or_simulate(ctx, state, payload, conv)` does the full unwind: closes every tracked perp short + Jupiter-swap JLP→USDC + clears `RebalanceState`. Already handles `u64::MAX`.
- `dispatch.rs::run(...)` already routes `MsgType::Withdraw`.

(b) **What gets added** (same shape as 3.1)
- `--emergency-destination` CLI flag, same logic.
- `DispatchCtx::emergency_destination`.
- New dispatch arm at `MsgType::EmergencyWithdraw` (insert at line 181 before Beacon).
- New `handle_emergency(...)` calling `unwind::run_or_simulate(ctx, &ctx.state, &WithdrawHedgedJlp { jlp_lamports: u64::MAX, deadline_unix: 0 }, conv)` then `sweep::sweep_usdc_and_sol(...)`.
- New `sweep.rs` (identical shape to stable-yield's; could be a shared utility crate later — see §9).

(c) **What gets modified**
- `dispatch.rs::payload_is_for_this_daemon(...)` at line 101-113 — add `EmergencyWithdraw` decode arm.
- `caps.rs` — `validate_emergency_withdraw`.

**Closest existing function: `crates/hedgedjlp-daemon/src/unwind.rs:99 run_or_simulate`.** Emergency withdraw is literally this function with destination passed through, then sweep.

Approx LOC delta: **~280 LOC + tests** (the sweep file is shared by copy with stable-yield).

### 3.3 multiply-daemon (hardest — see §9 risk)

(a) **What exists**
- `leverage.rs::run_or_simulate(...)` (lever-UP only).
- No Withdraw type, no unwind code anywhere.
- riskwatcher M7 soft-veto (`dispatch.rs:181-194`) can pause new Assigns but does not unwind.

(b) **What gets added**
- All of (3.1)'s CLI + DispatchCtx + dispatch arm boilerplate.
- A **brand-new** `crates/multiply-daemon/src/unwind.rs` that does the inverse of `leverage.rs`:
  - read current obligation state (`kamino_loader::fetch_obligation`)
  - for each round (cap at `caps::MAX_LEVERAGE_LOOP_ROUNDS`):
    1. `withdraw_obligation_collateral_and_redeem_reserve_collateral` jitoSOL
    2. `spl_stake_pool::withdraw_sol` or equivalent — convert jitoSOL → wSOL → SOL
    3. `kamino::repay_obligation_liquidity` SOL borrow
    4. repeat until borrow=0
  - final `withdraw_obligation_collateral_and_redeem_reserve_collateral` for the remaining USDC/jitoSOL
- This is non-trivial — the lever-down code does not exist in the monolith either (cross-checked by grep). Treat this as the work item that may force a scope cut.

(c) **What gets modified**
- `caps.rs` — add `validate_emergency_withdraw`.
- `kamino.rs::whitelist_program_ids()` — already includes everything we need (Kamino, Jito stake pool, SPL token, ATA, system, compute budget). No additions.

**Closest existing function: NONE.** This is the dominant risk; see §9.

Approx LOC delta: **~600 LOC + tests** (the unwind body is the bulk).

### 3.4 multiply scope alternative (recommended for v1)

Scope cut: instead of implementing a full multiply lever-down, the emergency
handler in multiply-daemon could:

1. immediately set the riskwatcher veto pause (`paused_until_unix`) to UTC max → blocks all new Assigns.
2. emit a `MsgType::Escalate` with `RiskSeverity::Critical` and a new `RiskKind::EmergencyHalt` (additive to `riskwatcher.rs` enum, also a new wire constant).
3. sweep only the **liquid** USDC + SOL sitting in the wallet (anything not currently inside a Kamino obligation).
4. return a Report with `ok=true` but `unwind_summary.usdc_reclaimed_lamports = 0` and a free-text-free `error_code = 8 = "halted; positions remain on-chain, drain via manual runbook"` (encoded as a numeric code per the no-strings rule).

This is **far** less risky than writing untested lever-down code under
emergency-pressure assumptions. Document the manual drain procedure in
the operator runbook.

The plan below assumes the **scope-cut variant** for multiply. If the
full lever-down is required, add roughly 4 more commits and a week of
test work.

---

## Section 4 — Dashboard changes

### 4.1 New endpoints

File: `tools/fleet-dashboard-server/src/api/emergency.rs` (new). Routes:

- `GET  /emergency/preview`     — returns `{ destination, daemons: [...], ready: bool, env_loaded: bool }`. No side effects.
- `POST /emergency/withdraw-all`  — body `{ confirm_destination: "Base58..." }`. Server compares `confirm_destination` to env-loaded `EMERGENCY_WITHDRAW_DESTINATION` and refuses (400) on mismatch. On match, it shells out (or library-calls) `fleet-pm-stub emergency-withdraw-all --recipient-agent-id=<each-daemon>` once per daemon, in parallel, and returns 200 with `{ broadcast_count, started_at_ms, status_endpoint: "/emergency/status/<run_id>" }`.
- `GET  /emergency/status/<run_id>` — polls the SQLite mesh_events table for matching Reports (msg_type = `REPORT`, conv_id = the one we used per daemon).

Wire-up: `tools/fleet-dashboard-server/src/api/mod.rs:42-44` — add `.merge(emergency::router())`. **CORS at line 36-39 must add `Method::POST`.**

### 4.2 Mesh broadcast: subprocess vs library

**Recommendation: subprocess `fleet-pm-stub` for v1.**

Rationale:
- `fleet-pm-stub` already has the full envelope-build + libp2p-boot + bilateral-send + Report-await loop (see §1.5). Reimplementing it inside the dashboard server doubles the surface area and would require pulling `zerox1-node-enterprise` (a heavy dependency) into the dashboard binary.
- Operator-friendly: the same command can be reproduced from a shell when the dashboard is down.
- Audit-friendly: every emergency-withdraw run leaves a `fleet-pm-stub` log line in `journalctl`.

Subprocess invocation pattern (server pseudo-code, NOT to be written yet):

```rust
let secrets = std::env::var("HEDGENTS_SECRETS_DIR")
    .unwrap_or_else(|_| "/var/lib/hedgents/secrets".into());
let mut handles = Vec::new();
for (role, recipient_hex) in &[
    ("stable-yield", env::var("STABLE_YIELD_PUBKEY")?),
    ("multiply",     env::var("MULTIPLY_PUBKEY")?),
    ("hedgedjlp",    env::var("HEDGEDJLP_PUBKEY")?),
] {
    let child = tokio::process::Command::new("/opt/hedgents/bin/fleet-pm-stub")
        .args([
            "--secrets-dir", &secrets,
            "--recipient-agent-id", recipient_hex,
            "emergency-withdraw-all",
            "--destination", &env::var("EMERGENCY_WITHDRAW_DESTINATION")?,
        ])
        .spawn()?;
    handles.push((role, child));
}
```

A new `fleet-pm-stub` subcommand `EmergencyWithdrawAll { destination: String }`
mirrors the existing `WithdrawStableLend` subcommand at lines 147-162. The
envelope-build branch in `build_envelope_from_cmd` at lines 529-665 adds an
`EmergencyWithdrawAll` arm using the new `MsgType::EmergencyWithdraw` +
CBOR-encoded `EmergencyWithdraw { destination, deadline_unix }`. **The
recipient_agent_id flag handles per-daemon targeting**; the dashboard runs
three subprocess invocations, one per daemon.

### 4.3 Env-var validation

In `tools/fleet-dashboard-server/src/main.rs::main()` at line 66 (just after
`Args::parse()`):

```rust
let live_units_enabled = check_live_units_enabled().unwrap_or(false);
let emergency_destination = std::env::var("EMERGENCY_WITHDRAW_DESTINATION").ok();
if live_units_enabled && emergency_destination.is_none() {
    bail!("EMERGENCY_WITHDRAW_DESTINATION must be set when any \
           hedgents-*-live.service unit is enabled. Edit /etc/hedgents/hedgents.env.");
}
```

`check_live_units_enabled` shells out to `systemctl is-enabled hedgents-multiply-live` etc. and returns true if any returns `enabled`. Falls back to false on `systemctl` missing (dev workstation).

The `emergency_destination` string gets validated as base58 → 32 bytes at boot. Stored in `AppState` so the GET /emergency/preview handler can echo it.

---

## Section 5 — Frontend changes

### 5.1 Insertion point

`app/page.tsx:8-29` — insert a new `<EmergencyBanner />` *immediately
after* `<NumbersPanel />` (line 15) so it's visually attached to the AUM
display but separated from telemetry by a hairline.

### 5.2 Component file

New: `components/EmergencyBanner.tsx`. Two visual states:

- **Calm** (default): a thin red bar with a "Liquidate all positions" link → opens an inline drawer.
- **Drawer open**: full-width red card showing:
  - The destination address (read from `GET /emergency/preview` — `formatPubkey()` truncate + Solscan link).
  - Per-daemon status pills (Idle / Pending / Confirmed).
  - A `<HoldToConfirmButton seconds={5} />` (new sub-component) — text on it: "Hold to liquidate everything".

`components/HoldToConfirmButton.tsx`. Uses `requestAnimationFrame` to drive
a 0→100% fill. Releases before 5s → resets with no-op. Holds full 5s →
calls `onConfirm()`. **No existing hold-to-confirm component in the
codebase** — would be new (~50 LOC).

### 5.3 API types

Add to `lib/api.ts`:

```ts
export interface EmergencyPreview {
  destination: string;          // base58
  env_loaded: boolean;
  daemons: { role: string; reachable: boolean; recipient_agent_id: string }[];
  ready: boolean;
}
export interface EmergencyRunStarted {
  run_id: string;
  broadcast_count: number;
  started_at_ms: number;
  status_endpoint: string;
}
export interface EmergencyStatus {
  run_id: string;
  done: boolean;
  per_daemon: {
    role: string;
    state: "pending" | "confirmed" | "failed" | "timeout";
    tx_signatures: string[];
    usdc_swept: number;
    sol_swept: number;
  }[];
}

export async function fetchEmergencyPreview(): Promise<EmergencyPreview> {
  const r = await fetch(`${API_BASE}/emergency/preview`);
  if (!r.ok) throw new Error(`fetchEmergencyPreview ${r.status}`);
  return r.json();
}
export async function startEmergencyWithdraw(confirm_destination: string): Promise<EmergencyRunStarted> {
  const r = await fetch(`${API_BASE}/emergency/withdraw-all`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ confirm_destination }),
  });
  if (!r.ok) throw new Error(`startEmergencyWithdraw ${r.status}`);
  return r.json();
}
export async function fetchEmergencyStatus(run_id: string): Promise<EmergencyStatus> {
  const r = await fetch(`${API_BASE}/emergency/status/${run_id}`);
  if (!r.ok) throw new Error(`fetchEmergencyStatus ${r.status}`);
  return r.json();
}
```

### 5.4 Hold-to-confirm semantics

- On mousedown / touchstart: start RAF loop, render fill.
- On mouseup before 100%: reset.
- On reach 100%: call confirm callback once, swap UI to "Broadcasting…" state.
- Keyboard: disabled by design — emergency action should not be triggerable by a stuck-down spacebar.

---

## Section 6 — Install + systemd changes

### 6.1 `install-hedgents.sh` diff (block at lines 178-221)

Add after the `SOLANA_WALLET_PUBKEY=...` block:

```bash
# v0.3 — emergency-withdraw destination. Prompted if missing on
# first run; preserved on upgrade. Address must be base58 32-byte.
if ! grep -q '^EMERGENCY_WITHDRAW_DESTINATION=' "$ENVFILE"; then
    read -r -p "Emergency-withdraw destination (Solana pubkey, base58): " EWD < /dev/tty || true
    if [[ -z "${EWD:-}" ]]; then
        warn "EMERGENCY_WITHDRAW_DESTINATION left unset — live units will refuse to start until you edit $ENVFILE"
        echo "EMERGENCY_WITHDRAW_DESTINATION=" >> "$ENVFILE"
    else
        # crude base58 length sanity (32 bytes → 43–44 chars)
        if [[ ${#EWD} -lt 32 || ${#EWD} -gt 44 ]]; then
            bail "destination address looks wrong length (${#EWD} chars); aborting"
        fi
        echo "EMERGENCY_WITHDRAW_DESTINATION=${EWD}" >> "$ENVFILE"
    fi
fi
```

### 6.2 Systemd unit diffs (one line each)

For each of:
- `deploy/systemd/hedgents-stable-yield-live.service`
- `deploy/systemd/hedgents-multiply-live.service`
- `deploy/systemd/hedgents-hedgedjlp-live.service`

Add a continuation line in `ExecStart=`:

```
    --emergency-destination=${EMERGENCY_WITHDRAW_DESTINATION} \
```

Add the same line to `deploy/systemd/hedgents-dashboard.service` so the
dashboard server picks it up via env (read by `main.rs`, not flag-parsed
on the dashboard side).

### 6.3 Migration for already-deployed v0.2.9

Operators upgrading from v0.2.9 will hit the `grep -q` branch and be
prompted by `install-hedgents.sh`. On non-interactive upgrades (CI / Ansible)
the script leaves the value blank and warns; the live units then refuse to
start (their daemon binaries `bail!` at boot when `--network=mainnet` is set
without `--emergency-destination`). This is intentional — same shape as the
existing `--orchestrator-agent-id` ack at stable-yield-daemon/src/main.rs:187.

---

## Section 7 — Test coverage plan

### Per-commit tests

- **Commit 1 (protocol)**: CBOR round-trip tests for `EmergencyWithdraw`,
  `ReportEmergencyWithdraw`, `UnwindSummary`. `MsgType::from_u16(0x19)`
  round-trip. `Display` output check.
- **Commit 2 (stable-yield)**: unit tests for `caps::validate_emergency_withdraw`,
  destination-mismatch rejection, payload-filter test (mirror
  `payload_filter_tests` block at dispatch.rs:520).
- **Commit 3 (hedgedjlp)**: same shape, plus sweep-balance computation tests.
- **Commit 4 (multiply, scope-cut variant)**: pause-set-and-Escalate tests.
- **Commit 5 (fleet-pm-stub)**: `build_envelope_from_cmd` arm test for `EmergencyWithdrawAll`.
- **Commit 6 (dashboard)**: handler-level tests for `/emergency/preview` and the destination-mismatch 400 path. Use the existing `tower`/`http-body-util` dev-deps at `Cargo.toml:38-40`.
- **Commit 7 (install + systemd)**: only `bash -n` syntax check + idempotency check (run install twice, diff `/etc/hedgents/hedgents.env`).
- **Commit 8 (frontend)**: jest/vitest snapshot of `<EmergencyBanner />`. HoldToConfirm RAF test using `vi.useFakeTimers()`.

### Mock daemon integration test

Add `tools/fleet-dashboard-server/tests/emergency_roundtrip.rs`. Spawns:
- a fake daemon (libp2p node bound to ephemeral port, just decodes the inbound `EmergencyWithdraw` and echoes a fake `ReportEmergencyWithdraw` with `ok=true`),
- the dashboard server pointed at it via env,
- exercises the full POST → fleet-pm-stub → Report → SQLite write → GET /emergency/status flow.

A simpler alternative: just unit-test the envelope builder in `fleet-pm-stub`
and trust the existing devnet smoke tests for the bilateral send path.

### What CANNOT be tested without a live broadcast

- **Real klend `withdraw_obligation_collateral_and_redeem_reserve_collateral` on the deployed reserve.** On devnet our reserve is a placeholder that always sim-fails; mainnet is the only place this resolves to a real outcome.
- **Jupiter Perps close-request execution by the keeper.** Same caveat as the existing `unwind.rs` comment (sim-only Jupiter Perps work is mainnet-only).
- **Real SOL transfer effects on rent-exemption** of the source wallet. Mock testing covers the ixn-build correctness; only mainnet shows whether the rent-reserve constant is sufficient.

Explicitly mark these in the runbook as "must be verified on a $50 tiny
position during commit 9 (mainnet smoke)".

---

## Section 8 — Commit sequence

| # | Commit | Surface | Why safe checkpoint |
|---|--------|---------|---------------------|
| 1 | Protocol: add MsgType::EmergencyWithdraw + EmergencyWithdraw struct + tests | `../p2p_architecture/.../message.rs`, `.../fleet/mod.rs`, `.../fleet/emergency.rs` | Pure additive. All existing code paths ignore unknown msg_types. Nothing depends on it yet. |
| 2 | stable-yield: dispatch arm + handler + sweep.rs + caps validate + CLI flag + tests | `crates/stable-yield-daemon/{dispatch,caps,sweep,lib,main}.rs` | Without commit 4, no orchestrator sends the envelope — so the new path is unreachable in production. CI tests cover it. |
| 3 | hedgedjlp: same shape as commit 2 | `crates/hedgedjlp-daemon/{dispatch,caps,sweep,lib,main}.rs` | Same logic — additive, gated on a CLI flag, unreachable until the orchestrator sends. |
| 4 | multiply: scope-cut handler (pause + Escalate + sweep liquid only) | `crates/multiply-daemon/{dispatch,caps,sweep,lib,main}.rs` | Same. |
| 5 | fleet-pm-stub: EmergencyWithdrawAll subcommand | `tools/fleet-pm-stub/src/main.rs` | Operator can now manually fire from CLI. Same audit shape as the existing WithdrawStableLend subcommand. |
| 6 | Dashboard backend: /emergency/preview + /emergency/withdraw-all + /emergency/status, env-var validation, AppState | `tools/fleet-dashboard-server/src/api/{mod,emergency}.rs`, `src/main.rs` | HTTP-only. Dashboard server itself still signs nothing. Requires commit 5's subprocess CLI to be installed. |
| 7 | Install + systemd: env-var prompt + `--emergency-destination` flag on three live units | `deploy/install-hedgents.sh`, `deploy/systemd/hedgents-*-live.service`, `deploy/systemd/hedgents-dashboard.service` | Operators upgrading from v0.2.9 get prompted; CI installs leave the value blank and live units refuse to start (safe default). |
| 8 | Frontend: EmergencyBanner + HoldToConfirmButton + api.ts types | `components/EmergencyBanner.tsx`, `components/HoldToConfirmButton.tsx`, `lib/api.ts`, `app/page.tsx` | Hidden behind the env-loaded destination check on the backend. If the backend returns `env_loaded: false`, the banner renders a "configuration required" notice instead of the button. |
| 9 | Mainnet $50 smoke + runbook | `docs/runbooks/emergency-withdraw-smoke.md` | Verifies the real klend withdraw, the real Jupiter-perps close-request, the real Jupiter swap, and the real rent-reserve constant. |

Between commits 1-5 nothing is reachable from the dashboard. Between
6-7 nothing reaches the daemons until the operator restarts live units
with the new flag. Between 7-8 nothing reaches the user until they
update the frontend.

---

## Section 9 — Open questions / risks

### 9.1 Top risks

1. **Multiply has no lever-down path.** A full unwind for a leveraged Kamino jitoSOL→SOL→jitoSOL position is several hundred LOC of unaudited new code. *Mitigation: ship the scope-cut variant (§3.4) and document a manual drain runbook. Reassess after v0.3.0 has been live a month.*
2. **u64::MAX semantics on `WithdrawHedgedJlp`** assume `RebalanceState` has correctly tracked the active position. Audit-fix C2 (read at `hedgedjlp-daemon/src/unwind.rs:131-154`) already covers the empty-state case as a zero-Report. Emergency Withdraw inherits this gracefully — if there's nothing tracked, the sweep step still drains any liquid USDC sitting in the wallet from earlier rebalances. *Verified in unit test `effective_positions_returns_empty_when_no_tracked_open`.*
3. **Race between unwind submit and sweep submit.** Unwind tx may take 1-3 slots to confirm; if sweep runs immediately the daemon's USDC ATA may not yet reflect the unwind receipt. *Mitigation: poll-on-confirmed before sweep, with a 60s timeout. If timeout elapses, sweep what's currently liquid and surface `error_code = 9 = "unwind unconfirmed; partial sweep"` in the report.*

### 9.2 Spec ambiguities to resolve before coding

- **Sweep order: before unwind, after unwind, or both?** The spec implies "fully unwinds … *then* sweeps". Recommended order: 1) Unwind, 2) wait for unwind confirmation, 3) sweep. This document assumes that order. *Question: should the daemon also do a pre-unwind sweep of pre-existing liquid USDC? Argument for: in case the unwind fails, at least the liquid funds escape. Argument against: complicates the audit trail. **Open.***
- **What if unwind fails partway?** stable-yield's full withdraw is one tx and atomic. hedgedjlp's involves N+1 txs (N perp-close-requests + 1 Jupiter swap). If a perp close fails, do we proceed with the Jupiter swap? **Recommendation: yes — partial unwind is better than no unwind. Each tx is best-effort, collected into `tx_signatures`, and the Report's `ok` field is `true` iff the sweep step succeeded at all (with details in `error_code` for partial failures).**
- **Should `EmergencyWithdraw` be approval-queued like normal Withdraw?** This plan assumes **no** — the hold-to-confirm UX is the human gate, queueing for orchestrator Approve would defeat the one-click design. *Sanity check needed with the operator before merging.*
- **Destination address rotation.** If the operator wants to change the destination, they edit `/etc/hedgents/hedgents.env` and restart all three live units + the dashboard. There is no in-band rotation. Document this in the runbook.

### 9.3 Recovery procedure if emergency-withdraw fails partway

1. Operator checks `journalctl -u hedgents-multiply-live -u hedgents-stable-yield-live -u hedgents-hedgedjlp-live` and reads the per-daemon Reports captured in the dashboard's SQLite.
2. For each daemon that returned `ok=false` or `error_code != 0`:
   - Use `fleet-pm-stub WithdrawStableLend` / `WithdrawHedgedJlp` directly to retry the unwind portion.
   - Once unwind succeeds, re-run `fleet-pm-stub emergency-withdraw-all` to retry the sweep (with the unwind step now a fast no-op because position is already 0).
3. For multiply (scope-cut variant), follow the existing M10 mainnet-tiny-position runbook to manually delever. Multiply's emergency-handler will have already paused it, so no new Assigns are in flight.

---

## Out-of-scope / future work

- Sharing `sweep.rs` between the three daemons via a new utility crate (`crates/zerox1-defi-sweep`). Currently each daemon will get its own copy; deduplicate after the second one is written.
- Multisig destination (require 2-of-N operators to confirm). Hold-to-confirm covers the single-operator case; multisig is a v0.4 feature.
- Webhook on completion to PagerDuty / Slack. Out of scope for the dashboard which is intentionally LAN-only.
