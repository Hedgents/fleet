# Hedgents

**A fleet of role-separated autonomous agents that deploys institutional DeFi strategies on the operator's own infrastructure.**

---

Version 0.1 · 2026-05-11 · Preliminary, for institutional discussions

Contact: contact@hedgents.com · Source: github.com/Hedgents/fleet

---

## 1 · Summary

Hedgents is software, not a fund. Institutions install it on their own hardware, fund their own on-chain wallet, and run a fleet of five specialised agents that execute three well-established Solana DeFi strategies under hard-coded risk limits. Custody never transfers. Strategy code is open source. Every action is on-chain and attributable to a specific agent role.

The thesis is that the current institutional on-chain yield offerings — BUIDL, FOBXX, USYC — all yield approximately 4% and all require custody transfer to a regulated wrapper. The strategies that earn meaningfully more on Solana are auditable, well-documented, and accessible without that transfer if you operate the infrastructure yourself. Hedgents is that infrastructure, designed with role-separated authority so no single agent or operator action can compromise the whole position.

Current target through-cycle return on a $150,000 equal-weight portfolio: **8–11 % annualised**. Live mainnet paper trading: continuous since 2026-05-09 with 5/5 daemons healthy and on-chain telemetry stored in SQLite. This document describes what we have built, what risks an operator is taking, and what we have not yet done.

---

## 2 · What this is, what this is not

**This is:** Software for operating leveraged-staking, delta-neutral LP, and money-market lending strategies on Solana. The operator holds the keys, signs the transactions through their hardware, and receives all yield directly to their on-chain wallet.

**This is not:** A fund. There is no LP agreement, no offering memorandum, no third-party NAV, no fund administrator, no transfer of beneficial ownership. We do not custody operator funds at any point.

**Why the distinction matters:** The DeFi failures of 2022–23 (Celsius, BlockFi, Voyager, FTX) were custody failures, not strategy failures. Lenders earning 6–8% on stablecoins lost principal because a custodian rehypothecated their deposits. Operators running Hedgents are structurally exposed only to the underlying DeFi protocols their strategies touch — not to us, and not to any intermediary.

---

## 3 · Strategies

The fleet runs three strategies in parallel. Each has a stated economic source, a quantifiable risk profile, and a published historical precedent. None of them are novel — the trade structures have been documented in DeFi for years. What we have built is the role-separated operational layer that runs them reliably and within hard limits.

### 3.1 Stable Yield · Kamino USDC supply

**Mechanic:** Deposit USDC into Kamino's main lending market. Earn the supply APR set by the utilisation curve.

**Yield source:** Interest paid by borrowers on Kamino. Pure money-market lending — the same instrument BlackRock effectively wraps in BUIDL, without the wrapper.

**Through-cycle range:** 3.5 – 5.5 % APR. Current observed (2026-05-11): 3.91 %.

**Risks:**

| Risk | Severity | Mitigation |
|---|---|---|
| Kamino smart contract exploit | Catastrophic | Protocol audited by OtterSec and Offside Labs; $1B+ TVL since 2024; no historic bad-debt event on USDC reserve |
| USDC depeg | High | Historical precedent (SVB, March 2023) recovered in 72 hours; not a permanent loss event |
| Withdrawal queue during utilisation spike | Medium (illiquidity, not loss) | Riskwatcher daemon emits `Important` signal at >85 % utilisation |

**Capacity:** Approximately 5 – 10 % of the Kamino USDC reserve before deposits move the rate materially. $50 – 100 million today.

### 3.2 Multiply · 2.5× leveraged jitoSOL

**Mechanic:** Deposit jitoSOL as collateral on Kamino. Borrow USDC at 60 % LTV. Swap the borrowed USDC into jitoSOL. Loop. Net position: 2.5× long jitoSOL, 1.5× short USDC.

**Yield source:** jitoSOL pays approximately 7.3 % APR (Solana inflation, validator commissions, Jito MEV tips). USDC borrowing costs approximately 4.7 % APR. The leveraged net is:

```
net_apr = jitosol_apy × 2.5  −  usdc_borrow × 1.5
        = 7.3 × 2.5  −  4.7 × 1.5  =  11.2 %
```

**Through-cycle range:** 8 – 14 % APR. Current observed: 10.48 %.

**Risks:**

| Risk | Severity | Mitigation |
|---|---|---|
| SOL price crash → liquidation | Catastrophic, time-sensitive | Riskwatcher polls Kamino obligation every 5 seconds; emits `Critical Escalate` at 70 % LTV (75 % is Kamino's liquidation threshold); Multiply daemon blocks new positions and triggers deleverage |
| jitoSOL / SOL depeg | High | Historical max depeg 0.4 %; stressed at 5 % — eats ~3 % of position |
| USDC borrow rate spike | Medium | Daemon detects when net APR turns negative and unwinds the loop |
| Smart contract risk | Catastrophic | Same Kamino exposure as Strategy 1 |

**Historical analog:** Lido stETH / Aave looping in Ethereum, $5B+ AUM at peak, well-documented since 2022. Same trade structure with a different staking token.

**Capacity:** Approximately $30 million before slippage on jitoSOL / USDC rebalancing becomes material.

**Worst-case stress test:** Modelled against November 2022 (SOL −50 % in four days). Without intervention, the position liquidates at approximately −25 % of capital. With Riskwatcher's 70 % LTV trigger active, deleveraging window 2 (24h into the move) caps loss at approximately −12 %.

### 3.3 Hedged JLP · delta-neutral Jupiter LP

**Mechanic:** Buy JLP, Jupiter's perpetuals-DEX liquidity-provider token (approximately 47 % SOL, 10 % ETH, 10 % BTC, 33 % stablecoins). Open a short position on Jupiter Perps sized to neutralise 75 % of the net delta exposure.

**Yield source:** JLP holders earn 75 % of Jupiter Perps trading fees and 75 % of funding paid by leveraged traders. The hedge has a borrow cost (paid to short SOL).

```
net_apr = jlp_fee_apy_7d  −  sol_borrow × 0.75
        ≈ 23 %  −  4.8 %  =  ~18 %
```

We use the 7-day rolling average of JLP fee APY rather than the daily snapshot, which can spike to 40 %+ during high-volume sessions but does not represent a sustainable rate.

**Through-cycle range:** 8 – 18 % APR. Current observed (7-day average basis): 16.98 %.

**Risks:**

| Risk | Severity | Mitigation |
|---|---|---|
| JLP fee yield collapse during quiet markets | Medium (yield reduction, not loss) | Strategy continues with reduced return; can be unwound at any time |
| Funding rate inversion | Medium | When market is sustained short, JLP pays funding instead of earning it; observed 6–8 times in 2024–25 |
| Composition drift away from hedge ratio | Low | Daemon rebalances every 10 minutes |
| Hedge slippage during fast SOL moves | Medium | Five-minute unhedged exposure on $50k position is approximately $2k mark-to-market swing, mean-reverting |
| Jupiter Perps smart contract | Catastrophic | Audited by Halborn and OtterSec; 18+ months live; $500M+ TVL |

**Structural caveat:** JLP is the counterparty to leveraged traders. When traders win net, JLP value drops below NAV. Historically, traders lose approximately $3–5M per week net on Jupiter Perps — the structural edge is real but is not a free lunch.

**Historical analog:** GMX / GLP delta-neutral, run by Umami Finance and Rage Trade at peak $20M+ AUM in 2023. Same structure on a different perpetuals DEX.

**Capacity:** Approximately $15 million — Jupiter Perps depth limits the hedge.

### 3.4 Portfolio construction

Equal-weight across the three strategies. At $150k notional and the rates observed on 2026-05-11:

| Strategy | Weight | APR | Contribution |
|---|---|---|---|
| Stable Yield | 33 % | 3.91 % | 1.30 % |
| Multiply | 33 % | 10.48 % | 3.49 % |
| Hedged JLP | 33 % | 16.98 % | 5.66 % |
| **Blended** | 100 % | — | **10.45 %** |

Through-cycle target: **8 – 11 % blended**. Compared to institutional Solana yield products (BUIDL 4.30 %, FOBXX 4.25 %, USYC 4.00 %), this is approximately 2 – 2.5× the institutional floor, achieved without custody transfer.

---

## 4 · Risk framework

The framework has four layers, each enforced in code rather than in operator discipline.

### 4.1 Hard caps in `caps.rs`

Every daemon ships with a `caps.rs` module containing compile-time constants. The numbers are not configurable at runtime; changing them requires re-compilation and re-deployment.

Examples (Multiply):

- `MAX_TARGET_LTV_BPS = 6500` — operator cannot request leverage above 65 %
- `MAX_SLIPPAGE_BPS = 50` — swaps reject above 50 bps
- `MAX_DEPOSIT_USDC_LAMPORTS = 100_000 * 1_000_000` — single-Assign cap

A misconfigured operator request that exceeds a cap is rejected before any transaction is built.

### 4.2 Role-separated authority

Each agent has exactly one role and one key. The roles partition signing authority at compile time:

- **Multiply, Stable Yield, Hedged JLP** — can sign Solana transactions, only for their specific strategy
- **Researcher** — emits `MarketSignal` events, no signing key compiled in
- **Riskwatcher** — emits `EscalateRisk` events, no signing key compiled in

The agent that monitors risk is structurally incapable of trading. The agents that trade are structurally incapable of changing their own caps or suppressing risk alerts. This is enforced by the type system, not by policy or operator vigilance.

### 4.3 Riskwatcher daemon

Continuous on-chain monitoring of every active position:

- Polls Kamino obligation accounts every 5 seconds
- Computes liquidation distance per position
- Emits `EscalateRisk` events at three severity levels: `Important` at 60 % LTV, `Critical` at 70 %, `Emergency` at 80 %
- The Multiply daemon's inbox handler must check the latest Riskwatcher state before processing any `AssignMultiply` — a `Critical` or higher escalation blocks new positions

The daemon writes a JSONL telemetry log and exposes a Prometheus-style metrics endpoint on `127.0.0.1:9091`.

### 4.4 Kill switches and operational gates

- `--simulate-only true` forces the daemon to compute and log transactions but never broadcast them. Default for new deployments.
- `--network mainnet` requires the explicit flag `--i-understand-this-is-mainnet`. Devnet is the default.
- Manual approval mode requires operator sign-off on each transaction before broadcast. Auto mode is only enabled after the documented runbook: $50 mainnet test → 24-hour soak → operator promotion.
- The operator can `pkill` any daemon process at any time. The fleet's mesh architecture means the remaining daemons continue functioning; there is no single point of failure.

---

## 5 · Architecture

Five Rust binaries, all on the operator's hardware, communicating via libp2p over a peer-to-peer mesh on localhost (or any private network the operator configures).

| Daemon | Authority | Function |
|---|---|---|
| `multiply-daemon` | Signs (own strategy) | Manages leveraged jitoSOL position on Kamino |
| `stable-yield-daemon` | Signs (own strategy) | Manages USDC supply position on Kamino |
| `hedgedjlp-daemon` | Signs (own strategy) | Manages JLP holding and Jupiter Perps hedge |
| `riskwatcher-daemon` | No signing key | Polls positions, emits risk escalations |
| `researcher-daemon` | No signing key | Polls market data, emits price / rate / funding signals |

The execution daemons each subscribe to `MarketSignal` and `EscalateRisk` events from the read-only daemons. The mesh is encrypted, authenticated by Ed25519 role keys, and survives individual daemon restarts.

A separate `fleet-dashboard-server` ingests JSONL log files from each daemon, stores events in SQLite, and serves a REST + WebSocket API for the operator's monitoring frontend. The dashboard has no authority over the fleet — it is read-only telemetry.

---

## 6 · Track record

This section is what an institution will scrutinise. We are explicit about what has been demonstrated and what has not.

**What has been demonstrated:**

- Continuous mainnet paper-trading soak since 2026-05-09, 14:17 UTC. All five daemons running, fleet healthy 5/5 for the duration. Telemetry stored in SQLite, queryable.
- Cumulative paper P&L: $9.31 simulated earnings on $150k notional over 31 hours, consistent with the through-cycle target.
- Daemon resilience: clean restart in under 12 seconds when individual processes are killed. Heartbeat tracking and Beacon emission verified.
- Per-strategy live rate ingestion: Kamino USDC supply / borrow, jitoSOL APY, Solana base inflation, Jupiter Perps borrow, JLP fee 7-day average. Verifiable against the source APIs at any time.
- Devnet end-to-end round-trips for all three strategies: Assign → Report cycle, transaction simulation, position telemetry, withdrawal path.

**What has not been demonstrated:**

- Live capital deployment. The fleet has not yet executed a real transaction with operator capital on mainnet. The mainnet $50 test runbook is documented but not yet executed.
- Live drawdown event. We have stress-tested the strategies against historical data, but the deployed fleet has not yet weathered a SOL-crash event.
- Long-duration uptime. The current soak is in days, not weeks or months.
- Third-party audit of the fleet code itself. The underlying protocols (Kamino, Jupiter, Marinade) are audited; the fleet code is open source for inspection but has not been engaged with an audit firm.

---

## 7 · Terms of engagement

**Deployment model.** The institution installs the fleet on their own hardware (or a VPS they control). We provide setup support, the runbook for mainnet promotion, and ongoing strategy updates as open-source releases.

**Funding.** The institution funds its own Solana wallet. No funds ever touch our infrastructure. All yield accrues directly to the operator's wallet.

**Fees.** None at this stage. We are pre-revenue and pre-fund. Engagement during this period is collaborative — we want institutional operators willing to put $50–500k of test capital through the fleet in exchange for input on the operational model and direct support.

**What we ask for in return.** Honest feedback on what is missing for institutional deployment at scale (third-party audit, compliance review, custody integration, reporting format). Permission to cite the engagement when seeking later capital.

**What we do not ask for.** No fee, no carry, no allocation, no token, no equity.

---

## 8 · What is missing for full institutional readiness

We list these openly because acknowledging them is the difference between a credible offering and a marketing pitch:

- **Third-party fleet code audit.** Underlying protocols are audited; the fleet's orchestration code is not. Planned engagement: Q3 2026.
- **Compliance / regulatory wrapper.** No registered fund structure. Operators run this as proprietary infrastructure on their own balance sheet. Suitability depends on the operator's jurisdiction.
- **NAV / reporting integration.** Telemetry exists in SQLite. Standard institutional reporting formats (Bloomberg, Eze, third-party fund administrator) are not yet integrated.
- **Multi-sig governance for upgrades.** Currently the operator runs the binaries they download. Verified release signing and multi-sig deployment gating is on the roadmap.
- **Insurance.** None.
- **Capacity expansion.** Listed AUM caps are real. Beyond $50M aggregate, additional strategies and venues would need to be added.

---

## 9 · Roadmap

| Phase | Target |
|---|---|
| Now | Continuous mainnet paper soak; pilot institutional operators with $50–500k test capital |
| Q3 2026 | Third-party audit of fleet code; mainnet auto-mode promotion runbook executed by external operator |
| Q4 2026 | Standard reporting integration; additional Solana yield venues (Drift, MarginFi) |
| 2027 | Multi-venue routing across Solana DEX/lending; institutional-grade release signing and upgrade governance |

---

## 10 · References

- Kamino Finance: kamino.finance · audits: ottersec.io, offside.io
- Jupiter Perps / JLP: jup.ag · audits: halborn.com, ottersec.io
- Jito (jitoSOL): jito.network
- BlackRock BUIDL: securitize.io · Franklin Templeton FOBXX: franklintempleton.com · Circle USYC: circle.com/usyc
- Live rate sources: api.kamino.finance, yields.llama.fi, api.mainnet-beta.solana.com
- Hedgents fleet source: github.com/Hedgents/fleet

---

**Contact:** contact@hedgents.com for institutional discussions, technical due diligence access, or pilot deployment.

*This document describes software, not a security or an investment. It is for the purpose of evaluating whether to license and deploy the Hedgents fleet on the institution's own infrastructure. Past performance of paper trading does not guarantee live deployment returns. The institution is responsible for its own jurisdictional and suitability analysis.*
