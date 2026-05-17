# Kamino Loader Audit (post-v0.1.23)

**Verdict: v0.1.23 still has bugs. DO NOT BROADCAST. Suggest v0.1.24.**

The borrow-slot field offsets (slot+88 / +104 / +120) ARE correct, but two other
constants in the same module are wrong by enough to corrupt the LTV calc and the
2nd–5th borrow slot decode:

1. `OBLIGATION_BORROW_SLOT_SIZE = 184` — actual on-chain slot is **200 bytes**.
2. `OBLIGATION_AGGREGATE_OFFSET = 2128` — actual offset is **2208** (off by 80).

The multiply obligation `4eHLdZpN59…UTjkK` will still decode its single borrow
correctly because slot index 0 starts at the right base (1208) and the per-slot
field offsets (88/104/120) match the ground truth. But every aggregate-field
read (LTV math, health-factor math) is reading garbage from the middle of
`borrows[4]`.

## Authoritative source

* Repo: https://github.com/Kamino-Finance/klend
* Branch/ref used: `master` @ `4c7653a12276ded3bcaf95a3474973ca135ca810`
* Canonical client-side type definitions (the ones whose `repr(C)` layout matches
  the on-chain account):
  * `libs/klend-interface/src/state/obligation.rs`
  * `libs/klend-interface/src/state/common.rs`  (defines `BigFractionBytes`, `LastUpdate`)
  * `libs/klend-interface/src/state/reserve.rs`
* Compile-time size assertion in source: `assert!(size_of::<Obligation>() == 3336)`.

The `programs/klend/src/state/obligation.rs` file currently in the repo also
defines `Obligation` but uses raw `u128`/`u64` instead of `PodU128` — both forms
produce the SAME `repr(C)` byte layout. The interface crate is the source of
truth for byte sizes because it adds `const _: () = assert!(size_of == 3336)`.

## Building-block sizes (verified)

| Type | Size | Source |
|---|---|---|
| `LastUpdate` | 16 bytes | common.rs (slot u64 + stale u8 + price_status u8 + placeholder [u8;6]) |
| `BigFractionBytes` | **48 bytes** | common.rs: `value: [u64;4] + padding: [u64;2]` — NOT 56 |
| `PodU128` | 16 bytes, align 1 | spl_pod (repr(transparent) over `[u8;16]`) |
| `FixedTermBorrowRolloverConfig` | 16 bytes | obligation.rs (u8×4 + u32 + u64) |
| `Pubkey` | 32 bytes | solana |

**Key surprise:** `BigFractionBytes` is 48 bytes, not 56. The v0.1.23 commit
message and comments say "bsf is 56 bytes". That number is wrong but the
mistake happens to cancel out for the borrow slot's `borrowed_amount_sf`
offset because we ALSO didn't account for `last_borrowed_at_timestamp: u64`
sitting between bsf and borrowed_amount_sf. 48 + 8 = 56, so the next field
lands at the same place — by coincidence.

## ObligationLiquidity (borrow slot) — PARTIALLY VERIFIED

Canonical struct (`libs/klend-interface/src/state/obligation.rs`):

```rust
pub struct ObligationLiquidity {
    pub borrow_reserve: Pubkey,                              // 32
    pub cumulative_borrow_rate_bsf: BigFractionBytes,        // 48
    pub last_borrowed_at_timestamp: u64,                     //  8
    pub borrowed_amount_sf: PodU128,                         // 16
    pub market_value_sf: PodU128,                            // 16
    pub borrow_factor_adjusted_market_value_sf: PodU128,     // 16
    pub borrowed_amount_outside_elevation_groups: u64,       //  8
    pub fixed_term_borrow_rollover_config: ...,              // 16
    pub borrowed_amount_at_expiration: u64,                  //  8
    pub padding2: [u64; 4],                                  // 32
}
// total = 200 bytes
```

| Field | Canonical slot-relative offset | v0.1.23 uses | Status |
|---|---|---|---|
| `borrow_reserve` | 0..32 | 0..32 | OK |
| `cumulative_borrow_rate_bsf` | 32..80 (48 bytes) | "32..88 (56 bytes)" per comment | comment WRONG, fields OK by coincidence |
| `last_borrowed_at_timestamp` | 80..88 | not read; folded into bsf | OK (not read) |
| `borrowed_amount_sf` | **88..104** | 88..104 | **OK** ✓ |
| `market_value_sf` | **104..120** | 104..120 | **OK** ✓ |
| `borrow_factor_adjusted_market_value_sf` | **120..136** | 120..136 | **OK** ✓ |
| ...padding | 136..200 | n/a | not read |
| **TOTAL SLOT SIZE** | **200** | **184** | **WRONG** ❌ |

**Implication:** For slot 0, all fields read correctly because the base offset
is right and the field positions within the slot are right. For slots 1..4 the
base offset is wrong by `i × 16` bytes (16, 32, 48, 64), so any obligation with
≥ 2 borrows mis-reads slots 2+.

## ObligationCollateral (deposit slot) — VERIFIED ✓

Canonical:

```rust
pub struct ObligationCollateral {
    pub deposit_reserve: Pubkey,                                                       // 32
    pub deposited_amount: u64,                                                         //  8
    pub market_value_sf: PodU128,                                                      // 16
    pub borrowed_amount_against_this_collateral_in_elevation_group: u64,               //  8
    pub padding: [u64; 9],                                                             // 72
}
// total = 136 bytes
```

| Field | Canonical | v0.1.23 uses | Status |
|---|---|---|---|
| `deposit_reserve` | 0..32 | 0..32 | OK |
| `deposited_amount` | 32..40 | 32..40 | OK |
| `market_value_sf` | 40..56 | 40..56 | OK |
| SLOT SIZE | 136 | 136 | OK |

## Obligation top-level — aggregate fields — WRONG

Canonical struct offsets (struct-relative; account = struct + 8 for Anchor disc):

```
tag:               0..8
last_update:       8..24
lending_market:    24..56
owner:             56..88
deposits[8]:       88..1176        (8 × 136)
lowest_reserve_..: 1176..1184      (u64)
deposited_value:   1184..1200      (PodU128)
borrows[5]:        1200..2200      (5 × 200)   <-- v0.1.23 assumes 5 × 184 = 920
bfa_debt_sf:       2200..2216      (PodU128)
borrowed_assets_market_value_sf: 2216..2232
allowed_borrow_value_sf:         2232..2248
unhealthy_borrow_value_sf:       2248..2264
```

Adding 8 for the Anchor account discriminator:

| Constant | Canonical (account offset) | v0.1.23 has | Status |
|---|---|---|---|
| `OBLIGATION_LENDING_MARKET_OFFSET` | 32 | 32 | OK |
| `OBLIGATION_OWNER_OFFSET` | 64 | 64 | OK |
| `OBLIGATION_DEPOSITS_OFFSET` | 96 | 96 | OK |
| `OBLIGATION_DEPOSIT_SLOT_SIZE` | 136 | 136 | OK |
| `OBLIGATION_DEPOSITED_VALUE_OFFSET` | 1192 | 1192 | OK |
| `OBLIGATION_BORROWS_OFFSET` | 1208 | 1208 | OK |
| `OBLIGATION_BORROW_SLOT_SIZE` | **200** | **184** | **WRONG** ❌ |
| `OBLIGATION_AGGREGATE_OFFSET` | **2208** | **2128** | **WRONG** (off by 80) ❌ |

`OBLIGATION_AGGREGATE_OFFSET = 2128` actually points at the *middle of
`borrows[4].borrow_factor_adjusted_market_value_sf`* — but since slot 4 is
empty for the live multiply obligation, you read 64 zero bytes there. So:

* `borrow_factor_adjusted_debt_value_sf` → 0
* `borrowed_assets_market_value_sf`     → 0
* `allowed_borrow_value_sf`             → 0
* `unhealthy_borrow_value_sf`           → 0

`query_position_ltv_bps` divides `borrowed_assets_market_value_sf / deposited_value_sf`
which gives 0/X = 0. RiskWatcher will report LTV = 0 for every multiply
position. The leverage loop's "soft veto on Critical Escalate" never trips
because LTV never crosses a threshold (it's always 0). Liquidation distance
math is unreliable.

For stable-yield (deposits only, zero borrows) the aggregate fields are
genuinely zero on chain, so the buggy decode happens to return the right
answer (0). That's the "stable-yield renders correctly" coincidence.

## Reserve constants — VERIFIED ✓

Cross-checked against `libs/klend-interface/src/state/reserve.rs` (struct asserts
`size_of::<Reserve>() == 8616`):

```
version:                        0..8       acct  8..16
last_update:                    8..24      acct 16..32
lending_market:                 24..56     acct 32..64       OK
farm_collateral:                56..88     acct 64..96       OK
farm_debt:                      88..120    acct 96..128      OK
ReserveLiquidity (1232 bytes) starting struct 120 / acct 128:
  mint_pubkey:                  acct 128..160
  supply_vault:                 acct 160..192     OK (LIQUIDITY_SUPPLY_VAULT_OFFSET=160)
  fee_vault:                    acct 192..224     OK (LIQUIDITY_FEE_VAULT_OFFSET=192)
  total_available_amount(u64):  acct 224..232     OK (LIQUIDITY_AVAILABLE_AMOUNT_OFFSET=224)
  borrowed_amount_sf(u128):     acct 232..248     OK (LIQUIDITY_BORROWED_AMOUNT_SF_OFFSET=232)
  ...
ReserveLiquidity struct ends + 150 u64 padding → ReserveCollateral at struct 2552/acct 2560:
  mint_pubkey:                  acct 2560..2592   OK (COLLATERAL_MINT_OFFSET=2560)
  mint_total_supply(u64):       acct 2592..2600   OK (COLLATERAL_MINT_TOTAL_SUPPLY_OFFSET=2592)
  supply_vault:                 acct 2600..2632   OK (COLLATERAL_SUPPLY_VAULT_OFFSET=2600)
```

All Reserve constants verified correct. `SCOPE_ORACLE_OFFSET = 5112` was empirically
verified against mainnet USDC reserve and the byte at that offset has the expected
Scope prices pubkey; not re-derived from source here because the ReserveConfig
internals (TokenInfo etc.) weren't fetched, but it's already validated by previous
mainnet check. Leave as-is.

## Discriminator — VERIFIED ✓

The struct uses `#[discriminator_hash_input("account:Obligation")]` and
`#[discriminator_hash_input("account:Reserve")]`. The 8-byte SHA-256 prefixes
are exactly what v0.1.23 hardcodes. No change needed.

## Walk-through: live multiply obligation `4eHLdZpN59…UTjkK` with v0.1.23 decoder

| Read | Account offset | Decodes to | Correct? |
|---|---|---|---|
| discriminator | 0..8 | `a8 ce 8d 6a 58 4c ac a7` | ✓ |
| lending_market | 32..64 | KAMINO_MAIN_MARKET | ✓ |
| owner | 64..96 | multiply agent identity | ✓ |
| deposits[0].reserve | 96..128 | jitoSOL reserve `EVbyPKrHG…` | ✓ |
| deposits[0].deposited_amount | 128..136 | 90_700_000 cTokens | ✓ |
| deposits[0].market_value_sf | 136..152 | ~$11 USD scaled | ✓ |
| deposits[1..7] | empty slots | skipped | ✓ |
| deposited_value_sf | 1192..1208 | ~$11 USD scaled | ✓ |
| borrows[0].reserve | 1208..1240 | SOL reserve `d4A2prbA…` | ✓ |
| borrows[0].borrowed_amount_sf | 1296..1312 (1208+88) | 16_666_666 << 60 | ✓ |
| borrows[0].market_value_sf | 1312..1328 (1208+104) | ~$1.55 USD scaled | ✓ |
| borrows[0].bfa_market_value_sf | 1328..1344 (1208+120) | ~$1.55 × BF scaled | ✓ |
| borrows[1] base | **1392** (=1208+184) | reads INSIDE canonical borrows[0].padding | reads 32 zeros — slot is treated as empty and skipped | ✓ silently OK |
| borrow_factor_adjusted_debt_value_sf | 2128..2144 | reads INSIDE canonical borrows[4].bfa_market_value_sf — that slot is empty → 0 | **WRONG** (should read real BFA debt ≈ $1.55) |
| borrowed_assets_market_value_sf | 2144..2160 | 0 | **WRONG** |
| allowed_borrow_value_sf | 2160..2176 | 0 | **WRONG** |
| unhealthy_borrow_value_sf | 2176..2192 | 0 | **WRONG** |

**Net effect:** the per-borrow detail is correct (so the leverage loop's
"how much SOL did I borrow last round" read is fine), but every health/LTV
check is reading zeros. RiskWatcher will see LTV = 0 forever and never escalate.

## Required fix for v0.1.24

```rust
const OBLIGATION_BORROW_SLOT_SIZE: usize = 200;      // was 184
const OBLIGATION_AGGREGATE_OFFSET: usize = 2208;     // was 2128
```

Also update the doc comments in the file (the `BigFractionBytes is 56 bytes`
note in the header block is wrong; it's 48 + the adjacent `last_borrowed_at_timestamp`
u64 = 56 from reserve start to `borrowed_amount_sf`). A precise comment helps
the next reviewer.

Tests to add:

1. A unit test that round-trips a 2-borrow obligation and asserts both borrow
   slots decode correctly (would have caught this).
2. A unit test that writes non-zero aggregate fields at offset **2208** and
   asserts they decode (would have caught the aggregate offset bug).
3. Optionally: a `const _: () = assert!(<computed obligation size> >= 3344)`
   sanity check derived from MIN_SIZE = 2208 + 64 = 2272, plus discriminator =
   2280. (Real account data is 3344 = 3336 struct + 8 disc — could pin that.)

## Confidence rating

**v0.1.23 still has bug at `OBLIGATION_BORROW_SLOT_SIZE` and
`OBLIGATION_AGGREGATE_OFFSET`. Suggest v0.1.24 with the two-line edit above.**

The good news: the per-borrow-slot field offsets within slot 0 are correct, so
the leverage loop's read of "what did I just borrow" works. The bad news: every
risk/health aggregate is zero, RiskWatcher will not escalate, and a 2-borrow
obligation (not the current test case but realistic if someone draws on two
reserves) will decode garbage from slot 1 onward.
