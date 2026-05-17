# Hedgents Devlog

Chronological shipping log. Each entry: tagged release → what shipped → why
it mattered. Pair with `ROADMAP.md` (what's next) and `LITEPAPER.md` (what
the product is).

Format: newest first.

---

## v0.4.0 — Orchestrator daemon, execute mode (2026-05-17)

- New crate `crates/orchestrator-daemon` — long-running autonomous
  rebalancer. Polls the dashboard `/strategies` + `/aum`, runs the
  pure `decide()` function, signs + dispatches Assign/Withdraw
  envelopes to the strategy daemons.
- Compile-time isolation extended: the wallet crate is **not** in the
  orchestrator's dep graph; it can sign mesh envelopes but cannot
  sign Solana transactions. Strategy daemons retain that authority.
- **Cooldown registry** — per-strategy lockout between dispatches
  prevents hot-loops. Default 5min; tunable.
- **Stale-snapshot guard** — re-fetches `/aum` between decision and
  emit; rejects actions that exceed the re-fetched idle/deployed by
  more than the configured slack factor (default 10%).
- **Two-stage promotion path**: `v0.4.0-rc1` ran dry-run for 24h+;
  `v0.4.0` enables `--execute` with conservative caps
  (`max_action_fraction=0.10`, `cooldown=300s`) and loosens after a
  7-day clean window.
- `fleet-pm-stub` refactored to lib + bin. `action_to_envelope_spec`
  moved into the library — same code path drives both the CLI's
  one-shot `allocator --execute` and the daemon's continuous tick
  loop. 14 envelope-spec tests cover Deposit/Withdraw across all
  three strategies + the missing-target error path.
- New systemd unit `hedgents-orchestrator.service` + addition to
  `hedgents.target`. Listens on `:19317`.
- Runbooks: `orchestrator-bringup.md` (rc1 dry-run),
  `orchestrator-devnet-smoke.md` (execute on devnet),
  `orchestrator-mainnet.md` (execute on mainnet with 7-day
  promotion path).

## v0.3.3 — klend repay account-list fix (2026-05-16)

- `repay_obligation_liquidity_v2_ix` was missing `lending_market_authority`
  and `farms_program` — klend rejected with `AccountNotEnoughKeys (3005)`.
- Re-derived account list from klend source; bundle now passes simulation.

**Mainnet effect:** multiply unwind round 3 broadcast; position drained from
35% LTV → 2.5% LTV across two rounds (sigs `4Zv1jL…RFwC`, `J2zkqT…RjiV`).
Round 3 hit klend's `NetValueRemainingTooSmall (6092)` dust-floor rule —
known protocol behavior, $0.22 residual; close-obligation path is Phase 1
v0.3.4 candidate.

## v0.3.2 — wSOL wrap in unwind bundle

- Jito `WithdrawSol` returns raw SOL, but klend repay expects wSOL token
  account. Inserted `CreateATA + system_transfer + sync_native` between
  the Jito withdraw and the klend repay.

## v0.3.1 — Jito WithdrawSol as swap leg

- v0.3.0 unwind bailed at runtime: "Iterative strategy selected but Jito
  direct-redeem swap leg not yet wired". Added `withdraw_sol_ix` to the
  Jito client + `StakePoolMeta::jitosol_to_sol_lamports` inverse helper.

## v0.3.0 — WithdrawMultiply protocol + iterative unwind

- New `WithdrawMultiply` mesh message type + `ReportMultiplyWithdraw`.
- Pure round-builder in `unwind.rs` (atomic flash-loan path + iterative
  deleverage path, daemon picks based on position size).
- `klend` v2 ixn builders for `repay_obligation_liquidity` and
  `withdraw_obligation_collateral`.
- `fleet-pm-stub withdraw-multiply` subcommand.
- Approval queue routing in `dispatch.rs`.

## v0.2.9 — systemd live target

- `hedgents-{stable-yield,multiply,hedgedjlp}-live.service` units +
  `hedgents-live.target`.
- `Conflicts=` directive ensures paper and live cannot run simultaneously.
- Live daemons now survive SSH logout (replacing nohup'd manual launches
  that died with the operator's terminal).
- Installer derives `EMERGENCY_WITHDRAW_DESTINATION` and
  `SOLANA_WALLET_PUBKEY` env vars at install time.

## v0.2.8 — combined APR in dashboard

- `/aum` endpoint exposes deployed-USD-weighted average APR across live
  strategies (`combined_apr_bps`) + projected annualised USD.
- Frontend benchmark widgets (`BenchmarkComparisonBar`,
  `YieldBenchmarkCard`) and `NumbersPanel` switched from Kamino-only to
  fleet-combined APR.

## v0.2.7 — riskwatcher leverage-frame fix

- Jupiter Perps liquidation-distance formula was producing 55+
  false-positive Critical escalates per day. Root cause: collateral was
  being divided by `custody.maxLeverage` *before* the leverage frame had
  scaled it. Fixed by reordering the math and correcting the
  `maxLeverage` scale.

## v0.2.5–v0.2.6 — riskwatcher polls Jupiter Perps positions

- New `jupiter_perps_poller`: discovers and classifies short positions
  held by a watched wallet; emits position view into the registry.
- Wired into the riskwatcher tick loop alongside existing Kamino
  obligation polling.

## v0.2.4 — JLP via Jupiter Swap aggregator

- Direct `add_liquidity_2` path on the JLP pool is closed in production.
- Replaced the buy/withdraw legs with Jupiter Swap quote+swap routing.
- Default slippage bumped to 150bps (Jupiter sim-path latency tolerance).

## v0.2.3 — hedgedjlp audit fixes

- Applied all 9 fixes from `hedgedjlp-daemon-audit-2026-05-15.md`.
- Live JLP pool custody wired into the buy leg; daemon now closes the
  full audit.

## v0.2.0–v0.2.2 — regime-aware allocator

- Pure decision function in `crates/fleet-pm-stub/src/allocator/` —
  takes `Snapshot` of strategy APRs, deployed USD, idle USD; returns
  `Deposit | Withdraw | NoAction`.
- Hurdle model: `stable_yield_apr + risk_premium[strategy]`.
- CLI subcommand with `--dry-run` and `--execute` modes; audit log to
  `allocator-actions.jsonl`.
- Spec: `docs/regime-aware-allocator.md`.

## v0.1.x — multiply on klend v2 handlers (the long road)

- Switched multiply from v1 to v2 klend handlers because v2 enforces the
  farm CPI refresh that v1 left as the caller's problem (and that the
  daemon was getting subtly wrong on round 2+).
- Address Lookup Table for Kamino main market
  (`284iwGtA9X9aLy3KsyV8uT2pXLARhYbiSi5SiM2g47M2`) — compressed lever-up
  bundle below the 1232-byte tx limit.
- `RefreshObligation` ordering corrected: must run *before*
  `BorrowObligationLiquidity` and *after* every reserve update.
- Kamino obligation byte-layout fixes: borrow slot is 200 bytes (not
  136), aggregate fields start at offset 2208.
- Result: lever-up rounds land cleanly on mainnet; dashboard reads
  obligation state correctly.

## v0.1.0 — full fleet on devnet → mainnet bring-up

- 5 daemons working end-to-end:
  - `stable-yield` — Kamino USDC supply
  - `multiply` — Kamino leveraged jitoSOL
  - `hedgedjlp` — JLP exposure + Jupiter Perps short hedge
  - `riskwatcher` — independent position poller with veto authority
  - `researcher` — market-signal emitter (lending, funding, peg, JLP yield)
- Approval queue + manual-approval flow per daemon
- libp2p mesh with role-bound Ed25519 keys per daemon
- Demo dashboard: `fleet-dashboard-server` ingesting logs into SQLite
  + REST + WS API; Next.js frontend

---

## Recurring themes

A few patterns that show up across the version history because they're
where the architecture earns its keep:

1. **Compile-time isolation has caught real bugs.** When `multiply` tried
   to construct an instruction whose type lived in the `hedgedjlp` crate,
   the build failed — not the runtime. That's the whole point.

2. **klend v2 handlers are right.** Every time we tried to use v1
   handlers "to save accounts" we eventually had to migrate. The farm
   CPI refresh is non-negotiable for positions with farm appendices.

3. **Riskwatcher catches things the daemons don't see.** The leverage-frame
   bug (v0.2.7) was emitted from the *poller*, not from the
   strategy daemon. Independent observation is a real defence.

4. **Address Lookup Tables are mandatory.** Any bundle with more than
   ~6 protocol accounts will exceed the 1232-byte tx limit without an
   ALT. Build the ALT account-list early.

5. **Dashboard surfaces drive product decisions.** Combined APR
   (v0.2.8) wasn't on anyone's plan — it became obvious once the
   benchmark widget had three rows of T-bill rates and only one row of
   "Kamino" APR. Showing the wrong number forces clarity faster than
   any spec review.
