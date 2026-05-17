# Multiply Unwind (lever-down) — implementation plan

Status: research / design only. **No code in this commit.** Companion to
`docs/emergency-withdraw-implementation-plan.md` §3.3 / §3.4 — that doc
recommended the scope-cut variant (pause + sweep liquid only) for v1
emergency-withdraw because multiply has no unwind path. This plan
specifies the **full unwind** so multiply can land in a future commit
and the emergency-withdraw handler can call it.

The lever-up grind cost 14 releases (v0.1.7 → v0.1.21) of iterative
klend ixn-shape debugging. This plan exists so lever-down lands in one
cohesive pass.

---

## §1 Authoritative ground truth

All paths absolute. Line numbers as of 2026-05-16.

### 1.1 klend program (Github, master)

- `programs/klend/src/handlers/handler_repay_obligation_liquidity.rs`
  - `RepayObligationLiquidity` accounts struct (9 fields). Verbatim verified.
  - `check_refresh_ixs!(ctx.accounts, ctx.accounts.repay_reserve, ReserveFarmKind::Debt)`.
  - Handler body: load mutable reserve+obligation, compute repay amount
    (incl. accumulated penalties), transfer tokens, verify post balances.
- `programs/klend/src/handlers/handler_withdraw_obligation_collateral_and_redeem_reserve_collateral.rs`
  - `WithdrawObligationCollateralAndRedeemReserveCollateral` struct
    (13 fields, incl. `lending_market_authority`).
  - `check_refresh_ixs!(... withdraw_reserve, ReserveFarmKind::Collateral)`.
  - Conditionally closes the obligation account when inactive after withdraw.
- `programs/klend/src/lib.rs` — confirms v2 entry points exist:
  - `repay_obligation_liquidity_v2(...)`
  - `withdraw_obligation_collateral_v2(...)`
  - Both wrapped with `#[access_control(emergency_mode_disabled(...))]`.
  - v1 variants marked deprecated since program v1.8.0 — Kamino directs
    integrators to v2.
- `programs/klend/src/handlers/handler_repay_and_withdraw_redeem.rs`
  - Combined `RepayAndWithdraw` struct: nests `RepayObligationLiquidity`
    + `WithdrawObligationCollateralAndRedeemReserveCollateral` + two
    `OptionalObligationFarmsAccounts` + Farms program. Process signature:
    `(repay_amount: u64, withdraw_collateral_amount: u64)`. **This is
    klend's own canonical "deleverage in one CPI" entry point.**
  - Cross-constraints: same owner, same obligation, same lending_market.
- `programs/klend/src/utils/refresh_ix_utils.rs::check_refresh` — same
  `[N-2]=RefreshReserve, [N-1]=RefreshObligation` pattern as borrow/
  deposit. Discussed at length in `docs/kamino-klend-bundle-spec.md`.

### 1.2 klend-sdk leverage path (Github, master)

- `src/leverage/operations.ts::buildWithdrawWithLeverageIxs` — the production
  deleverage routine.
- **Single tx, flash-loan based:** the SDK's deleverage is NOT iterative.
  Verified pattern (verbatim from sdk):
  1. setup ATAs, compute-budget ixns
  2. fill wSOL ATA (when debt is SOL)
  3. `flashBorrowReserveLiquidity(debt_reserve, repay_amount)`
  4. `repay_obligation_liquidity` against the just-flash-borrowed debt
  5. `withdraw_obligation_collateral_and_redeem_reserve_collateral`
  6. swap collateral → debt token (Jupiter swap or DEX)
  7. `flashRepayReserveLiquidity(debt_reserve, repay_amount + fee)`
  8. close wSOL ATA
  9. apply compute-budget
- The flash loan is hosted by the **debt reserve** (SOL reserve for us),
  amount = exact repay amount in debt-token lamports. Pair index passed
  to `flashRepay` matches our existing `flash_borrow_reserve_liquidity_ix`
  and `flash_repay_reserve_liquidity_ix` builders.

### 1.3 Local fleet code we mirror / extend

- `crates/multiply-daemon/src/leverage.rs:67-342` — lever-UP. The state-
  machine + bump-locally pattern is the template for lever-DOWN.
- `crates/multiply-daemon/src/leverage.rs:377-525` — `build_lever_up_ixns`
  — the pure per-round bundle builder; lever-down's per-round builder
  mirrors this shape.
- `crates/multiply-daemon/src/seed.rs:230-392` — `maybe_seed_obligation`
  — first-touch obligation hydration. Lever-down has no equivalent
  (obligation already exists when we unwind), but the load-reserve +
  load-jito-pool prelude pattern is reused.
- `crates/multiply-daemon/src/dispatch.rs:161-324` — `run(...)` inbox loop.
  Currently handles `Assign`, `Approve`, `Escalate`, `Beacon`. **No
  `Withdraw` arm.** We add `MsgType::WithdrawMultiply` here.
- `crates/multiply-daemon/src/caps.rs:1-107` — caps. **No
  `validate_withdraw_multiply`.**
- `crates/multiply-daemon/src/kamino.rs:63-83` — `whitelist_program_ids()`:
  klend, Kamino Farms, SPL Stake Pool, SPL Token, ATA, System, Compute
  Budget. **All programs we need for unwind are already whitelisted.**
- `crates/zerox1-defi-protocols/src/protocols/kamino.rs:539-611` —
  v1 `withdraw_ix` (4-ixn bundle: ATA-create-idempotent + refresh_reserve
  + refresh_obligation + withdraw_obligation_collateral_and_redeem_reserve_collateral).
  Bare ixn data uses discriminator `withdraw_obligation_collateral_and_redeem_reserve_collateral`.
- `crates/zerox1-defi-protocols/src/protocols/kamino.rs:715-766` —
  v1 `repay_obligation_liquidity_ix` (3-ixn bundle: ATA-create + refresh + repay).
- `crates/zerox1-defi-protocols/src/protocols/kamino.rs:898-1176` —
  v2 builders for borrow / deposit-obligation-collateral / deposit-reserve-
  liquidity-and-obligation-collateral. The v2 pattern (v1 accounts +
  3-account farm appendix) is the template for `repay_v2_ix` and
  `withdraw_v2_ix` we will add.
- `crates/zerox1-defi-protocols/src/protocols/kamino.rs:778-864` — flash-
  borrow / flash-repay ixn builders **already exist**. Verified against
  IDL v1.19.0, 12 accounts each. Borrow_instruction_index is u8.
- `crates/zerox1-defi-protocols/src/protocols/kamino_loader.rs` — `fetch_obligation`,
  `load_reserve`, `query_position_ltv_bps`. Used by lever-up; same calls
  drive lever-down.
- `crates/stable-yield-daemon/src/lend.rs:409-533` — `run_withdraw_or_simulate`
  — simplest withdraw shape in the codebase. ReportStableWithdraw shape
  is the template for ReportWithdrawMultiply.
- `tools/fleet-pm-stub/src/main.rs:147-162` — `WithdrawStableLend` subcommand
  (clap variant). Template for new `WithdrawMultiply` subcommand.
- `tools/fleet-pm-stub/src/main.rs:637-660` — envelope-build branch for
  `WithdrawStableLend`. Template for `WithdrawMultiply` arm.
- `docs/kamino-klend-bundle-spec.md` — the canonical bundle-shape spec.
  We will extend it (commit 2 of this plan) with repay + withdraw sections.
- `docs/kamino-loader-audit-2026-05-14.md` — verified obligation layout
  offsets (deposits 96+136*i, borrows 1208+200*i). Used by `decode_obligation`.

### 1.4 Mesh protocol (sibling worktree)

Pulled in via `Cargo.toml:53-54` from `../p2p_architecture/crates/zerox1-protocol`.

- `src/message.rs` — `MsgType` enum. Allocated `0x18 = Withdraw` (generic).
  Per emergency-withdraw plan §2.1, `0x19` is being claimed for
  `EmergencyWithdraw`. **Next free Collaboration slot: `0x1A` —
  we claim it for `WithdrawMultiply`.**
- `src/fleet/multiply.rs` — currently has only `AssignMultiply` and
  `ReportMultiply`. We add `WithdrawMultiply` and `ReportMultiplyWithdraw`.
- `src/fleet/stable_lend.rs` — pattern for `Withdraw*` + `Report*Withdraw`
  pairs. Lever-down mirrors this shape.

---

## §2 Strategy decision — Option 1 (iterative) vs Option 2 (flash-loan)

**Recommended: Option 2 — flash-loan unwind (single-tx).**

### Why flash-loan (matches klend-sdk's production deleverage)

| Criterion | Option 1 (iterative N rounds) | Option 2 (flash-loan, single tx) |
|---|---|---|
| Bundles per unwind | 3-5 | 1 |
| Mid-unwind liquidation risk | **High** — between round N and N+1, an adversarial SOL pump can briefly push the obligation above its liquidation threshold | **None** — atomic |
| Slippage exposure | Compounds across rounds | Single Jupiter quote |
| Round-2+ LTV recomputation | Required (LTV moves each round) | Not needed |
| Code complexity | High (mirror the 14-release lever-up grind) | Moderate (single bundle, but more ixns per tx) |
| klend-sdk precedent | None | **Yes — `buildWithdrawWithLeverageIxs` is canonical** |
| CU budget | 1M per round × N rounds | ~1.4M for the single combined tx |
| ALT requirement | Already in place (KAMINO_MAIN_MARKET_LOOKUP_TABLE covers ~50 accts) | Same ALT covers it; +1-2 extra Jupiter route accts |
| Partial-failure recovery | Easier (each round is independent) | One re-try |
| Flash-loan fee | None | Kamino SOL reserve flash-fee (verify on-chain — typically 30 bps) |

The flash-loan fee is small relative to the slippage + liquidation-window
costs Option 1 imposes. **The 14-release lever-up grind is the strongest
prior — replicating that iteration cost on the way down would burn ~2
weeks. The flash-loan path is what klend-sdk ships, so it's the path
Kamino's own teams maintain compatibility for.**

### Why NOT pure Option 1

Even if Option 1 felt simpler at first glance, it has three lurking traps:

1. **Stale-reserve cascades.** Same `RefreshReserve(every_obligation_reserve)`
   + `RefreshObligation` discipline as lever-up, repeated N times. Each
   round's RPC round-trip risks read-replica lag (the v0.1.20 bug). The
   local-bump pattern works but needs to handle the borrow shrinking to 0
   (slot clearing), which is a different state transition than lever-up's
   adds-only.
2. **Mid-unwind SOL pump.** If SOL spikes between round 1's withdraw and
   round 2's withdraw, the obligation's LTV jumps; an over-aggressive δ
   on round 1 leaves no headroom on round 2.
3. **Compounding swap slippage.** Round-1 swap quote diverges from round-2
   quote when wallet jitoSOL accumulates between rounds.

### Why NOT klend's own `RepayAndWithdraw` (the combined handler)

`handler_repay_and_withdraw_redeem.rs` is tempting — it bundles repay +
withdraw atomically inside klend. **But it still requires the caller to
fund the repay** (you have to bring the SOL to repay before withdrawing
the jitoSOL). For an unwind from a position that has zero idle SOL in
the wallet, the only ways to fund the repay are:

- Flash-borrow SOL from Kamino itself — then we're back to Option 2 with
  extra steps, OR
- Pre-swap some jitoSOL → SOL outside the obligation — circular, doesn't
  exist.

So even with `RepayAndWithdraw`, we still need the flash-borrow leg. Once
we have the flash-borrow leg, we can use the existing standalone
`repay_obligation_liquidity_v2` + `withdraw_obligation_collateral_v2`
ixns and skip the combined handler. That's what the SDK does.

### Settled design

Bundle structure: **flash-borrow SOL → repay SOL → withdraw jitoSOL →
swap jitoSOL → SOL → flash-repay SOL.** Single transaction per unwind
attempt. Fallback to a 2-tx pattern (split into "repay+withdraw" tx 1,
"swap + manual repay-out-of-wallet" tx 2) only if CU budget proves
insufficient after sim — but the SDK reports 1-tx works for production
positions, so we plan for 1 tx.

### Iterative fallback (Option 1) — when we'd reach for it

Keep Option 1 as a documented escape hatch in case:

- The unwind position is too large for a single Jupiter route quote
  (jitoSOL→SOL liquidity dries up at size).
- Kamino's SOL reserve flash-loan cap is below our debt size.

In those cases, the unwind splits into N rounds of `min(δ_max, debt/N)`
**without** flash loans, sized so each round's δ keeps LTV strictly
below the liquidation threshold with a safety buffer. Each round:

```
1. Withdraw δ jitoSOL collateral (sized so post-withdraw LTV < liq_threshold - safety_buffer)
2. Swap δ jitoSOL → SOL via Jito stake-pool WithdrawSol  (the inverse of DepositSol)
3. Repay δ SOL to the borrow
```

The final round, after debt = 0, is a single withdraw-all (`u64::MAX`)
because klend permits full collateral release on an obligation with no
borrows.

§3 below specifies the flash-loan bundle in detail. The iterative path is
described in §6 (state-machine sketch) as a fallback branch only.

---

## §3 Per-round bundle — flash-loan unwind (canonical)

Reference shape from klend-sdk + verified against klend handler accounts.
All ixn numbering matches the order written into the tx.

### 3.1 Inputs

- `user`: signer (= multiply daemon wallet)
- `sol_reserve: ReserveAccounts`, `jitosol_reserve: ReserveAccounts`
  — loaded via `kamino_loader::load_reserve` (same as lever-up)
- `jito_pool: StakePoolMeta` — loaded via `jito_loader::load_jito_pool`
  (for the jitoSOL→SOL leg if we route through Jito; see §3.4)
- `obligation_addr` — derived via `derive_user_obligation_with_seed(...,
  MULTIPLY_OBLIGATION_SEED.0, .1)`
- `obligation: DecodedObligation` — current on-chain state, includes
  `deposits[].deposited_amount` and `borrows[].borrowed_amount_sf`
- `total_borrow_sol_lamports` = sum of `borrows` for `sol_reserve`,
  rounded UP to absorb the next accrued-interest tick + flash fee
- `withdraw_jitosol_collateral` = full deposited cToken amount on
  `jitosol_reserve` (i.e. `u64::MAX` sentinel — klend interprets max
  as "redeem everything")

### 3.2 Bundle (all ixns in one tx)

CU budget: **1_400_000 CU**. Priority fee: **10_000 microlamports** (mirror
lever-up).

ALT: `KAMINO_MAIN_MARKET_LOOKUP_TABLE` + (optional) Jupiter route ALT
returned in the Jupiter quote response. The v0 message compiler MUST be
used (matches lever-up).

```
  0  ComputeBudgetProgram::set_compute_unit_limit(1_400_000)
  1  ComputeBudgetProgram::set_compute_unit_price(10_000)
  2  ATA-create-idempotent(user, wSOL_MINT)                              -- ensures wSOL ATA for flash-borrow destination + swap output
  3  ATA-create-idempotent(user, jitoSOL_MINT)                           -- ensures jitoSOL ATA for collateral redemption + swap input

                                                                          -- flash leg open --
  4  FlashBorrowReserveLiquidity(sol_reserve, total_borrow_sol_lamports) -- index N for FlashRepay's borrow_instruction_index field

                                                                          -- pre-repay refreshes --
  5  RefreshReserve(jitoSOL)                                             -- because it's in obligation.deposits
  6  RefreshReserve(SOL)                                                 -- the repay target
  7  RefreshObligation(remaining=[jitoSOL_reserve, SOL_reserve])         -- deposits ++ borrows in obligation array order

                                                                          -- repay debt --
  8  RepayObligationLiquidityV2(sol_reserve, total_borrow_sol_lamports)  -- v2: CPI-internal Debt-farm refresh

                                                                          -- pre-withdraw refreshes (RepayV2 marks SOL reserve stale) --
  9  RefreshReserve(jitoSOL)
 10  RefreshReserve(SOL)
 11  RefreshObligation(remaining=[jitoSOL_reserve, SOL_reserve])         -- borrows array now empty post-Repay BUT obligation may not yet have garbage-collected the slot — pass both to be safe; klend tolerates empty slots, see decode_obligation

                                                                          -- withdraw collateral --
 12  WithdrawObligationCollateralAndRedeemReserveCollateralV2(
        jitosol_reserve, withdraw_jitosol_collateral=u64::MAX)            -- v2: CPI-internal Collateral-farm refresh

                                                                          -- swap leg: jitoSOL → SOL --
 13..K Jupiter swap ixns (variable count, from /v6/swap-instructions)    -- input mint = jitoSOL, output mint = wSOL, amount = wallet balance, slippage = max_slippage_bps from WithdrawMultiplyParams
                                                                          -- OR jito_stake_pool::WithdrawSol(jitosol_amount) if Jupiter route unavailable; see §3.4

                                                                          -- flash leg close --
 K+1 FlashRepayReserveLiquidity(sol_reserve,
        total_borrow_sol_lamports + flash_fee,
        borrow_instruction_index = 4)                                    -- must be > 0 because ix 0,1 are compute budget; the exact index of FlashBorrow above

                                                                          -- cleanup --
 K+2 SPL-Token::CloseAccount(wSOL ATA → user)                            -- unwraps any leftover wSOL to native SOL

```

### 3.3 Account constraints crib (verified against klend source)

#### RepayObligationLiquidityV2 (ixn 8)

v1 account list (9, verified verbatim from `handler_repay_obligation_liquidity.rs`):
```
0  owner                              Signer
1  obligation                         AccountLoader, mut, has_one=lending_market
                                       constraint: obligation.lending_market == repay_reserve.lending_market
2  lending_market                     AccountLoader
3  repay_reserve                      AccountLoader, mut, has_one=lending_market  (= sol_reserve in our case)
4  reserve_liquidity_mint             address = repay_reserve.liquidity.mint_pubkey  (= WSOL_MINT)
5  reserve_destination_liquidity      mut, address = repay_reserve.liquidity.supply_vault
6  user_source_liquidity              mut, token::mint = repay_reserve.liquidity.mint_pubkey  (= user's wSOL ATA, holds flash-borrowed wSOL)
7  token_program                      Interface<TokenInterface>
8  instruction_sysvar_account         address = SysInstructions::id()
                                       constraint: ix_utils::no_restricted_programs_within_tx
```

v2 farm appendix (3, by analogy with our existing `borrow_obligation_liquidity_v2_ix`):
```
9   obligation_farm_user_state         mut if reserve.farm_debt != Pubkey::default(), else readonly None sentinel (= KAMINO_LEND_PROGRAM_ID)
10  reserve_farm_state                 mut if farm present, else readonly None sentinel
11  farms_program                      KAMINO_FARMS_PROGRAM_ID
```

Anchor errors that can fire:
- 6001 InvalidAccountInput
- 6009 ReserveStale — if a listed reserve wasn't RefreshReserved this slot
- 6029 ObligationStale — if RefreshObligation didn't run pre-ix
- 6050 CpiDisabled
- 6051 IncorrectInstructionInPosition — pre-ix pattern mismatch
- 6052/6053/6054 Price oracle staleness
- ObligationLiquidityEmpty (analog to 6020 for deposits) — repaying against
  a borrow slot that's already at 0; safe to swallow at our layer because
  it means the unwind already happened
- arithmetic overflow on penalty/interest accumulation (rare)

`check_refresh_ixs!` for v1 (mirrored in v2 minus the manual farm-refresh
positional check, which v2 collapses into the CPI):
```
[current_idx - 1] = RefreshObligation(obligation at accounts[1])
[current_idx - 2] = RefreshReserve(repay_reserve at accounts[3])
```

#### WithdrawObligationCollateralAndRedeemReserveCollateralV2 (ixn 12)

v1 account list (13, verified verbatim):
```
0   owner                              Signer, mut
1   obligation                         mut, has_one=lending_market, has_one=owner
2   lending_market                     AccountLoader
3   lending_market_authority           seeds=[LENDING_MARKET_AUTH, lending_market], bump
4   withdraw_reserve                   mut, has_one=lending_market  (= jitosol_reserve)
5   reserve_liquidity_mint             address = withdraw_reserve.liquidity.mint_pubkey  (= JITOSOL_MINT)
                                       mint::token_program = liquidity_token_program
6   reserve_source_collateral          mut, address = withdraw_reserve.collateral.supply_vault
7   reserve_collateral_mint            mut, address = withdraw_reserve.collateral.mint_pubkey
8   reserve_liquidity_supply           mut, address = withdraw_reserve.liquidity.supply_vault
9   user_destination_liquidity         mut, token::mint = withdraw_reserve.liquidity.mint_pubkey, token::authority = owner  (= user's jitoSOL ATA)
10  placeholder_user_destination_coll  Option<AccountInfo>  — pass KAMINO_LEND_PROGRAM_ID as None sentinel
11  collateral_token_program           Program<Token>  (classic SPL, NOT Token-2022)
12  liquidity_token_program            Interface<TokenInterface>
13  instruction_sysvar_account         address = SysInstructions::id()
```

v2 farm appendix (3, mirroring v2 deposit-and-collateral):
```
14  obligation_farm_user_state         mut if reserve.farm_collateral != default, else readonly None sentinel
15  reserve_farm_state                 mut if farm present, else readonly None sentinel
16  farms_program                      KAMINO_FARMS_PROGRAM_ID
```

`check_refresh_ixs!` for v1:
```
[current_idx - 1] = RefreshObligation(obligation at accounts[1])
[current_idx - 2] = RefreshReserve(withdraw_reserve at accounts[4])
```

Anchor errors that can fire:
- 6001/6009/6029/6050/6051 — same suite as Repay
- LtvExceeded family — withdrawing collateral cannot push remaining LTV above
  liquidation threshold. With flash-loan-driven debt-to-zero, this is moot
  because the borrow is repaid BEFORE the withdraw — the post-withdraw LTV
  is 0/0 = undefined and klend short-circuits.
- WithdrawTooLarge — never our case (we always pass u64::MAX which klend
  clamps to the actual deposited amount)

#### FlashBorrow / FlashRepay (ixns 4 + K+1)

Already covered in `kamino.rs:778-864`. Key points:

- `borrow_instruction_index` field on FlashRepay is the **absolute** ix
  index of FlashBorrow within the tx. With compute-budget ix at 0,1, the
  FlashBorrow ends up at index 4 (after our two ATA-create-idempotent
  ixns at 2,3). **Pass `borrow_instruction_index = 4`** (NOT `5` and
  NOT relative to a sub-slice).
- klend pairs them by walking the instruction sysvar and requiring the
  match. Mispair → klend rejects the tx.
- Flash fee is read from the reserve's `liquidity.flash_loan_fee_sf`
  (fixed-point u128). At bundle-build time, fetch the reserve, compute
  `flash_fee = ceil(amount * flash_loan_fee_sf / 2^60)` (the standard
  klend fixed-point scaling), repay `amount + flash_fee`.
- The flash-borrow user_destination IS the user's wSOL ATA — same
  account the flash-repay reads from. So ixns 2 (create wSOL ATA),
  4 (flash-borrow → wSOL ATA), 8 (repay reads wSOL ATA), 13..K (swap
  output wSOL goes back to wSOL ATA), K+1 (flash-repay reads wSOL ATA)
  all touch the same ATA.

#### RefreshObligation remaining_accounts (ixns 7 + 11)

Order: deposits-array order first, then borrows-array order. At unwind
start: `[jitoSOL_reserve, SOL_reserve]`. After ixn 8 (Repay), klend may
zero the SOL borrow slot but does NOT compact it — slot is left as
borrowed_amount_sf=0, reserve=SOL_reserve unchanged. So ixn 11 still
passes `[jitoSOL_reserve, SOL_reserve]`. **Verified:** klend's
`refresh_obligation` iterates slots, skips inactive entries. Mirror the
lever-up local-bump pattern: pre-compute the remaining_accounts slice
ONCE and reuse for ixns 7 and 11. The post-Repay obligation refresh
does not need a different slice.

### 3.4 Swap leg: Jupiter vs Jito stake-pool WithdrawSol

The jitoSOL → SOL leg has two candidate implementations.

**Option A — Jupiter swap (preferred for production).**
- Single source of truth for routing: Jupiter aggregator picks the best
  venue (Jito, Marinade, Sanctum spot-wrapped, DEX pools) at quote time.
- `GET /v6/swap-instructions` returns an instruction list (Anchor-encoded)
  + an ALT list — caller appends both to the tx and the ALT vec.
- Slippage controlled via `max_slippage_bps` from `WithdrawMultiplyParams`
  (caps at `caps::MAX_SLIPPAGE_BPS = 200`).
- Adds 8-15 accts to the tx; ALT handles them.

**Option B — Jito stake-pool `WithdrawSol` (fallback).**
- Uses the same `spl_stake_pool` program already in our whitelist.
- Burns jitoSOL → withdraws SOL at the pool's exchange rate, minus the
  pool's withdraw fee (currently 0.1%). NO routing — direct redeem.
- Smaller account list. Useful when Jupiter quote fails or as a
  liquidity-safe backstop.
- Caveat: jito stake-pool has a per-instant-withdraw cap; large
  positions hit the cap and fall back to "queued unstake" (multi-epoch).
  Need to check `pool.lamports_available_for_instant_withdraw` at
  bundle-build time.

**Decision: Jupiter primary, Jito-pool fallback.** If Jupiter quote
fails (rate-limit, route-not-found, slippage > cap) the bundle builder
attempts Jito-pool with the same amount and the same slippage cap.

The Jupiter integration already exists in hedgedjlp-daemon (via the
hedgedjlp M6 milestone). We will **lift** the
`jupiter::get_swap_instructions(...)` helper from
`crates/hedgedjlp-daemon/src/jupiter.rs` into a shared location
(`crates/zerox1-defi-protocols/src/protocols/jupiter.rs`) in a
preliminary commit so both daemons can call it. See §5 / §8.

### 3.5 Total ixn count + CU sanity

Round count estimate: 2 compute-budget + 2 ATA-create + 1 FlashBorrow +
3 refreshes + 1 Repay + 3 refreshes + 1 Withdraw + ~8 Jupiter swap +
1 FlashRepay + 1 close = **22-25 ixns per unwind tx.**

Lever-up's 14-ixn round runs at ~700k CU sim'd on mainnet. Unwind has
extra refreshes + Jupiter swap CPI + flash-loan CPIs. **1_400_000 CU**
is the safe budget; 1_600_000 if Jupiter routes through 3+ hops.

ALT savings: same KAMINO_MAIN_MARKET_LOOKUP_TABLE covers ~50 of the 80+
accounts. Jupiter's quote response includes route-specific ALTs to
collapse the swap accts. With both ALTs the tx fits comfortably in the
1232-byte raw limit; without them, we'd overflow.

---

## §4 Protocol additions

### 4.1 New MsgType slot

File: `../p2p_architecture/crates/zerox1-protocol/src/message.rs`

Existing allocations (per emergency-withdraw plan):
- `0x18 = Withdraw` (generic — used by stable-yield, hedgedjlp)
- `0x19 = EmergencyWithdraw` (reserved per emergency-withdraw plan)

**Claim `0x1A = WithdrawMultiply`.**

Rationale for a dedicated slot vs. reusing `Withdraw=0x18`: the lever-down
flow has a different payload shape (no amount field — full unwind is the
only mode) and a different report shape (multiple tx signatures because
the unwind bundle may broadcast 1-3 txs in error-recovery paths). A
distinct MsgType keeps the dispatch tables type-safe and lets
`payload_is_for_this_daemon` cleanly filter at the envelope level
(mirrors the lever-up `Assign` vs the eventual `Withdraw=0x18` story —
the latter is already claimed by stable-yield / hedgedjlp).

Add to enum:
```rust
    /// Multiply daemon: fully unwind the leveraged jitoSOL position
    /// to USDC (or jitoSOL, per params). One-shot — no amount field —
    /// because partial unwinds at multiply are a different design
    /// (would need separate per-asset LTV targeting).
    WithdrawMultiply = 0x1A,
```

Add the `from_u16` arm and `Display` arm in the same shape as the existing
`Withdraw` variant.

### 4.2 New payload + report types

File: `../p2p_architecture/crates/zerox1-protocol/src/fleet/multiply.rs`

Add to the existing `multiply.rs` (which currently holds `AssignMultiply` +
`ReportMultiply`):

```rust
/// Sent by the orchestrator to fully unwind the multiply daemon's
/// leveraged jitoSOL position. No amount param — the unwind is always
/// 100% (partial deleverage uses a smaller AssignMultiply target_ltv).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WithdrawMultiply {
    /// Mirrors AssignMultiply.vault — the multiply daemon's wallet
    /// pubkey, kept for routing parity. Daemon ignores it (single-vault
    /// per daemon) but validates non-zero.
    pub vault: [u8; 32],
    /// Max slippage on the jitoSOL→SOL swap leg, in bps. Capped at
    /// caps::MAX_SLIPPAGE_BPS (200) at the daemon side.
    pub max_slippage_bps: u16,
    /// 0 = no deadline. Otherwise UNIX-seconds — daemon refuses if
    /// now > deadline_unix.
    pub deadline_unix: u64,
}

/// Report sent back after a WithdrawMultiply. The unwind bundle may
/// involve multiple txs (the main flash-loan unwind, plus any optional
/// follow-ups for sweeping leftover dust to USDC) so tx_signatures is
/// Vec, not Option.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportMultiplyWithdraw {
    pub header: ReportHeader,
    /// 0 if the unwind didn't reach the final swap-to-USDC leg.
    pub final_usdc_lamports: u64,
    /// Native SOL left in the wallet after the unwind (lamports).
    /// May be non-zero if the unwind chose to leave the SOL unconverted.
    pub residual_sol_lamports: u64,
    /// All Solana tx signatures from this unwind, in order. Empty on
    /// build/sim failure.
    pub tx_signatures: Vec<String>,
}
```

Round-trip CBOR test mirrors `multiply.rs`'s existing
`assign_multiply_roundtrip` test.

### 4.3 Error codes

Reuse the existing `ReportHeader::err(conv, error_code)` pattern. New codes
introduced for the unwind path:

| Code | Meaning |
|------|---------|
| 1 | Generic build/sim failure (mirror existing leverage failure code) |
| 2 | Cap validation failure (`caps::validate_withdraw_multiply`) |
| 3 | Deadline exceeded |
| 4 | Paused by riskwatcher veto (mirrors `ERR_PAUSED_BY_RISKWATCHER`) |
| 5 | Approval queue full |
| 10 | Flash-loan fee fetch failed |
| 11 | Jupiter quote failed AND Jito-pool fallback exceeded slippage |
| 12 | Reserve loader failed (RPC) |
| 13 | Obligation already empty (nothing to unwind — return ok=true with `final_usdc_lamports=0`, NOT this code) |
| 14 | Post-unwind sanity check failed (debt non-zero after broadcast confirms) |

---

## §5 Per-file diff plan

### 5.1 `crates/zerox1-defi-protocols/src/protocols/kamino.rs`

Add two new v2 ixn builders. Both follow the existing v2 pattern (v1
accounts + 3-account farm appendix using `v2_farm_accounts()` helper).

Insertion point: after line 999 (end of `borrow_obligation_liquidity_v2_ix`),
before line 1024 (`deposit_obligation_collateral_v2_ix`).

```rust
/// Bare `repay_obligation_liquidity_v2` — v2 of [`repay_obligation_liquidity_ix`].
///
/// Account list:
///   [0]  owner (signer)
///   [1]  obligation (mut)
///   [2]  lending_market
///   [3]  repay_reserve (mut)
///   [4]  reserve_liquidity_mint
///   [5]  reserve_destination_liquidity (mut)  (= reserve.liquidity_supply)
///   [6]  user_source_liquidity (mut)
///   [7]  token_program
///   [8]  instruction_sysvar_account
///   [9]  obligation_farm_user_state (v2 — mut if Debt farm present)
///   [10] reserve_farm_state         (v2 — mut if farm present)
///   [11] farms_program              (v2)
///
/// Caller MUST ensure RefreshReserve + RefreshObligation appear immediately
/// before this ixn in the tx. The v2 handler does the Debt-farm refresh
/// CPI internally; no manual RefreshObligationFarmsForReserve required.
pub fn repay_obligation_liquidity_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    // ... mirror borrow_obligation_liquidity_v2_ix shape, swap discriminator
    // to "repay_obligation_liquidity_v2", swap V2FarmKind::Debt (same kind
    // since repay touches debt-side state).
}

/// Bare `withdraw_obligation_collateral_and_redeem_reserve_collateral_v2`.
///
/// Account list — 14 v1 accounts + 3-account farm appendix:
///   [0]  owner (signer, mut)
///   [1]  obligation (mut)
///   [2]  lending_market
///   [3]  lending_market_authority
///   [4]  withdraw_reserve (mut)
///   [5]  reserve_liquidity_mint
///   [6]  reserve_source_collateral (mut)
///   [7]  reserve_collateral_mint (mut)
///   [8]  reserve_liquidity_supply (mut)
///   [9]  user_destination_liquidity (mut)
///   [10] placeholder_user_destination_collateral (None sentinel = KAMINO_LEND_PROGRAM_ID)
///   [11] collateral_token_program (Token classic)
///   [12] liquidity_token_program
///   [13] instruction_sysvar_account
///   [14] obligation_farm_user_state (v2 — mut if Collateral farm present)
///   [15] reserve_farm_state
///   [16] farms_program
pub fn withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
    user: &Pubkey,
    reserve: &ReserveAccounts,
    amount: u64,
    obligation_seed: (u8, u8),
) -> Result<Instruction> {
    // amount==u64::MAX → klend redeems the obligation's full cToken slot.
    // Use V2FarmKind::Collateral.
}
```

Unit tests (mirror existing `borrow_data_starts_with_anchor_discriminator` +
the v2 versions at lines 1769-1846):
- Discriminator matches `anchor_discriminator("global", "repay_obligation_liquidity_v2")`
- Discriminator matches `anchor_discriminator("global", "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2")`
- Account count = 12 (repay) / 17 (withdraw)
- u64::MAX amount round-trips through Borsh
- Farm-present path: indices 9-11 / 14-16 are mut + KAMINO_FARMS_PROGRAM_ID
- Farm-absent path: indices 9-11 are readonly + KAMINO_LEND_PROGRAM_ID sentinel

### 5.2 `crates/zerox1-defi-protocols/src/protocols/jupiter.rs` (new — preliminary commit)

**Preliminary work**, NOT in the multiply commits below. Lift the existing
`get_swap_instructions(...)` helper from
`crates/hedgedjlp-daemon/src/jupiter.rs` into the protocols crate so multiply
can call the same code. Re-export from hedgedjlp; otherwise it stays a
private clone.

This is a refactor commit landing **before** commit 1 below.

### 5.3 `crates/multiply-daemon/src/kamino.rs`

Already has the v1 `build_supply_ixns` / `build_withdraw_ixns` (for the
obligation's USDC slot — irrelevant to the leveraged jitoSOL position).
No changes to the file itself, but **the whitelist already covers
everything unwind needs** (klend, farms, SPL stake pool, token, ATA,
system, compute budget). Jupiter is the only program not currently
whitelisted.

Add to `whitelist_program_ids()` at line 63-83:

```rust
// Jupiter aggregator v6 — used by unwind.rs for the jitoSOL→SOL leg.
// jupiter_v6::ID would be the const-ref; pinning to the on-chain
// pubkey "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4" via
// zerox1_defi_protocols::constants::JUPITER_V6_PROGRAM_ID.
JUPITER_V6_PROGRAM_ID,
```

(Verify `JUPITER_V6_PROGRAM_ID` already exists as a const in
`zerox1-defi-protocols::constants` — it should, given hedgedjlp uses it.
If not, add to constants.rs in the preliminary refactor commit.)

### 5.4 New file `crates/multiply-daemon/src/unwind.rs`

Mirror the structure of `seed.rs` (pure-function decisions + impure
RPC-driven runner). Skeleton:

```rust
//! Lever-down unwind for multiply-daemon.
//!
//! Single-tx flash-loan unwind:
//!   1. flash-borrow total SOL debt from Kamino SOL reserve
//!   2. repay obligation borrow (v2 — CPI Debt-farm refresh)
//!   3. withdraw jitoSOL collateral (v2 — CPI Collateral-farm refresh)
//!   4. swap jitoSOL → SOL via Jupiter (or Jito stake-pool fallback)
//!   5. flash-repay (amount + flash fee)
//!   6. close wSOL ATA
//!
//! Iterative fallback (when single-tx exceeds CU or Jupiter route
//! unavailable at size) — N rounds of (withdraw δ, swap, repay) as
//! Option 1 in the spec.

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
// ... imports identical shape to seed.rs

const UNWIND_CU_LIMIT: u32 = 1_400_000;
const UNWIND_PRIORITY_FEE: u64 = 10_000;

/// Decision returned by [`decide_unwind_strategy`]: pure function over
/// position + reserve state.
#[derive(Debug, PartialEq, Eq)]
pub enum UnwindStrategy {
    /// Single-tx flash-loan unwind. Preferred when:
    /// - Kamino SOL reserve flash-loan cap >= debt
    /// - Jupiter quote available within slippage cap
    FlashLoan {
        flash_amount_lamports: u64,
        flash_fee_lamports: u64,
        expected_sol_out_from_swap: u64,
    },
    /// Iterative fallback. Used when flash cap < debt or Jupiter quote
    /// unavailable. N rounds of bounded δ.
    Iterative {
        rounds: u8,
        per_round_collateral_withdraw_jitosol: u64,
    },
    /// Nothing to unwind — obligation has no SOL borrow AND no jitoSOL
    /// collateral. Caller returns ok=true with final_usdc_lamports=0.
    Noop,
}

pub fn decide_unwind_strategy(
    obligation: &DecodedObligation,
    sol_reserve: &ReserveAccounts,
    sol_reserve_flash_cap: u64,
    jupiter_quote_in_slippage: bool,
    // ... other inputs
) -> UnwindStrategy { ... }

/// Build the single-tx flash-loan unwind bundle.
pub fn build_unwind_flash_bundle(
    user: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    flash_amount_lamports: u64,
    flash_fee_lamports: u64,
    jupiter_swap_ixns: Vec<Instruction>,
    jupiter_alts: Vec<AddressLookupTableAccount>,
    obligation_reserves: &[Pubkey],
) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
    // ... ordered bundle per §3.2
}

/// Build one round of the iterative fallback (no flash loan).
pub fn build_unwind_iterative_round_bundle(
    user: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    jito_pool: &StakePoolMeta,
    withdraw_jitosol_lamports: u64,
    expected_sol_out: u64,
    repay_sol_lamports: u64,
    obligation_reserves: &[Pubkey],
) -> Result<Vec<Instruction>> {
    // For each round:
    //   refreshes → WithdrawObligationCollateralAndRedeemReserveCollateralV2(δ jitoSOL)
    //   → jito_stake_pool::WithdrawSol(δ jitoSOL → δ SOL)
    //   → refreshes → RepayObligationLiquidityV2(δ SOL)
}

/// Either simulate or submit the unwind. Mirrors leverage::run_or_simulate.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    withdraw: &WithdrawMultiply,
    conv: [u8; 16],
) -> Result<ReportMultiplyWithdraw> {
    // Fetch obligation + reserves + jito pool + flash cap.
    // Decide strategy.
    // Build bundle(s).
    // Whitelist-verify (audit-fix I1).
    // simulate_only → build_sign_simulate_with_alts; submit → build_sign_send_with_alts.
    // Iterative path: loop up to caps::MAX_LEVERAGE_LOOP_ROUNDS, with
    // local obligation_reserves bump and post-broadcast LTV re-query
    // (same lever-up pattern from leverage.rs lines 192-335).
}
```

Add `pub mod unwind;` to `crates/multiply-daemon/src/main.rs` (since
multiply-daemon is a binary crate, modules are declared in `main.rs`).

### 5.5 `crates/multiply-daemon/src/dispatch.rs`

Two edits:

(a) `payload_is_for_this_daemon` at lines 150-157 — add WithdrawMultiply
decode check:

```rust
fn payload_is_for_this_daemon(env: &Envelope) -> bool {
    match env.msg_type {
        MsgType::Assign => ciborium::de::from_reader::<AssignMultiply, _>(...).is_ok(),
        MsgType::WithdrawMultiply => {
            ciborium::de::from_reader::<WithdrawMultiply, _>(&env.payload[..]).is_ok()
        }
        _ => true,
    }
}
```

(b) New arm in `run()` (insert at line 318, before the `MsgType::Beacon =>`
arm). Follow the existing `MsgType::Assign` arm's shape but route to
`crate::unwind::run_or_simulate`:

```rust
MsgType::WithdrawMultiply => {
    let conv = env.conversation_id;
    let recipient = env.sender;
    if !sender_is_authorised(ctx.orchestrator_agent_id, env.sender, "WithdrawMultiply") {
        continue;
    }
    if is_paused(&ctx) {
        warn!(?conv, "WithdrawMultiply rejected — paused by riskwatcher veto");
        // ERR_PAUSED_BY_RISKWATCHER (code 4) — same code as Assign rejection
        let report = ReportMultiplyWithdraw {
            header: ReportHeader::err(conv, ERR_PAUSED_BY_RISKWATCHER),
            final_usdc_lamports: 0,
            residual_sol_lamports: 0,
            tx_signatures: vec![],
        };
        let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
        continue;
    }
    match handle_withdraw(&handle, &ctx, &env).await {
        Ok(report) => { let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await; }
        Err(e) => {
            warn!(?e, ?conv, "withdraw failed; sending error Report");
            let report = ReportMultiplyWithdraw {
                header: ReportHeader::err(conv, 1),
                final_usdc_lamports: 0,
                residual_sol_lamports: 0,
                tx_signatures: vec![],
            };
            let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
        }
    }
}
```

New helpers:
- `handle_withdraw(handle, ctx, env)` — decode, cap-validate, deadline-check,
  approval-queue if `ctx.require_approval`, then call
  `crate::unwind::run_or_simulate`.
- `send_report_withdraw(handle, ctx, recipient, conv, report)` — bilateral
  send + riskwatcher CC, mirror `send_report` at lines 418-489 but for
  `ReportMultiplyWithdraw`.

Approval-queue integration: extend `crate::approval::ApprovalQueue` to
support `WithdrawMultiply` as a distinct queue entry (or use a tagged
enum). Simpler: a parallel queue. Mirror what stable-yield does at
`crates/stable-yield-daemon/src/dispatch.rs` (already has two queues
for Assign/Withdraw).

### 5.6 `crates/multiply-daemon/src/caps.rs`

Add at end:

```rust
/// Validate WithdrawMultiply. No real caps to enforce beyond slippage
/// (no amount field). Refuses zero-vault as defensive check.
pub fn validate_withdraw_multiply(w: &WithdrawMultiply) -> Result<()> {
    if w.vault == [0u8; 32] {
        return Err(anyhow!("WithdrawMultiply.vault is zero"));
    }
    if w.max_slippage_bps > MAX_SLIPPAGE_BPS {
        return Err(anyhow!(
            "max_slippage_bps {} exceeds hard cap {}",
            w.max_slippage_bps, MAX_SLIPPAGE_BPS
        ));
    }
    Ok(())
}
```

Tests: accepts within caps, rejects zero vault, rejects slippage > cap.

### 5.7 `tools/fleet-pm-stub/src/main.rs`

New clap variant in `Cmd` (insert after `WithdrawHedgedJlp` at line 106):

```rust
    /// Send WithdrawMultiply to the multiply desk.
    WithdrawMultiply {
        #[arg(long)]
        vault: String,
        #[arg(long, default_value_t = 100)]
        max_slippage_bps: u16,
        #[arg(long, default_value_t = 0)]
        deadline_unix: u64,
    },
```

New arm in `build_envelope_from_cmd` (after `WithdrawStableLend` at line 637):

```rust
        Cmd::WithdrawMultiply { vault, max_slippage_bps, deadline_unix } => {
            let vault_bytes = decode_base58_pubkey(vault)?;
            let withdraw = WithdrawMultiply {
                vault: vault_bytes,
                max_slippage_bps: *max_slippage_bps,
                deadline_unix: *deadline_unix,
            };
            let mut payload = Vec::new();
            ciborium::ser::into_writer(&withdraw, &mut payload)
                .context("serialize WithdrawMultiply")?;
            (
                MsgType::WithdrawMultiply,
                conv,
                payload,
                "WithdrawMultiply",
            )
        }
```

And in `expected_report_for_label`, add the new label:

```rust
        "WithdrawMultiply" => ExpectedReport::MultiplyWithdraw,
```

(Adds a new `ExpectedReport::MultiplyWithdraw` variant in the same
file.)

### 5.8 `crates/multiply-daemon/src/main.rs`

Add `pub mod unwind;` (one line) at module declarations.

No CLI flag additions needed — unwind reuses the existing
`--rpc-url`, `--simulate`, `--orchestrator-agent-id`, `--riskwatcher`,
`--require-approval` flags.

---

## §6 State-machine sketch

Pseudocode for `unwind::run_or_simulate`. Mirrors `leverage::run_or_simulate`
at `leverage.rs:67-342` but inverts the direction.

```text
fn run_or_simulate(ctx, withdraw, conv) -> ReportMultiplyWithdraw {
    user = ctx.wallet.pubkey()
    lending_market = KAMINO_MAIN_MARKET

    // Deadline gate
    if withdraw.deadline_unix > 0 && withdraw.deadline_unix < now() {
        return Err(deadline)
    }

    // Pre-load: same accounts as lever-up
    sol_reserve = load_reserve(KAMINO_MAIN_SOL_RESERVE)
    jitosol_reserve = load_reserve(KAMINO_MAIN_JITOSOL_RESERVE)
    jito_pool = load_jito_pool()
    obligation = fetch_obligation(derive_user_obligation_with_seed(MULTIPLY_OBLIGATION_SEED))

    // Noop check
    total_debt_sol = sum(obligation.borrows where reserve == sol_reserve, borrowed_amount_sf)
                     |> sf_to_lamports
    total_collateral_jitosol_ctokens = sum(obligation.deposits where reserve == jitosol_reserve)
    if total_debt_sol == 0 && total_collateral_jitosol_ctokens == 0 {
        return Ok(ReportMultiplyWithdraw {
            header: ok(conv),
            final_usdc_lamports: 0,
            residual_sol_lamports: get_balance(user),
            tx_signatures: vec![],
        })
    }

    // Strategy decision (pure)
    flash_cap = sol_reserve.liquidity.available_amount  // klend's "available for flash"
    flash_fee = compute_flash_fee(sol_reserve, total_debt_sol)  // fixed-point math from reserve config
    debt_plus_fee = total_debt_sol + flash_fee

    // Quote Jupiter for jitoSOL → SOL on the full collateral amount
    jupiter_quote = jupiter::quote(jitoSOL, wSOL, expected_jitosol_from_redeem,
                                    slippage = withdraw.max_slippage_bps)
                    .ok()

    strategy = decide_unwind_strategy(
        obligation, sol_reserve, flash_cap,
        jupiter_quote.is_some() && jupiter_quote.in_slippage(),
    )

    let mut signatures = vec![]

    match strategy {
        Noop => unreachable (we checked above)
        FlashLoan { flash_amount, flash_fee, expected_sol_out } => {
            // Re-fetch obligation reserves for RefreshObligation
            let obligation_reserves = obligation.deposits.iter().map(|d| d.reserve)
                                      .chain(obligation.borrows.iter().map(|b| b.reserve))
                                      .collect()

            let jupiter_ixns = jupiter::get_swap_instructions(...)
            let (ixs, alts) = build_unwind_flash_bundle(...)

            // Whitelist-verify (audit-fix I1 mirror)
            ctx.whitelist.verify_ixns(&ixs)?

            // ALTs: KAMINO_MAIN_MARKET_LOOKUP_TABLE + jupiter_alts
            let alts = [KAMINO_MAIN_MARKET_LOOKUP_TABLE].iter()
                       .chain(alts.iter()).copied().collect()

            if ctx.simulate_only {
                let sim = ctx.rpc.build_sign_simulate_with_alts(...)
                classify_simulation(sim)
                return Ok(ReportMultiplyWithdraw { ok, sigs=[], ... })
            }
            let sig = ctx.rpc.build_sign_send_with_alts(...)
            signatures.push(sig.to_string())
        }
        Iterative { rounds, per_round_collateral_withdraw_jitosol } => {
            // Mirror leverage.rs round loop (lines 192-335)
            let mut local_obligation_reserves = obligation.deposits ++ obligation.borrows  // by pubkey
            for round in 1..=rounds {
                // Deadline + paused checks each round
                if deadline exceeded { break; }

                // Re-quote per round (jitoSOL/SOL rate drifts)
                let δ_jitosol = per_round_collateral_withdraw_jitosol
                let expected_sol_out = jito_pool.jitosol_to_sol(δ_jitosol) * 0.995  // 0.5% haircut
                let δ_sol_repay = expected_sol_out.min(remaining_debt)

                let ixns = build_unwind_iterative_round_bundle(...)
                ctx.whitelist.verify_ixns(&ixns)?

                let sig = ctx.rpc.build_sign_send_with_alts(...)
                signatures.push(sig)

                // Local-bump (mirrors leverage.rs lines 314-319 in reverse):
                //   - if remaining_debt now 0, conceptually remove SOL from
                //     local_obligation_reserves; but klend leaves the slot
                //     with borrowed_amount=0, so safer to keep it (the
                //     RefreshObligation tolerates it).
                //   - withdrawing collateral does NOT remove the deposit
                //     slot until full redeem with obligation_close.

                // Re-read LTV for next round's δ sizing (this is the slow
                // path — accepts the same RPC read-replica lag risk as
                // lever-up has).
                let new_ltv = query_position_ltv_bps(user, lending_market)
                if new_ltv == 0 { break; }
            }

            // Final round: withdraw any remaining jitoSOL with u64::MAX
            // (no debt left → klend permits full release).
            if any_jitosol_left {
                let final_ixns = build_full_collateral_release_bundle(...)
                let sig = ...
                signatures.push(sig)
            }
        }
    }

    // Optional follow-up tx: swap residual jitoSOL/SOL → USDC
    // (only if WithdrawMultiplyParams someday gets a "leave_as=USDC" flag —
    // v1 leaves SOL native).
    let final_usdc = 0  // we don't auto-convert in v1; emergency-withdraw
                        // sweep handles its own currency choice
    let residual_sol = ctx.rpc.client.get_balance(user)?

    Ok(ReportMultiplyWithdraw {
        header: ok(conv),
        final_usdc_lamports: final_usdc,
        residual_sol_lamports: residual_sol,
        tx_signatures: signatures,
    })
}
```

### Post-unwind sanity check

After broadcast confirms (or sim ok), fetch obligation once more and
assert `borrows.iter().all(|b| b.borrowed_amount_sf == 0)` AND
`deposits.iter().all(|d| d.deposited_amount == 0)`. If either is
non-zero on submit (i.e., it's a real tx, not a sim), surface
`error_code = 14 = "post-unwind sanity check failed"`. This catches
silent-success failures where the tx lands but some round didn't
fully drain.

---

## §7 Test plan

### Per-commit unit tests

- **protocols/kamino.rs (commit 1)**
  - `repay_obligation_liquidity_v2_ix`: discriminator matches, 12 accounts,
    farm-present vs farm-absent paths, u64::MAX round-trip via Borsh.
  - `withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix`:
    discriminator, 17 accounts, lending_market_authority present, farm
    paths, placeholder-None sentinel = KAMINO_LEND_PROGRAM_ID.
- **kamino-klend-bundle-spec.md (commit 2)** — no runnable tests; pure docs.
- **multiply-daemon/src/dispatch.rs (commit 3)**
  - `payload_filter_tests`: WithdrawMultiply CBOR passes filter,
    AssignStableLend with WithdrawMultiply msg_type slot rejected.
  - `sender_allowlist_tests`: WithdrawMultiply from unauthorized sender
    is silently dropped.
  - Paused-by-riskwatcher rejects WithdrawMultiply with code 4.
- **multiply-daemon/src/unwind.rs (commit 4)**
  - `decide_unwind_strategy`:
    - Empty obligation → Noop
    - Borrow only, no collateral (impossible in production but defensive) → Iterative noop
    - Normal multiply position with adequate flash cap → FlashLoan
    - Position larger than flash cap → Iterative
    - Jupiter quote fails → Iterative (Jito stake-pool fallback inside builder)
  - `build_unwind_flash_bundle` shape assertions, identical pattern to
    lever-up's bundle tests at leverage.rs:644-735:
    - FlashBorrow precedes FlashRepay in ix order
    - `borrow_instruction_index` field on FlashRepay equals the absolute
      index of FlashBorrow (not relative)
    - RefreshObligation appears immediately before Repay and Withdraw
    - RefreshReserve(jitoSOL) AND RefreshReserve(SOL) both appear before
      EACH RefreshObligation
    - close_wsol_ata is the last ixn
    - Jupiter swap ixns appear between Withdraw and FlashRepay
  - `build_unwind_iterative_round_bundle` shape assertions:
    - Withdraw precedes Repay
    - All required refreshes positioned correctly
- **multiply-daemon/src/caps.rs (commit 4)**
  - `validate_withdraw_multiply`: accepts within caps, rejects zero vault,
    rejects slippage > MAX_SLIPPAGE_BPS.
- **multiply-daemon/src/dispatch.rs wiring (commit 5)**
  - Re-validate caps on Approve path (mirror existing I2 pattern at
    dispatch.rs:233-243).
  - Approval-queue integration: enqueueing a WithdrawMultiply emits
    NeedsApproval Escalate to the orchestrator; Approve from same sender
    triggers `unwind::run_or_simulate`.
- **fleet-pm-stub (commit 6)**
  - `build_envelope_from_cmd` for `Cmd::WithdrawMultiply` produces an
    envelope with `MsgType::WithdrawMultiply` and a CBOR-decodable
    `WithdrawMultiply` payload.

### Integration tests (commit 7, optional)

- `crates/multiply-daemon/tests/unwind_roundtrip.rs` — spawn a fake
  orchestrator + fake daemon (libp2p mesh on ephemeral port), send
  WithdrawMultiply, assert ReportMultiplyWithdraw comes back with
  `header.ok=true` and tx_signatures matches the simulated count.
  Mock RPC: use `mock-rpc-server` (already a dev-dep on multiply-daemon
  via stable-yield's pattern) for the obligation+reserve fetches.

### What CANNOT be unit-tested

- Real flash-loan landed-on-chain — only mainnet sim or real submit
  resolves this. **The kamino-klend-bundle-spec.md devnet placeholder
  reserves will always return "InvalidAccountInput" on simulate** —
  same caveat as lever-up's M5 milestone.
- Real Jupiter quote — devnet doesn't have Jupiter aggregator. Sim-only
  mode swaps in a synthetic Jupiter ixn list (1 ixn touching the
  Jupiter v6 program id) to exercise the whitelist + ix-ordering tests.
- Read-replica lag between rounds — only a mainnet multi-round iterative
  unwind reproduces it. Mirror lever-up's mitigation: local obligation
  reserve bump.

---

## §8 Commit sequence

Target 7 commits. Each compilable + tested. Lever-down lands as a
chained PR set, identical shape to how lever-up's M1-M6 milestones
shipped.

| # | Commit | Surface | Why safe checkpoint |
|---|--------|---------|---------------------|
| **0 (prelim)** | protocols: lift `jupiter::get_swap_instructions` from hedgedjlp-daemon into `zerox1-defi-protocols/src/protocols/jupiter.rs`; re-export from hedgedjlp | `crates/zerox1-defi-protocols/src/protocols/jupiter.rs` (new); `crates/zerox1-defi-protocols/src/protocols/mod.rs`; `crates/hedgedjlp-daemon/src/jupiter.rs` (deletion) | Pure refactor. Existing hedgedjlp behavior unchanged. Required so multiply can call Jupiter without depending on hedgedjlp. |
| 1 | protocols: add `repay_obligation_liquidity_v2_ix` + `withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix` + unit tests | `crates/zerox1-defi-protocols/src/protocols/kamino.rs` | Pure additive. No caller yet. Tests prove discriminator + account-list correctness. |
| 2 | docs: extend `kamino-klend-bundle-spec.md` with repay + withdraw + flash-loan unwind sections | `docs/kamino-klend-bundle-spec.md` | Pure docs. Mirror existing borrow/deposit sections shape. |
| 3 | multiply: `WithdrawMultiply` msg type plumbing + dispatch arm with no-op body | `crates/multiply-daemon/src/dispatch.rs`; `crates/multiply-daemon/src/caps.rs`; `crates/multiply-daemon/src/approval.rs`; mesh protocol crate (`../p2p_architecture/.../multiply.rs` + `message.rs`) | Envelope plumbing only. Body returns ok=true with empty signatures so the orchestrator can round-trip the path before any chain work. |
| 4 | multiply: `unwind.rs` pure round-builder + strategy decision + tests | `crates/multiply-daemon/src/unwind.rs` (new); `crates/multiply-daemon/src/main.rs` (module decl); `crates/multiply-daemon/src/kamino.rs` (Jupiter whitelist) | Builder functions are pure; tests pin the bundle shape. No live exec yet — `run_or_simulate` is still unwired. |
| 5 | multiply: wire `unwind::run_or_simulate` into dispatch + approval-queue integration | `crates/multiply-daemon/src/dispatch.rs`; `crates/multiply-daemon/src/approval.rs` | Plumbing-only diff. Behavior gated by `--simulate` until M-promote step. |
| 6 | fleet-pm-stub: `WithdrawMultiply` subcommand | `tools/fleet-pm-stub/src/main.rs` | Operator can now manually fire from CLI. |
| 7 (optional) | integration test with mock RPC + libp2p loopback | `crates/multiply-daemon/tests/unwind_roundtrip.rs` | End-to-end round-trip without mainnet. |
| (out-of-band) | M8 mainnet $50 unwind on the existing live position; runbook at `docs/runbooks/multiply-withdraw.md` | `docs/runbooks/multiply-withdraw.md` (new) | Verifies klend v2 acct lists, real flash-fee math, real Jupiter route. |

**LOC delta estimate (excluding tests):**
- Commit 0 (prelim): ~150 LOC moved (refactor, net ~0)
- Commit 1: ~280 LOC of ixn builders (mirror v2 borrow/deposit at 245 LOC)
- Commit 2: ~120 lines of markdown
- Commit 3: ~250 LOC (mesh types + dispatch arm + caps)
- Commit 4: ~400 LOC (unwind.rs + builders)
- Commit 5: ~150 LOC (dispatch wiring + approval queue extension)
- Commit 6: ~50 LOC (clap variant + envelope-build arm)
- Commit 7: ~200 LOC (integration test)
- **Total: ~1300 LOC + ~600 LOC tests.** ~3-4 working days for a
  developer who has already done the lever-up grind, given the bundle
  shape is now spec'd up-front.

---

## §9 Open questions / risks

### 9.1 Top risks

1. **Flash-loan availability on Kamino main SOL reserve.** Production
   positions ($14 collateral / $5 debt) are well within any plausible
   cap, but the cap is set by Kamino governance and could shift. *Mitigation:
   the strategy decision (§6) falls back to Option-1 iterative if
   `flash_cap < total_debt`. The fallback is implemented in commit 4
   alongside the primary path.*
2. **Jupiter route availability for full-position swap.** A $14 jitoSOL
   swap is trivial today, but in a future where the multiply daemon
   operates at $5M cap, route availability + slippage cap might force
   the iterative path. *Mitigation: bundle builder attempts Jupiter first,
   falls back to `jito_stake_pool::WithdrawSol` direct redemption with
   the same slippage check.*
3. **klend v2 handler files were 404'd via WebFetch.** The lib.rs
   declarations exist (verified). The handler structs may be inside the
   v1 handler files behind a `#[cfg]` flag or in a shared module — same
   pattern Kamino uses for the v2 borrow + deposit that we already
   integrate. *Mitigation: the v2 account list pattern is identical to
   our existing v2 builders (verified verbatim against klend-sdk codegen
   in the lever-up era). Sim-only on devnet against placeholder reserves
   will surface any mismatch before mainnet broadcast.*

### 9.2 Spec ambiguities

- **Default leave-as: SOL or USDC?** Stable-yield withdraws to USDC.
  Multiply could leave the user with native SOL (less swap risk + closer
  to the asset they staked) or convert to USDC (closer to "return on
  principal"). **Recommendation: leave as SOL in v1, with a future
  param `leave_as: AssetKind` for USDC conversion. Emergency-withdraw's
  sweep step handles the currency-of-record question separately.**
  Open — needs operator decision.
- **Should `WithdrawMultiply` be approval-queued like `AssignMultiply`?**
  Mirror Assign's behavior: queue when `require_approval` is set, emit
  NeedsApproval Escalate. This is consistent with stable-yield. **Open**
  — but mirroring is the safe default.
- **Iterative-path final round semantics.** When debt reaches 0 and
  collateral remains, klend permits `withdraw u64::MAX` to drain the
  cToken slot AND closes the obligation account. The latter is
  irreversible without re-`init_obligation_ix`. If we close it,
  the next lever-up will pay the rent for re-init (~0.0025 SOL). *Recommend:
  keep the obligation open (don't withdraw the literal last lamport of
  cToken). Re-leveraging is then a fast path.* — but this is mainnet
  experimentation, mark as **open**.

### 9.3 Failure modes

- **Partial unwind (iterative path, RPC desync).** Round N's tx lands
  but RPC read-replica returns the pre-round state; round N+1 builds
  the wrong δ. *Recovery: local-bump pattern from lever-up; if it still
  diverges, the post-unwind sanity check (§6) catches it and returns
  error_code=14. Operator retries via the fleet-pm-stub WithdrawMultiply
  command, which is idempotent — the strategy decision will re-evaluate
  to a smaller round count.*
- **Flash-loan tx lands but Jupiter swap step underflows the SOL needed
  for FlashRepay.** Slippage on the swap returns less SOL than we
  flash-borrowed. *Mitigation: pre-quote Jupiter at bundle-build time
  with a 0.5% safety haircut on `expected_sol_out`. If quote < debt
  even at no-slippage, decide_unwind_strategy returns Iterative.*
- **wSOL ATA close at the end fails because Jupiter routes left wSOL
  in a different ATA.** Not a real risk because we always ATA-create
  the user's canonical wSOL ATA at ixn 2, and Jupiter respects the
  destination ATA passed in /swap-instructions params. Defensive: only
  close-wSOL after assertion that the canonical wSOL ATA has zero
  balance (a runtime check would need yet another ixn — skip for now).
- **Unwind stalls partway.** Operator re-runs `fleet-pm-stub
  WithdrawMultiply`. The strategy decision detects partial state
  (`obligation.borrows[].borrowed_amount_sf > 0` AND/OR collateral
  remaining) and selects the appropriate strategy for the residual.
  No manual klend ops needed unless the obligation gets stuck in a
  state klend's refresh rejects.

### 9.4 Operator-decision items (before coding commit 4)

- Leave-as default: SOL vs USDC.
- Approval-queue for WithdrawMultiply: yes/no.
- Final-round obligation-close: yes/no.
- Whether to introduce error_code 14 (post-unwind sanity check) or
  silently allow apparent successes with non-zero residuals.

---

## Out-of-scope / future work

- Partial unwind (lever to a smaller target_ltv without going to 0).
  Today this is handled by sending a smaller `AssignMultiply` with the
  new target_ltv; the lever-up loop will not act when LTV is already
  ≤ target, but it also won't lever DOWN to a smaller target. A future
  `AdjustMultiply { target_ltv_bps }` envelope could call into a
  combined lever-up-or-down strategy.
- Multi-asset multiply (depositing both jitoSOL + mSOL, or borrowing
  USDC instead of SOL). The current unwind is hard-coded to the
  jitoSOL/SOL pair. Generalize when a second multiply asset pair ships.
- Combined-handler optimization. Migrate from the flash-loan-around
  pattern to klend's native `repay_and_withdraw_redeem` handler when we
  add a Jupiter-pre-funded SOL wallet (i.e., when the orchestrator
  pre-positions SOL in the multiply daemon's wallet before sending
  WithdrawMultiply). Not v1.
- Auto-trigger from riskwatcher: when `LIQUIDATION_DISTANCE_CRITICAL_BPS`
  fires AND auto-unwind is enabled in caps.rs, multiply could
  self-unwind without an orchestrator envelope. Out of scope until the
  full unwind has been mainnet-soaked for a month.
