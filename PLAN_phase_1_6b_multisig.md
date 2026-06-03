# Phase 1.6b — Squads multisig scoping

Engineering scope for replacing the single hot key on wallet
`QesSR3TtkyrZmSEsRqrbg1DB3CHVSZDxMNLj5gZHuaJ` with a Squads
2-of-3 multisig. Decision-blocker for opening the closed beta
to outside capital larger than friends-and-family.

## What this doc is NOT

- **Not a decision on who the signers are.** Three signer slots are
  identified below (operator hot, cold backup, recovery / trusted
  third) — choosing the humans / hardware behind them is the
  operator's call. This doc scopes the engineering only.
- **Not a deadline.** Phase 1.6b is gated on Phase 1.6a's
  closed-beta cohort reaching ~30 days of clean operations, NOT on
  engineering readiness. The engineering can be built ahead of
  time and sit in a feature flag.

## The three signer slots

| Slot | Role | Where the key lives | Authority |
|------|------|---------------------|-----------|
| **Operator hot** | Day-to-day daemon signatures | Existing role-key infra on Hetzner (Ed25519 in `/var/lib/hedgents/secrets/`). Per-daemon, NOT a shared key. | Auto-signs actions below the policy threshold (typically $1k single-tx). Requires 1-of-3 below threshold. |
| **Cold backup** | Disaster recovery, geographically separated | Ledger / Trezor hardware wallet held by operator's principal. NOT online. | Co-signs high-value moves with operator hot; alone, can recover if recovery slot is unavailable. |
| **Recovery / trusted third** | Two-of-three quorum for high-value moves | Trusted third party (existing crypto-native contact, attorney with infosec creds, or institutional custody adapter). Hardware wallet or HSM. | Co-signs high-value moves; cannot move funds alone. |

Two-of-three policy: any signature ≥ policy threshold (operator-set
$ amount) requires 2 of 3 signers. Sub-threshold actions remain
1-of-3 (the operator hot key alone) so the autonomous fleet keeps
working for routine moves.

## Engineering scope

### 1. Squads UI setup (operational, ~30 min)

- Create a new Squads account
- Add the three signer pubkeys
- Set the 2-of-3 quorum + per-tx threshold policy
- Note the resulting multisig vault pubkey

Output: a new Solana pubkey that holds the AUM, replacing the
current single-keypair address.

### 2. Daemon signer adapter (~2-3 days code)

Each signing daemon currently signs Solana transactions directly:

```rust
let signed = tx.sign(&wallet.keypair, &recent_blockhash);
ctx.rpc.send_transaction(signed).await?;
```

Replace with a Squads propose-then-execute flow:

```rust
// 1. Build the inner Solana tx as today.
// 2. Wrap as a Squads transaction proposal addressed to the vault.
// 3. Send the proposal — counts as 1 of 2 signatures.
// 4. Operator confirms via phone (Squads mobile app) — 2 of 2.
// 5. Squads executes; daemon polls for confirmation.
```

Implementation surfaces:

- `crates/zerox1-defi-wallet/src/squads_adapter.rs` — NEW. Wraps the
  existing `Wallet::keypair()` access with a `propose_via_squads` /
  `wait_for_execution` API.
- Each strategy daemon's transaction build site (multiply's
  `leverage.rs`, hedgedjlp's `jlp_hedge.rs`, stable-yield's deposit
  path) routes the resulting tx through the adapter.
- Auto-mode policy: actions below `--squads-auto-threshold-usd`
  (default $1000) sign with operator hot alone; above the threshold
  trigger the 2-of-3 propose-then-execute flow.

### 3. Dashboard signer surface (~half day)

- The dashboard server currently reads from a single wallet pubkey
  (`HEDGENTS_WALLET_PUBKEY` env). Update to read from the multisig
  vault pubkey.
- Add a "Pending Squads proposals" surface so the operator can see
  what's queued waiting for the 2nd signature.

### 4. Risk-watcher visibility (~half day)

- New `RiskKind::SquadsProposalStuck` — if a proposal is queued > 1
  hour without execution, escalate. Stuck proposals usually mean
  the operator hot key submitted but the phone confirmation didn't
  happen.
- Reuse rc15's classifier pattern (pure logic + threshold bands).

### 5. Migration sequence (~half day on the day-of)

1. Fund the new multisig vault from the existing wallet (one-time
   on-chain transfer; needs the existing single keypair to sign
   ONCE for this transfer — last action of the legacy wallet).
2. Update `HEDGENTS_WALLET_PUBKEY` env, restart the dashboard.
3. Update each daemon's env to use the Squads vault pubkey + the
   signer's role key path.
4. Daemons resume operation against the new vault; rebalance is
   from-scratch (the original obligation/perp positions remain
   addressable, just under the new wallet).

### 6. Runbook (~2 days documentation)

`docs/runbooks/multisig-recovery.md` covering:

- How to add / remove signers (Squads governance flow)
- Lost-key recovery: 2 of 3 still functional → propose a new signer
  via Squads UI
- Compromised hot key incident: cold + recovery sign a Squads
  proposal to rotate the operator hot pubkey
- Operator unavailability: cold + recovery can execute the queued
  proposals via Squads mobile

## What this does NOT replace

- **Per-strategy authority isolation** stays. Each daemon still has
  its own role key. The multisig is the *wallet*-level guard; the
  per-daemon compile-time isolation is still what stops a multiply
  compromise from touching hedgedjlp.
- **The "self-deployed" product** doesn't change. Self-deployed
  operators choose their own custody model; Hedgents the binary
  works against any Solana keypair source (single, multisig,
  Squads, Anchorage adapter, etc.). The vault-tracked-custody
  closed beta is what needs the multisig.

## Cost / time estimate

- Engineering: ~4-5 focused days (Squads SDK learning + adapter +
  daemons + dashboard + risk-watcher + tests)
- Operational: ~2 hours migration day (one-time)
- Squads costs: roughly $10-20 in setup fees on-chain + minor
  per-tx overhead (one extra ix per signed tx)

## Open questions for the operator

1. **Cold backup signer.** Operator's principal, with offline hardware
   wallet?
2. **Recovery / trusted third.** Specific person or institutional
   adapter (Anchorage, Fireblocks)?
3. **Policy threshold.** What $ amount triggers the 2-of-3 flow vs
   stays auto-signed by operator hot? Default proposal: $1000 / single
   tx.
4. **Migration timing.** Run the engineering against devnet first
   (~2-3 days) before mainnet migration? Recommended yes.
