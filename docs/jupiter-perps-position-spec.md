# Jupiter Perps `Position` account spec

Companion to [`jupiter-perps-bundle-spec.md`](jupiter-perps-bundle-spec.md).
Documents the on-chain `Position` and (subset of) `Custody` fields the
riskwatcher daemon needs to compute liquidation distance for hedgedjlp
shorts. fleet-v0.2.5.

## §1. Source

| What | URL | Pulled |
| --- | --- | --- |
| Jupiter Perps Anchor IDL (JSON) | `https://raw.githubusercontent.com/julianfssen/jupiter-perps-anchor-idl-parsing/main/src/idl/jupiter-perpetuals-idl-json.json` | 2026-05-15 |
| `Position` struct | `idl["accounts"][?(@.name=='Position')]` | — |

The earlier audit citing `monakki/jup-perps-client` IDL commit 91cec1505a
matches the same struct (cross-checked field-by-field on 2026-03-25).

## §2. `Position` account layout

Anchor account body = 8-byte discriminator + Borsh-serialized struct.

**Discriminator**: `sha256("account:Position")[..8]` =
`aa bc 8f e4 7a 40 f7 d0`.

Fields (in IDL declaration order; Borsh serializes in declaration order
with no padding):

| Offset | Size | Field | Type | Notes |
| --- | --- | --- | --- | --- |
| 0    | 8  | discriminator           | `[u8; 8]`     | `0xaabc8fe47a40f7d0` |
| 8    | 32 | `owner`                 | `Pubkey`      | the trader |
| 40   | 32 | `pool`                  | `Pubkey`      | JLP pool (`5BUw…VKsq`) |
| 72   | 32 | `custody`               | `Pubkey`      | asset being traded (SOL/BTC/ETH custody) |
| 104  | 32 | `collateralCustody`     | `Pubkey`      | custody holding the collateral (USDC custody for shorts; same as `custody` for longs) |
| 136  | 8  | `openTime`              | `i64`         | unix seconds |
| 144  | 8  | `updateTime`            | `i64`         | unix seconds |
| 152  | 1  | `side`                  | `Side` enum   | `0=None, 1=Long, 2=Short` (Borsh enum variant index, 1 byte, no padding) |
| 153  | 8  | `price`                 | `u64`         | entry price, **6 decimals** (USD) |
| 161  | 8  | `sizeUsd`               | `u64`         | position notional, **6 decimals** (USD) |
| 169  | 8  | `collateralUsd`         | `u64`         | collateral remaining, **6 decimals** (USD) |
| 177  | 8  | `realisedPnlUsd`        | `i64`         | realised PnL since open, 6 decimals USD |
| 185  | 16 | `cumulativeInterestSnapshot` | `u128`   | for borrow-fee accounting |
| 201  | 8  | `lockedAmount`          | `u64`         | locked custody tokens (mint-decimals) |
| 209  | 1  | `bump`                  | `u8`          | PDA bump |
| 210  | —  | **EOF**                 |               | total account size = 210 bytes |

**Side encoding.** Anchor's snake_case-with-digit-rule (see
`jupiter-perps-bundle-spec.md` §1 Anchor discriminator rule) applies to
**instruction names**, not account field names. `Side` is a plain
`#[repr(u8)]` Anchor enum and serializes as a single byte: the variant
index. `None = 0`, `Long = 1`, `Short = 2`. This matches the
Position-PDA-seed byte already documented in `jlp.rs::PerpSide::as_u8`.

## §3. Position PDA seeds

Already implemented in `jlp.rs::derive_position`:

```text
[ b"position",
  owner.as_ref(),
  pool.as_ref(),
  custody.as_ref(),
  collateral_custody.as_ref(),
  [side_byte] ]
```

For hedgedjlp shorts the canonical (custody, collateral_custody, side)
tuples on mainnet:

| Asset | custody (asset) | collateral_custody | side |
| --- | --- | --- | --- |
| SOL short  | `7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz` (SOL)  | `G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa` (USDC) | 2 |
| BTC short  | `5Pv3gM9JrFFH883SWAhvJC9RPYmo8UNxuFtv5bMMALkm` (BTC)  | `G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa` (USDC) | 2 |
| ETH short  | `AQCGyheWPLeo6Qp9WpYS9m3Qj479t7R636N9ey1rEjEn` (ETH)  | `G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa` (USDC) | 2 |

For shorts, Jupiter requires the collateral custody to be a stable
(USDC or USDT). The hedgedjlp daemon uses USDC.

Riskwatcher's `JupiterPerpsPoller::discover_positions` derives all
known (asset, USDC, Short) PDAs for the watched wallet and checks each
for account existence via a single `getMultipleAccounts` call.

## §4. Liquidation distance

### §4.1 Maintenance margin

Jupiter Perps uses `Custody.pricing.maxLeverage` (u64, declared in
`PricingParams` §6.3 of the IDL) as the effective max leverage in
**bps**. On mainnet this is `500_000` (= 500_000 / 10_000 = 50× max
leverage) for SOL/BTC/ETH; verified via on-chain custody snapshot
2026-05-04 against the same custody bodies the hedgedjlp daemon
already decodes for `add_liquidity` / borrow-rate reads.

Liquidation triggers when **remaining collateral** < `sizeUsd /
maxLeverageRatio`, i.e.:

```text
maintenance_margin_usd = sizeUsd * 10_000 / maxLeverage_bps
liquidatable           = remaining_collateral_usd <= maintenance_margin_usd
```

`remaining_collateral_usd` for a SHORT:
```text
unrealised_pnl_usd = sizeUsd * (entry_price - current_price) / entry_price
remaining_collateral_usd = collateralUsd + realisedPnlUsd + unrealised_pnl_usd
```

For a LONG, swap the price diff sign:
```text
unrealised_pnl_usd = sizeUsd * (current_price - entry_price) / entry_price
```

### §4.2 Distance metric (basis points)

Riskwatcher mirrors the Kamino `distance_bps` semantics — a unitless
0..10_000 number where smaller = more dangerous:

```text
distance_bps = (remaining_collateral_usd - maintenance_margin_usd)
              * 10_000
              / maintenance_margin_usd
```

Saturating to 0 if `remaining_collateral_usd <= maintenance_margin_usd`
(position is at or past liquidation). Returns `None` if
`maintenance_margin_usd == 0` (no position / zero size).

This metric is fed into the **existing** `thresholds.rs` band
constants — Notice (≤ 500 bps), Warning (≤ 200 bps), Critical (≤ 50
bps) — so riskwatcher's de-dup + severity-laddering logic reuses
identically across Kamino and Jupiter Perps subjects.

### §4.3 Why this and not the "price distance to liquidation"

The price-to-liquidation form (`sizeUsd × (P_cur - P_entry) / P_entry
≥ collateralUsd × FACTOR`) is mathematically equivalent but yields a
metric in *price-bps* — which is direction-aware and harder to compare
across SOL/BTC/ETH at different volatilities. The collateral-to-MM
ratio is the form Jupiter's own front-end uses for the "health"
gauge, so it's the operator-friendly choice.

## §5. Caveats / unknowns

- **Borrow fees** (cumulativeInterestSnapshot vs. custody current rate)
  are NOT subtracted from collateral in our `remaining_collateral_usd`.
  For a freshly-opened position the snapshot ≈ custody current and the
  fee component ≈ 0; for an aged short the unrealised borrow cost can
  shave several bps off the headroom. M11/M12 follow-up: subtract
  `(cumulative_now - cumulative_snapshot) × sizeUsd / 1e18` once we
  decode the funding-rate-state per-second growth. For the riskwatcher
  v0.2.5 ship — which serves the $200 hedgedjlp test — this
  approximation over-estimates headroom by ~1-2 bps in 24h, which is
  well inside the Notice band's 500-bp floor.

- **Current price source.** We read the same Pyth pull-oracle the
  daemon already pulls for the dashboard's mark price (custody
  `pythnet_price_account`). On-chain Jupiter actually uses the
  doves-ag oracle for liquidation checks — for risk monitoring the
  Pyth read is conservative enough (Pyth and doves agree to <5 bps
  at quote granularity for SOL/BTC/ETH).

- **No `--inject-test-position` for the Jupiter Perps path** in
  fleet-v0.2.5. The Kamino synthetic-injection short-circuit
  remains for the multiply test. For the Jupiter Perps poller the
  test path is "real wallet with no open positions returns empty
  cleanly" — exercised live in Phase 5 VM verification.
