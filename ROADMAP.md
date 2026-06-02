# Hedgents Roadmap

Where the fleet is going, in priority order. Each phase ships behind a feature flag
or as a separately-tagged release; nothing breaks the running mainnet daemons.

Current state (May 2026): **6 daemons live on Solana mainnet at v0.4.1** —
`multiply`, `stable-yield`, `hedgedjlp`, `riskwatcher`, `researcher`,
`orchestrator`. Combined APR ~9-12% on $260+ AUM (founder seed). hedgedjlp
delta-neutral on Jupiter Perps with all three shorts (SOL/ETH/BTC) confirmed
on-chain; orchestrator runs in execute mode with **active cross-strategy
rebalance** (rc29) — capital actively reshuffles between strategies as
APR-weighted targets drift, not just when idle USDC arrives.

**50+ releases shipped in ~3 weeks** (verifiable on GitHub releases).
Two real mainnet incidents handled cleanly: (1) 3 perp shorts orphaned by a
daemon state-machine bug (rc27), $79.48 collateral + $1.61 PnL recovered,
3 structural root causes patched same day; (2) Helius RPC intermittently
served stale Kamino reserve account bytes causing v2-withdraw failures
(rc48-rc49), retry-with-backoff defense shipped within hours. A 5-minute
systemd monitor (rc33) alerts on any anomaly — down daemon, recent tx
failures, orphan shorts, stuck allocator.

**Strategy pivot (May 30, 2026):** Solana Foundation grant and Alliance
both rejected. CCTP bidirectional bridge (the prior Phase 1.5 headline
that was framed around institutional onboarding) is moving down the
priority list. The new immediate focus is a closed-beta vault funded by
friendly depositors — TVL becomes the institutional pitch we don't have
to ask permission for. Phase 1.6 below replaces what was Phase 1.5.

---

## Phase 1 — Orchestrator daemon ✓ shipped v0.4.0

Promoted `fleet-pm-stub allocator` to a long-running `orchestrator-daemon`.
Joins the libp2p mesh, polls `/strategies` + `/aum`, runs `decide()`, emits
`Assign`/`Withdraw` envelopes. Auto-mode lets strategy daemons accept actions
within configured caps without operator approval. See DEVLOG rc1–rc2.

---

## Phase 1.6 — Closed-beta vault → public vault (the new immediate focus)

After grant funnels said no, the working hypothesis is that **TVL speaks
louder than committees**. A small invite-only beta seasons the operational
track record on real other-people's-money flows; once the cohort has 30+
days of clean accounting and zero rug-vector incidents, open the gate to
public depositors. This is the path where the institutional pitch becomes
"$X TVL across N depositors earning Y% — proof, not promises."

### Stage 1.6a — Closed beta (invite-gated) ✓ shipped v0.4.1

Already live:
- `POST /api/invite/validate` + `POST /api/invite/register` (rc50) —
  bearer-tokened, sqlite-backed; 10 alpha codes seeded
  (`alpha-001` … `alpha-010`)
- `VaultInviteModal` on the landing site — "Park USDC. The fleet trades."
  narrative, three-step flow (code → email → confirmation)
- v0.4.1 mutual-fund accounting: founder seed at NAV=$1.00, pro-rata
  share mint/burn, integer-only math in micro-USDC + share-lamports
- Admin endpoints (`/api/beta/admin/*`) for operator-side deposit /
  withdrawal record-keeping; bearer auth via `HEDGENTS_BETA_ADMIN_TOKEN`

### Stage 1.6b — Multisig wallet (the trust-cost-zero upgrade)

Single-keypair custody is fine for the founder's own $260 but unacceptable
once user funds land. Squads 2-of-3 multisig replaces the single hot key
on wallet `QesSR3TtkyrZmSEsRqrbg1DB3CHVSZDxMNLj5gZHuaJ`:

| Signer | Role |
|---|---|
| Tobias's hot key | Day-to-day operator signatures |
| Cold backup | Disaster recovery, geographically separated |
| Recovery / trusted third | Two-of-three quorum for high-value moves |

Tasks:
- Nominate the cold + recovery signers
- Squads UI setup (~30 min) or scripted via Squads SDK
- Migrate the dashboard daemon to read the new multisig pubkey; update
  the wallet env var
- Migrate the trading daemons to sign via Squads' propose-then-execute
  flow (daemon `propose` → operator `approve` via phone → squads
  `execute`) — same Solana tx shape, just one extra signature layer
- Runbook: `docs/runbooks/multisig-recovery.md`

The strategy daemons need a small adapter: instead of signing directly,
they emit a Squads proposal envelope; operator approves via phone /
hardware wallet within minutes; Squads finalises. For auto-mode within
the existing caps the operator's "auto-approve below $X" Squads policy
runs the signature automatically (so the autonomous loop keeps working
for sub-cap actions, but >$X requires manual approval).

### Stage 1.6c — Public vault launch (gates lift)

Once Stage 1.6a has 30 days clean (no operational incidents, all
withdrawals reconciled, NAV math checks against on-chain reality every
day):

- Remove the invite-code gate; landing modal becomes deposit-address-
  capture + email
- Operationally a still-tracked-custody beta — same accounting layer,
  just open to anyone who emails their Solana address
- Target: $10k-$100k AUM in first 60 days
- Distribution: Twitter / Solana DeFi communities / no paid ads
  initially
- Public position-lookup endpoint (`GET /api/beta/lookup/:address`) so
  a depositor can verify their share + earned without a dashboard login

### Stage 1.6d — On-chain vault program (true non-custodial)

The "honest non-custodial" milestone — gated by both AUM ($100k+) and
audit cost being justified by what's at stake. Until then the multisig
custody model in 1.6b is the operating reality, and the landing copy
reflects this honestly ("tracked custody beta · non-custodial vault
planned"). Engineering scope:

- Solana program (Anchor) accepting USDC deposits, minting an LP token
  (`hSHARE` or similar) proportional to share, burning LP for redemption
- Program holds the AUM; strategy daemons CPI-call the program for
  rebalance moves (with multisig admin still gating strategy parameter
  changes)
- Audit by Ottersec or Offside before any mainnet bring-up
- Migration path from custodial beta to program: depositors burn their
  off-chain "beta share" and receive program-minted `hSHARE` 1:1 at
  current NAV; founder shares migrate the same way

### Why this phase exists (the honest framing)

The Solana Foundation and Alliance both rejected, which is fine —
their selection bar is committee-driven and our deliverable is a
running fleet with three strategies live, not a deck. Public TVL is
the credible proof that doesn't require anyone's approval. The risk
is that early operational mistakes with real users dollars damage
reputation worse than a grant rejection ever could; the closed-beta
cohort is the dress rehearsal.

What this is NOT: a retail-targeted product. The market segment is
still institutional-shaped USDC treasuries (offshore family offices,
non-US asset managers, DeFi-native funds) — they just don't have to
come via a grant committee or audit-gated procurement to deploy. The
closed beta is small institutionals + crypto-native individuals at
$1k-$100k cheque sizes.

---

## Phase 1.5 — CCTP bidirectional bridge (deposit + cash-out)

**Status: deferred behind Phase 1.6 after the grant pivot.** The CCTP
work below is still the right institutional-onboarding flow long-term,
but is no longer the immediate next ship. Resume when (a) Phase 1.6c
public vault is operating cleanly with $100k+ AUM and the institutional
pitch needs the on-chain bridge to compose source-chain treasuries onto
Solana, or (b) a specific institutional pilot commits contingent on
CCTP delivery.

**Shape: a CLI tool, not a daemon.** CCTP is operator-initiated
(deposit or cash-out), low-frequency, and doesn't react to mesh
envelopes — there is no plan to integrate EVM DeFi protocols, so the
"always-on bridge listening for orchestrator BridgeUSDC envelopes"
shape has no use case. A standalone tool (`tools/cctp-bridge` or
`hedgents bridge`) with its own signing keys (source-chain EVM key +
Solana key for `receiveMessage`) and a sqlite-backed pending-attestation
queue covers Phase 1.5 fully. **Compile-time authority isolation is
preserved via the dependency graph** — the bridge binary's Cargo.lock
excludes `kamino`, `jupiter`, `jito`; it cannot reach a lending
position. Same isolation property, no daemon overhead, no role key,
no libp2p mesh participation, no systemd unit.

The tool moves native USDC between the operator's source-chain
treasury (Ethereum, Base, Arbitrum, Avalanche, etc.) and the Hedgents
Solana wallet using Circle's CCTP V2 protocol. **Bidirectional from
day one** — operators need to be able to cash out as easily as they
deposited.

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

The compile-time authority isolation thesis extends naturally — and
this is the part that's load-bearing, NOT the "is it a daemon"
question. Isolation comes from the **dependency graph**, not the
process boundary:

| Binary | What it can do | What it cannot do |
|--------|----------------|-------------------|
| `multiply` / `stable-yield` / `hedgedjlp` daemons | Trade against Kamino / Jupiter / Jito | Touch CCTP. The `cctp` module isn't in the dep graph. |
| `cctp-bridge` CLI tool | Burn USDC on source chain, submit `receiveMessage` on Solana | Trade. Open lending positions. Touch Kamino, Jupiter, or Jito — none of those crates are linked. |
| `riskwatcher` daemon | (Optional future addition) Observe CCTP message status + emit Escalate on stuck attestations | Sign anything (existing rule) |
| `orchestrator` daemon | **No role in the bridge flow** — the tool is operator-initiated. Phase 5 may later add cross-chain rate watching to drive auto-bridge proposals, but that's explicitly out of scope here. | — |

The bridge tool holds the USDC-burn authority (source-chain EVM key)
and a Solana key for `receiveMessage`. It holds nothing else. A
compromise of the bridge tool cannot drain a lending position; a
compromise of multiply cannot move funds to another chain.

### The compelling demo (Stage 1.5b)

**CCTP V2 introduces Hooks** — automated post-transfer actions that
fire when USDC lands on the destination chain. We compose this into a
**single-transaction treasury deployment**:

> Operator on Base: one signed tx burns USDC on Base, includes a Hook
> payload that auto-invokes Kamino's deposit ixn on the Solana side
> via the stable-yield reserve. ~15 minutes later the operator's
> treasury USDC is earning yield in Hedgents. No multi-step bridging,
> no manual handoff, no wrapped assets.

That's the institutional-onboarding demo. It's not a future promise
— CCTP V2 Hooks have been live on Solana mainnet since October 2025;
we're using an existing primitive, not asking the Solana ecosystem to
ship something for us.

### Stages

| Stage | Scope | Estimate |
|-------|-------|----------|
| **1.5a** — Standalone bridge CLI | `tools/cctp-bridge/` — `deposit` and `withdraw` subcommands. Operator runs locally. Devnet first, then mainnet. Polls Circle attestation API; submits the destination-chain message. Pending attestations persist in a local sqlite queue between invocations. | ~1 week |
| **1.5b** — CCTP V2 Hooks for atomic source→deploy | Operator's source-chain burn carries a Hook payload that auto-invokes Kamino's deposit ixn on the Solana side. Single tx from source chain → deployed yield position. Still operator-initiated, still a CLI tool — the Hook is a property of the transaction shape, not the binary shape. | ~2 weeks |

**Previously the roadmap had a Stage 1.5b "promote to daemon" step
between these two.** That stage was reasoning from "always-on bridge
listening for orchestrator BridgeUSDC envelopes" — which only matters
if Phase 5's cross-chain rate-watching ships. With Hedgents staying
Solana-native (no plan to integrate EVM DeFi protocols), the always-on
case has no use case. Stage 1.5b is now reserved for the CCTP V2 Hooks
work that used to be 1.5c.

A future Stage 1.5c (or Phase 5 work) could promote the tool to a
daemon if cross-chain rate watching is committed to — but only at
that point. Don't add the process overhead before the use case exists.

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

### Tasks (Stage 1.5b — the headline, CCTP V2 Hooks)

- Compose CCTP V2 Hook payloads that target the Kamino deposit ixn
  for the operator's stable-yield reserve
- Reverse path: cash-out via `withdraw` Hook that unwinds from
  stable-yield → CCTP burn on Solana → mint on source chain
- End-to-end demo: source chain tx → ~15 min → operator's USDC is in
  Kamino earning yield (or reverse: USDC is back in their source-chain
  treasury)
- Operator runbook: `docs/runbooks/cctp-atomic-deploy.md`
- Still a CLI tool, NOT a daemon. The Hook payload is a property of
  the source-chain transaction the operator signs; no new mesh
  participant, no new systemd unit, no `Role::Bridge`. The tool's
  Cargo.lock continues to exclude Kamino/Jupiter/Jito crates.

### Optional add-on: riskwatcher CCTP visibility

If desired, the existing riskwatcher daemon can be extended (no new
daemon needed) to poll Circle's attestation API for any operator-
submitted bridge messages and emit `EscalateRisk` when an attestation
stalls > 30 min. This is a pure monitoring add-on inside riskwatcher,
not a new daemon. Defer until operators report attestation-delay pain.

### Out of scope for Phase 1.5

- **Source-chain custody.** The bridge tool signs source-chain burns
  from an operator-controlled key. We do not custody the key; we sign
  burns against it. Phase 4 adds Anchorage / Fireblocks / Safe (multi-
  sig) signer adapters so the key lives in HSM-backed custody.
- **Cross-chain arbitrage / yield routing.** This is a treasury-flow
  primitive, not a yield strategy. Hedgents stays Solana-native — no
  plan to integrate EVM DeFi protocols.
- **Auto-bridging based on rate differentials.** The orchestrator
  could in principle decide "Solana yields are higher than Base — burn
  USDC on Base." Not on the roadmap. Cross-chain rate-watching would
  introduce a much larger trust surface in the orchestrator and would
  be the only justification for promoting the bridge to a daemon —
  neither of which is currently desired.
- **Bridge as daemon.** Explicitly chosen against. CCTP is operator-
  initiated, low-frequency, and has no mesh subscriptions to listen
  for. A CLI tool with sqlite-backed attestation queue covers the
  whole flow without the always-on overhead.

### Why this matters for Solana

The single biggest friction in institutional treasury adoption of
Solana DeFi is "how do I get USDC here without taking wrapped-asset
risk." Hedgents' CCTP integration removes that friction structurally:

1. **Native USDC inflow to Solana** — burn on Ethereum/Base/
   Arbitrum/Avalanche → mint as *native* USDC on Solana. No Wormhole-
   wrapped intermediate, no third-party bridge counterparty risk.
   Every institutional dollar this routes is a dollar of new TVL the
   Solana ecosystem captures from EVM treasuries.
2. **Atomic source→deploy via CCTP V2 Hooks** — Solana was the
   first non-EVM CCTP V2 Hooks deployment (March 2026). Stage 1.5b
   exercises that capability end-to-end: an operator signs one tx on
   Ethereum that lands deployed yield on Kamino. This is the
   institutional onboarding flow that makes Solana DeFi accessible
   to TradFi-shaped treasuries without a multi-step CEX bounce.
3. **Bidirectional from day one** — institutions don't deposit unless
   they can cash out. Bake-in cash-out parity removes the strongest
   compliance objection to deploying any treasury capital into
   Solana DeFi at all.
4. **Compile-time isolation extends to bridging** — the
   `cctp-bridge` CLI tool carries USDC-burn authority and nothing
   else. Its Cargo.lock excludes the Kamino, Jupiter, and Jito
   crates; it cannot trade, cannot touch Kamino, cannot open lending
   positions. Hedgents' authority-by-binary thesis stays intact
   across cross-chain flows — the isolation property comes from the
   dependency graph, not from being a long-running process.

Shippable on devnet in ~1-2 weeks; mainnet behind it. A working
source→deploy transaction graph (Sepolia → Solana → Kamino deposit
in a single operator signature) is the deliverable, not a promise.

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

- **Retail UX / mobile app.** Different product, different repo (`01
  Pilot`). The Phase 1.6 vault is institutional-shaped capital with
  $1k+ cheque sizes, not a Coinbase-grade consumer experience.
- **Cross-chain (Ethereum, Base, Hyperliquid).** Solana-native is a
  feature, not a limitation. CCTP (Phase 1.5) is the *only* cross-chain
  surface — it moves native USDC, doesn't replicate strategies on EVM.
- **Token.** The product is software + a USDC vault; revenue is the
  vault performance fee (TBD %). A token is neither necessary nor
  desired by the institutional segment we serve.

**Previously a non-goal, now Phase 1.6:** hosted vault product. The
grant rejections shifted this from "would change the trust model
unhelpfully" to "the trust model the operator wanted (institutional
infra licensing) isn't underwriting itself — the alternative trust
model (depositor-funded vault) is what produces TVL and therefore
runway." Same fleet, different go-to-market wrapper.

---

## How to read this

Phase numbers are priority order, not strict sequencing. Phase 4 work
(audit, custody integrations) runs in parallel with Phases 1–3 because it's
gating institutional pilots regardless of feature scope.

See `DEVLOG.md` for what has already shipped and the running version
history.
