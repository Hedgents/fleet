# Hedgents Devlog

Chronological shipping log. Each entry: tagged release → what shipped → why
it mattered. Pair with `ROADMAP.md` (what's next) and `LITEPAPER.md` (what
the product is).

Format: newest first.

---

## v0.4.0-rc16 — AUM accounting includes hedge collateral (2026-05-20)

Live-deploy of rc15 surfaced a dashboard accounting bug: after the
orchestrator's $119 Assign rebuilt the hedgedjlp position, the operator's
reported AUM dropped from $239 → $184 even though no capital was lost.
The missing $55 went into Jupiter Perps short-position collateral —
real on-chain capital that the dashboard's `/aum` simply wasn't
counting. The wallet ATA correctly read $1 (the safety reserve), but
the orphan in the accounting made it look like the fleet had leaked
~25% of its AUM.

- `/aum.per_strategy.hedgedjlp_collateral_usd` — new field summing
  `collateral_usd_micro` across every open short. Already discovered
  per-position in `chain/jupiter_perps.rs`; rc16 just rolls it up.
- `/aum.total_usdc` now includes hedge collateral. The topline matches
  the operator's true on-chain capital.
- `/strategies` hedgedjlp card exposes `hedge_collateral_usdc` as an
  optional field (omitted for non-hedgedjlp strategies via
  `#[serde(skip_serializing_if = "Option::is_none")]`).
- `deployed_usdc` deliberately keeps reporting JLP value only — the
  daemon's APR claim is calibrated against JLP value, and folding
  collateral into the denominator would understate effective yield
  without a corresponding daemon-side recalibration.
- Frontend: `StrategyCardsRow` now shows `+ $X collateral` under the
  primary position line for hedgedjlp; the live-status badge displays
  the sum (JLP + collateral) so the headline matches committed capital.
  `NumbersPanel.Allocation` adds a "HedgedJLP (collateral)" row that
  appears only when non-zero.

5 new dashboard tests pin the JSON shape (`#[serde(skip)]` omission for
non-hedgedjlp; field present for hedgedjlp; aggregation math). Workspace
test count: 31 dashboard + 30 fleet-pm-stub.

## v0.4.0-rc15 — allocator hysteresis + idle-deploy fall-through (2026-05-20)

Live incident the night before: the orchestrator unilaterally liquidated
the $174 hedgedjlp position at 2026-05-20T00:27:46 UTC and left $175 of
USDC sitting idle at 0% APR for ~5 hours. Cost: ~$0.50 in gas + Jupiter
swap slippage. Cost if undetected: a full day of foregone yield on 73%
of fleet AUM.

Four bugs in one cascade:

- **Bug A — `WithdrawHedgedJlp` always emits `u64::MAX`.** The allocator
  computed `amount_usd: $23.93` (10% of AUM, properly clamped by
  `max_action_fraction`). The envelope-construction layer in
  `allocator_runner.rs` discarded the dollar amount and sent the
  full-withdraw sentinel `jlp_lamports: u64::MAX` because *"the
  allocator does not price JLP."* A 10% rebalance became 100%
  liquidation. Investigation showed the hedgedjlp daemon's `unwind.rs`
  iterates `active.open_positions` and closes every short
  unconditionally — there is no proportional-close path today — so
  "partial JLP burn but full short close" would actually be worse
  (leaves residual JLP unhedged). The envelope behaviour is therefore
  *correct given the daemon constraint*; the real fix is to make sure
  the allocator only emits Withdraw when it really means "liquidate."
  Documented the constraint inline, added an invariant test pinning
  `u64::MAX` until proportional unwind lands.
- **Bug B — under-hurdle Withdraw below `min_action_usd` blocks idle
  deposit.** Post-liquidation, multiply ($8.33 deployed) sat ~30 bps
  under its hurdle. Step 3 picked it for Withdraw, computed amount =
  $8.33 < min $10, returned `NoAction`, and never reached step 4
  (idle deposit). $175 of idle USDC sat at 0% APR for hours while
  stable_yield was paying 5–9%. Fix: when step-3 can't act, record
  the observation and **fall through to step 4** instead of
  short-circuiting.
- **Bug C — `Deposit→multiply` returns `None`.** `AssignMultiply` has
  no USD-sizing field (the daemon trades against whatever balance is
  in its ATA; allocator-driven deposits would need an out-of-band
  wallet transfer). When the deposit-picker selected multiply as best
  above-hurdle, the envelope layer returned `None` and the orchestrator
  emitted `skipped:no_dispatch`. Idle stayed idle. Fix: introduce
  `is_deployable_via_allocator()` (currently filters out `multiply`)
  and apply it in the deposit-picker so it falls through to next-best
  or stable_yield.
- **Bug D — no hysteresis on Withdraw triggers.** The actual incident
  trigger was a single-tick 352 bps spike in Kamino's reported USDC
  supply APR (5.44% → 8.96%), driving the hedgedjlp hurdle past its
  net APR (gap = -143 bps). The 3% risk premium couldn't absorb a
  3.5% noise event. Added `AllocatorConfig::min_withdraw_gap_bps`
  (default 150 bps). Withdraw fires only when the gap exceeds the
  threshold. The rc15 incident gap was -143 < 150 → would not have
  triggered.

Six new tests pin the incident shape, including
`rc15_regression_apr_spike_does_not_trigger_full_unwind` and
`rc15_regression_post_unwind_idle_redeploys`. Updated audit reason
strings carry both the under-hurdle observation AND the eventual
action so operators can debug a single audit line without
re-reading the allocator's decision tree.

## v0.4.0-rc14 — resize auto-executes when require_approval=false (2026-05-19)

Root cause of the approval deadlock: `run_resize` always enqueued the
plan and emitted `NeedsApproval` to the orchestrator regardless of the
`--require-approval` flag. The flag only gated incoming *Assign* /
*Withdraw* envelopes in `dispatch.rs`; the rebalancer's self-generated
resize plans had no equivalent bypass. On daemon restart the nonce
counter resets to 1; the orchestrator's replay-protection guard records
the last-seen nonce per sender and rejects anything ≤ last-seen, so
every `Escalate(NeedsApproval)` from the freshly restarted daemon was
silently dropped. The SOL+ETH resize plan was stuck in the queue for
hours.

- `ResizeCtx` gains `require_approval: bool` (mirrors `DispatchCtx`).
- `run_resize`: when `!ctx.require_approval`, calls `execute_resize`
  directly after computing the plan — no queue, no escalate. Returns
  `queued_to_approval: false`.
- `deploy/systemd/hedgents-hedgedjlp-live.service` updated to
  `--require-approval=false` so the install script no longer clobbers
  the setting on every deploy.
- First tick after rc14 deployed: `"resize auto-executed successfully"
  sig_count:2`. Next tick: `queued:0, skipped:3`. All three shorts
  confirmed live. `current_delta_bps: 0`.

## v0.4.0-rc13 — slippage fallback accepts any oracle price (2026-05-19)

rc12's fallback for Jupiter API unavailability still used stale dollar
amounts (`SOL=$100, ETH=$1000, BTC=$50k`). On a bad day those numbers
are *above* the oracle, which means `short_price_floor_micro_usd(fallback) =
fallback × 90%` would still be above oracle and the keeper would still
reject. Only affects the API-down path, but a bug on a degraded path is
still a bug.

- `sim_mark_price_micro_usd` now returns `1` regardless of asset.
  Floor = `1 × 90% → 0` (integer). Keeper fills at whatever the oracle
  says. No stale dollar amounts anywhere in the short-open path.

## v0.4.0-rc12 — live oracle prices for short position slippage floor (2026-05-19)

**The original bug that caused ~$25 of losses.** The
`price_slippage` field in `CreateIncreasePositionMarketRequest` is a
*floor* for shorts: the keeper only fills if oracle ≥ floor. The daemon
had hardcoded stale prices as the slippage input: `ETH=$3,535`,
`SOL=$151.50`. When the position was run live, ETH was at $2,100 and
SOL at $84. Every SOL and ETH open request was silently rejected by the
keeper. BTC filled once (BTC was still above the $70k floor at the
time). The fleet ran with 1 of 3 shorts for ~2 weeks while SOL and ETH
drifted freely against the unhedged JLP long, resulting in roughly $25
of directional loss.

- `open_short_requests` and `execute_resize` both now call
  `crate::prices::fetch_custody_prices_micro_usd` before the asset
  loop to get live Jupiter prices.
- New function `short_price_floor_micro_usd(live_mark) → live_mark -
  live_mark / 10` — floor is 10% below the current oracle. Keeper
  fills unless the market moves >10% between tx submission and
  execution, which is an acceptable exit condition.
- `sim_mark_price_micro_usd` retained as fallback (patched to return 1
  in rc13 — see above).

## v0.4.0-rc9–rc11 — fill verification, custody decoder, /pnl bracketing (2026-05-18–19)

Three smaller fixes that surfaced during rc7/rc8 prod validation:

- **rc9 — keeper fill verification**: `open_short_requests` now polls
  the on-chain `PositionRequest` PDA for up to 20 s after submitting;
  only records the short in `ActivePosition.open_positions` once the
  keeper has flipped the account to filled. Prevents phantom-position
  entries from request PDAs that expire without fill.
- **rc10 — JLP custody decoder offset fix**: asset offset corrected
  from 1080→214, pythnet field from byte 107→106. The wrong offsets
  produced `decoded_mint = [0u8; 32]` for every custody that landed
  past the first; SOL and ETH deltas read as $0.
- **rc11 — /pnl bracket scan past sentinel rows**: dashboard `/pnl`
  reported `delta=$0` when the bracketing rows for a daemon happened to
  be sentinel-mode rows (`jlp_value_usd_micro: 0`). Fixed by scanning
  forward/backward with `find_map` instead of taking `first()`/`last()`
  directly.

## v0.4.0-rc7–rc8 — USDC pre-flight gate + partial-price-response retry (2026-05-18)

First on-chain resize execution (rc5+rc6) revealed two more bugs:

- **rc7 — USDC pre-flight gate**: the resize path submitted an ETH
  short-open against an under-funded USDC ATA, producing a 1200-line
  `custom program error: 0x1` (SPL Token InsufficientFunds) in the
  program log. Added `fetch_wallet_free_usdc_lamports` pre-flight in
  both `run_resize` (queue time) and `execute_resize` (execute time);
  legs that exceed available USDC are dropped with
  `SkipReason::InsufficientUsdcLiquidity`. Also added the one-shot
  retry on Jupiter partial price responses (a request for 3 mints
  returned only 1 that tick, causing SOL+BTC deltas to read $0 and the
  rebalancer to compute a wrong-shape plan).
- **rc8 — pubkey-aware delta bucketing**: added
  `delta::compute_delta_with_pubkeys` fallback to match well-known JLP
  custody PDAs when `decoded_mint` doesn't resolve (covers a future
  custody migration or decoder regression).

## v0.4.0 — withdraw-recovery, auto-mode, tier-1+2 hardening (2026-05-18)

Closed the open items from rc1–rc6 prod bring-up before promoting to
a stable v0.4.0 tag. Three structural fixes + operational polish:

- **Withdraw-recovered positions**: rc4 left `open_counter=0`
  placeholders; `WithdrawHedgedJlp` couldn't derive the close-request
  PDA. Per Jupiter Perps spec §3.6 the counter is a randomisation nonce
  — no structural link between open and close. Unwind now reads the
  on-chain `Position` account at the recorded pubkey and generates a
  fresh close-counter at withdraw time. `open_counter` removed from
  `ActivePosition.open_positions` entirely.
- **Auto-mode (M11)**: strategy daemons auto-accept Assign/Withdraw
  envelopes from the configured orchestrator pubkey when the action
  stays within single-action and 24h cumulative caps. Operator approval
  no longer blocks the autonomous path; out-of-cap actions still queue.
- **`require_approval` flag**: `--require-approval=false` lets the
  Assign/Withdraw dispatch path skip the approval queue. Default stays
  `true` on mainnet.

## v0.4.0-rc6 — JLP custody pricing via Jupiter Price API (2026-05-18)

The rebalancer-resize action from rc5 was blocked at chain-read time:
`read_pool_state` was fetching each JLP custody's `pythnet_price_account`
directly, but Pyth migrated from V1 standalone-account oracles to Pull V2
ephemeral PDAs and the legacy pubkey stored in the custody account no
longer resolves on mainnet (`AccountNotFound`).

- New module `crates/hedgedjlp-daemon/src/prices.rs` — batched Jupiter
  Price API fetcher (`https://lite-api.jup.ag/price/v3`), pure parser
  for offline unit tests, micro-USD scale matching the rest of the
  daemon's math.
- `read_pool_state` now does one Jupiter HTTP call + one
  `get_multiple_accounts` RPC per tick instead of N individual account
  fetches. Soft-fail: missing price map entry → custody contributes
  $0, log WARN, continue (rebalancer sees slightly-wrong delta but
  doesn't blow up).
- 13 new tests; daemon-crate total 129 passing.

Caught in prod execute-mode smoke (rc5 → first rebalancer tick).
**Without this fix the entire resize action chain would be dead.**

## v0.4.0-rc5 — hedgedjlp rebalancer resize action (2026-05-18)

Closes the M9 TODO that left the rebalancer detecting drift but doing
nothing about it. The prod fleet ran for days with a $174 JLP position
hedged only by an $18 BTC short (~$96 of SOL+ETH long exposure
unhedged), bleeding daily directional drift while the rebalancer
logged the problem and emitted Escalates without acting.

- New module `crates/hedgedjlp-daemon/src/resize.rs` — pure
  `compute_per_asset_targets` + `compute_legs_to_open` math (reuses
  hedge.rs's `allocate_per_asset` shape).  Delta-to-open per asset =
  `max(0, target - existing)`. Never closes existing legs, never
  overshoots, scales proportionally when
  `MAX_POSITION_USDC_LAMPORTS` would be exceeded, drops below
  `MIN_HEDGE_NOTIONAL_USD` dust.
- New `ResizeApprovalQueue` (third instance of the existing generic;
  same sender-match audit-fix C1, same `Escalate(NeedsApproval)`
  emission shape — no new authority surface).
- `dispatch.rs` `handle_approve` drains the resize queue first; on
  approve calls `resize::execute_resize` which submits the open-short
  request ixns and updates `state.active.open_positions` +
  `hedge_notional_usdc`.
- `rebalance.rs` `tick_once` now accepts `Option<Arc<ResizeCtx>>` and
  invokes `resize::run_resize` after the Escalate(DeltaDrift). The
  Escalate stays as telemetry.
- 13 new tests pinning the prod-incident shape:
  `prod_174_case_btc_present_sol_eth_missing` (existing BTC short
  present, SOL+ETH legs missing, queue SOL+ETH skip BTC), plus
  idempotency, cap-scaling, dust-drop, and CBOR round-trip cases.

## v0.4.0-rc4 — hedgedjlp boot-time state recovery (2026-05-18)

Before: `RebalanceState.active` lived only in memory. Every daemon
restart orphaned the on-chain position — rebalancer ticked forever
with "no active position" and `WithdrawHedgedJlp` short-circuited to a
zero-Report sentinel. The prod fleet had $174 JLP + $18 BTC short
unmanageable for hours after a restart earlier in the day.

- New module `crates/hedgedjlp-daemon/src/recover.rs` — on boot, reads
  the wallet's JLP token balance via the associated-token account,
  decodes the JLP pool's custody list, discovers open SOL/ETH/BTC
  short PDAs the wallet owns (mirrors the dashboard's
  `discover_hedge_positions`), reconstructs an `ActivePosition`, seeds
  `state.active`.
- Read-failure tolerant at every step. Account-not-found → zero
  balance / empty list, rebalancer's existing `is_empty()` branch
  logs+skips. Non-zero JLP + zero shorts logs WARN (the
  prod-incident shape) but still seeds `state.active` so the
  rebalancer can size up the missing legs (which rc5 then does).
- `conv = [0xFF; 16]` sentinel so recovered positions are grep-able
  in telemetry. Documented `open_counter = 0` placeholder limitation
  (rebalance-only; withdraw mis-derives close PDA — pending the
  withdraw-recovery fix).
- 5 new tests; daemon-crate total 104 passing.

## v0.4.0-rc3 — dashboard /pnl reads real on-chain fields (2026-05-18)

`/pnl` was reporting `start_aum_usdc: 3001` for a fleet whose actual
deployed value was $264. Cause: every yield daemon writes
`total_aum_usdc = paper_principal_usdc + paper_earned_usdc` into its
telemetry row regardless of mode; in live mode `paper_principal_usdc`
is a hardcoded $1000 synthetic baseline. The threshold filter
(`PAPER_PRINCIPAL_THRESHOLD_USDC = $10k`) caught the old $50k paper
rows but let the $1k live-mode synthetics through, giving 3 × $1000 =
$3001 of phantom AUM.

- Replaced `pnl_row_to_usd` with a strict reader that derives USD per
  daemon from real on-chain fields only: multiply
  `net_equity_uusdc` (deposited − borrowed), stable-yield
  `deposited_usdc_lamports` (Kamino USDC supply), hedgedjlp
  `jlp_value_usd_micro` (mark-to-market JLP). All u-USDC integers, no
  floats. Rows without a non-zero real-position field return `None`
  and drop out of any AUM aggregation.
- Deleted `is_paper_row` + `PAPER_PRINCIPAL_THRESHOLD_USDC`
  threshold-based filtering — no longer needed; the synthetic-vs-real
  distinction now lives in field selection, not a magic number.
- 5 new tests pinning per-daemon extraction + synthetic-only-returns-None.

## v0.4.0-rc2 — orchestrator nonce-replay fix (2026-05-18)

First execute-mode action on prod got rejected by the multiply
daemon with `Bilateral validation failed: nonce replay — received 33,
last seen 1778942442`. The CLI's allocator-execute path uses
`now_unix()` as the envelope nonce; recipients record the highest
nonce seen per (sender_pubkey, peer_id) pair. The orchestrator daemon
started its nonce from `AtomicU64::new(1)` and could never exceed the
unix-timestamp high-water mark left by prior CLI invocations — every
emit landed billions below the recorded last_seen and got dropped
silently at the application-level inbox (handle.send() returned Ok at
libp2p).

- One-line fix: seed `outbound_nonce` from
  `now_unix()` at boot. Beacon emitter + tick emitter share this
  counter so all outbound envelopes carry strictly-increasing nonces
  across daemon restarts.

## v0.4.0-rc1 — orchestrator daemon (Phase 1 dry-run) (2026-05-17)

Lifts the existing `fleet-pm-stub allocator` from a manual CLI tool
into a long-running autonomous daemon. Joins the libp2p mesh as
`Role::Orchestrator`, polls the dashboard's `/strategies` + `/aum`
on a tick, runs the pure `allocator::decide` function against the
live snapshot, and writes every decision to an append-only JSONL
audit log. **No envelope emission, no wallet, no authority to move
funds.** Dry-run by default; `--execute` opt-in (held for rc2+).

- New crate `crates/orchestrator-daemon` (~600 LoC + tests).
- Compile-time isolated: wallet crate intentionally absent from the
  dep graph.
- `fleet-pm-stub` refactored to lib + bin so `allocator::decide` and
  `allocator_runner::action_to_envelope_spec` can be reused by both
  the CLI and the daemon. Single source of truth for envelope
  construction across the two surfaces.
- New systemd unit `hedgents-orchestrator.service` + addition to
  `hedgents.target`. Listens on `:19317`.
- Three runbooks: `orchestrator-bringup.md` (rc1 dry-run),
  `orchestrator-devnet-smoke.md` (execute on devnet),
  `orchestrator-mainnet.md` (execute on mainnet with 7-day
  promotion window).
- 24 fleet-pm-stub library tests + 10 orchestrator-daemon tests pass.

---

## Why the rc march matters

rc1 through rc14 shipped over ~48 hours of prod execute-mode bring-up.
Each rc fixed a real bug surfaced by live mainnet behaviour that no
unit test could have predicted:

| rc | bug | how it surfaced |
|----|-----|-----------------|
| rc2 | orchestrator nonce-replay | execute-mode envelope silently dropped at recipient |
| rc3 | $3001 phantom AUM | dashboard `/pnl` query during incident triage |
| rc4 | state.active orphaned after restart | manual WithdrawHedgedJlp returned zero |
| rc5 | rebalancer-detected drift was a no-op | M9 TODO comment, surfaced by rc4 reading the recovered state |
| rc6 | Pyth V1→V2 oracle migration | first rc5 rebalancer tick blew up on `AccountNotFound` |
| rc7–rc8 | USDC InsufficientFunds + partial Jupiter price response | first on-chain resize produced a 1200-line program error |
| rc9 | phantom positions from unfilled keeper requests | resize recorded a short before keeper had executed |
| rc10 | JLP custody decoder wrong offsets | SOL + ETH delta read as $0, rebalancer skipped them |
| rc11 | /pnl delta=$0 with sentinel bracket rows | real AUM disappeared from dashboard during position recovery |
| rc12 | stale hardcoded slippage floors above oracle | SOL/ETH keeper fill silently rejected for ~2 weeks, ~$25 loss |
| rc13 | fallback prices still above oracle on bad day | API-down path could repeat the rc12 failure |
| rc14 | resize queue blocked by orchestrator nonce-replay | daemon restart resets nonce; orchestrator dropped every Escalate |
| rc15 | 4-bug cascade: APR-spike → full-unwind → idle stuck at 0% APR | orchestrator liquidated $174 hedgedjlp and left $175 idle for ~5h |
| rc16 | dashboard understated AUM by hedge-collateral amount | operator reported "missing $40" after rc15 redeployed the position |

The ~$25 loss from rc12 is real and verifiable on-chain. The root
cause (a floor price set above the oracle at time of execution) is the
kind of bug that passes every unit test and only surfaces when you
actually send the transaction to a live keeper. Each fix from rc7
onward has a regression test pinning the exact incident shape.

This is the maturity profile institutional reviewers underwrite:
mainnet shipping with honest in-code commentary about what's
load-bearing and what isn't, and an unbroken chain from incident to
root cause to regression test.

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
