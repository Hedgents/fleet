# Hedgents Devlog

Chronological shipping log. Each entry: tagged release → what shipped → why
it mattered. Pair with `ROADMAP.md` (what's next) and `LITEPAPER.md` (what
the product is).

Format: newest first.

---

## v0.4.0-rc35 — reserve USDC for perp short collateral before JLP buy (2026-05-27)

rc34 unblocked the deposit-side allocator math. Live state confirmed
the rc29 cross-strategy rebalance loop closed end-to-end: stable_yield
withdraw → idle accumulates → orchestrator emits `AssignHedgedJlp`
→ hedgedjlp daemon receives $120.63.

Then the daemon bought $120 of JLP via Jupiter Swap successfully
(sig `34XFbLvxy…cJyi`) — and immediately failed to open any of the
three short legs with `Token program error 0x1 (insufficient funds)`.
Net: **$120 of unhedged long JLP** in production. The literal
opposite of "delta-neutral".

Root cause: `jlp_hedge::run_jlp_buy_only` passed **the full
`payload.usdc_lamports`** to the Jupiter Swap call. With 100% of
input converted to JLP, the wallet had zero USDC left for the perp
short collateral (each `create_increase_position_market_request`
transfers USDC from the wallet to the position PDA — at
`HEDGE_LEVERAGE=5x`, ~16% of JLP value is needed as collateral).

This bug has been latent since day one of the open path. It stayed
masked because prior Assigns had been triggered against wallets with
residual USDC from earlier cycles. rc29's cross-strategy rebalance is
what changed the regime: the orchestrator now sends **exactly the
idle balance**, leaving zero USDC after the buy. First Assign under
rc29's regime exposed the bug.

Fix: carve out a USDC reserve before the JLP buy.

```rust
const HEDGE_COLLATERAL_RESERVE_BPS: u64 = 2000; // 20% of input

fn jlp_buy_amount_after_reserve(usdc_lamports: u64) -> u64 {
    let reserve = (usdc_lamports as u128 * 2000) / 10_000;
    usdc_lamports.saturating_sub(reserve as u64)
}
```

Sizing math: JLP long delta ≈ 82%. At 5x leverage,
short_collateral = (0.82 × jlp_value) / 5 = 0.164 × jlp_value.
With 20% reserve (1.5x safety margin over the minimum), input
splits to 80% JLP buy + 20% USDC reserve. The synthetic delta
computation also takes the post-reserve amount so short notionals
size correctly.

**Important relationship to rc27 (orphan-shorts incident).** rc27
fixed the *close-side* — orphan shorts left open after a Withdraw
burned the JLP. rc35 fixes the *open-side* — buying JLP without
reserving short collateral. Same architectural class (multi-leg
operations failing to preserve operational state across legs), two
different instances. The open path should have been audited
alongside the close path during rc27; the miss cost one production
cycle of unhedged exposure.

Workspace tests: 643 passing (no test-surface change — the synthetic-
delta path is exercised by existing rebalance tests).

## v0.4.0-rc34 — AprWeighted excludes non-deployable strategies (2026-05-27)

rc33 deploy unblocked the on-chain Withdraw path: $120 of USDC drained
out of stable_yield as the rebalance designed. But the orchestrator's
*next* dispatch was `AssignStableLend` — depositing the freed capital
right back into stable_yield instead of into hedgedjlp. Watched the
cycle live: Withdraw → idle accumulates → re-Deposit to stable_yield
→ no net movement.

Root cause: the AprWeighted target resolver was including `multiply`
in the gap-weighted budget split. Multiply consistently has the
largest APR-vs-hurdle gap on the live fleet (~170 bps over hurdle),
so multiply captured ~92% of the non-stable target share. But
`AssignMultiply` has no `usdc_lamports` field — the allocator cannot
size a deposit envelope for it (see `is_deployable_via_allocator`).
With multiply's 92% effectively dead capacity, hedgedjlp's target
shrank to ~6%, equal to $16 at our $264 AUM. That's below
hedgedjlp's $100 desk floor, so the rc28 fallback dumped the
rebalance into stable_yield. Net: an expensive no-op cycle that
churned gas without moving exposure.

Fix is one branch in `TargetMode::AprWeighted::resolve`: zero
`apr_bps` for any strategy that fails `is_deployable_via_allocator`.
That collapses multiply's gap to ≤ 0, drops its target share to 0,
and the non-stable budget redistributes proportionally among the
strategies the allocator *can* fund. With only hedgedjlp left
non-stable on the live fleet, hedgedjlp now captures the full 80%
non-stable target — well above its $100 floor at any non-trivial AUM.

One pre-existing test (`apr_weighted_targets_react_to_apr_shifts`)
was rewritten — the old test pinned multiply-vs-hedgedjlp share
ratios, which doesn't make sense once multiply is always 0. The
"dynamic targets respond to APR shifts" contract still holds; the
new test exercises hedgedjlp's hurdle-cross regime (above-hurdle →
non-stable budget, below-hurdle → all-in-stable). Plus one new test
(`rc34_mid_rebalance_state_routes_idle_to_hedgedjlp_not_stable_yield`)
pinning the exact 2026-05-27 production snapshot that exposed the
bug. Workspace: **643 tests passing** (+1 from rc33).

## v0.4.0-rc33 — fleet monitor + systemd approval-gate fix (2026-05-27)

Two operational fixes that surfaced together while testing rc32.

**Fix 1: systemd unit reset bug.** Every `install-hedgents.sh` overwrites
`/etc/systemd/system/hedgents-*.service` from the release tarball. The
in-repo `hedgents-stable-yield-live.service` template shipped with
`--require-approval=true`, so every rc-deploy silently restored that
default and any operator runtime override (`sed -i`'d on the live box)
was wiped. Effect: orchestrator emits `WithdrawStableLend`, daemon
queues for `Approve` envelope that never comes, capital stays stuck.
Took ~90 min of incident-debugging to diagnose because the daemon's
log said `WithdrawStableLend queued — awaiting Approve` exactly once
per cycle and the symptom looked identical to the Kamino bundle bugs
from rc30-rc32. Fixed at the source: template now defaults to
`--require-approval=false`, matching `hedgedjlp-live`'s long-standing
config. Multiply-live still ships `--require-approval=true` because
multiply withdraws are riskier (leveraged unwind with slippage).

**Fix 2: fleet-monitor.sh + hedgents-monitor.timer.** A one-shot
health probe run every 5 minutes via systemd timer. Checks:
1. Every live daemon is `systemctl is-active`.
2. No `build_sign_send failed` lines in any daemon log in the last
   10 minutes.
3. No `orphan shorts` warnings from `recover.rs` in the last 10 min.
4. Allocator hasn't emitted `NoAction` 6 ticks in a row (stuck
   capital signal).
5. Orchestrator's most recent log line is within 5 minutes
   (heartbeat).

On any new failure, emits an email alert (deduped per signature for
1 hour, so the same error doesn't spam) and writes to
`/var/log/hedgents-monitor.log`. Runs independently of the rc25
watchdog timer (which only probes the dashboard's `/aum` endpoint).

Both fixes packaged: the install script copies `fleet-monitor.sh`
into `$PREFIX/bin` and enables `hedgents-monitor.timer` automatically
if the unit files are present in the tarball — same opt-out shape as
the rc25 watchdog timer.

Workspace tests still 642 passing (no test-surface change).

## v0.4.0-rc32 — Kamino withdraw bundle adds farm refresh (2026-05-27)

rc31 switched the withdraw to the v2 discriminator and the on-chain
error changed from `0xbc0 InvalidProgramId` to `0x17a3
IncorrectInstructionInPosition` — same path failing, different gate.
Kamino's klend `check_refresh` requires a specific sequence of
refresh ixns to appear in the tx before the v2 withdraw when the
reserve has a farm attached. Specifically:

```
Required ix: 0 → RefreshFarmsForObligationForReserve
Required ix: 1 → RefreshObligation
Required ix: 2 → RefreshReserve
```

The USDC reserve on Klend (`D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59`)
has farm `JAvnB9AKtgPsTEoKmn24Bq64UMoYcrtWtq42HHBdsPkh` attached, so
the farm-refresh ixn was required. Pre-rc32 our bundle was
`[ATA, refresh_reserve, refresh_obligation, withdraw_v2]` — missing
the farm refresh entirely.

This is why multiply's withdraw path worked without it: jitoSOL and
SOL reserves don't have farms attached. The farm-refresh requirement
only kicks in when `reserve.farm_collateral != Pubkey::default()`.

rc32 adds a conditional `refresh_obligation_farms_for_reserve_ix` to
the bundle when the reserve has a farm:

```
[ATA, refresh_reserve, refresh_obligation, refresh_farms?, withdraw_v2]
```

Two new tests pin both branches: farm-present (bundle = 5 ixns) and
farm-absent (bundle stays = 4 ixns, multiply's path unchanged).

This is the third Kamino-withdraw bug in three days. rc30 fixed an
imaginary account-layout problem, rc31 switched to the v2
discriminator (real fix but exposed a new gate), rc32 fixes the
real-but-different farm-refresh requirement. Each step uncovered the
next, which is the cost of an on-chain path that was never exercised
in production until rc29. **642 tests passing (was 640, +2 rc32 tests).**

## v0.4.0-rc31 — Kamino withdraw delegates to v2 discriminator (2026-05-25)

rc30 attempted to fix the on-chain `liquidity_token_program:
InvalidProgramId (Error 3008)` by realigning the v1 account layout
to mirror v2's. Deployed it, restarted everything, watched the next
Withdraw tick — **same error**.

Re-diagnosed: the bug isn't account ordering. The v1 discriminator
`withdraw_obligation_collateral_and_redeem_reserve_collateral`
points to a Kamino entry point whose IDL **expects different
accounts at the same positions** than v2 does. No reordering against
v2 will ever make a v1-discriminator tx land — Kamino's program
checks the program-id-typed accounts and rejects whatever we pass.

The fix that works: just use the v2 discriminator. `multiply-daemon`
has been calling
`withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix`
successfully since rc26-era; it's the path Kamino keepers actually
exercise. So `kamino::withdraw_ix` (the high-level wrapper) now
delegates the final ixn to the v2 builder while keeping the same
external `Vec<Instruction>` shape (`[ATA, refresh_reserve,
refresh_obligation, withdraw_v2]`).

Regression test `withdraw_ix_delegates_to_v2_under_the_hood` pins:
the bundle still produces 4 instructions, the last one has 17 accounts
(14 v1-shaped + 3 v2 farm appendix), and the critical slots match
v2's IDL ordering.

Also fixed operationally during this debug: `stable-yield-live` was
running with `--require-approval=true --auto-accept-orchestrator=false`,
which queued every orchestrator Withdraw for human approval instead
of executing it. Aligned to hedgedjlp-live's `--require-approval=false`
so the rc29 cross-strategy rebalance can complete unattended. The
sender-allowlist gate (only orchestrator's agent_id) remains the
authority boundary.

Workspace: **640 tests** (unchanged).

## v0.4.0-rc30 — fix Kamino v1 withdraw_ix account layout (2026-05-25)

rc29 deployed cleanly: the orchestrator did exactly what it was
supposed to do, emitting a `WithdrawStableLend` envelope every 5
minutes to free capital from the overweight stable_yield slice. But
each Withdraw failed on-chain with:

```
Instruction: WithdrawObligationCollateralAndRedeemReserveCollateral
AnchorError caused by account: liquidity_token_program.
Error Code: InvalidProgramId. Error Number: 3008.
```

Two latent bugs in `kamino::withdraw_ix` (the v1 builder used by
`stable-yield-daemon::lend.rs`) — both there since day one but
dormant because **no Withdraw against stable_yield had ever fired in
production**. Greedy-mode allocator only withdrew when a strategy
fell below its hurdle, and `stable_yield` IS the hurdle anchor, so
it could never be under-hurdle. rc29 was the first time anything
ever asked stable-yield to give USDC back.

The bugs:

1. **Missing placeholder slot at [10].** Kamino's IDL for the v1
   withdraw discriminator has the same shape as the deposit one:
   `... user_destination_liquidity, placeholder_user_destination_collateral
   (optional), collateral_token_program, liquidity_token_program,
   sysvar_instructions`. Our v1 builder omitted the placeholder. The
   two token programs and the sysvar shifted up by one slot, so
   `liquidity_token_program` ended up receiving SYSVAR_INSTRUCTIONS
   — Anchor checked the program ID, failed, error 3008.

2. **Slots [6] and [8] swapped.** v1 had
   `[6]=liquidity_supply, [8]=collateral_supply` (copied from
   the deposit layout). The Kamino IDL for withdraw expects
   `[6]=reserve_source_collateral=collateral_supply, [8]=liquidity_supply`
   — the opposite, since withdraw flows in the reverse direction.

Both fixed by aligning v1's account layout to v2's (which has been
working for `multiply-daemon`'s unwind path). The discriminator stays
`withdraw_obligation_collateral_and_redeem_reserve_collateral` (v1),
but the account positions now mirror `_v2_ix`. New test
`withdraw_v1_account_layout_matches_kamino_idl` pins every critical
slot so a future copy-paste error won't reintroduce either bug.

Workspace: **640 tests** (was 639).

## v0.4.0-rc29 — active cross-strategy rebalance (2026-05-24)

Post-rc28, the live wallet sat with $256 in stable_yield (5.41% APR)
and $0 in hedgedjlp (9.51% APR) — a 410 bps gap on $250 ≈ $10/year
of yield being left on the table. The allocator had **no path to move
capital between two healthy strategies**: it deposited idle, withdrew
under-hurdle, and that was it. Initial entry timing locked the
allocation forever.

Two patches make this a real allocator instead of a one-way ratchet:

**Patch 1 — `AprWeighted` is the default `target_mode`.** The drift
machinery + APR-weighted target resolver have been sitting unused
since rc22-rc23; rc29 flips the default to `"apr-weighted"` so every
boot routes through `decide_drift_step`. Operators who want the
legacy greedy path can set `--target-mode=greedy`.

**Patch 2 — cross-strategy rebalance in `decide_drift_step`.** When
the normal deposit picker can't fire because `idle_usd < min_action_usd`,
the new `try_cross_strategy_rebalance` checks for:

1. an eligible underweight target (the `best` from drift sort),
2. an overweight strategy with `drift_bps ≥ rebalance_overweight_bps`
   (default 1500 = 15% above target),
3. an APR gap (`best.apr − over.apr`) `≥ rebalance_min_apr_gap_bps`
   (default 200 = 2% — well above Kamino's typical jitter).

If all three hold, emit a `Withdraw` for the overweight strategy
sized at `0.5 × overweight_dollars` (damping factor keeps single-tick
moves smaller; multiple ticks converge). Two-tick rebalance: tick 1
emits Withdraw, on-chain settlement frees USDC, tick 2 sees idle and
the regular deposit picker routes it to the underweight target.

**Why not LLM-based?** A `(APR_a − APR_b) × balance × horizon vs
move_cost` decision is arithmetic, not judgement — LLMs add latency,
cost, and non-determinism without changing the answer. Regime
detection (is JLP fee APR durable, is this volatility transient) is
where an LLM could earn its keep; rebalancing between known yields
is not that. The cost gate via `rebalance_min_apr_gap_bps` does the
same job in a few constants.

Five new tests pin the production-snapshot shape, the cost-gate veto,
the idle-sufficiency interaction with rc28's deposit picker, the
overweight-threshold veto, and two-tick convergence.

Workspace: **639 tests** (was 634).

## v0.4.0-rc28 — allocator min-deposit awareness + libp2p dedupe (2026-05-23)

Two structural bugs surfaced while resolving the rc27 orphan-shorts
incident. Both deserved their own release before they bite something
larger.

**Patch 1 — allocator: skip deposits below each desk's min floor.**
Post-rc27 the wallet held $81 recovered from the orphan shorts. The
orchestrator's greedy picker chose `hedgedjlp` (highest gap) and
proposed `Deposit hedgedjlp $81.09` every tick. The desk rejected
each attempt at the `cap_validation` gate (`usdc_lamports 81094896
below minimum 100000000`); cooldown reset and the loop repeated.

`tools/fleet-pm-stub/src/allocator.rs` now carries a small
`min_deposit_usd(id)` table mirroring each desk's
`MIN_POSITION_USDC_LAMPORTS`. Both `decide_greedy_step` and
`decide_drift_step` gate their deposit picks on it:

- if the picked leveraged candidate's amount is below its desk floor,
  fall back to stable_yield (which has a $1 floor that any non-dust
  amount clears)
- if even stable_yield's floor isn't met, emit `NoAction` with a
  clear "below all desk minimums" reason instead of proposing a
  rejected envelope

Four pre-rc28 tests used small idle amounts that worked fine when the
floor was implicit; those have been bumped past the $100 hedgedjlp
floor so they still target the picker logic they were written for.
New tests cover the rc27 incident shape ($81 idle, $175 in
stable_yield, $0 in hedgedjlp → falls back to stable_yield) and the
boundary ($150 idle below the AUM cap → falls back; $300 AUM lifts
the cap and hedgedjlp wins). Allocator suite: 77 tests pass.

**Patch 2 — dispatch: dedupe duplicate envelopes by conversation_id.**
Hit twice in one session — once on the rc27 Withdraw (the stub
displayed an empty `tx_signatures` from a zero-Report second delivery
even though the first delivery had submitted three close-requests
correctly), once on the rc28-fix stable_yield deposit (first
delivery deposited $81 successfully; libp2p re-delivered the same
envelope 3 seconds later; daemon tried to deposit *another* $81 and
got "insufficient funds" from the Token program — the stub then
reported `ok=false` even though the first deposit had landed). The
second case is only safe by accident: if the wallet had been topped
up between deliveries, the daemon would have happily double-deposited.

Each desk's `dispatch.rs` (`hedgedjlp`, `stable-yield`, `multiply`)
now carries a 64-slot FIFO `ConvDedupe`. On Assign / Withdraw
delivery: if the conv_id was already processed, drop silently
(after logging "rc28: duplicate Assign/Withdraw dropped") instead of
re-running the on-chain work. Approve / Beacon / MarketSignal are
NOT deduped — Approve is idempotent (queue is empty after first
run), the others are observations. State is per-process; a restart
would re-process retransmissions from before the restart, which is
acceptable because `recover.rs` rebuilds any divergence on boot.

Total workspace test count: **634 passing** (was 631, +3 dedupe
tests +3 allocator rc28 tests, with 3 pre-existing tests retargeted
above the new floor).

## v0.4.0-rc27 — orphan-shorts incident fix: recover, slip, verify (2026-05-23)

**Incident.** On 2026-05-22T00:24:14 UTC a Withdraw closed the JLP leg
but left 3 perp shorts ($304 SOL + $70 BTC + $24.70 ETH notional,
$79.48 collateral) open on Solana mainnet. State machine had cleared
`ActivePosition`, so subsequent runs found "nothing to do" — the 3
positions were architecturally invisible to the daemon for ~24h.

**Three structural bugs, three patches:**

1. **`recover.rs` short-circuited on `jlp_balance == 0`** and never
   inspected perp position PDAs. Any successful JLP redeem that left
   shorts open would orphan them forever. Patch reads the position
   list first; if shorts exist they get added to the rebuilt
   `ActivePosition` regardless of JLP balance, with a `warn!` flagging
   the orphan-shorts case.

2. **`unwind.rs` close slippage was `sim_mark_price_micro_usd - buffer`
   = 1 - 0 = 1 micro-USD.** `sim_mark_price_micro_usd` returns 1 as a
   fail-safe (correct for opens — floor, oracle must be ≥ floor —
   broken for closes — ceiling, oracle must be ≤ ceiling). The Jupiter
   keeper saw "only fill if SOL trades at $0.000001" and silently
   rejected every request. Patch fetches live oracle-class prices via
   `prices::fetch_custody_prices_micro_usd` once before the loop, then
   per asset computes `short_price_ceiling_micro_usd(live_mark) =
   live_mark + 10%`. Mirror of the rc12 open-side fix.

3. **`unwind.rs` cleared `ActivePosition` unconditionally after submit
   — no fill verification.** A silent keeper reject was therefore
   indistinguishable from a successful close. Patch tracks every
   submitted close, polls each Position PDA (30 × 1s = 30s window)
   for `is_empty()` / account-gone before redeeming JLP, gates the
   JLP redeem on full close success (burning JLP while shorts remain
   would convert hedged book → naked shorts), and does a partial-clear
   of `ActivePosition` retaining any positions the keeper failed to
   close, so the next Withdraw picks them up.

**Deploy + recovery (observed, 2026-05-23 18:52–18:56 UTC).** Shipped
rc27 to the Hetzner ARM VM. Restart of `hedgedjlp-live` produced the
expected recovery log within 200ms of boot:

```
WARN recover: no JLP balance but 3 open Jupiter Perps short(s)
     discovered — orphan shorts from a partially-failed unwind.
     short_count: 3
INFO recovered active position: jlp_lamports=0, open_shorts=3,
     custodies=5, hedge_notional_usdc: 399012333   # $399.01
```

`fleet-pm-stub withdraw-hedgedjlp --jlp-lamports u64::MAX` (the
`0` value is rejected by the dispatch cap-validation gate; full-
withdraw sentinel resolves to `jlp_acquired = 0` so the close-only
path runs and the JLP redeem leg is skipped). Within 3 seconds the
unwind path submitted three close-requests on mainnet:

- SOL: `4YGm5JB7XCxoD5pWQ9truVPJaso2U8uBexmYbiMG4ZsnbTi2qe2gkHXGypLGudVQBw8HvnsmjYW8VTdzr6ad3BSE`
- BTC: `APqFzJvxQNv4boXx6ZyUkQ4jNUkp5pfcgrW1ZSttuyNbTGCxFdfP74MhAwwQP4u1Ym6SHGu4zX9ykhJv9LakTrY`
- ETH: `22qQVnSi8cWBJhc53FfjNxbjXfFJy7AZtbVdisNGDAu4DJAXcFzXUYQE1muTKjZqgV5CYGJMEWVTjoxzvzD1Suei`

Jupiter keeper executed all three within a single slot window;
`wait_for_position_closed` returned `true` on the first poll for
SOL and ETH and on the second poll for BTC (logged once at attempt
1/30 with size_usd=70212491, then silent success at attempt 2).
`still_open` ended empty → `clear_active_position` ran → ok=true.

Post-restart re-scan from a clean process confirmed the book is
flat:

```
INFO recover: no JLP balance, no open shorts — fresh start,
     state.active stays None
```

Wallet USDC after recovery: **$81.09** (= $79.48 returned collateral
+ ~$1.61 short-side PnL accrued during the 24h the positions were
orphan). Net incident cost: a libp2p envelope-retransmission caused
the stub to display the empty zero-Report from the duplicate
delivery instead of the real Report (cosmetic only — daemon log
shows the actual sigs above).

**Lesson.** Every silent-rejection branch that previously logged a
warn but kept marching forward should be re-audited against this
pattern: submit ≠ execute on a 2-tx protocol. The rc9 open-side
`wait_for_nonzero_position_size` already had this discipline; rc27
extends it to the close side.

## v0.4.0-rc26 — rc25 follow-up: bundle frontend.service + ship .timer files (2026-05-22)

rc25 added the watchdog timer and the `Restart=always` policy across
templates, but the release-artifact pipeline had two latent gaps that
the rc25 install surfaced:

1. **`hedgents-frontend.service` lived only on the production server**,
   not under `deploy/systemd/`. The install script's
   `install -m 0644 "$SRC/systemd/"*.service` therefore never refreshed
   it — operators kept whatever was hand-deployed on first bring-up.
   Result: rc25's `Restart=always` for the frontend was a no-op
   because the on-disk template didn't ship the unit.
2. **`release-fleet.yml` only staged `*.service` + `*.target`** into
   the release tarball. `*.timer` files (introduced in rc25 for the
   watchdog) silently weren't shipped. `systemctl enable
   hedgents-watchdog.timer` on a fresh rc25 install returned
   `not-found` because the file wasn't there.

Both fixed:

- `deploy/systemd/hedgents-frontend.service` vaulted in-repo (was a
  server-only artifact); installer can now refresh it across
  releases.
- Release workflow stages `*.timer` files when present (conditional
  on glob match, so older tags without timers don't break).
- Live server was hot-patched (scp + daemon-reload + enable) so the
  current deployment doesn't wait for rc26 — every unit is
  `Restart=always` and the watchdog is firing every 5 minutes.

This is the same lesson as rc17/rc18 (installer pipeline gap → UI
work undeployed): a manifest entry without a corresponding `cp` is
the silent-failure class that this codebase keeps re-discovering.
The rc26 fix-release pattern (small commit per gap, clear DEVLOG note)
keeps each one auditable.

## v0.4.0-rc25 — always-on hardening: Restart=always + boot-survival + watchdog (2026-05-22)

Operator question: *"make sure the agent is always running and dashboard
is always showing data."* Audit on the live VM found two structural
gaps:

1. **6 of 8 hedgents-* units were `disabled` at boot.** The install
   script told operators to `systemctl enable --now hedgents.target`,
   which activates the target but doesn't create the per-unit
   `WantedBy=hedgents.target.wants/` symlinks. On a VM reboot the
   target came up but most daemons didn't.
2. **`Restart=on-failure`** only catches non-zero exits. A daemon that
   exits cleanly (signal, normal shutdown, OOM-killer-without-cause)
   stays dead.

Fixes:

- **All daemon unit templates**: `Restart=on-failure` → `Restart=always`.
  The existing `StartLimitBurst=10` / `IntervalSec=120` still caps the
  crash-loop scenario; `always` adds coverage for clean-exit cases.
- **New `hedgents-watchdog.service` + `.timer`**: every 5min after
  boot, `curl -fsm 10 /aum`. If unresponsive → `systemctl restart
  hedgents-dashboard`. Catches the harder case where the process is
  alive but the HTTP server has wedged (which `Restart=always` can't
  see). No external monitoring dependency.
- **`install-hedgents.sh`** now `systemctl enable`s every hedgents
  unit + the watchdog timer unconditionally. Idempotent on re-runs;
  operators who disabled a unit for debugging can re-disable after
  install.
- **Live server** had all 6 disabled units enabled immediately so the
  current configuration survives a reboot without waiting for the
  rc25 release artifact.

This closes the "is it always running?" question structurally — every
class of failure that can be caught by a systemd-level mechanism is
now caught. What's left is application-level state corruption (rare)
and the operator-visible incidents that the rc1-rc24 march has been
pinning with regression tests one by one.

## v0.4.0-rc24 — /pnl from chain-state snapshots (2026-05-21)

Operator question: *"are we actually making money?"* The dashboard
said "no", but the chain said yes — three latent bugs in the pre-rc24
`/pnl` pipeline silently zeroed out a real $200 hedgedjlp position:

1. **hedgedjlp daemon's `ActivePosition` was desynced from chain.**
   After a partial unwind at 2026-05-21T00:29:00 UTC, the daemon
   cleared its internal state but the on-chain shorts + JLP remained.
   Subsequent Assigns reopened on-chain but didn't restore the
   internal ActivePosition → telemetry rows reported `jlp_lamports: 0`
   while chain held $200.
2. **multiply and stable_yield were running paper-mode systemd units**
   (`hedgents-multiply.service`, not `-live`). Real positions exist
   on-chain; paper-mode daemons polled them but wrote telemetry to
   `*-pnl.jsonl` not `*-live-pnl.jsonl`. Pre-rc24 `/pnl` only scanned
   the `*-live-pnl.jsonl` paths → 2.4-day-stale data.
3. **`/pnl` aggregated per-daemon rows** → any single broken stream
   poisoned the whole window.

`/aum` was already correct because it reads chain state directly. The
rc24 fix makes `/pnl` use the same source of truth.

- New `chain_aum_snapshots` SQLite table: per-tick `(ts_unix, total_usd,
  multiply_usd, stable_yield_usd, hedgedjlp_jlp_usd,
  hedgedjlp_collateral_usd, idle_usd)`. `ON CONFLICT (ts_unix) DO
  NOTHING` for idempotency.
- New `ingest::aum_sampler` background task: 60s cadence, calls the
  shared `read_chain_aum_breakdown` helper that `/aum` now also uses,
  inserts one row per tick. Refuses to write `total_usd == 0` rows
  (treats as transient RPC failure → next tick retries).
- `/pnl` rewritten: brackets `chain_aum_snapshots` within the window,
  computes `delta = end - start` and `annualised_apy = (delta/start)
  × (year/elapsed) × 100`. Same `PnlOut` JSON shape — no frontend
  break. Windows: `1h`, `24h`, `7d`, `all`.
- `/aum` refactored to call `read_chain_aum_breakdown` so both
  endpoints share the same chain reads — `/pnl` and `/aum` can no
  longer disagree by construction.
- Legacy `pnl_row_to_usd` helper kept under `#[allow(dead_code)]` for
  future per-daemon-secondary-signal use; its tests still pin the
  daemon telemetry JSON shape that daemons emit.

8 new tests: 2 Store round-trip + dedup, 4 `/pnl` HTTP integration
(empty-history note, computed delta, single-snapshot edge, window=all
cutoff math), plus the existing `/aum` tests still pass after the
refactor. Workspace: 67 dashboard tests (up from 34) — the big jump
is from existing tests being moved + new coverage; not a separate count.

**What this answers concretely:** on a fresh rc24 deploy the dashboard
will show a 5s-delayed `/pnl` populating from the very first snapshot,
and within 1-2 ticks operators see actual realized P&L on real
on-chain positions — including the $200 hedgedjlp position that
pre-rc24 was invisible.

## v0.4.0-rc23 — allocator v2 M5: APR-weighted dynamic targets (2026-05-21)

Follow-up to rc22's static drift mode. rc22 shipped the static path
(operator types `30/30/40` once in conf, allocator drifts toward it).
A natural follow-up question: "is `30/30/40` still static?" Yes — and
M5 is the dynamic answer.

`TargetMode::AprWeighted` recomputes the target tilt each tick from
the live per-strategy gap to hurdle:

```text
gap_i        = max(0, apr_i_bps - hurdle_i_bps)
non_stable_budget = 1 - stable_yield_floor
weight_i     = non_stable_budget * (gap_i / sum_of_gaps)
weight_stable = stable_yield_floor (default 0.20)
```

Higher-yield strategies auto-pull capital toward them. A `stable_yield_floor`
keeps the operator's risk-off anchor populated even when other strategies
are yielding well. If everything falls below hurdle, weights collapse to
100% stable_yield.

Wiring:
- new `TargetMode` enum: `Static(TargetWeights) | AprWeighted(AprWeightedConfig)`
- new `allocator_apr_weighted.rs` — pure math, validation, 12 unit tests
- `decide()` resolves the mode once per tick; downstream drift-step is
  mode-agnostic
- orchestrator CLI:
  - `--target-mode=static` (default with `--target-weights=…`)
  - `--target-mode=apr-weighted` (uses live APR gaps)
  - `--stable-yield-floor=0.20` (default)
  - `--min-per-strategy=0.0` (default — opt-in floor for noise dips)
- AuditLog refactored: `append_with_result` now accepts a resolved
  `Option<&TargetWeights>` from the caller (per-tick). Static mode
  passes the same vector every tick; AprWeighted passes a freshly
  resolved one. The forensic record matches the picker's input
  exactly, even when the target vector is moving.

15 new tests (12 math + 3 end-to-end integration). 74 fleet-pm-stub
tests pass total.

**Activation:**
```
EXECUTE_FLAGS=--execute --targets-json=... --cooldown-secs=300 \
    --max-action-fraction=0.50 --min-action-usd=5.0 --stale-slack=1.10 \
    --target-mode=apr-weighted \
    --stable-yield-floor=0.20 \
    --min-drift-bps=200
```

**Deferred to a later rc (M6):** riskwatcher overrides — a layer that
lets `EscalateRisk` envelopes from the riskwatcher pubkey temporarily
shrink a strategy's effective target weight. The mesh-integration cost
of subscribing to the inbox is the same class of work that introduced
rc2/rc14 nonce bugs; deferring it until we have soak data from M5 in
production is the conservative call. Plumbing-only M6 (manual override
CLI without inbox subscription) is scoped at ~250 LoC for whenever we
revisit.

## v0.4.0-rc22 — allocator v2: drift-from-target allocation (2026-05-21)

Pre-rc22 the allocator was greedy: one action per tick, "best-gap-above-
hurdle wins for Deposit, worst-under-hurdle wins for Withdraw." Over
time + cooldowns + caps that produced an *emergent* allocation — the
highest-APR strategy accumulated more deposits — but never a *designed*
one. Operators couldn't say "I want 30% in stable_yield, 30% in
multiply, 40% in hedgedjlp" and have the orchestrator drive toward
those targets. The rc15 incident analysis also surfaced this directly:
when an operator dashboard showed `1:1:1` across the three strategies,
that was just three daemons each independently using a hardcoded
$50k paper-principal baseline — there was no allocator math behind it.

rc22 adds drift-from-target mode as a first-class allocator path. Four
shipping milestones (all behind a single CLI flag):

- **M1 — `TargetWeights` struct + CLI parser** (`allocator_targets.rs`,
  368 LoC, 13 tests). Operator-set tilt `(stable=0.30, multiply=0.30,
  hedgedjlp=0.40)`. Validates no-negative, sum within [0.99, 1.01]
  tolerance, normalises to exactly 1.0. CLI parser tolerates whitespace,
  order-independence, missing strategies (default 0.0). Rejects unknown
  strategy names with an actionable error listing the expected three.
  Inert — no caller yet.
- **M2 — drift dispatcher + `decide_drift_step`** (`allocator.rs`,
  566 LoC, 11 tests). `decide()` becomes a dispatcher; when
  `target_weights` is `Some`, the new drift path runs after the shared
  hurdle gate. Picks the largest absolute *underweight* drift among
  eligible strategies (deployable, above-hurdle, target > 0). Overweight
  is never proactively withdrawn — that would whipsaw against a strategy
  whose APR is rewarding being there. New `min_drift_bps` (default 200)
  is the rebalance band; drifts inside it are noise. When
  `target_weights: None` (default), every rc15–rc21 behavioural test
  passes byte-for-byte.
- **M3 — audit log shape** (`allocator_runner.rs`, 216 LoC, 4 tests).
  `AuditStrategy` gains optional `current_weight`, `target_weight`,
  `drift_bps`. `from_with_targets(snap, Some(&t))` populates all three;
  `from(snap)` defaults to greedy and emits only `current_weight`.
  Backwards-compatible: pre-M3 readers see no schema break.
- **M4 — orchestrator wiring + systemd conf** (~120 LoC + 2 tests).
  New CLI flags `--target-weights="stable_yield=0.30,multiply=0.30,
  hedgedjlp=0.40"` and `--min-drift-bps=200`, plumbed to
  `AllocatorConfig`. `AuditLog::open(path, Option<TargetWeights>)`
  threads the targets to every emitted JSONL row. systemd unit's
  EXECUTE_FLAGS example documents the operator-facing setup.

**Total: ~1270 LoC, 30 new tests across 4 crates.** No envelope shape
changes, no daemon-side changes, no on-chain behaviour change until
the operator sets the env var.

**Activating drift mode (rc22 reference deployment):**
```
EXECUTE_FLAGS=--execute --targets-json=... --cooldown-secs=300 \
    --max-action-fraction=0.50 --min-action-usd=5.0 --stale-slack=1.10 \
    --target-weights="stable_yield=0.30,multiply=0.30,hedgedjlp=0.40" \
    --min-drift-bps=200
```
Empty/unset `--target-weights` keeps greedy mode. The orchestrator
refuses to boot on a malformed spec rather than silently fall back.

This is the largest single-rc feature the allocator has shipped since
rc1 (the orchestrator-daemon itself). Greedy mode stays the default
because (a) zero-config operators get the rc21 behaviour they tested
against, and (b) drift mode needs operator-set weights to be meaningful
in the first place — there's no "right" default tilt across all
operators.

## v0.4.0-rc21 — audit follow-up: install atomicity + API_BASE configurability + test rigor (2026-05-21)

Independent code review of rc17–rc20 (the deployment-pipeline arc)
found three real defects and two test-rigor gaps. All addressed here.

**H1 — Install-script atomic swap is now atomic.** Pre-rc21 the
`/opt/hedgents-frontend` swap stopped the service AFTER the move,
ran `chown -R` AFTER restart (race), and used a cross-filesystem `mv`
from `/tmp` (copy+unlink, not atomic rename). On failure mid-swap the
UI was wiped with no rollback. Fixed: stage into
`/opt/.hedgents-frontend.staging` (same FS → atomic rename), stop the
service before swap, chown the staging dir before swap, restore `.old`
on any failure.

**H2 — `NEXT_PUBLIC_API_BASE` is now configurable.** Pre-rc21 every
release shipped with `https://api.hedgents.com` hardcoded — fine for
the reference deployment, a data-leak surface for any self-hosted
operator whose dashboard would silently fetch from upstream. Now:
`workflow_dispatch` input `api_base` (default reference URL); a
fail-fast guard rejects builds where the input still points at
localhost, so a misconfigured dispatch can't re-introduce the rc18 bug.

**H3 — `LifetimeBanner` hardened.** `animate-ping` is now
`motion-safe:animate-ping` (respects `prefers-reduced-motion`). Fetch
failures log to console instead of rendering "—" silently. Refetches
on `visibilitychange` so a tab open for a week re-anchors to the
server's authoritative `uptime_secs` instead of drifting on the
client clock.

**M1 — Lifetime constants now cross-checked against LITEPAPER.**
rc20's hardened test was one-sided (year ≥ 2026 caught backwards,
not forwards). Tightened to: literal pin + year-floor + `<= now()` at
test time + `include_str!("../../../../LITEPAPER.md").contains("Live
mainnet operation since 2026-05-09")`. Flip the constant without the
LITEPAPER and CI fails. Same approach catches off-by-year-forward.

**M2 — Allocator derived invariants + status thresholds pinned.**
- `default_config_derived_invariants`: enforces
  `risk_premium_bps_hedgedjlp > risk_premium_bps_multiply` (the
  docstring rationale), `min_withdraw_gap_bps > 143` (the rc15
  incident gap), and unit-range floors/ceilings for all four knobs.
  Catches "decimal typo" silently re-calibrating downstream tests.
- `status_thresholds_are_ordered_and_in_seconds_not_micros`:
  STATUS_GREEN_MS / STATUS_YELLOW_MS had no test pre-rc21. Pins
  ordering + unit-range (10s–60s green, 60s–10min yellow).

**L1 — Polish.**
- Navbar now displays the live API host (parsed from `API_BASE`)
  instead of the hardcoded "localhost:7700" string that pre-rc21
  showed on every dashboard regardless of which API it was talking to.
- `lifetime_output_shape_serializes` test no longer seeds the wrong
  2025 timestamp — uses `LIVE_SINCE_UNIX` constant. Test data should
  never encode a value we declared incorrect.
- `lib/api.ts` warns at import time if `NEXT_PUBLIC_API_BASE` is the
  localhost fallback — the same trap that bit rc18 now surfaces in
  every devtools console.

5 new tests across `state.rs` and `allocator.rs`. Workspace count:
34 dashboard + 31 fleet-pm-stub + remainder unchanged.

## v0.4.0-rc20 — fix LIVE_SINCE_UNIX off-by-one-year (2026-05-20)

rc19 shipped the lifetime banner with `LIVE_SINCE_UNIX = 1_746_748_800`,
which is 2025-05-09 — one year before the actual go-live. Dashboard
reported **376 days of uptime** instead of 11. The
`lifetime_constants_match_devlog` test passed because it asserted the
constant against itself; the year wasn't independently verified.

- Constant updated to `1_778_284_800` (2026-05-09T00:00:00Z).
- Test hardened: in addition to the literal pin, assert
  `LIVE_SINCE_UNIX >= 1_767_225_600` (2026-01-01) so any future
  off-by-365-days typo fails loud at test time. The lesson generalises:
  when a constant encodes a real-world value, the test should compare
  against a *derived* property, not the literal itself — otherwise the
  test is just paraphrasing the bug.

This is the smallest, most embarrassing rc on the board, which makes
it the most useful one to ship transparently: the regression test
discipline only works if the test is independent of the constant it
guards.

## v0.4.0-rc19 — lifetime hero banner: make time-on-mainnet visible (2026-05-20)

The product thesis ("operational reliability earns trust over time")
only compounds as a moat if prospects can see the time accumulating.
Pre-rc19 the fleet had three weeks of mainnet uptime and 18 publicly-
resolved incidents — all of it invisible to anyone who didn't read
DEVLOG.md. The dashboard rendered live numbers (AUM, APR) but no
historical context.

- New backend endpoint `GET /lifetime` returns `live_since_unix`,
  `now_unix`, `uptime_secs`, and `incidents_resolved`. Constants
  bumped per release tag; `now_unix` derived at request time so the
  client can drift-free-tick a per-second uptime counter against the
  server clock.
- New frontend `LifetimeBanner` hero component renders above the
  Treasury panel: pulsing live indicator, three big-number stats
  (Live Since · Uptime · Incidents Resolved), subtle emerald gradient
  accent. The uptime counter ticks every second locally, anchored to
  the server's `uptime_secs` so refreshes don't reset to 0.
- Page hierarchy reorganised with section dividers (Treasury /
  Strategies / Allocator / Activity) so the dashboard reads as a
  narrative top-to-bottom instead of a grid of similarly-weighted
  cards.

2 new dashboard tests pin the response shape. The header is the first
piece of operator-facing surface that explicitly says "this thing has
been running" — every prior version forced the visitor to infer it
from the changelog.

## v0.4.0-rc18 — release pipeline bakes the right API base into the frontend (2026-05-20)

rc17 closed the install-side gap (frontend now flows through the
installer), which immediately surfaced a deeper bug in the build side:
the release pipeline had been compiling the frontend with
`NEXT_PUBLIC_API_BASE=http://localhost:7700` since the bundle was first
added. Next.js bakes `NEXT_PUBLIC_*` into the client chunks at build
time, so every browser loading dashboard.hedgents.com tried to fetch
data from the user's own machine on port 7700 — mixed-content blocked,
connection refused, empty cards. The original May 17 hand-deployed
bundle had been built locally with the right URL, which is why nobody
noticed; rc17 was the first time the (broken) GH Actions build ever
reached the reference VM.

- `.github/workflows/release-fleet.yml`: `NEXT_PUBLIC_API_BASE` →
  `https://api.hedgents.com`, with an inline comment explaining the
  failure mode so a future engineer doesn't toggle it back.
- Reference VM hot-patched with a locally-built bundle that has the
  correct API URL while the rc18 release rolled.

This is the dual of the rc17 incident: rc17 fixed "the installer
never deployed the frontend", rc18 fixes "the frontend that gets
deployed was built wrong." Together they close the UI deployment
pipeline end-to-end.

## v0.4.0-rc17 — installer actually deploys the frontend bundle (2026-05-20)

The rc16 dashboard fix shipped clean — but operators ran the install
script and the UI didn't change. Investigation: every release since the
frontend was added to the build pipeline produced a `hedgents-frontend-*.tar.gz`
artifact and a `manifest.json` entry for it, but **`install-hedgents.sh`
never downloaded or extracted that tarball**. It read
`FRONTEND_URL` from the manifest into a shell variable and then
silently dropped it. Result: `/opt/hedgents-frontend` carried whatever
bundle was manually deployed during initial bring-up (May 17, 2026 on
the reference VM) and stayed frozen there through every "upgrade."

- `install-hedgents.sh` now does what its manifest-read implies:
  downloads the frontend tarball, verifies the sha256 sidecar (same
  format the fleet binaries use), and atomically swaps
  `/opt/hedgents-frontend` for the new bundle (keeping `.old` for
  rollback). If `hedgents-frontend` is running, restart it; otherwise
  leave the unit alone so first installs don't auto-start.
- Empty `FRONTEND_URL` in manifest is non-fatal (older fleet tags
  pre-date the bundle).
- Manual rc16 hot-deploy on the reference VM verified the swap shape
  before the patch landed — same atomic-move + chown + restart flow.

This was 50 LoC of UI work going undeployed for an unknown number of
prior releases. The fix is structurally cheap; the audit habit it
teaches (never trust a manifest field you don't actually consume) is
the real value.

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
| rc17 | installer never deployed the frontend bundle | rc16 UI work didn't appear after `install-hedgents.sh` ran on the VM |
| rc18 | release pipeline baked localhost:7700 into the frontend | rc17 install replaced the working hand-built bundle with the broken CI one |
| rc19 | (hero banner shipped) | — |
| rc20 | LIVE_SINCE_UNIX off by exactly one year (2025 instead of 2026) | banner showed 376 days uptime instead of 11 |
| rc21 | audit follow-up: install atomicity + API_BASE configurability + test rigor | independent code review of the rc17-rc20 arc |
| rc22 | allocator v2: drift-from-target allocation behind a CLI flag | operator-facing question "does the allocator do proportional math? 1:1:1 in paper looks like no math" |
| rc23 | allocator v2 M5: APR-weighted dynamic targets | operator-facing question "is 30/30/40 still static?" |
| rc24 | /pnl from chain-state snapshots (kills 3 telemetry desync classes) | operator-facing question "what about the trading part, actually making money? but it's not shown on dashboard" |
| rc25 | Restart=always + boot-survival + watchdog timer | operator-facing question "make sure the agent is always running and dashboard is always showing data" |
| rc26 | bundle frontend.service into deploy/ + ship .timer files in release tarball | rc25 install surfaced two artifact-pipeline gaps |

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
