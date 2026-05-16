# Kamino klend bundle spec

Sources:
- klend program: https://github.com/Kamino-Finance/klend/tree/master/programs/klend/src
  - `handlers/handler_borrow_obligation_liquidity.rs`
  - `handlers/handler_deposit_obligation_collateral.rs`
  - `handlers/handler_deposit_reserve_liquidity_and_obligation_collateral.rs`
  - `handlers/handler_refresh_obligation.rs`
  - `utils/refresh_ix_utils.rs` (the `check_refresh` function)
  - `utils/macros.rs` (the `check_refresh_ixs!` macro)
- klend-sdk: https://github.com/Kamino-Finance/klend-sdk/blob/master/src/leverage/operations.ts (`buildDepositWithLeverageIxs`)

---

## The two error sources, decoded

### 6051 IncorrectInstructionInPosition

Raised by `check_refresh` in `utils/refresh_ix_utils.rs`. The macro `check_refresh_ixs!(ctx.accounts, ctx.accounts.<target>_reserve, <FarmKind>)` is invoked at the top of each action handler (borrow/deposit/withdraw/repay v1). It builds a `required_pre_ixs` vec, then walks the sysvar back from `current_idx` and requires the **immediately preceding** instructions to match — by discriminator AND by a specific account-key-at-index check.

For a v1 action on a single reserve with NO farm of the relevant kind, the required pre-ix sequence (closest-to-action first) is:

```
[current_idx - 1] = RefreshObligation(obligation at accounts[1])
[current_idx - 2] = RefreshReserve(target_reserve at accounts[0])
```

If the reserve has a farm of `mode` configured (`reserve.get_farm(mode) != Pubkey::default()`), an additional `RefreshObligationFarmsForReserve` is required at `[current_idx - 1]` (pre) AND `[current_idx + 1]` (post).

CRITICAL: `check_refresh_ixs!` for borrow/deposit ONLY passes a SINGLE reserve (the target). It does NOT require RefreshReserve for the obligation's other reserves to immediately precede the action. Those other reserves only need to be fresh **inside** `RefreshObligation`'s own logic.

### 6009 ReserveStale

Raised inside `RefreshObligation` (and inside `refresh_obligation` / collateral & liquidity checks) when any reserve passed in `remaining_accounts` has `last_update.is_stale(slot, PriceStatusFlags::ALL_CHECKS) == true`. `RefreshReserve` must have been called at the current slot for every reserve in the obligation.

---

## BorrowObligationLiquidity

### Account list (in order)

```
0  owner                              Signer, mut(implicit fee payer)
1  obligation                         AccountLoader, mut, has_one=lending_market, has_one=owner
2  lending_market                     AccountLoader
3  lending_market_authority           PDA [LENDING_MARKET_AUTH, lending_market]
4  borrow_reserve                     AccountLoader, mut, has_one=lending_market
5  borrow_reserve_liquidity_mint      InterfaceAccount<Mint>, address = borrow_reserve.liquidity.mint_pubkey
6  reserve_source_liquidity           mut, address = borrow_reserve.liquidity.supply_vault
7  borrow_reserve_liquidity_fee_recv  mut, address = borrow_reserve.liquidity.fee_vault
8  user_destination_liquidity         mut, token::mint=reserve_source_liquidity.mint, token::authority=owner
9  referrer_token_state               Option<AccountLoader<ReferrerTokenState>>, mut  (None if obligation has no referrer; pass Pubkey::default() or omit per anchor Option encoding)
10 token_program                      Interface<TokenInterface>
11 instruction_sysvar_account         address = SysInstructions::id()  (Sysvar1nstructions...)
```

### remaining_accounts

`deposit_reserves_iter` is built directly from `remaining_accounts` (after stripping the optional trailing permission account). The handler reads them as `FatAccountLoader<Reserve>` and uses them to compute collateral value for the borrow.

**Order:** ALL `obligation.deposits` reserves, in the same slot order they appear in `obligation.deposits` (NOT including borrow reserves — borrow reserves are NOT in remaining_accounts for BorrowObligationLiquidity).

Optional last item: lending-market permission account (only when the market is permissioned; klend will detect via `check_permissions` and strip it).

### Required pre-instructions (in order, immediately before the borrow)

For a borrow_reserve with NO debt farm:
```
N-2: RefreshReserve(borrow_reserve)            // borrow_reserve at accounts[0]
N-1: RefreshObligation(obligation)             // obligation at accounts[1]
N:   BorrowObligationLiquidity
```

For a borrow_reserve WITH a `Debt` farm configured:
```
N-3: RefreshReserve(borrow_reserve)
N-2: RefreshObligation(obligation)
N-1: RefreshObligationFarmsForReserve(reserve at accounts[3], obligation at [1], farm at [4], mode=Debt)
N:   BorrowObligationLiquidity
N+1: RefreshObligationFarmsForReserve(...)     // post-ix mirror
```

Note: `RefreshObligation` itself requires its own `remaining_accounts` to contain every reserve currently in the obligation (deposits then borrows), each of which must have been refreshed at the current slot. So a real bundle looks like:

```
RefreshReserve(deposit_reserve_1)
RefreshReserve(deposit_reserve_2)
...
RefreshReserve(borrow_reserve_target)
RefreshObligation(obligation, remaining=[all deposits..., all borrows...])
BorrowObligationLiquidity(borrow_reserve_target, remaining=[all deposits...])
```

### Anchor errors that can fire

- 6009 ReserveStale — a reserve in obligation.deposits or obligation.borrows hasn't been refreshed this slot
- 6017 BorrowTooLarge / 6018 BorrowTooSmall
- 6020 ObligationDepositsEmpty
- 6024 ObligationBorrowsZero
- 6029 ObligationStale — obligation not refreshed this slot
- 6050 CpiDisabled
- 6051 IncorrectInstructionInPosition — pre-ix pattern mismatch (see above)
- 6052 PriceTooOld / 6053 NoPriceFound / 6054 InvalidTwapPrice — price stale (treat as upstream Pyth/Scope refresh)
- LtvExceeded / borrow-cap variants from `borrow_obligation_liquidity` in lending_operations.rs

---

## DepositObligationCollateral

### Account list (in order)

```
0  owner                              Signer
1  obligation                         mut, has_one=owner, has_one=lending_market
2  lending_market                     AccountLoader
3  deposit_reserve                    mut, has_one=lending_market
4  reserve_destination_collateral     mut, address = deposit_reserve.collateral.supply_vault
5  user_source_collateral             mut, token::mint = deposit_reserve.collateral.mint_pubkey
6  token_program                      Program<Token>  (SPL Token classic, NOT Token-2022 — this is the cToken side)
7  instruction_sysvar_account         address = SysInstructions::id()
```

### remaining_accounts

For v1: optional permission account only (`remaining_accounts.last()`). The handler does NOT iterate other reserves — collateral is bookkept against the obligation directly via `deposit_obligation_collateral` in lending_operations.

### Required pre-instructions

`check_refresh_ixs!(accounts, deposit_reserve, ReserveFarmKind::Collateral)` — same single-reserve pattern as borrow.

For deposit_reserve with NO collateral farm:
```
N-2: RefreshReserve(deposit_reserve)
N-1: RefreshObligation(obligation)             // remaining_accounts = ALL obligation reserves (deposits + borrows)
N:   DepositObligationCollateral
```

For deposit_reserve WITH a `Collateral` farm: prepend `RefreshReserve` further back, and add `RefreshObligationFarmsForReserve` immediately before & immediately after (same shape as borrow).

### Anchor errors

- 6009 ReserveStale (inside RefreshObligation), 6029 ObligationStale
- 6051 IncorrectInstructionInPosition
- 6045 ReserveDeposit­LimitExceeded
- MaxReservesAsCollateralCheck — fails if adding this reserve would exceed `lending_market.max_reserves_as_collateral`. NOTE: idempotent re-deposit of an already-deposited reserve is fine.
- 6019 DepositTooSmall / 0 amount errors

---

## DepositReserveLiquidityAndObligationCollateral

This is the **combined** instruction the seed-deposit step uses. It performs `DepositReserveLiquidity` (mint cTokens to a placeholder) + `DepositObligationCollateral` in a single ix, so the user only has to hold the underlying liquidity token, not the cToken.

### Account list (in order)

```
0  owner                                Signer, mut
1  obligation                           mut, has_one=lending_market, has_one=owner
2  lending_market                       AccountLoader
3  lending_market_authority             PDA
4  reserve                              mut, has_one=lending_market
5  reserve_liquidity_mint               InterfaceAccount<Mint>, address = reserve.liquidity.mint_pubkey
6  reserve_liquidity_supply             mut, address = reserve.liquidity.supply_vault
7  reserve_collateral_mint              mut, address = reserve.collateral.mint_pubkey
8  reserve_destination_deposit_coll     mut, address = reserve.collateral.supply_vault
9  user_source_liquidity                mut, token::mint = reserve.liquidity.mint, authority = owner
10 placeholder_user_destination_coll    Option<AccountInfo>  (None — cToken is auto-deposited; pass program id sentinel)
11 collateral_token_program             Program<Token>          (classic SPL)
12 liquidity_token_program              Interface<TokenInterface>  (Token-2022 capable; match reserve.liquidity.mint)
13 instruction_sysvar_account           address = SysInstructions::id()
```

### remaining_accounts

Same as plain DepositObligationCollateral: optional last permission account, nothing else.

### Required pre-instructions

Inside this combined handler the macro again does `check_refresh_ixs!(accounts, reserve, ReserveFarmKind::Collateral)`. So:

```
N-2: RefreshReserve(reserve)
N-1: RefreshObligation(obligation, remaining = all existing obligation reserves)
N:   DepositReserveLiquidityAndObligationCollateral
```

If `reserve` has a Collateral farm: insert `RefreshObligationFarmsForReserve` immediately pre and immediately post.

### Anchor errors

Same set as DepositReserveLiquidity + DepositObligationCollateral. Watch out for `ReserveDeposit­LimitExceeded` (6045) on the reserve-deposit half.

---

## RefreshObligation

### Account list

```
0  lending_market                       AccountLoader
1  obligation                           mut, has_one=lending_market
```

### remaining_accounts (EXACT shape)

```
[0 .. deposit_count]                     = obligation.deposits[*].deposit_reserve in slot order
[deposit_count .. deposit_count+borrow_count] = obligation.borrows[*].borrow_reserve in slot order
[deposit_count+borrow_count ..]          = referrer_token_state per active borrow (only if obligation.has_referrer())
```

Handler enforces `remaining_accounts.len() == reserves_count + (borrow_count if has_referrer else 0)`. Mismatch → `InvalidAccountInput` (6001).

**Order matters.** Deposits first, in the exact order they appear in `obligation.deposits`, then borrows in `obligation.borrows` order.

### Required pre-instructions

None enforced by `check_refresh_ixs!` — but `refresh_obligation` internally calls `check_obligation_collateral_deposit_reserve` and `check_obligation_liquidity_borrow_reserve` on each reserve, both of which fail with 6009 ReserveStale if the reserve hasn't had `RefreshReserve` at the current slot.

So in practice: every reserve in remaining_accounts needs a `RefreshReserve` somewhere earlier in the tx, at the current slot.

### Anchor errors

- 6001 InvalidAccountInput — wrong remaining_accounts length
- 6009 ReserveStale — one of the reserves wasn't RefreshReserved this slot
- 6052/6053/6054 — price oracle issues
- 6027 ObligationReserveLimit, etc.

---

## RefreshReserve

### Account list (in order)

```
0  reserve                              mut
1  lending_market                       AccountLoader
2  pyth_oracle                          Option<AccountInfo>
3  switchboard_price_oracle             Option<AccountInfo>
4  switchboard_twap_oracle              Option<AccountInfo>
5  scope_prices                         Option<AccountInfo>
```

Pass `crate::id()` (or `None` per Option encoding) for whichever oracles the reserve is not configured for. The reserve's configured oracle source dictates which must be present.

### Required pre-instructions

None.

### Notes

- Idempotent within a slot — calling twice is wasteful but safe.
- Touches Pyth/Scope prices; if the oracle itself is stale you'll get 6052/6053/6054.

---

## Multi-reserve obligation gotchas

1. **"All obligation reserves must be fresh" applies to whatever is currently in `obligation.deposits` + `obligation.borrows` at the moment the action runs.** This list changes mid-bundle:
   - After a successful `BorrowObligationLiquidity` of SOL into a previously-jitoSOL-only obligation, the obligation now has 2 reserves (jitoSOL deposit + SOL borrow).
   - The NEXT `RefreshObligation` in the same tx must include BOTH reserves in remaining_accounts, AND BOTH must have been `RefreshReserve`d at the current slot.

2. **RefreshReserve is idempotent per-slot but cheap to repeat.** When in doubt, refresh every reserve fresh before each RefreshObligation.

3. **`check_refresh_ixs!` only looks back 2-4 slots** depending on farm presence. It does NOT walk to the start of the tx. So you can have "stale-looking" reserve refreshes elsewhere — what matters is the IMMEDIATE pre-ix pattern and that all listed reserves are slot-fresh by the time the obligation gets refreshed.

4. **Borrow's remaining_accounts ≠ RefreshObligation's remaining_accounts.** Borrow only wants deposit reserves (used to compute collateral value). RefreshObligation wants deposits AND borrows.

5. **Farm post-ixns.** If either the target reserve has a farm OR an existing obligation reserve has a farm of the matching kind, you need the post-ix `RefreshObligationFarmsForReserve` mirror. With jitoSOL collateral + SOL borrow on Kamino main market: jitoSOL reserve has a Collateral farm; SOL reserve has a Debt farm. **You will hit 6051 the moment you skip the post-ix.** Check `reserve.get_farm(<kind>)` for each before assembling.

6. **wSOL ATA lifecycle around SOL borrow.** Borrowing SOL really borrows wSOL into the user's wSOL ATA. Anchor ATA-create idempotent is fine. After the borrow, sync-native is implicit (the transfer credits the wrapped account). To unwrap: `CloseAccount(wSOL)` → lamports flow to owner. Then to wrap for jito deposit: re-create wSOL ATA + transfer SOL + sync_native. Jito's `DepositSol` itself takes raw SOL though, so the unwrap-then-pass-lamports flow works without re-wrapping.

7. **DepositObligationCollateral does NOT auto-refresh the deposit_reserve in v1.** The handler calls `refresh_reserve` internally for the deposit reserve, BUT this happens AFTER `check_refresh_ixs!`. So you still need the RefreshReserve+RefreshObligation pre-ix pair, and the internal refresh is just defense-in-depth for the deposit math.

8. **`MaxReservesAsCollateralCheck::Perform` runs on every DepositObligationCollateral.** Limits the number of distinct collateral reserves. For a jitoSOL-only multiply, irrelevant. For a mixed-collateral obligation, can fire 6034-ish errors.

9. **Compute budget.** Multi-reserve refresh + obligation refresh + borrow + farm refreshes is heavy. `check_refresh` itself logs CU at start/end ("Beginning check_refresh" / "Finished check_refresh"). Budget at least 600k CU for a 2-reserve lever-up round; 1M is safe.

10. **Order within remaining_accounts is positional.** RefreshObligation reads by index (`take(deposit_count)` then `skip(deposit_count).take(borrow_count)`). Get the order from `obligation.deposits` and `obligation.borrows` arrays directly — do NOT use any sort or set ordering.

---

## Recommended bundle template for multiply lever-up round

Scenario: obligation currently has `[jitoSOL deposit]` (round 1) or `[jitoSOL deposit, SOL borrow]` (round 2+). Action: borrow more SOL, swap to jitoSOL via jito's stake-pool DepositSol, deposit the resulting jitoSOL.

Assume Kamino main-market: jitoSOL reserve has a Collateral farm, SOL reserve has a Debt farm. Adjust if your market config differs (check `reserve.config.token_info.<farm_kind>`).

### Round 1 (obligation = [jitoSOL deposit] only, no SOL borrow yet)

```
1.  ComputeBudgetProgram.SetComputeUnitLimit(1_000_000)
2.  ComputeBudgetProgram.SetComputeUnitPrice(<priority fee>)
3.  AssociatedTokenAccount.CreateIdempotent(wSOL ATA)
                                                            -- start borrow side --
4.  RefreshReserve(jitoSOL_reserve)         // because it's in obligation.deposits
5.  RefreshReserve(SOL_reserve)             // the borrow target
6.  RefreshObligationFarmsForReserve(jitoSOL_reserve, mode=Collateral)   // jitoSOL has Collateral farm AND it's an existing reserve
7.  RefreshObligation(obligation, remaining=[jitoSOL_reserve])
8.  RefreshObligationFarmsForReserve(SOL_reserve, mode=Debt)             // SOL has Debt farm, target of the borrow
9.  BorrowObligationLiquidity(SOL_reserve, remaining=[jitoSOL_reserve])
10. RefreshObligationFarmsForReserve(SOL_reserve, mode=Debt)             // POST-ix mirror
                                                            -- swap leg --
11. CloseAccount(wSOL)                                                   // unwrap to native SOL
12. Jito StakePool DepositSol (instruction 1 of 2)
13. Jito StakePool DepositSol (instruction 2 of 2)
                                                            -- deposit collateral side --
14. AssociatedTokenAccount.CreateIdempotent(jitoSOL ATA)                 // if not already
15. RefreshReserve(jitoSOL_reserve)         // fresh again for the deposit; same slot but RefreshObligation needs it
16. RefreshReserve(SOL_reserve)             // because after borrow at step 9, SOL is in obligation.borrows
17. RefreshObligationFarmsForReserve(jitoSOL_reserve, mode=Collateral)   // target of the deposit
18. RefreshObligation(obligation, remaining=[jitoSOL_reserve, SOL_reserve])  // deposits=[jitoSOL], borrows=[SOL]
19. DepositObligationCollateral(jitoSOL_reserve, remaining=[])           // OR DepositReserveLiquidityAndObligationCollateral if depositing native jitoSOL liquidity
20. RefreshObligationFarmsForReserve(jitoSOL_reserve, mode=Collateral)   // POST-ix mirror
```

### Round 2+ (obligation already has [jitoSOL deposit, SOL borrow])

Same as round 1, except step 4-7 already need SOL_reserve in the RefreshObligation remaining:

```
4. RefreshReserve(jitoSOL_reserve)
5. RefreshReserve(SOL_reserve)
6. RefreshObligationFarmsForReserve(jitoSOL_reserve, mode=Collateral)    // existing deposit with farm
7. RefreshObligationFarmsForReserve(SOL_reserve, mode=Debt)              // existing borrow with farm
8. RefreshObligation(obligation, remaining=[jitoSOL_reserve, SOL_reserve])
9. (immediately before borrow) RefreshObligationFarmsForReserve(SOL_reserve, mode=Debt)   // pre-ix for borrow
10. BorrowObligationLiquidity(SOL_reserve, remaining=[jitoSOL_reserve])
11. RefreshObligationFarmsForReserve(SOL_reserve, mode=Debt)             // post
... (then swap + deposit half same as round 1)
```

NOTE: `check_refresh_ixs!` only requires the immediately-preceding refresh sequence. It does NOT care that you also refreshed farms for other reserves earlier in the tx. So you can refresh all farms once up-front, then add a "duplicate" farm refresh immediately before each action — and a post-ix mirror immediately after.

### Why two RefreshObligations (steps 7/18) in one tx?

After step 9 (BorrowObligationLiquidity), the obligation's borrows array changes (a new entry is added, or the existing SOL borrow position grows + slot mark moves). The obligation gets marked stale (`obligation.last_update.mark_stale()`). The deposit at step 19 requires obligation freshness, hence the second RefreshObligation at step 18. Yes, this is expensive on CU — but unavoidable on v1 handlers.

(v2 handlers do CPI farm refresh internally, but they only collapse the farm refresh step, not the obligation refresh requirement.)

---

## Diff vs our current bundle (v0.1.16)

Our current bundle:
```
1. ComputeBudget setUnits
2. ComputeBudget setPrice
3. ATA-create idempotent (wSOL)
4. RefreshReserve(jitoSOL_obligation_collateral)
5. RefreshReserve(SOL_borrow_action)
6. RefreshObligation(remaining_accounts = obligation's reserves)
7. BorrowObligationLiquidity(SOL)
8. CloseAccount(wSOL)
9. Jito DepositSol (2 ixns)
10. RefreshReserve(jitoSOL)
11. RefreshObligation
12. DepositObligationCollateral(jitoSOL)
```

### Divergences from the canonical pattern

**D1. Missing `RefreshObligationFarmsForReserve` pre-ix for the SOL borrow.** If Kamino's SOL reserve has a Debt farm configured (it does on main market — every PYUSD/USDC/SOL reserve on the main market has farms), then `check_refresh` for `BorrowObligationLiquidity` requires `RefreshObligationFarmsForReserve` immediately before the borrow AND immediately after. Without it: 6051 IncorrectInstructionInPosition. **This is your next bug.** Either:
   - Detect `borrow_reserve.config.token_info.debt_farm != Pubkey::default()` and inject the pre+post farm refresh ixns, OR
   - Switch to the v2 handler (`BorrowObligationLiquidityV2`) which does the farm refresh via CPI internally.

**D2. Missing `RefreshObligationFarmsForReserve` pre-ix for the jitoSOL deposit.** Same story on the deposit side — jitoSOL reserve has a Collateral farm on main market. Need pre+post `RefreshObligationFarmsForReserve(jitoSOL, mode=Collateral)` around step 12. Without it: 6051. **Same bug class.**

**D3. RefreshObligation remaining_accounts at step 11 must include SOL borrow reserve.** After step 7 succeeds, the obligation has a SOL borrow entry. Step 11's `RefreshObligation` must therefore have remaining_accounts = `[jitoSOL_reserve, SOL_reserve]` in that order (deposits then borrows). If you currently pass only `[jitoSOL_reserve]`, step 11 will fail with `InvalidAccountInput` (6001), not 6009.
   - Additionally, the SOL reserve must have been `RefreshReserve`d at the current slot. Add a step `10.5: RefreshReserve(SOL_reserve)` before step 11.

**D4. `RefreshObligation` at step 6 — verify remaining_accounts shape.** On round 1, obligation only has jitoSOL deposit, so remaining_accounts=[jitoSOL] is correct. On round 2+, it must be `[jitoSOL, SOL]`. The runtime needs to know the round and build accordingly. If you hardcode `[jitoSOL]` you'll get 6001 on round 2 and beyond.

**D5. CloseAccount(wSOL) before deposit — re-create needed if jitoSOL ATA missing.** You currently close wSOL before depositing, which is fine for the SOL→jitoSOL swap step. But ensure you `CreateIdempotent(jitoSOL ATA)` BEFORE step 12. Right now your bundle relies on it existing; add an idempotent create to be safe.

**D6. Compute budget.** 200k default is not enough with all farm refreshes layered on. Bump to 800k–1_000_000 explicitly.

**D7. Post-ix farm refreshes.** `check_refresh` also walks FORWARD (`AppendedIxType::PostIxs`). When farms are present, a `RefreshObligationFarmsForReserve` is required immediately after the action. Currently you have no post-ix at all between borrow and the next thing → 6051 the instant farms are involved.

### Recommended fix order

1. Detect farm presence per-reserve at bundle-build time (load reserve account, check `reserve.farm_collateral` and `reserve.farm_debt`). Cache it.
2. Conditionally inject `RefreshObligationFarmsForReserve` pre+post around `BorrowObligationLiquidity` and around `DepositObligationCollateral`.
3. Track the obligation's deposit/borrow set across rounds. The second `RefreshObligation` (post-borrow, pre-deposit) needs to reflect the just-incremented borrow.
4. Bump CU limit to 1_000_000.
5. Consider migrating to the v2 handlers (`BorrowObligationLiquidityV2`, `DepositObligationCollateralV2`) which collapse the farm-refresh ixns into a CPI inside the action. That eliminates the 6051 farm-position trap entirely — but their account list is slightly different (additional `lending_market_authority`, `farms_accounts`, `farms_program`).

---

## RepayObligationLiquidityV2

Source: `handlers/handler_repay_obligation_liquidity.rs` (v1 account list) +
klend lib.rs (`repay_obligation_liquidity_v2` declaration). The v2 handler
appends a 3-account farm appendix and does the Debt-farm refresh CPI
internally — eliminating the manual `RefreshObligationFarmsForReserve`
pre/post-ix pair required by the v1 handler when the repay reserve has a
Debt farm configured.

### Account list (in order, 12 total = v1 9 + v2 farm appendix 3)

```
0   owner                              Signer
1   obligation                         AccountLoader, mut, has_one=lending_market
                                       constraint: obligation.lending_market == repay_reserve.lending_market
2   lending_market                     AccountLoader
3   repay_reserve                      AccountLoader, mut, has_one=lending_market  (= SOL reserve for our unwind)
4   reserve_liquidity_mint             address = repay_reserve.liquidity.mint_pubkey  (= WSOL_MINT)
5   reserve_destination_liquidity      mut, address = repay_reserve.liquidity.supply_vault
6   user_source_liquidity              mut, token::mint = repay_reserve.liquidity.mint_pubkey  (= user's wSOL ATA)
7   token_program                      Interface<TokenInterface>
8   instruction_sysvar_account         address = SysInstructions::id()
                                       constraint: ix_utils::no_restricted_programs_within_tx
─── v2 farm appendix ──────────────────────────────────────────────────────
9   obligation_farm_user_state         mut if reserve.farm_debt != Pubkey::default(), else readonly None sentinel (= KAMINO_LEND_PROGRAM_ID)
10  reserve_farm_state                 mut if farm present, else readonly None sentinel
11  farms_program                      KAMINO_FARMS_PROGRAM_ID
```

### remaining_accounts

None. The v1 handler accepts an optional permission account as the last
remaining_account when the market is permissioned; klend strips it via
`check_permissions`. Skip for permissionless markets (Kamino main market
is permissionless).

### Required pre-instructions (in order, immediately before the repay)

`check_refresh_ixs!(ctx.accounts, ctx.accounts.repay_reserve, ReserveFarmKind::Debt)`
runs at the top of the v2 handler. Even though the Debt-farm refresh CPI
happens inside the handler, the macro still enforces the standard pre-ix
shape:

```
N-2: RefreshReserve(repay_reserve)             // repay_reserve at accounts[3]
N-1: RefreshObligation(obligation)             // obligation at accounts[1]
N:   RepayObligationLiquidityV2
```

`RefreshObligation` itself still requires `remaining_accounts` to contain
every reserve currently in the obligation (deposits then borrows), each
of which must have been `RefreshReserve`d at the current slot. So the
real bundle looks like:

```
RefreshReserve(deposit_reserve_1)              // every obligation deposit reserve
RefreshReserve(...)
RefreshReserve(repay_reserve)                  // the Repay target (may already be in obligation.borrows)
RefreshObligation(obligation, remaining=[all deposits..., all borrows...])
RepayObligationLiquidityV2(repay_reserve, amount)
```

### Anchor errors that can fire

- 6001 InvalidAccountInput — wrong remaining_accounts length / wrong key
- 6009 ReserveStale — a reserve in obligation hasn't been refreshed this slot
- 6029 ObligationStale — obligation not refreshed this slot
- 6050 CpiDisabled
- 6051 IncorrectInstructionInPosition — pre-ix pattern mismatch
- 6052 / 6053 / 6054 — price oracle staleness
- ObligationLiquidityEmpty — repaying against a borrow slot already at 0
  (safe to swallow at our daemon layer: it means unwind already happened)
- arithmetic overflow on penalty / accrued-interest accumulation (rare)

### `amount` semantics

- `amount == u64::MAX` is the klend sentinel meaning "repay the full borrow
  slot". klend clamps server-side to `borrowed_amount_sf` at the moment of
  execution. Recommended for unwind to avoid leaving sub-lamport interest
  accrued between bundle-build and tx-land.
- Zero is rejected (handler ZeroAmount).

---

## WithdrawObligationCollateralV2

Source: klend lib.rs (`withdraw_obligation_collateral_v2` declaration) +
the v1 `handler_withdraw_obligation_collateral.rs` account list. The v2
handler appends a 3-account farm appendix and does the Collateral-farm
refresh CPI internally.

### Account list (in order, 12 total = v1 9 + v2 farm appendix 3)

```
0   owner                              Signer
1   obligation                         mut, has_one=lending_market, has_one=owner
2   lending_market                     AccountLoader
3   lending_market_authority           PDA seeds=[LENDING_MARKET_AUTH, lending_market]
4   withdraw_reserve                   mut, has_one=lending_market
5   reserve_source_collateral          mut, address = withdraw_reserve.collateral.supply_vault
6   user_destination_collateral        mut, token::mint = withdraw_reserve.collateral.mint_pubkey, token::authority = owner
7   token_program                      Program<Token>  (SPL Token classic — cToken side)
8   instruction_sysvar_account         address = SysInstructions::id()
─── v2 farm appendix ──────────────────────────────────────────────────────
9   obligation_farm_user_state         mut if reserve.farm_collateral != Pubkey::default(), else readonly None sentinel
10  reserve_farm_state                 mut if Collateral farm present, else readonly None sentinel
11  farms_program                      KAMINO_FARMS_PROGRAM_ID
```

### remaining_accounts

None (optional permission account on permissioned markets — irrelevant
for Kamino main market).

### Required pre-instructions

`check_refresh_ixs!(accounts, withdraw_reserve, ReserveFarmKind::Collateral)`:

```
N-2: RefreshReserve(withdraw_reserve)          // withdraw_reserve at accounts[4]
N-1: RefreshObligation(obligation)             // remaining_accounts = ALL obligation reserves
N:   WithdrawObligationCollateralV2
```

### Anchor errors that can fire

- 6001 / 6009 / 6029 / 6050 / 6051 — same suite as Repay
- WithdrawTooLarge — never our case for full unwind; klend clamps
  `u64::MAX` to the actual deposited amount
- LtvExceeded family — withdrawing collateral cannot push remaining LTV
  above the liquidation threshold. With flash-loan-driven debt-to-zero,
  this is moot: the borrow is repaid BEFORE the withdraw, so post-withdraw
  LTV is 0/0 = undefined and klend short-circuits.

### `collateral_amount` semantics

- `collateral_amount == u64::MAX` → klend redeems the obligation's entire
  cToken slot for this reserve. Standard sentinel for full unwind.
- Zero is rejected.
- Output is **cTokens** to the user's cToken ATA — caller is then
  responsible for redeeming them back to underlying liquidity (or use the
  combined `WithdrawObligationCollateralAndRedeemReserveCollateralV2`
  below to do both in one ixn).

---

## WithdrawObligationCollateralAndRedeemReserveCollateralV2

Source: `handlers/handler_withdraw_obligation_collateral_and_redeem_reserve_collateral.rs`
(13-account v1 struct) + v2 farm appendix. **This is the canonical
collateral-out leg for the lever-down unwind** — pulls cTokens from the
obligation, burns them, and sends the underlying liquidity (jitoSOL) to
the user's liquidity ATA in a single ixn.

### Account list (in order, 17 total = v1 14 + v2 farm appendix 3)

```
0   owner                              Signer, mut
1   obligation                         mut, has_one=lending_market, has_one=owner
2   lending_market                     AccountLoader
3   lending_market_authority           PDA
4   withdraw_reserve                   mut, has_one=lending_market  (= jitoSOL reserve)
5   reserve_liquidity_mint             address = withdraw_reserve.liquidity.mint_pubkey  (= JITOSOL_MINT)
                                       mint::token_program = liquidity_token_program
6   reserve_source_collateral          mut, address = withdraw_reserve.collateral.supply_vault
7   reserve_collateral_mint            mut, address = withdraw_reserve.collateral.mint_pubkey
8   reserve_liquidity_supply           mut, address = withdraw_reserve.liquidity.supply_vault
9   user_destination_liquidity         mut, token::mint = withdraw_reserve.liquidity.mint_pubkey, token::authority = owner
10  placeholder_user_destination_coll  Option<AccountInfo> — pass KAMINO_LEND_PROGRAM_ID as None sentinel
11  collateral_token_program           Program<Token>  (classic SPL, NOT Token-2022)
12  liquidity_token_program            Interface<TokenInterface>
13  instruction_sysvar_account         address = SysInstructions::id()
─── v2 farm appendix ──────────────────────────────────────────────────────
14  obligation_farm_user_state         mut if reserve.farm_collateral != default, else readonly None sentinel
15  reserve_farm_state                 mut if Collateral farm present, else readonly None sentinel
16  farms_program                      KAMINO_FARMS_PROGRAM_ID
```

### remaining_accounts

None (modulo the optional permission account, irrelevant for the main market).

### Required pre-instructions

Identical to `WithdrawObligationCollateralV2`:

```
N-2: RefreshReserve(withdraw_reserve)          // withdraw_reserve at accounts[4]
N-1: RefreshObligation(obligation)             // remaining_accounts = ALL obligation reserves
N:   WithdrawObligationCollateralAndRedeemReserveCollateralV2
```

`RefreshObligation` itself again requires every obligation reserve to
have been `RefreshReserve`d at the current slot.

### Anchor errors that can fire

- 6001 / 6009 / 6029 / 6050 / 6051 — same suite as Repay
- WithdrawTooLarge — never our case (we always pass u64::MAX)
- LtvExceeded — same short-circuit as above when debt is already 0
- Conditionally closes the obligation account when it becomes inactive
  post-withdraw (no remaining deposits AND no remaining borrows). For the
  unwind we **leave the obligation open** (don't pass `u64::MAX` for the
  literal last lamport) so the next lever-up can re-use the seeded
  obligation without paying the rent for re-init.

### `collateral_amount` semantics

Same as `WithdrawObligationCollateralV2` — `u64::MAX` redeems the full
deposited cToken slot, clamped server-side; zero is rejected.

---

## §10 Lever-down (unwind) bundle template

Companion to the lever-up template above. Canonical atomic-flash-loan
unwind for a multiply obligation with `deposits=[jitoSOL]` +
`borrows=[SOL]`. Mirrors klend-sdk's
`buildWithdrawWithLeverageIxs` (single-tx flash-loan deleverage).

Strategy: flash-borrow SOL → repay obligation debt → withdraw jitoSOL
collateral → swap jitoSOL → SOL via Jupiter (Jito-pool fallback) →
flash-repay SOL + fee. Atomic; if any leg fails the entire tx reverts.

CU budget: **1_400_000 CU**. Priority fee: **10_000 microlamports**.
ALT: `KAMINO_MAIN_MARKET_LOOKUP_TABLE` + the route ALTs returned by
Jupiter's `/v6/swap-instructions`. v0 message compiler.

### Bundle (one transaction)

```
  0  ComputeBudgetProgram::set_compute_unit_limit(1_400_000)
  1  ComputeBudgetProgram::set_compute_unit_price(10_000)
  2  ATA-create-idempotent(user, wSOL_MINT)                                 -- flash-borrow destination + swap output
  3  ATA-create-idempotent(user, jitoSOL_MINT)                              -- collateral redemption + swap input

                                                                             -- flash leg open --
  4  FlashBorrowReserveLiquidity(sol_reserve, total_borrow_sol_lamports)    -- absolute ix index N for FlashRepay's borrow_instruction_index

                                                                             -- pre-repay refreshes --
  5  RefreshReserve(jitoSOL)                                                -- in obligation.deposits
  6  RefreshReserve(SOL)                                                    -- repay target (in obligation.borrows)
  7  RefreshObligation(remaining=[jitoSOL_reserve, SOL_reserve])            -- deposits ++ borrows in obligation order

                                                                             -- repay debt --
  8  RepayObligationLiquidityV2(sol_reserve, u64::MAX)                      -- u64::MAX → repay full debt slot

                                                                             -- pre-withdraw refreshes (RepayV2 marked SOL reserve stale) --
  9  RefreshReserve(jitoSOL)
 10  RefreshReserve(SOL)
 11  RefreshObligation(remaining=[jitoSOL_reserve, SOL_reserve])            -- post-Repay; klend tolerates the now-zeroed SOL borrow slot

                                                                             -- withdraw collateral --
 12  WithdrawObligationCollateralAndRedeemReserveCollateralV2(
        jitosol_reserve, u64::MAX)                                          -- redeem full deposited cToken slot

                                                                             -- swap leg: jitoSOL → SOL --
 13..K Jupiter swap ixns (variable count, from /v6/swap-instructions)       -- input mint = jitoSOL, output mint = wSOL
                                                                             -- OR jito_stake_pool::WithdrawSol(jitosol_amount) if Jupiter route unavailable

                                                                             -- flash leg close --
 K+1 FlashRepayReserveLiquidity(sol_reserve,
        total_borrow_sol_lamports + flash_fee,
        borrow_instruction_index = 4)                                       -- must equal the absolute index of FlashBorrow above

                                                                             -- cleanup --
 K+2 SPL-Token::CloseAccount(wSOL ATA → user)                               -- unwraps leftover wSOL to native SOL
```

### Account-list cribs

- Ix 4 (FlashBorrow): 12 accounts — see `flash_borrow_reserve_liquidity_ix`
  in `crates/zerox1-defi-protocols/src/protocols/kamino.rs:778`.
- Ix 8 (RepayV2): 12 accounts — see the `RepayObligationLiquidityV2`
  section above (and the v2 builder
  `repay_obligation_liquidity_v2_ix:1039`).
- Ix 12 (WithdrawAndRedeemV2): 17 accounts — see the
  `WithdrawObligationCollateralAndRedeemReserveCollateralV2` section above
  (and the v2 builder
  `withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix:1211`).
- Ix K+1 (FlashRepay): 12 accounts — see
  `flash_repay_reserve_liquidity_ix:825`. `borrow_instruction_index` is
  the **absolute** tx index of the matching FlashBorrow (here: 4, NOT 0).

### Flash-loan fee math

`flash_fee = ceil(total_borrow_sol_lamports * reserve.liquidity.flash_loan_fee_sf / 2^60)`
— the standard klend fixed-point scaling. Read
`reserve.liquidity.flash_loan_fee_sf` (fixed-point u128) once at
bundle-build time; pass `total_borrow_sol_lamports + flash_fee` as the
FlashRepay amount.

### wSOL ATA lifecycle

All flash-borrow output + Jupiter swap output land in the user's
canonical wSOL ATA (ix 2 creates it). RepayV2 reads from it (ix 8).
FlashRepay reads from it (ix K+1). CloseAccount (ix K+2) unwraps any
residual wSOL to native SOL.

### Pre-broadcast sanity checks

Before submitting:
1. `flash_cap = sol_reserve.liquidity.available_amount` — confirm
   `flash_cap >= total_borrow_sol_lamports`.
2. Jupiter quote `expected_sol_out >= total_borrow_sol_lamports + flash_fee`
   at the configured slippage. If not, fall back to Jito-pool direct
   redeem (`spl_stake_pool::WithdrawSol`) with the same slippage cap,
   provided `pool.lamports_available_for_instant_withdraw >= jitosol_amount`.
3. `whitelist.verify_ixns(&ixs)` — every ixn's program id must be in the
   daemon's signing whitelist (klend, Kamino Farms, SPL Stake Pool, SPL
   Token, ATA, System, Compute Budget, Jupiter v6).

### Post-broadcast sanity check

After landing (or sim ok in simulate-only mode), re-fetch the obligation
and assert
`borrows.iter().all(|b| b.borrowed_amount_sf == 0) && deposits.iter().all(|d| d.deposited_amount == 0)`.
If non-zero on a real submit, surface `error_code = 14 = "post-unwind sanity
check failed"` so the operator can retry.

### Iterative fallback (non-flash path)

When the flash-loan cap is below total debt OR Jupiter quote is
unavailable at size, the unwind falls back to N rounds of bounded δ
withdraws (per `caps::MAX_LEVERAGE_LOOP_ROUNDS = 6`). Each round:

```
1. RefreshReserve(every obligation reserve)
2. RefreshObligation
3. WithdrawObligationCollateralAndRedeemReserveCollateralV2(δ jitoSOL)
4. jito_stake_pool::WithdrawSol(δ jitoSOL → δ SOL)        -- direct redeem (no Jupiter)
5. RefreshReserve(every obligation reserve)
6. RefreshObligation
7. RepayObligationLiquidityV2(δ SOL)
```

δ is sized so the post-withdraw LTV stays strictly below the liquidation
threshold with a safety buffer. After debt = 0, a final-round
`u64::MAX` withdraw drains the remaining cToken slot.
