# Mesh envelope consumers — actual vs documented

Reality check on which daemons consume which envelope types.
Built at v0.4.16 by greping each daemon's `dispatch.rs` for
`MsgType::` branches. Update this doc when the wiring changes.

## Truth table

| Daemon         | Assign | Withdraw* | Approve | Report | Escalate | MarketSignal | Beacon (observe) |
|----------------|--------|-----------|---------|--------|----------|--------------|------------------|
| multiply       |   ✓    |    ✓      |   ✓     |   —    |    ✓     |   log only   |        ✓         |
| stable_yield   |   ✓    |    ✓      |   ✓     |   —    |    —     |   log only   |        ✓         |
| hedgedjlp      |   ✓    |    ✓      |   ✓     |   —    |    —     |   log only   |        ✓         |
| riskwatcher    |   —    |    —      |   —     |   ✓    |    —     |     —        |        ✓         |
| researcher     |   —    |    —      |   —     |   —    |    —     | (emits only) |        ✓         |
| orchestrator   |   —    |    —      |   —     |   —    |    —     |   ✓ (rc14)   |        ✓         |

`*` Withdraw covers `MsgType::Withdraw` (stable_yield, hedgedjlp) and
`MsgType::WithdrawMultiply` (multiply, which carries strategy-specific
unwind args distinct from the generic envelope).

"log only" = the daemon receives the envelope per researcher's
`--subscriber` list and logs it explicitly at info level (v0.4.16
explicit log; pre-rc16 was silently bucketed under `"ignoring inbox
envelope"`), but there is no strategy-specific consumer logic.
Researcher signals reach the operator's log; they do not yet drive
strategy decisions.

## Where the docs vs code drift comes from

The LITEPAPER claims:
> The execution daemons each subscribe to `MarketSignal` and
> `EscalateRisk` events from the read-only daemons.

That's true at the *delivery* layer — researcher's broadcast does
reach each execution daemon's inbox. It's false at the *consumer*
layer — only multiply consumes `EscalateRisk`, and no execution
daemon meaningfully consumes `MarketSignal` today.

## Wiring follow-ups (per-RiskKind / per-SignalKind)

These are the natural next-rc items in priority order:

1. **multiply ← MarketSignal::PriceMovedBps (SOL)** — pause new
   leverage when SOL is in a sharp move. Mirror rc14's pattern
   (cache the signal, check on each AssignMultiply pre-flight).
2. **hedgedjlp ← MarketSignal::JlpYieldChanged** — defer the rebalance
   resize loop one tick after a major JLP yield change so the basis
   trade math uses fresh inputs.
3. **stable_yield ← MarketSignal::LendingBorrowRateAbove** — useful
   only when a Phase 2 venue diversification ships (USDY / USYC).
   Today stable_yield polls Kamino directly and doesn't need
   external signals.
4. **stable_yield ← MarketSignal::StableDepegBps** — pre-emptive
   withdraw from venues where the stablecoin is depegging beyond a
   tolerance. Materially affects rc16-era Phase 2 work; not urgent
   while only Kamino USDC is live.
5. **hedgedjlp + multiply ← EscalateRisk** — currently only multiply
   subscribes (line 431 of multiply/dispatch.rs); the other two
   should at minimum pause new opens on Critical from any RiskKind.

Each is a focused 1-2h rc shape. The classifier functions for the
relevant RiskKinds landed in rc15.
