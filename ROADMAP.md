# Hedgents Roadmap

Where the fleet is going, in priority order. Each phase ships behind a feature flag
or as a separately-tagged release; nothing breaks the running mainnet daemons.

Current state (May 2026): **6 daemons live on Solana mainnet** —
`multiply`, `stable-yield`, `hedgedjlp`, `riskwatcher`, `researcher`,
`orchestrator`. Combined APR ~10.5%. hedgedjlp delta-neutral with all three
shorts (SOL/ETH/BTC) confirmed on-chain; rebalancer auto-executes resize plans.
Orchestrator running in execute mode. CCTP bridge next.

---

## Phase 1 — Orchestrator daemon ✓ shipped v0.4.0

Promoted `fleet-pm-stub allocator` to a long-running `orchestrator-daemon`.
Joins the libp2p mesh, polls `/strategies` + `/aum`, runs `decide()`, emits
`Assign`/`Withdraw` envelopes. Auto-mode lets strategy daemons accept actions
within configured caps without operator approval. See DEVLOG rc1–rc2.

---

## Phase 1.5 — CCTP bidirectional bridge (deposit + cash-out)

A new compile-time isolated daemon, `cctp-bridge-daemon`, that moves
native USDC between the operator's source-chain treasury (Ethereum,
Base, Arbitrum, Avalanche, etc.) and the Hedgents Solana wallet using
Circle's CCTP V2 protocol. **Bidirectional from day one** — operators
need to be able to cash out as easily as they deposited.

### Why this phase exists

Institutional USDC treasuries overwhelmingly live on EVM chains.
Today, deploying into Hedgents requires either:
- Off-chain OTC desk + wire (slow, expensive, off-platform)
- Wormhole/Allbridge wrap of USDC (introduces wrapped-asset risk that
  defeats the point of using native USDC)

CCTP V2 solves both: burn native USDC on source chain → Circle
attestation → mint native USDC on destination. ~15 min, no wrap,
official Circle infrastructure. Same flow runs in reverse for cash-out.

### Why this is the right architectural fit

The compile-time authority isolation thesis extends naturally:

| Daemon | What it can do | What it cannot do |
|--------|----------------|-------------------|
| `multiply` / `stable-yield` / `hedgedjlp` | Trade against Kamino / Jupiter / Jito | Touch CCTP. The Bridge module isn't in the dep graph. |
| `cctp-bridge-daemon` | Burn USDC on source chain, mint on destination | Trade. Open lending positions. Touch Kamino. |
| `riskwatcher` | Observe CCTP message status + emit Escalate on stuck attestations | Sign anything (existing rule) |
| `orchestrator` | Decide *when* to request a bridge action | Sign the bridge tx itself — it emits a `BridgeUSDC` envelope; bridge daemon signs |

Bridge daemon holds the USDC-burn authority and nothing else. A
compromise of the bridge cannot drain a lending position; a compromise
of multiply cannot move funds to another chain.

### The compelling demo (Stage 1.5c)

**CCTP V2 introduces Hooks** — automated post-transfer actions that
fire when USDC lands on the destination chain. We compose this into a
**single-transaction treasury deployment**:

> Operator on Base: one signed tx burns USDC on Base, includes a Hook
> payload that auto-invokes Kamino's deposit ixn on the Solana side
> via the stable-yield reserve. ~15 minutes later the operator's
> treasury USDC is earning yield in Hedgents. No multi-step bridging,
> no manual handoff, no wrapped assets.

That's the headline grant pitch. It's not a future promise — CCTP V2
Hooks are live on Solana mainnet since October 2025.

### Stages

| Stage | Scope | Estimate |
|-------|-------|----------|
| **1.5a** — Standalone bridge CLI | `tools/cctp-bridge/` — `deposit` and `withdraw` subcommands. Operator runs locally. Devnet first, then mainnet. Polls Circle attestation API; submits the destination-chain message. | ~1 week |
| **1.5b** — `cctp-bridge-daemon` long-running variant | Same logic as 1.5a but as a daemon under the existing systemd target. Listens for `BridgeUSDC` envelopes from operator (via fleet-pm-stub) or orchestrator. New compile-time isolated `Role::Bridge`. Approval-queue gated like every other daemon. | ~1 week |
| **1.5c** — CCTP V2 Hooks for atomic source→deploy | Operator's source-chain burn carries a Hook payload that auto-invokes Kamino's deposit ixn on the Solana side. Single tx from source chain → deployed yield position. | ~2 weeks |

### Tasks (Stage 1.5a)

- `crates/zerox1-defi-protocols/src/protocols/cctp.rs` — Solana-side
  ixn builders for `TokenMessengerMinter` (depositForBurn) and
  `MessageTransmitter` (receiveMessage)
- `tools/cctp-bridge/Cargo.toml` — separate binary tool, not part of
  the live-daemon systemd target
- Source-chain support: **Base** first (lowest fees, most institutional
  treasuries are migrating to it), then Ethereum / Arbitrum
- EVM-side signing: `ethers-rs` or `alloy` for source-chain burns; key
  loaded from operator's environment (NOT bundled with Hedgents
  binaries)
- Devnet demo flow: Sepolia testnet → Circle attestation → Solana
  devnet mint; dashboard surfaces the message in flight
- Mainnet runbook: $50 round-trip first, $500 second, then scale

### Tasks (Stage 1.5b)

- `crates/cctp-bridge-daemon/` — daemon shape mirroring riskwatcher
  (read-only-ish, no Solana trading, just CCTP signing)
- `Role::Bridge` added to `zerox1-defi-runtime::identity` (already has
  Orchestrator, Multiply, HedgedJlp, etc.)
- New protocol message: `BridgeUSDC { direction, amount_usdc,
  source_chain, dest_chain, deadline_unix, hook_payload }` +
  `ReportBridge { message_hash, status, attestation_url }`
- Approval queue + manual-approve flow (operator confirms each
  bridge before it executes; auto-mode promotion follows the same
  $50 → $500 → unrestricted pattern as the other daemons)
- Riskwatcher: CCTP message-status poller; Escalate when an
  attestation is pending > 30 min (source-chain reorg or Circle
  attestation service issue)
- systemd unit: `hedgents-cctp-bridge.service` + addition to
  `hedgents.target`

### Tasks (Stage 1.5c — the headline)

- Compose CCTP V2 Hook payloads that target the Kamino deposit ixn
  for the operator's stable-yield reserve
- Reverse path: cash-out via `withdraw` Hook that unwinds from
  stable-yield → CCTP burn on Solana → mint on source chain
- End-to-end demo: source chain tx → ~15 min → operator's USDC is in
  Kamino earning yield (or reverse: USDC is back in their source-chain
  treasury)
- Operator runbook: `docs/runbooks/cctp-atomic-deploy.md`

### Out of scope for Phase 1.5

- **Source-chain custody.** The bridge daemon signs source-chain burns
  from an operator-controlled key. We do not custody the key; we sign
  burns against it. Phase 4 adds Anchorage / Fireblocks / Safe (multi-
  sig) signer adapters so the key lives in HSM-backed custody.
- **Cross-chain arbitrage / yield routing.** This is a treasury-flow
  primitive, not a yield strategy. Phase 5 portfolio-mode could
  compose it, but Phase 1.5 stays focused on operator-initiated
  deposit + cash-out.
- **Auto-bridging based on rate differentials.** The orchestrator
  could in principle decide "Solana yields are higher than Base — burn
  USDC on Base." Don't ship this in Phase 1.5. It introduces
  cross-chain rate-watching complexity and a much larger trust surface
  in the orchestrator. Phase 5 maybe.

### Why this lands the Circle grant

CCTP is *the* Circle product. Three reasons reviewers will care:

1. **Real Circle differentiator integration** — not "we hold USDC"
   but "we are first-class on Circle's CCTP rails"
2. **CCTP V2 Hooks composition** — Solana V2 was the first non-EVM
   Hooks deployment; we're early adopters of the headline new
   capability
3. **Bidirectional treasury flow** — institutional cash-in *and*
   cash-out. Operators staying in control of their funds end-to-end
   is the grant-friendly version of the "non-custodial" pitch

A demoable devnet flow is shippable in 1-2 weeks. The grant
application can cite a working source→deploy transaction graph as
evidence, not a promise.

---

## Phase 2 — Tokenized T-bills as `stable-yield` venues

Add **Ondo USDY** and (optionally) **Circle USYC** as alternative
venues under the existing `stable-yield` daemon. The allocator gains
real "DeFi vs T-bill" rotation: when Kamino USDC supply drops below
the T-bill rate + risk premium, the daemon redeems from Kamino, swaps
to USDY/USYC, and parks there until DeFi yields recover.

### Access model — be explicit

Tokenized T-bills on Solana are **not permissionless**. Hedgents'
institutional-offshore target segment (Anchorage Switzerland, BVI /
Cayman / Singapore family offices, non-US asset managers) is exactly
who these instruments are designed for, so this is a fit — but the
roadmap needs to state it plainly.

| Asset | Access | Min | Onboarding |
|-------|--------|-----|------------|
| **USDY** (Ondo) | Non-US Persons (Reg S) | None | Transfer-allowlist via Ondo compliance contract; no per-user KYC, but operator wallet must be allowlisted via Ondo's portal |
| **USYC** (Circle/Hashnote) | Non-US institutions only | **$100k** | Portal onboarding, KYC/AML, wallet allowlisting |
| **OUSG** (Ondo, BlackRock T-bill wrapper) | Accredited Investor + Qualified Purchaser | $5k | Ondo portal |

US-based operators cannot deploy into these directly. The honest pitch
is "Hedgents converts non-US institutional USDC into productive T-bill
yield." That's the segment Anchorage Switzerland, Coinbase Custody
Trust (non-US trust company), and offshore family offices already
serve — Hedgents fits inside their existing access stack.

### Tasks

- `crates/zerox1-defi-protocols/src/protocols/ondo.rs` — USDY
  transfer leg using **Token-2022 program** (USDY uses Token Extensions
  with Transfer Hooks for compliance — legacy SPL won't work). USDC ↔
  USDY routing via Jupiter-Swap; decoded position via Token-2022
  account read
- `crates/zerox1-defi-protocols/src/protocols/usyc.rs` — same shape,
  USYC-specific (optional; ship only if operator pilot is non-US
  institutional with $100k+)
- `stable-yield`: venue selector that picks highest-APR among
  `{Kamino USDC, USDY, USYC}` subject to per-venue cap; honour
  `--max-rwa-deployed-usd` (default $50k for first pilot)
- Researcher: `rwa_rate_watcher` — pulls USDY's live APY from Ondo's
  on-chain pricing source, USYC's APY from Hashnote's published rate;
  emits `MarketSignal::RwaSpread { defi_apr, rwa_apr, spread_bps }`
  when spread breaches threshold
- Riskwatcher: add USDY-balance + USYC-balance pollers alongside
  Kamino obligation poller; per-issuer counterparty concentration cap
  (single issuer ≤ 30% of fleet AUM)
- Orchestrator: no change — `stable_yield` is already the risk-free
  anchor in the hurdle model; adding venues under it doesn't move the
  decision shape
- Audit: confirm the Ondo / Hashnote / Token-2022 program IDs + mint
  addresses at build time as hard-coded constants
- Runbook: document the one-time operator-side allowlist onboarding
  step for each issuer

---

## Phase 3 — RWA-collateralized leverage + lending venue diversification

Three parallel additions, all expanding the *protocol* surface area
without changing the daemon shape. **All three are conditional on
verified Kamino listings — the listings exist as of May 2026 but
re-confirm at build time.**

### 3a — RWA collateral in `multiply` (the high-conviction add)

Extend the `multiply` daemon with a `--collateral-mint` flag so the
same leverage loop that runs against jitoSOL can run against tokenized
T-bills. Kamino already accepts both **USDY** (largest yield-bearing
RWA on Solana, $175M cap) and **OUSG** ($79.6M cap) as collateral —
this is a near-term deliverable, not a wait-and-see.

**The economics.** At 50% LTV (Kamino's typical RWA LTV ceiling):

| Position | USDY rate | USDC borrow | USDC redeploy | Net APR |
|----------|-----------|-------------|---------------|---------|
| Single-loop carry (USDY collateral, borrow USDC, deposit USDC in Kamino supply) | 5.0% | 6.0% | 10.0% | 5.0% + (10% × 0.5) − (6% × 0.5) = **7.0%** |
| Recursive loop (4 iterations, USDY-only) | 5.0% | 6.0% | — | ~9% (USDY-only amplified) |
| Recursive loop + USDC redeploy combined | — | — | — | ~11–13% (depends on rate regime) |

The institutional pitch: **~11–13% APR equivalent to leveraged DeFi,
but the collateral is government debt**, not pure stablecoin smart-
contract exposure. For non-US custodians who cannot underwrite open-
ended DeFi but can underwrite T-bills, this is a categorical risk
shift.

**Tasks**

- `multiply`: `--collateral-mint` flag (default jitoSOL); LTV ceilings
  per-mint (jitoSOL 75%, USDY 50%, OUSG 50%)
- `multiply`: handle Token-2022 Transfer Hooks for USDY deposits
- Riskwatcher: extend liquidation-distance model with three new bands:
  1. **Redemption-window risk** — USDY/USYC redemption is 24/5 (money-
     market hours). Off-hours liquidation can't fully unwind. Trigger
     defensive band earlier on weekends/nights.
  2. **Oracle-staleness band** — RWA prices update slowly; veto new
     positions when oracle age > N minutes
  3. **Issuer-concentration cap** — single-issuer (Ondo or Circle)
     exposure ≤ 30% of fleet AUM
- Researcher: `rwa_rate_watcher` (from Phase 2) feeds the regime
  signal the orchestrator uses to choose between RWA-loop and
  jitoSOL-loop
- Runbook: `docs/runbooks/multiply-rwa-collateral.md` covering the
  operator-side allowlist prerequisites + the first $50k smoke

### 3b — MarginFi as an alternative lending venue

Same shape as Phase 2's venue work, but for the lending side.

**Tasks**

- `crates/zerox1-defi-protocols/src/protocols/marginfi.rs` — lending
  ixn builder + account decoder, audited as carefully as kamino.rs
- `stable-yield` + `multiply`: extend their venue selector to include
  MarginFi
- Riskwatcher: MarginFi position poller alongside Kamino
- Audit: account-layout audit against MarginFi before any mainnet
  bring-up

**Why MarginFi, not Drift.** Drift's April 2026 $285M admin-key exploit
is the live proof point we cite for compile-time authority isolation —
deploying capital to Drift would be incoherent with our own thesis.

### 3c — Tokenized equities as `multiply` collateral

Kamino already lists **xStocks** (SPYx, NVDAx, MSTRx, AAPLx, etc.) as
collateral. Same `--collateral-mint` infrastructure as 3a, just with
equity-oracle gotchas.

**Tasks (only ship after 3a is operating cleanly)**

- `multiply`: extend `--collateral-mint` validation to xStocks mints
- Riskwatcher: add equity-oracle staleness band (markets-closed gap on
  evenings/weekends/holidays) — equities have a much bigger off-hours
  oracle-gap problem than T-bills
- Researcher: `equity_price_watcher` for Pyth-fed prices on the
  deployed collateral
- Per-issuer cap: stay diversified across at least 3 tickers; no
  single-name > 15% of fleet AUM

### 3d — Tokenized gold as `multiply` collateral (deferred)

XAUT on Solana via LayerZero exists, but a Kamino listing has not been
verified. Treat this as deferred until a Kamino reserve goes live
(Aave V3 has active proposals; Solana lending markets typically follow
on a 6–12 month timeline).

### 3e — Institutional-gated RWA (operator-side, not core)

For operators who already have Securitize / Maple / Centrifuge
relationships, document an integration path without shipping a built-in
venue:

- **BUIDL** ($550M on Solana) — via Securitize subscribe/redeem; the
  operator runs the off-chain workflow, deposits BUIDL into Kamino
  when/if it becomes a Kamino collateral asset, and the daemon treats
  it as another collateral mint
- **Maple syrupUSDC** — restricted access lending; same pattern
- **Centrifuge** ($400M Solana deployment, Janus Henderson live) —
  private-credit; same pattern

These don't get protocol-client modules. They get a runbook + an
on-chain mint allowlist so the daemons recognise them when they hit
the operator's wallet.

---

## Phase 4 — Production hardening for institutional pilots

Independent of strategy expansion — required for $5M+ institutional pilots.

- **Third-party audit** of fleet orchestration code (target: Q3 2026, Ottersec
  or Offside). Underlying protocols (Kamino, Jupiter, Jito) are already audited.
- **Verified release signing** — every binary signed; runbook for institutions
  to verify before deploying. Multi-sig gating for production upgrades.
- **NAV / reporting integration** — Bloomberg-compatible export, fund-admin
  CSV format, daily statement generator
- **Anchorage / Coinbase Custody / BitGo integration** — operator-controlled
  signer adapters so the fleet's authority keys live in HSM-backed custody
- **Compliance wrappers** — Reg D / Reg S deployment guide for operator
  jurisdictions (the fleet itself does not change; the wrapper is operational)
- **Insurance** — Nexus Mutual / Sherlock cover for the orchestration layer
  (separately from the underlying-protocol cover)

---

## Phase 5 — Intelligence layer + cross-asset portfolio mode

Once the deterministic orchestrator is proven, layer two new
capabilities on top.

**Intelligence layer (advisory only).** An LLM that reads researcher
signals + position state and emits human-readable rebalance proposals.
Not auto-execute — it emits Escalate-style recommendations that an
operator approves.

- LLM advisor with backtesting harness — historical regime replay
  before any proposal promotion
- Light Protocol integration for confidential position metadata
  (institutional operators often cannot publicly disclose AUM-level
  positions)

**Portfolio mode (cross-asset allocator).** A separate daemon family
that treats the existing yield strategies as one building block and
adds an allocator across non-yield assets: tokenized equities (Ondo
Global Markets, xStocks), tokenized gold (XAUT). This is *different
product wedge* from the current "deploy idle stablecoins to yield"
thesis — it's "run a TradFi-style portfolio on-chain."

- New daemon: `portfolio-allocator` — drives target weights across
  `{cash, T-bill yield, leveraged carry, equities, gold}` based on
  operator-supplied policy
- The existing yield daemons become *sub-strategies* it composes
- Tokenized stocks: Ondo Global Markets is live since 21 Jan 2026
  (200+ tickers, 65% of Solana RWA tokens by count); xStocks via
  Backed/Kraken ($25B total volume, ~25% of tokenized equity sector)
- Tokenized gold: XAUT on Solana via LayerZero (XAUT+PAXG control
  89–95% of the $6B tokenized-gold market)
- Riskwatcher: extends to handle equity-oracle staleness and
  market-hours gaps

Phase 5 is intentionally vague on timing — it's a roadmap signal that
the fleet shape extends naturally to TradFi-style portfolio
construction, not a committed deliverable for any specific quarter.

---

## Non-goals

These are intentionally **not** on the roadmap:

- Hosted vault product. Hedgents is on-premise infrastructure. A hosted
  product changes the trust model and the customer.
- Retail UX / mobile app. Different product, different repo (`01 Pilot`).
- Cross-chain (Ethereum, Base, Hyperliquid). Solana-native is a feature, not
  a limitation.
- Token. The product is software; revenue is licence + execution fee. A
  token is neither necessary nor desired by the institutional buyer.

---

## How to read this

Phase numbers are priority order, not strict sequencing. Phase 4 work
(audit, custody integrations) runs in parallel with Phases 1–3 because it's
gating institutional pilots regardless of feature scope.

See `DEVLOG.md` for what has already shipped and the running version
history.
