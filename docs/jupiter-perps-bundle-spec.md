# Jupiter Perps + JLP bundle spec

Canonical reference for the Jupiter Perpetuals program + JLP pool, used by
the hedgedjlp-daemon. Mirrors the structure of
[`kamino-klend-bundle-spec.md`](kamino-klend-bundle-spec.md). Every account
list, PDA seed list, account-layout offset, and discriminator preimage in
this document is verified against the authoritative sources cited below.

## §1. Source citations

| What | URL | Pulled | Notes |
| --- | --- | --- | --- |
| Jupiter Perps Anchor IDL (JSON) | `https://raw.githubusercontent.com/julianfssen/jupiter-perps-anchor-idl-parsing/main/src/idl/jupiter-perpetuals-idl-json.json` | 2026-05-15 | commit blob sha `fbb7cda2b4cf58a9bb3c843d5d1a9cec5b0b1b86`. Community-maintained mirror of the on-chain IDL. |
| Jupiter Perps IDL parsing repo (examples) | `https://github.com/julianfssen/jupiter-perps-anchor-idl-parsing` | 2026-05-15 | `src/examples/generate-position-and-position-request-pda.ts`, `src/examples/create-market-trade-request.ts`, `src/constants.ts`. |
| Jupiter Perps program id | `PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu` | 2026-05-15 | Stable since program launch. |
| JLP pool account | `5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq` | 2026-05-15 | |
| JLP mint | `27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4` | 2026-05-15 | 6 decimals. |
| Event authority | `37hJBDnntwqhGbK7L6M1bLyvccj4u55CCUiLPdYkiqBN` | 2026-05-15 | PDA seed `__event_authority`. |
| Doves oracle program id | `DoVEsk76QybCEHQGzkvYPWLQu9gzNoZZZt3TPiL597e` | 2026-05-15 | Used by `*Doves*` price accounts. |
| Custody pubkeys (mainnet) | constants.ts | 2026-05-15 | SOL `7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz`, BTC `5Pv3gM9JrFFH883SWAhvJC9RPYmo8UNxuFtv5bMMALkm`, ETH `AQCGyheWPLeo6Qp9WpYS9m3Qj479t7R636N9ey1rEjEn`, USDC `G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa`, USDT `4vkNeXiYEUizLdrpdPS1eC2mccyM4NUPRtERrk6ZETkk`. |

The IDL JSON file does NOT include explicit Anchor `seeds` declarations
for PDAs — those live in the on-chain program source, not the
public-facing IDL. PDA seeds documented here come from
`generate-position-and-position-request-pda.ts` in the parsing repo,
which the Jupiter team maintains and uses against the live program.

### Anchor discriminator rule

Discriminator = `sha256("global:<snake_case_ix_name>")[..8]`.

The IDL emits camelCase names (`addLiquidity2`,
`createIncreasePositionMarketRequest`); the on-chain program defines
those handlers in snake_case (`add_liquidity_2`,
`create_increase_position_market_request`). The snake_case form is what
goes into the discriminator preimage.

---

## §2. JLP buy flow (`add_liquidity_2`)

Direct deposit into the JLP pool, mint JLP at NAV. **No Jupiter
aggregator route required** — `add_liquidity_2` is a pool-native ix.

### Discriminator preimage

`sha256("global:add_liquidity_2")[..8]`.

### Account list (in order; 14 accounts)

```
[ 0] owner                          Signer        (readonly per IDL)
[ 1] funding_account                writable      user's input-mint ATA
[ 2] lp_token_account               writable      user's JLP ATA
[ 3] transfer_authority             readonly      PDA(["transfer_authority"])
[ 4] perpetuals                     readonly      PDA(["perpetuals"])
[ 5] pool                           writable      JLP_POOL
[ 6] custody                        writable      input asset's Custody PDA
[ 7] custody_doves_price_account    readonly      custody.docvesAgOracle (verify field name)
[ 8] custody_pythnet_price_account  readonly      custody.oracle.oracleAccount
[ 9] custody_token_account          writable      custody's SPL vault
[10] lp_token_mint                  writable      JLP_MINT
[11] token_program                  readonly      SPL Token
[12] event_authority                readonly      PDA(["__event_authority"])
[13] program                        readonly      JUPITER_PERPETUALS_PROGRAM_ID (self)
```

### Args (Anchor-serialized after disc)

```
AddLiquidity2Params {
  token_amount_in:        u64,
  min_lp_amount_out:      u64,
  token_amount_pre_swap:  Option<u64>,   // 1 tag byte + 8 if Some
}
```

### Pre/post ixns

- **Compute budget set_unit_limit** + **set_unit_price** must be present
  (or the cluster default 200k CU will time out — `add_liquidity_2`
  consumes ~250-350k on a fresh oracle).
- ATA-create-idempotent for `funding_account` and `lp_token_account`
  before the ix.
- No oracle-refresh ix is required from the caller. Jupiter runs
  keepers that call `refresh_assets_under_management` continuously, and
  the cached `pool.aum_usd` is consumed inside `add_liquidity_2`. If
  AUM is too stale, the program rejects with a custom error; let the
  keeper refresh and retry.

### Slippage

`min_lp_amount_out` is the floor. Compute via the view ix
`getAddLiquidityAmountAndFee2` (or its on-chain function — IDL exposes
it) over a recent slot, then subtract your slippage allowance (50-100
bps typical). Passing `0` disables slippage protection and should never
be used on live submit.

### Compute budget recommendation

600k CU is the canonical envelope (matches Jupiter's own UI).
Priority fee: 10k µ-lamports min on mainnet under normal load.

### Anchor errors that can fire

The IDL JSON includes a long `errors` array; the most likely on
`add_liquidity_2`:
- `PoolAmountLimit` (custody owned at cap)
- `MaxUtilization` (perp utilization too high)
- `PriceTooOld` / oracle staleness
- `MinLpOutBreached` (slippage)
- generic Anchor account-validation errors when oracle pubkeys mismatch
  `custody.oracle.oracleAccount` / `custody.docvesAgOracle`.

---

## §3. Open short hedge flow (`create_increase_position_market_request`)

This is the request half of Jupiter Perps' 2-tx model. The trader
submits the request; a Jupiter keeper picks it up within 1-3 slots and
calls a separate execute ix.

### Important: the v1/v2/Market naming

The current IDL exposes **`createIncreasePositionMarketRequest`**
(snake_case `create_increase_position_market_request`) — NOT a
`_v2`-suffixed name. The historical `createIncreasePositionRequest`
(no suffix) appears to have been retired in favor of split
market-vs-limit request ixs (`createIncreasePositionMarketRequest`
for at-market opens, `instantIncreasePosition` for the same flow with
the new "instant" routing).

Any code that builds `sha256("global:create_increase_position_request_v2")`
will produce a discriminator that does NOT exist in the deployed
program and will fail at the very first sim run with
`InstructionError::InvalidInstructionData`.

### Discriminator preimage

`sha256("global:create_increase_position_market_request")[..8]`.

### Account list (in order; 16 accounts incl. 1 optional)

```
[ 0] owner                          Signer, writable
[ 1] funding_account                writable      user's collateral ATA (input mint)
[ 2] perpetuals                     readonly      PDA(["perpetuals"])
[ 3] pool                           readonly      JLP_POOL (NOTE: readonly here)
[ 4] position                       writable      Position PDA (see seeds below)
[ 5] position_request               writable      PositionRequest PDA (program inits)
[ 6] position_request_ata           writable      ATA(position_request, input_mint)
[ 7] custody                        readonly      position-asset custody (e.g. SOL custody)
[ 8] collateral_custody             readonly      USDC custody for the daemon's path
[ 9] input_mint                     readonly      collateral mint (the SPL Mint account)
[10] referral                       readonly, OPTIONAL — pass `null` / omit if no referral
[11] token_program                  readonly
[12] associated_token_program       readonly
[13] system_program                 readonly
[14] event_authority                readonly
[15] program                        readonly      self
```

Optional accounts: in Anchor an `Option<Pubkey>` account is wired as
EITHER the actual key OR omitted from the account list entirely. The
TS SDK pattern is `.accounts({ referral: null })` — at the Rust ixn
builder level this is equivalent to leaving the slot out. **Do not
fill the slot with `Pubkey::default()`** — Anchor will try to validate
`Pubkey::default()` as an actual account and reject with
`AccountNotInitialized`.

### Args (Anchor-serialized after disc)

```
CreateIncreasePositionMarketRequestParams {
  size_usd_delta:        u64,
  collateral_token_delta: u64,
  side:                  Side,         // u8 variant index — see §3.4
  price_slippage:        u64,          // 6-decimal USD price, NOT bps
  jupiter_minimum_out:   Option<u64>,
  counter:               u64,
}
```

Wire encoding for the params struct, byte-by-byte:
```
[ 0.. 8] size_usd_delta (u64 LE)
[ 8..16] collateral_token_delta (u64 LE)
[   16] side (u8) — 1 = Long, 2 = Short  (NOT 0/1!)
[17..25] price_slippage (u64 LE)
[   25] jupiter_minimum_out option tag (0 = None, 1 = Some)
        if Some: [26..34] u64 LE; counter follows at byte 34
        if None:                     counter follows at byte 26
[..  +8] counter (u64 LE)
```

`price_slippage` is a USD price scaled to 6 decimals (USDC scale), NOT
basis points. For an open: it's the worst acceptable mark price. Pass
`current_price ± 50bps` shifted to 6-decimal scale. See the SDK
comment in `create-market-trade-request.ts`:

> `priceSlippage` here is scaled to 6 decimal places as per the USDC
> mint, so for example if the price of SOL is $100, the value would
> `new BN(100_000_000)`. For shorts and for a lower chance of
> exceeding the price slippage, use a value that is 5-10% higher than
> the current token price.

### §3.4. The `Side` enum encoding

`Side` is an Anchor `#[repr(u8)]` enum with THREE variants in this
declared order:

```
Side::None  = 0
Side::Long  = 1
Side::Short = 2
```

The daemon currently has only `Long/Short` and maps `Long=0, Short=1`
which is wrong on both counts: Long should be `1`, Short should be `2`.

This is also the byte value used inside the Position PDA seed list
(§3.5). A `Side` mismatch corrupts BOTH the params encoding AND the
account PDA, so a wrong `side` byte means: the discriminator parses,
but Anchor's `has_one`/PDA constraint check on `position` fails →
`ConstraintSeeds` error.

### §3.5. Position PDA seeds

```
seeds = [
  b"position",
  owner.as_ref(),
  JLP_POOL.as_ref(),
  custody.as_ref(),
  collateral_custody.as_ref(),
  [side_byte],            // 1 = Long, 2 = Short
]
program_id = JUPITER_PERPETUALS_PROGRAM_ID
```

There are 6 seed slices total. The current daemon code matches this
shape EXCEPT the `side_byte` value (uses 0/1 instead of 1/2).

### §3.6. PositionRequest PDA seeds

```
seeds = [
  b"position_request",
  position.as_ref(),
  counter.to_le_bytes(),  // u64 LE
  [request_change_byte],  // 1 = Increase, 2 = Decrease
]
program_id = JUPITER_PERPETUALS_PROGRAM_ID
```

There are 4 seed slices. The current daemon has only 3 (missing the
trailing `request_change` byte). A keeper executing the request
derives the same PDA from on-chain state including `request_change`
— a missing byte means the daemon's request lands at a PDA that no
keeper will look at, and the keeper-side execute will fail
`AccountNotInitialized` against the address the daemon DID derive.

`counter` is just a randomization nonce (the SDK uses a 30-bit random
u32 cast to u64) to allow concurrent requests against the same
Position. Using `unix_seconds + i` like the daemon does is fine as
long as no two requests collide within the same second.

### §3.7. The 2-tx request-execute model in operational terms

1. Trader signs + submits `create_increase_position_market_request`.
   On-chain: the `position_request` PDA is `init`'d, the user's
   collateral moves from `funding_account` into the
   `position_request_ata`, and the request is queued.
2. A Jupiter keeper picks the request up 1-3 slots later. It calls a
   different on-chain ix (`increasePosition4` or equivalent) that
   reads the request, executes against the live oracle price, debits
   `position_request_ata` into the `collateral_custody.token_account`,
   writes/updates the `Position` PDA, and closes the
   `position_request` account via `closePositionRequest2`.
3. If the price slippage check fails, or oracle is stale, the keeper
   may call `closePositionRequest2` directly without executing —
   collateral is returned to the user's ATA.

**Timeout / cancel path.** There is no auto-timeout. If a keeper
never picks it up (or always rejects on slippage), the user can call
`closePositionRequest2` themselves to reclaim the collateral. The
ix takes the `position_request` + `position_request_ata` + the
user's receiving ATA and an optional `keeper` (signer) slot — when
self-cancelling, omit the keeper slot.

### §3.8. Funding rate / borrow rate

Jupiter Perps does not use traditional funding — instead, each
custody charges an hourly borrow rate (`hourlyFundingDbps` in the
`FundingRateState` struct, in decimal bps i.e. 100k = 100%). The rate
is determined by the Gauntlet jump-rate model on custody utilization.
This is what the daemon's `max_borrow_rate_bps` cap should be checked
against — see §5 of the audit doc.

### Compute budget recommendation

400-600k CU for a single open-request submit. The SDK example uses
`simulation.value.unitsConsumed` to right-size dynamically, then sets
`setComputeUnitLimit` from that.

### Anchor errors that can fire

- `ConstraintSeeds` — wrong Position or PositionRequest PDA
- `AccountNotInitialized` — referral slot filled with default pubkey,
  or position/custody/etc. wrong
- `InvalidInstructionData` — discriminator or params layout wrong
- `MinReserveUtilization` / `MaxReserveUtilization` — pool busy
- `PriceTooOld` — oracle stale (keeper will retry)
- `OpenPositionSizeTooLow` / `MaxLeverage` — sizing rejected

---

## §4. Close short / decrease position flow (`create_decrease_position_market_request`)

Symmetric to §3. Same 2-tx model.

### Discriminator preimage

`sha256("global:create_decrease_position_market_request")[..8]`.

There is ALSO a v2-style `createDecreasePositionRequest2` in the IDL
with a different account list (it adds `custody_doves_price_account`
and `custody_pythnet_price_account` slots) and a richer params struct
that includes `requestType`, `triggerPrice`, `triggerAboveThreshold`.
Use the `Market` variant for market-style full closes (matches what
the daemon's hedge flow needs).

### Account list (in order; 16 accounts incl. 1 optional) — Market variant

```
[ 0] owner                          Signer, writable
[ 1] receiving_account              writable      user's ATA for the desired output mint
[ 2] perpetuals                     readonly      PDA
[ 3] pool                           readonly
[ 4] position                       readonly      (existing Position PDA)
[ 5] position_request               writable      (new PositionRequest PDA, init)
[ 6] position_request_ata           writable      ATA(position_request, desired_mint)
[ 7] custody                        readonly
[ 8] collateral_custody             readonly
[ 9] desired_mint                   readonly      SPL Mint account of the payout token
[10] referral                       readonly, OPTIONAL
[11] token_program                  readonly
[12] associated_token_program       readonly
[13] system_program                 readonly
[14] event_authority                readonly
[15] program                        readonly
```

For the daemon's path `desired_mint = USDC_MINT`.

### Args (Market variant)

```
CreateDecreasePositionMarketRequestParams {
  collateral_usd_delta: u64,         // 0 for proportional
  size_usd_delta:       u64,         // 0 if entire_position=true
  price_slippage:       u64,         // 6-decimal USD scale
  jupiter_minimum_out:  Option<u64>,
  entire_position:      Option<bool>,// Some(true) for full close
  counter:              u64,
}
```

NOTE the param ORDER vs the increase variant: collateral first, size
second (opposite of the increase ix).

### Args (Request2 variant) — informational

```
CreateDecreasePositionRequest2Params {
  collateral_usd_delta: u64,
  size_usd_delta:       u64,
  request_type:         RequestType,    // Market=0 or Trigger=1
  price_slippage:       Option<u64>,
  jupiter_minimum_out:  Option<u64>,
  trigger_price:        Option<u64>,
  trigger_above_threshold: Option<bool>,
  entire_position:      Option<bool>,
  counter:              u64,
}
```

The daemon's current decrease builder serializes 10 fixed-non-option
u64 fields with no Option tags — it does not match EITHER variant.
It will reject at discriminator decode (wrong name) and also at
params decode (wrong field types and order).

### PDA reuse on full close

The Position PDA is the same as the open. The PositionRequest PDA
uses `request_change_byte = 2`. So a decrease-request PDA is DIFFERENT
from the open-request PDA — they coexist briefly.

### Compute budget

400-600k. Same envelope as the open.

---

## §5. Withdraw JLP flow (`remove_liquidity_2`)

Mirror of `add_liquidity_2`. Burns JLP, transfers underlying basket
asset out to `receiving_account`.

### Discriminator preimage

`sha256("global:remove_liquidity_2")[..8]`.

### Account list (in order; 14 accounts)

```
[ 0] owner                          Signer (readonly)
[ 1] receiving_account              writable      user's output-mint ATA
[ 2] lp_token_account               writable      user's JLP ATA (burned from)
[ 3] transfer_authority             readonly
[ 4] perpetuals                     readonly
[ 5] pool                           writable
[ 6] custody                        writable      output asset's custody
[ 7] custody_doves_price_account    readonly
[ 8] custody_pythnet_price_account  readonly
[ 9] custody_token_account          writable
[10] lp_token_mint                  writable
[11] token_program                  readonly
[12] event_authority                readonly
[13] program                        readonly
```

### Args

```
RemoveLiquidity2Params {
  lp_amount_in:   u64,
  min_amount_out: u64,
}
```

A redeem can only output a SINGLE underlying (the one specified by
`custody`). For a full JLP→USDC unwind: pass the USDC custody. The
pool charges a 2-7 bps swap-style fee weighted by the asset's
current vs target weight (see §6).

### Compute budget

600k. Same as the buy leg.

---

## §6. Multi-asset hedging — economics

JLP composition is documented in the Jupiter docs (current weights
are dynamic; the daemon must read live custody balances + oracle
prices to compute deltas). For the hedge to be delta-neutral the
daemon must:

1. Read the live composition (per-custody `assets.owned * oracle_price`).
2. Compute its pro-rata share = `our_jlp_lamports / jlp_total_supply`.
3. For each non-stable custody (SOL, BTC, ETH — there are 3, not 2),
   compute USD exposure and open a Jupiter Perps short of that size.
4. Each short is its own `(custody, collateral_custody, Side::Short)`
   tuple → its own Position PDA → its own PositionRequest at open
   AND another PositionRequest at close.

**Custody table** (mainnet, verified 2026-05-15 from the parsing
repo's `constants.ts`):

| Asset | Custody pubkey                                | Mint                                          | Stable |
| ----- | --------------------------------------------- | --------------------------------------------- | ------ |
| SOL   | `7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz`| `So11111111111111111111111111111111111111112`| no     |
| BTC   | `5Pv3gM9JrFFH883SWAhvJC9RPYmo8UNxuFtv5bMMALkm`| (wBTC portal)                                 | no     |
| ETH   | `AQCGyheWPLeo6Qp9WpYS9m3Qj479t7R636N9ey1rEjEn`| (wETH portal)                                 | no     |
| USDC  | `G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa`| USDC                                          | yes    |
| USDT  | `4vkNeXiYEUizLdrpdPS1eC2mccyM4NUPRtERrk6ZETkk`| USDT                                          | yes    |

The daemon's `synthetic_custody` pattern (everything pointing at the
USDC custody address) is fundamentally broken for live submit — every
real custody field MUST be the real on-chain custody PDA's decoded
contents. The audit-fix C3 synthetic-check is a correct guard.

### §6.1. Custody account decoding — CRITICAL OFFSET BUG

The Anchor IDL's `Custody.assets` field is a nested struct with this
field order:

```
Assets {
  feesReserves:             u64,
  owned:                    u64,
  locked:                   u64,
  guaranteedUsd:            u64,
  globalShortSizes:         u64,
  globalShortAveragePrices: u64,
}
```

The daemon's `Assets` decoder reads in this order:

```
locked, owned, guaranteed_usd, global_short_sizes, global_short_average_prices
```

at fixed byte offsets 1080, 1088, 1096, 1104, 1112. This is **wrong
on two counts**:
1. The order is swapped: `feesReserves` comes first in the IDL, NOT
   `locked` or `owned`. The daemon reads `feesReserves` and labels it
   `locked`; reads `owned` and labels it `locked` (offset error
   cascades — same shape as the kamino-loader `borrowed_amount_sf`
   bug).
2. The daemon omits `feesReserves` entirely from its `Assets` struct,
   so even a corrected offset table can't round-trip without adding
   the field.

The base offset of the assets block within Custody also needs
re-verification. The variable-length `permissions` field upstream
of `assets` makes hard-coded offsets fragile. Recommended: use the
Anchor account `discriminator + AnchorDeserialize` round-trip
instead of byte-offset reads.

### §6.2. Borrow rate / `FundingRateState`

The IDL defines `Custody.fundingRateState`:

```
FundingRateState {
  cumulativeInterestRate: u128,
  lastUpdate:             i64,
  hourlyFundingDbps:      u64,  // decimal-bps; 100_000 = 100% APR
}
```

This is where the rebalancer's borrow-rate watch must read. Daemon's
`decode_custody_borrow_rate_bps` currently returns `None`
unconditionally — the borrow-rate watch is effectively disabled.

Conversion to bps:

```
hourly_dbps = funding_rate_state.hourlyFundingDbps;  // decimal-bps per hour
hourly_bps  = hourly_dbps / 10;
annual_bps  = hourly_bps * 24 * 365;   // ≈ 8760× hourly
```

---

## §7. Address Lookup Tables for Jupiter Perps

Jupiter Perps' larger ixs (especially `increasePosition4` /
`decreasePosition4` which the keepers execute) are above the 1232-byte
legacy-tx ceiling. The user-facing REQUEST ixs are smaller (16
accounts × 32 bytes = 512 + discriminator + params + 64-byte sig +
1-byte counts ≈ 700-800 bytes total) — they fit in a v0 tx WITHOUT
an ALT, but only just.

**Authoritative source for an ALT pubkey.** The parsing repo's
example (`create-market-trade-request.ts`) does NOT use an ALT — it
builds a v0 message with `compileToV0Message([])` (empty ALT list).
This is sufficient for a single request ix. The Jupiter team has
historically published a per-program ALT
(`PERPS_ALT` ≈ pool + custodies + oracles + perpetuals PDA + event
authority) but the address is not in the IDL JSON or the parsing
repo as of 2026-05-15. **Action item for Session B:** before mainnet
submit, capture the ALT pubkey from a recent on-chain successful
Jupiter Perps tx (look at the `addressLookupTableAddresses` field on
a known-good transaction).

For now, the daemon's strategy of NOT using an ALT is acceptable for
the request half (open + close) because each tx contains only a
single request ix.

### §7.1. Compute budget vs. tx-size budget

Note these are TWO independent ceilings:
- **Compute budget**: ~1.4M CU max per tx; we set 400-600k.
- **Tx size**: 1232 bytes for legacy, 1232 + 32×ALT-keys for v0.

A v0 message with no ALT for a single 16-account ix should land at
~750 bytes. Adding a single compute-budget ix is ~30 bytes. ATA-create
is ~120 bytes. So a typical open-request tx is ~900 bytes — safe.

---

## §8. Recommended bundle template

### Open hedge short (one asset)

```
1. ComputeBudgetProgram.SetComputeUnitLimit(600_000)
2. ComputeBudgetProgram.SetComputeUnitPrice(<priority fee>)
3. ATA-create idempotent(USDC ATA for owner)
4. create_increase_position_market_request(
     accounts: [owner, fundingAccount=usdc_ata, perpetuals,
                pool, position, position_request,
                position_request_ata=ATA(position_request, USDC),
                custody=ASSET_CUSTODY, collateral_custody=USDC_CUSTODY,
                input_mint=USDC, /* referral: OMIT */,
                token_program, ata_program, system_program,
                event_authority, program],
     params: { size_usd_delta, collateral_token_delta,
               side=Side::Short (u8=2),
               price_slippage = current_mark + 100bps (in 6-dec USD),
               jupiter_minimum_out = None,
               counter = randomU64 })
```

PDAs derived BEFORE the ix call:
- `position = PDA(["position", owner, JLP_POOL, ASSET_CUSTODY, USDC_CUSTODY, [2]])`
- `position_request = PDA(["position_request", position, counter.to_le_bytes(), [1]])`

### Close hedge short (one asset, full close)

```
1. ComputeBudgetProgram.SetComputeUnitLimit(600_000)
2. ComputeBudgetProgram.SetComputeUnitPrice(<priority fee>)
3. ATA-create idempotent(USDC ATA for owner)
4. create_decrease_position_market_request(
     accounts: [owner, receivingAccount=usdc_ata, perpetuals,
                pool, position (the EXISTING one from open),
                position_request (NEW PDA with request_change=2),
                position_request_ata=ATA(new_pos_req, USDC),
                custody=ASSET_CUSTODY, collateral_custody=USDC_CUSTODY,
                desired_mint=USDC, /* referral: OMIT */,
                token_program, ata_program, system_program,
                event_authority, program],
     params: { collateral_usd_delta=0, size_usd_delta=0,
               price_slippage = current_mark - 100bps (Short ↔ better price = lower),
               jupiter_minimum_out = None,
               entire_position = Some(true),
               counter = randomU64 })
```

PDA derived BEFORE the ix call:
- `position_request_close = PDA(["position_request", position, counter.to_le_bytes(), [2]])`

### Buy JLP (deposit USDC)

```
1. ComputeBudgetProgram.SetComputeUnitLimit(600_000)
2. ComputeBudgetProgram.SetComputeUnitPrice(<priority fee>)
3. ATA-create idempotent(USDC ATA)
4. ATA-create idempotent(JLP ATA)
5. add_liquidity_2(
     accounts: [owner, funding=usdc_ata, lp_token_account=jlp_ata,
                transfer_authority, perpetuals, pool,
                custody=USDC_CUSTODY,
                custody_doves_price_account=<read from USDC custody>,
                custody_pythnet_price_account=<read from USDC custody>,
                custody_token_account=<read from USDC custody>,
                lp_token_mint=JLP_MINT, token_program,
                event_authority, program],
     params: { token_amount_in, min_lp_amount_out = quote * (1 - slippage),
               token_amount_pre_swap = None })
```

### Withdraw JLP (redeem to USDC)

Same shape as buy, with `remove_liquidity_2` discriminator and
`receiving_account` (USDC ATA) in slot [1] in place of `funding`.
Pass the USDC custody for `custody` to redeem into USDC. `params =
{ lp_amount_in, min_amount_out }`.

---

## §9. Live verification checklist before any mainnet submit

For each ixn the daemon emits, run this checklist:

1. **Discriminator round-trip**: take the daemon's first 8 bytes of
   `Instruction.data`, look up the ix in the IDL by computing
   `sha256("global:<snake_case_name>")[..8]` for each entry, confirm
   match.
2. **Account count**: matches IDL's `accounts.length` minus omitted
   optionals.
3. **Account order**: compare slot-by-slot to the IDL list.
4. **PDA seeds**: derive each PDA in a unit test using the seed list
   in this doc, compare to what the daemon emits.
5. **Params round-trip**: emit the daemon's params bytes, decode with
   Anchor IDL types, confirm field-for-field match.
6. **Live custody fetch**: decode a real Custody account body via RPC
   using AnchorDeserialize (NOT byte offsets), confirm
   `assets.feesReserves / owned / locked / guaranteedUsd /
   globalShortSizes / globalShortAveragePrices` round-trip.
