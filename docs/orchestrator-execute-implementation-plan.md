# Orchestrator execute-mode (v0.4.0) — implementation plan

Target tag: `fleet-v0.4.0`. Prerequisite: `v0.4.0-rc1` orchestrator dry-run
ticking on mainnet for ≥24h with audit-log entries that pass operator
eyeball-validation.

The whole point of this release: the orchestrator gains authority to
emit `Assign*` / `Withdraw*` envelopes that the strategy daemons honour
through the existing approval queue — **no new authority surface inside
the strategy daemons themselves**. Same route the operator CLI uses
today, just driven by the regime-aware allocator on a tick.

## Milestones

### M1 — Extract `action_to_envelope_spec` into the library

`main.rs` currently owns the `action_to_cmd` → `build_envelope_from_cmd`
pipeline. Move both into a single public function in
`fleet_pm_stub::allocator_runner`:

```rust
pub struct EnvelopeSpec {
    pub msg_type: MsgType,
    pub recipient: [u8; 32],
    pub conv_id: [u8; 16],
    pub payload: Vec<u8>,
    pub label: &'static str,
}

pub fn action_to_envelope_spec(
    action: &AllocatorAction,
    targets: &ExecuteTargets,
) -> Result<Option<EnvelopeSpec>>;
```

`Cmd`-enum coupling stays inside `main.rs`. The library function builds
the `AssignStableLend` / `AssignHedgedJlp` / `WithdrawMultiply` etc
payloads directly via `ciborium::ser`. CLI's execute path collapses to
one call to this function.

**Files:**
- `tools/fleet-pm-stub/src/allocator_runner.rs` — add struct + fn + ciborium tests
- `tools/fleet-pm-stub/src/main.rs` — replace `action_to_cmd` + envelope
  build with the library call (regression-tested by the existing
  `action_to_cmd_tests` module ported to the new shape)

### M2 — Cooldown registry

Prevent the orchestrator from hot-looping the same strategy. Per-strategy
in-memory state: `last_action_unix: HashMap<&'static str, u64>`. Reject
any envelope whose `(strategy, action_kind)` last fired within
`--cooldown-secs` (default `300` = 5min). NoAction-style audit line
emitted in the meantime so the audit log still records the suppressed
decision.

**Files:**
- `crates/orchestrator-daemon/src/cooldown.rs` — `CooldownTracker` + tests
- `tick.rs` — consult the tracker before building the envelope

### M3 — `--execute` flag + targets-json wiring

Add to `main.rs` of orchestrator-daemon:
- `--execute` — opt into envelope emission. Defaults `false`.
- `--targets-json <path>` — required when `--execute` is set; refuse to
  boot otherwise.
- `--cooldown-secs` — see M2.
- `--max-actions-per-tick` (default `1`) — even if multiple are
  eligible, only the highest-conviction one fires.

When `--execute=false`: behaves exactly as v0.4.0-rc1 (existing
dry-run). When `--execute=true`: tick loop wires the envelope path.

### M4 — Envelope emission with BEACON + wait_for_peer + retry

Mirror the CLI's send path (lines 1051–1086 in
`tools/fleet-pm-stub/src/main.rs`):
1. `send_one_beacon`
2. `handle.wait_for_peer(recipient, bounded_timeout)` (60s cap)
3. `Envelope::build` with the per-tick nonce from `outbound_nonce`
4. `handle.send(env)` with `Ok("sent")` / `Err(format!("failed:{e}"))`
   strings written into the audit record's `envelope_result`

Crucially: **no Report-wait loop.** Reports flow through the dashboard's
existing ingest. The orchestrator doesn't block on them — that's why the
periodic process keeps ticking even if a single Report is slow.

**Files:**
- `crates/orchestrator-daemon/src/emit.rs` — pure emit fn returning the
  result string + the envelope's nonce/conv for audit
- `tick.rs` — call `emit` when `--execute` and not cooled-down

### M5 — Stale-snapshot guard (race protection)

Between snapshot fetch and envelope land, AUM can drift. Reject any
action whose `amount_usd > current_idle_usd × 1.1` (10% slack) when the
action is a `Deposit`, or `amount_usd > current_deployed_usd × 1.1` for
`Withdraw`. Re-fetch the snapshot immediately before emit; compare. If
the slack is blown, audit-log `skipped:stale_snapshot` and continue.

### M6 — Devnet smoke (sim-only round-trip)

Devnet run with paper-mode daemons + orchestrator-daemon `--execute`:
1. Boot all five paper daemons + dashboard
2. Boot orchestrator with `--execute --targets-json devnet-targets.json --cooldown-secs=10`
3. Manually adjust paper APRs to provoke each AllocatorAction variant:
   - Drop multiply's reported APR below stable_yield + 200bps → expect
     `Withdraw{multiply}` envelope dispatched
   - Bump hedgedjlp's APR above stable_yield + 300bps → expect
     `Deposit{hedgedjlp}` envelope dispatched
4. Verify each strategy daemon's approval queue receives + reports the
   action; verify orchestrator's audit log line has `envelope_result: "sent"`

Captured as `docs/runbooks/orchestrator-devnet-smoke.md`.

### M7 — Mainnet runbook + tag

Bring-up runbook mirrors the rc1 runbook but with the additional steps:
1. Generate + commit `mainnet-targets.json` (operator-specific
   `recipient_agent_id_hex` for each strategy daemon, mainnet
   `market_b58` / `reserve_b58` for stable_yield)
2. Promote rc1 → v0.4.0 by stopping the unit, replacing the binary,
   adding `--execute --targets-json /etc/hedgents/orchestrator-targets.json`
   to the systemd ExecStart, restarting
3. First-action observation window: 1h with operator watching the audit
   log + dashboard
4. Roll-back: revert to rc1 unit (no `--execute`); state is forward-only
   in the audit log

Captured as `docs/runbooks/orchestrator-mainnet.md`.

## Out of scope (defer to v0.4.1+)

- **Riskwatcher escalate subscription.** Strategy daemons already
  honour `EscalateRisk` as a soft-veto (riskwatcher M7 — 5min pause on
  Critical). The orchestrator's envelopes are queued and the strategy
  daemons reject during pause windows. v0.4.1 adds the orchestrator
  *also* tracking escalates so it doesn't waste tick cycles dispatching
  doomed envelopes.
- **Multi-action per tick.** The decision function only emits one
  recommendation per tick. v0.4.x could batch a Withdraw+Deposit pair
  (rebalance) into a single conv_id; today they happen on consecutive
  ticks.
- **Researcher MarketSignal consumption.** The hurdle inputs come from
  the dashboard's `/strategies` (which already reflects researcher's
  rate observations). Direct subscription to `MarketSignal` envelopes
  is a v0.5 intelligence-layer concern.
- **Idle-cap deposits on `multiply`.** Multiply's `AssignMultiply` has
  no USD-sizing field; depositing to multiply requires an out-of-band
  wallet transfer first. CLI logs and skips today; orchestrator does
  the same. Fixing this requires extending the protocol type — out of
  scope.

## Risks + mitigations

| Risk                                                         | Mitigation                                                                                                                       |
| ----                                                         | ----------                                                                                                                       |
| `decide()` recommends Withdraw during a transient APR dip   | Cooldown (M2) bounds blast radius; rc1's 24h soak surfaces flapping behaviour before this milestone ships                     |
| Race between snapshot and emit                              | Stale-snapshot guard (M5) + re-fetch before emit                                                                              |
| Orchestrator and operator both dispatch the same action     | Approval queue de-dups by conv_id; second arrival reports `already_pending` and is logged                                     |
| Riskwatcher Critical pause coincides with orchestrator emit | Strategy daemon rejects envelope; orchestrator records `failed:strategy_paused` in audit and re-tries on next tick after cooldown |
| `targets-json` is wrong (typo'd recipient agent_id)         | Validate at boot — load + check hex length + log the resolved recipients before starting the tick loop                        |
| Operator forgets to provide `--targets-json` with `--execute` | Hard-fail at boot, refuse to start                                                                                              |
