# hedgedjlp-daemon audit — 2026-05-15

Field-by-field comparison of the existing daemon (12 files, ~4300 LOC)
against the canonical spec in
[`jupiter-perps-bundle-spec.md`](jupiter-perps-bundle-spec.md). All
section references below are to that spec.

**Bottom line up front.** The JLP buy + redeem path (`add_liquidity_2`,
`remove_liquidity_2`) is structurally correct — accounts, args, and
discriminator all match the IDL. The Jupiter Perps hedge legs
(`create_increase_position_request_v2`,
`create_decrease_position_request_v2`) are **fundamentally broken at
every layer**: wrong ix name (no `_v2` exists), wrong params struct,
wrong Side enum byte values, missing PositionRequest seed byte,
synthetic-custody stand-ins. The daemon's own audit-fix C3 already
hard-stops mainnet submit because of the synthetic custodies, so no
silent corruption has shipped — but the entire hedge path needs to be
rebuilt before sim makes any economic sense.

---

## §1. Ixn builder audit (`crates/zerox1-defi-protocols/src/protocols/jlp.rs`)

### `add_liquidity_ix` (jlp.rs:160-222) — OK

- 14 accounts, matches IDL `addLiquidity2` exactly.
- Discriminator `sha256("global:add_liquidity_2")[..8]` — correct.
- `AddLiquidity2Params { token_amount_in, min_lp_amount_out,
  token_amount_pre_swap: Option<u64> }` — matches IDL.
- ATA-create-idempotent for both input and JLP ATAs prepended.
- **One minor**: slot [0] `owner` is marked `new_readonly(... , true)`
  (signer, not writable). IDL says `isMut: false, isSigner: true`. Match.
  But Anchor's fee payer must be writable — Solana adds the payer
  writability flag implicitly when the tx is built (and signer slot 0
  is the fee payer by convention). This is fine in practice but a
  reviewer should double-check that `RpcContext::build_signed` marks
  the payer writable. **Verify in Session B.**

### `remove_liquidity_ix` (jlp.rs:234-289) — OK

- 14 accounts, matches IDL `removeLiquidity2` exactly.
- Discriminator `sha256("global:remove_liquidity_2")[..8]` — correct.
- Params match.

### `create_increase_position_request_ix` (jlp.rs:626-718) — CRITICAL

| Item                            | Daemon                                                       | IDL truth                                                                       |
| ------------------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| Ix name (discriminator preimage)| `global:create_increase_position_request_v2`                 | `global:create_increase_position_market_request`                                |
| Account count                   | 16                                                           | 16 (incl. 1 optional `referral`)                                                |
| Account [10] referral           | `Pubkey::default()` (non-optional)                           | `Option<Pubkey>` — pass `null` / omit, NOT default                              |
| Params field count              | 11 explicit fields                                            | 6 fields (the rest are absent in this Market variant)                          |
| Params field 1                  | `size_usd_delta: u64`                                         | `size_usd_delta: u64` ✓                                                         |
| Params field 2                  | `collateral_token_delta: u64`                                 | `collateral_token_delta: u64` ✓                                                 |
| Params field 3                  | `price_slippage_bps: u64` (in bps)                            | `side: Side` (u8 enum) — DAEMON ORDER WRONG                                    |
| Params: `side` encoding         | `Long=0, Short=1`                                             | `Side::None=0, Side::Long=1, Side::Short=2`                                    |
| Params: `price_slippage` units  | "bps" per daemon docstring                                    | 6-decimal USD scale (e.g. $100 → `100_000_000`), NOT bps                       |
| Params: `jupiter_minimum_out`   | `u64` (no Option tag)                                         | `Option<u64>` — must emit tag byte                                              |
| Params: `pre_swap_amount`       | Present                                                       | Absent in Market variant                                                        |
| Params: `trigger_price`         | Present                                                       | Absent                                                                          |
| Params: `trigger_above_threshold`| Present                                                      | Absent                                                                          |
| Params: `entire_position`       | Present                                                       | Absent in increase ix                                                           |
| Params: `request_type`          | Present                                                       | Absent in Market variant (request_type is implicit)                            |

**Severity**: Critical. Discriminator won't match any deployed ix,
so simulation returns `InstructionError::InvalidInstructionData` and
the daemon's `error_code=5` masks the root cause. Even if the name
were corrected, the params struct's wire bytes are 88+ bytes vs the
expected ~34 bytes — Anchor borsh-decode would fail.

### `create_decrease_position_request_ix` (jlp.rs:803-883) — CRITICAL

Same class of issues as the increase variant:

| Item                            | Daemon                                          | IDL truth                                              |
| ------------------------------- | ----------------------------------------------- | ------------------------------------------------------ |
| Ix name                         | `create_decrease_position_request_v2`           | `create_decrease_position_market_request` (or `_request2`) |
| Account count                   | 16                                              | 16 for Market variant, 18 for Request2 variant         |
| Params: `collateral_usd_delta`  | Present (good)                                  | First field ✓                                          |
| Params: `size_usd_delta`        | Present (good)                                  | Second field ✓                                         |
| Params field order              | size_usd_delta, collateral_usd_delta            | collateral first, size second — DAEMON SWAPPED        |
| Params: `entire_position`       | `u8` (0/1)                                      | `Option<bool>` — daemon misses option tag             |

(Daemon field order: `size_usd_delta, collateral_usd_delta, price_slippage_bps, jupiter_minimum_out, trigger_price, trigger_above_threshold, entire_position, side, request_type, counter` — the close ix in the IDL doesn't have `side` at all because side is read from the existing Position, and has no `request_type` in the Market variant.)

**Severity**: Critical.

### `derive_position` (jlp.rs:890-910) — IMPORTANT

- 6 seed slices in correct order: `[b"position", owner, pool, custody,
  collateral_custody, &[side_byte]]`.
- `side_byte = PerpSide::as_u8()` returns `Long=0, Short=1` — WRONG per
  IDL Side enum (`Long=1, Short=2`).

**Severity**: Important. The PDA derived will never match the program's
`Anchor #[account(seeds=...)]` constraint → all increase requests will
fail `ConstraintSeeds`. (This is unrelated to the wrong ix name — even
if the discriminator were fixed, the position PDA still wouldn't
match.)

### `derive_position_request` (jlp.rs:916-926) — CRITICAL

- 3 seed slices: `[b"position_request", position, counter.to_le_bytes()]`.
- IDL truth (per `generate-position-and-position-request-pda.ts`): 4
  seed slices, with a trailing `request_change_byte` (1=increase,
  2=decrease).

**Severity**: Critical. The PDA the daemon derives is the address it
puts on-chain — the keeper, when it looks for outstanding requests,
derives the PDA INCLUDING the `request_change` byte and will look at
a different address.

### `decode_custody` + `Assets` decoder (jlp.rs:295-489) — IMPORTANT

Per §6.1 of the spec, the IDL field order is:

```
feesReserves, owned, locked, guaranteedUsd, globalShortSizes, globalShortAveragePrices
```

The daemon reads at fixed offsets 1080/1088/1096/1104/1112 and
labels them `locked, owned, guaranteed_usd, global_short_sizes,
global_short_average_prices`.

If 1080 is correctly the start of the assets block, the daemon reads:
- offset 1080 (truly `feesReserves`)         → labeled `locked`
- offset 1088 (truly `owned`)                → labeled `owned` (accidentally OK)
- offset 1096 (truly `locked`)               → labeled `guaranteed_usd`
- offset 1104 (truly `guaranteedUsd`)        → labeled `global_short_sizes`
- offset 1112 (truly `globalShortSizes`)     → labeled `global_short_average_prices`

So `owned` is right by coincidence but `locked` reads fees-reserves
and the upper fields are all shifted by one slot. This is the
kamino-loader bug pattern (wrong offset → silent zero/junk reads).
The delta math in `read_pool_state` uses `assets.owned * pyth_price`
which happens to be correct; `locked` and the short fields are wrong.

Also: the offset 1080 was "verified live on 2026-05-04" per a code
comment, but the IDL `Custody` struct has variable-length fields
upstream of `assets` (`permissions`) so a single re-verification can
go stale on the next program upgrade.

**Severity**: Important. The daemon currently uses `owned` only (right
by coincidence) but any future code that touches `locked` /
`guaranteedUsd` / `globalShortSizes` will be wrong.

**Fix recommendation**: replace byte-offset reads with
`AnchorDeserialize::try_deserialize` (or a hand-rolled borsh decode
that walks the variable-length fields properly). For the offsets
the daemon currently needs to read, prefer reading from the IDL
account types or via Anchor's `account!` macro.

### `decode_position_request` (jlp.rs:981-1007) — IMPORTANT

Offsets `OWNER=8, POOL=40, CUSTODY=72, COLL_CUSTODY=104, SIZE_USD=184,
COLL_DELTA=192, SIDE=209, COUNTER=232` are best-effort per the daemon's
own code comment.

Per the IDL `PositionRequest` field list:

```
[0..8]  Anchor disc
[8..40]  owner          ✓
[40..72] pool           ✓
[72..104] custody       ✓
[104..136] position     ← NOT collateral_custody (daemon mislabeled)
[136..168] mint
[168..176] openTime (i64)
[176..184] updateTime (i64)
[184..192] sizeUsdDelta ✓
[192..200] collateralDelta ✓ (daemon: collateral_token_delta — matches by intent)
[200..]   requestChange enum + requestType enum + side enum + 6 options + executed + counter + bump + referral option
```

The daemon labels offset 104 as `collateral_custody`. The IDL stores
`position` (the parent Position PDA) at that slot, not the collateral
custody. The daemon's caller never uses `collateral_custody` from the
decoded PositionRequest for live logic — only for telemetry — so this
is latent.

`SIDE=209` and `COUNTER=232` are highly speculative. The actual offset
depends on how many Options upstream are encoded (each Option is 1 tag
byte + 0-or-N payload bytes). For an actual decode the daemon should
do borsh-deserialize from offset 0, not byte-offset reads.

**Severity**: Important (latent — used only in telemetry).

---

## §2. Account-layout decoder audit

### Custody `assets` block

Covered in §1 under `decode_custody`. **Important.** All five `Assets`
field reads except `owned` are off by one slot. Recommend replacing
with full borsh-decode.

### `FundingRateState` (borrow rate)

`decode_custody_borrow_rate_bps` (jlp.rs:402-404) returns `None`
unconditionally. The rebalancer's borrow-rate watch is a no-op.

The IDL says the field lives inside `Custody.fundingRateState`
(`cumulativeInterestRate: u128, lastUpdate: i64, hourlyFundingDbps: u64`).
Once the custody offsets are corrected (or borsh-decoded), reading
`hourlyFundingDbps` and converting to annual bps is straightforward
(see §6.2 of the spec).

**Severity**: Important. The borrow-rate watch is the daemon's main
real-time risk control — running it blind on mainnet means a runaway
borrow rate (e.g. 200% APR during a spike) won't trigger the auto-pause.

### Pool / Perpetuals account decoding

The daemon does not decode the Pool account body to enumerate
custodies; the rebalancer accepts a `custody_pubkeys: Vec<Pubkey>`
input. M11+ TODO per the code comment. Acceptable for now — but the
runbook must hard-code the 5 mainnet custody pubkeys from §6 of the
spec (they're stable).

---

## §3. Daemon flow audit

### `dispatch::handle_assign` / `run_or_simulate`

- **Bundle order** (jlp_hedge.rs:101-215): JLP buy first, then hedge
  shorts. Reasonable. Each leg builds its own ixn slice and runs
  whitelist + simulate/submit separately. OK.
- **Compute budget** is prepended by `RpcContext::build_sign_simulate`
  / `build_sign_send` per the daemon's comments. CU limits:
  - JLP buy: 600k (`JLP_BUY_CU_LIMIT`) ✓
  - Hedge open: 400k (`HEDGE_CU_LIMIT`) — slightly tight for a 16-account
    create_increase_position_market_request. Recommend bumping to 600k.
  - Burn / close: 600k (`BURN_CU_LIMIT`) ✓
- **ALT wiring**: no ALT used. Acceptable for request-half txs (~900
  bytes each, single ix per tx). The buy/burn tx has 14 accounts +
  2 ATA-creates and is ALSO ~900 bytes. **OK without ALT.**
- **Oracle / AUM freshness**: not enforced by the daemon. Jupiter
  keepers refresh AUM continuously; the daemon doesn't need a pre-ix
  for this. OK.
- **Synthetic custody guard** (audit-fix C3) hard-stops live submit
  with `error_code=6` because the daemon never wires the real custody
  decode in the dispatch path. Audit-fix C3 is correct — the upstream
  cause (no live custody loader) is Session B work.
- **Multi-position concurrency**: `RebalanceState::active` is a
  `Mutex<Option<ActivePosition>>` — single position globally. The 3
  asset shorts are stored inside one `ActivePosition.open_positions`
  vec. OK for the v0 mandate (one assign at a time).

### `hedge::compute_hedge_short_usd` (hedge.rs:114-134)

Math is **correct** per the corrected M9 formula:
- `target_net_long = total * bps / 10_000` (signed)
- `hedge_short = max(current_long - target_net_long, 0) + max(-target_net_long, 0)`

Test coverage is good (verified `target=0 → full neutralize`,
`target=±500 → small bias`, edge cases).

### `hedge::allocate_per_asset` (hedge.rs:139-180)

Pro-rata over (SOL, ETH, BTC) by USD exposure. MIN_HEDGE_NOTIONAL_USD
= $10 dust filter. OK.

### `rebalance::tick_once` (rebalance.rs:159-303)

- Drift detection logic is correct (compute_drift_bps tests pass).
- Borrow-rate watch is gated by `decode_custody_borrow_rate_bps`
  returning `None` → effectively disabled. Once the offsets are fixed
  (§2), this will start firing. Until then: silent gap.
- Pause window (1h) after a borrow-rate exceedance is reasonable.

### `unwind::run_or_simulate`

Sequence: close all per-asset hedges → burn JLP → clear active.

Audit-fix C2 (use real `open_counter` for close-request PDA derivation)
is correctly wired — `unwind.rs` reads `active.open_positions` and
passes the counter to `derive_position_request`. **However**: even with
the right counter, `derive_position_request` is missing the
`request_change_byte` seed (§1) so the close PDA still won't match.

The `FULL_CLOSE_SIZE_PLACEHOLDER = u64::MAX / 2` trick (unwind.rs:418)
is a workaround. With the correct `Market` decrease params struct, the
canonical pattern is `size_usd_delta=0` + `entire_position=Some(true)`
(see §4 of the spec) — keeper reads `entire_position` and ignores
size.

---

## §4. Findings table

| Sev      | Location                              | Issue                                                                                                  | Spec ref       | Fix sketch                                                                                              |
|----------|---------------------------------------|--------------------------------------------------------------------------------------------------------|----------------|---------------------------------------------------------------------------------------------------------|
| Critical | jlp.rs:658                            | Discriminator preimage `create_increase_position_request_v2` does not exist in the program             | spec §3        | Use `create_increase_position_market_request`. Re-run the discriminator round-trip test.               |
| Critical | jlp.rs:837                            | Discriminator preimage `create_decrease_position_request_v2` does not exist                            | spec §4        | Use `create_decrease_position_market_request` (Market variant matches daemon intent).                  |
| Critical | jlp.rs:564-600                        | `CreateIncreasePositionRequestParams` has wrong field order, wrong types, extra fields                 | spec §3        | Replace with `{ size_usd_delta, collateral_token_delta, side: Side, price_slippage, jupiter_minimum_out: Option<u64>, counter }`. |
| Critical | jlp.rs:735-768                        | `CreateDecreasePositionRequestParams` swapped field order + missing Option encoding                    | spec §4        | Replace with Market variant params struct.                                                              |
| Critical | jlp.rs:916-926                        | PositionRequest PDA missing trailing `request_change` byte seed (1=increase, 2=decrease)               | spec §3.6      | Add a 4th seed slice. Plumb `request_change` enum through both open + close call sites.                |
| Critical | jlp.rs:544-552 (PerpSide::as_u8)      | Side enum encoded as Long=0/Short=1; correct is Long=1/Short=2 (Side::None=0 is implicit)              | spec §3.4      | Fix `PerpSide::as_u8` mapping. Affects both PDA seeds AND params byte.                                  |
| Critical | jlp.rs:703                            | `referral` slot filled with `Pubkey::default()`; should be Optional/omitted                            | spec §3, §4    | Use Anchor's optional-account encoding (omit the slot OR use the dedicated sentinel — verify SDK pattern). |
| Critical | hedge.rs:496-506, unwind.rs:466-476   | All hedge legs run on synthetic CustodyMeta (USDC custody pubkey as stand-in for everything)           | spec §6        | Wire `jlp_hedge::read_pool_state`-style custody loader into the dispatch path. Re-use the existing decoder once §2's Assets bug is fixed. |
| Important| jlp.rs:415-425, jlp.rs:471-477        | Custody Assets reads at wrong offsets — `locked`/`feesReserves` swapped, all upper fields shifted      | spec §6.1      | Borsh-decode the full Custody account via Anchor's deserialize; eliminate hand-rolled offsets.         |
| Important| jlp.rs:402-404                        | `decode_custody_borrow_rate_bps` returns None unconditionally — borrow-rate watch is dead              | spec §6.2      | Read `Custody.fundingRateState.hourlyFundingDbps` after fixing offsets; convert dbps → bps → annual.   |
| Important| hedge.rs:476 (HEDGE_SLIPPAGE_BPS=50)  | `price_slippage` param treated as bps; actual program expects 6-decimal USD price                      | spec §3        | Compute `current_oracle_price_micro_usd ± slippage`. Plumb a price-read step into the open path.       |
| Important| hedge.rs:83 (HEDGE_CU_LIMIT=400k)     | 400k CU may be tight for a 16-account request ix                                                       | spec §3        | Bump to 600k to match the SDK example envelope.                                                         |
| Important| jlp.rs:965-973 (PositionRequest decode)| Offset 104 labeled `collateral_custody` but IDL stores `position` there                                | spec §1 audit  | Replace byte-offset decode with full borsh decode.                                                      |
| Cosmetic | jlp.rs:198 (owner slot)               | Slot [0] `owner` is `new_readonly(..., true)` — relies on tx builder to mark fee payer writable        | spec §1 audit  | Verify `RpcContext::build_signed` flags the fee payer writable in the assembled message.                |
| Cosmetic | adrena.rs                             | Dead file referenced as unused — not on hedgedjlp's path but reviewer should confirm                   | n/a            | Add a `#![allow(dead_code)]` or document the parking purpose.                                          |
| Cosmetic | jupiter.rs                            | The Jupiter Swap aggregator types are unused by hedgedjlp (no swap path); JLP buy is direct deposit    | n/a            | Document the unused-by-hedgedjlp status, or remove the import from hedgedjlp deps if not used.         |
| Cosmetic | All synthetic_pool() helpers          | Use `JLP_POOL` + derived PDAs but empty custody list — only useful for unit tests                       | n/a            | After real loader lands, delete these helpers from the hot path.                                       |

---

## §5. Confidence rating

**Critical issues found — Session B should fix 8 items before sim.**

The JLP buy + redeem path is sim-ready against mainnet (assuming the
real custody loader lands, which is itself a Session B item).

The hedge open + close path requires substantial restructuring:
1. Rename ix calls to the correct `*_market_request` names (and recompute
   discriminators).
2. Replace both params structs with the IDL-matching field lists.
3. Add the missing PositionRequest seed byte.
4. Fix `PerpSide::as_u8` to use 1/2 instead of 0/1.
5. Convert `referral` to an Optional/omitted slot.
6. Wire a real custody loader and remove the synthetic stand-ins.
7. Fix the Custody assets offset/field-order decoder.
8. Implement `decode_custody_borrow_rate_bps` against
   `FundingRateState.hourlyFundingDbps`.

Each item above is mechanical with the spec in hand. **Estimate: 3-5
hours for a careful Session B**, plus 1-2 hours of test-writing for
round-trip discriminator / PDA / params parity tests.

Session C (sim) is structurally fine — `simulate_only` mode already
flows through the whitelist + build_sign_simulate path. But until
Session B lands, every hedge-leg sim will return
`InvalidInstructionData` (wrong discriminator) and the daemon will
mark `error_code=5`. A clean Session C run requires Session B done
first.

**Do NOT broadcast to mainnet until:**
1. All "Critical" findings in §4 are closed.
2. A live mainnet `simulateTransaction` round-trip returns
   `unitsConsumed > 0` AND `err == null` for at least one open hedge
   leg AND one close hedge leg.
3. The Custody decoder is verified by round-tripping a real on-chain
   custody account through the borsh decoder.

The daemon's existing audit-fix C3 (synthetic-custody hard-stop)
correctly prevents accidental mainnet submit until item 1 is done,
which is exactly the kind of belt-and-suspenders guard the multiply
incident taught us to keep.
