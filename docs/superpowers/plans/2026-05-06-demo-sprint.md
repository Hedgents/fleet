# Demo Sprint Plan (7 days to demo day)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to execute this plan task-by-task.

**Goal:** Ship a local-only frontend + dashboard server that surfaces the fleet's inter-agent communication as a live human-readable feed, alongside live AUM/P&L numbers. Pre-soak the system for 24+ hours with real mainnet money so demo morning shows genuine history. Five daemons all live (multiply + stable-yield + hedgedjlp + riskwatcher + researcher); one risk level (moderate); one wallet (operator's local keypair).

**Architecture:** Add a new `tools/fleet-dashboard-server/` crate that aggregates JSONL telemetry + tracing logs from all 5 daemons + on-chain position reads, persists to a local SQLite, and serves a REST + WebSocket API on `127.0.0.1:7700`. Frontend at `localhost:3000` (Next.js or Vite + React) tails the WS for the live mesh feed and polls REST for numbers. No hosted infrastructure, no auth, no wallet adapter — everything local. Operator's Solana keypair lives in `secrets-dir/solana-wallet.json` per existing daemon convention.

**Tech Stack:** Rust + axum + sqlx + tokio (dashboard server), Next.js 14 + Tailwind + shadcn/ui (frontend), JSONL tail + Solscan/RPC reads (data sources). NO new mesh peer; the fleet stays exactly as shipped.

**Hero feature:** the mesh feed — every signed CBOR envelope decoded into one human-readable sentence, scrolling in reverse-chronological order. This is what makes the demo land.

---

## Critical pre-decisions (lock before Day 1)

- [x] Local-only frontend (no hosting)
- [x] One risk level: **Moderate** — 60% stable-yield, 40% multiply (hedgedjlp sim-only for safety unless live custody loader lands first)
- [x] All three strategy daemons running (multiply + stable-yield + hedgedjlp); riskwatcher + researcher run as infra
- [x] Pre-soak ~24h before demo (start ~Day 6 evening for Day 7 demo)
- [x] No vault Anchor program, no multi-user, no KYC
- [x] Wallet keypair file lives in operator's local `secrets-dir/`

---

## File Structure

```
tools/fleet-dashboard-server/
├── Cargo.toml
└── src/
    ├── main.rs              — axum boot, CLI args, log-tail orchestration
    ├── ingest/
    │   ├── mod.rs
    │   ├── log_tailer.rs    — tails each daemon's tracing log (JSON lines)
    │   ├── envelope_decoder.rs — converts log lines → MeshEvent enum
    │   └── pnl_jsonl.rs     — parses each daemon's *-pnl.jsonl
    ├── store/
    │   ├── mod.rs
    │   └── sqlite.rs        — append-only events table + indices
    ├── chain/
    │   ├── mod.rs
    │   ├── kamino.rs        — cache 30s of obligation reads
    │   ├── jupiter_perps.rs — cache 30s of position reads
    │   └── balance.rs       — wallet USDC + JLP + SOL balance
    ├── api/
    │   ├── mod.rs
    │   ├── events.rs        — GET /events, WS /events/live
    │   ├── state.rs         — GET /aum, /pnl, /positions
    │   └── orchestrate.rs   — POST /assign-moderate (lifts fleet-pm-stub logic)
    └── lib.rs

frontend/                     — new dir at repo root
├── package.json
├── next.config.js
├── tailwind.config.ts
├── app/
│   ├── layout.tsx
│   ├── page.tsx             — main dashboard
│   └── globals.css
├── components/
│   ├── NumbersPanel.tsx     — AUM + P&L + allocation pie + daemon pills
│   ├── MeshFeed.tsx         — scrolling envelope feed
│   ├── EventCard.tsx        — one mesh event, color-coded
│   ├── BehaviorTimeline.tsx — grouped events + sparkline charts
│   └── DaemonPill.tsx
├── lib/
│   ├── api.ts               — REST + WS client to localhost:7700
│   ├── decode.ts            — MeshEvent → human sentence
│   └── colors.ts            — per-MsgType palette
└── public/
```

---

## Milestones

### Day 1 — Dashboard server + log ingest (whole day, ~6-8h)

**Files:**
- Create: `tools/fleet-dashboard-server/Cargo.toml`, `src/main.rs`, `src/ingest/{mod.rs,log_tailer.rs,envelope_decoder.rs,pnl_jsonl.rs}`, `src/store/{mod.rs,sqlite.rs}`
- Modify: root `Cargo.toml` (workspace member)

- [ ] **Step 1: Create crate scaffold**
  - axum 0.7, sqlx 0.8 with sqlite + runtime-tokio, tokio, serde, serde_json, tracing, anyhow, clap, notify (for fs watch)
  - Boot: parse CLI args (`--secrets-dir-list`, `--db-path`, `--listen 127.0.0.1:7700`, `--solana-wallet`), init tracing, open SQLite, print "fleet-dashboard-server listening"

- [ ] **Step 2: SQLite schema**
  ```sql
  CREATE TABLE IF NOT EXISTS mesh_events (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      ts_unix INTEGER NOT NULL,
      ts_ms INTEGER NOT NULL,        -- millisecond resolution from log
      sender_role TEXT NOT NULL,     -- "multiply" / "stable-yield" / "researcher" / etc.
      direction TEXT NOT NULL,       -- "in" / "out" / "internal"
      msg_type TEXT NOT NULL,        -- "Beacon" / "Assign" / "Report" / "Escalate" / "MarketSignal" / "Internal"
      payload_summary TEXT NOT NULL, -- pre-decoded human sentence
      payload_json TEXT,             -- raw JSON for click-to-expand
      conv_id TEXT,
      tx_signature TEXT
  );
  CREATE INDEX idx_ts ON mesh_events(ts_unix DESC);
  CREATE INDEX idx_role ON mesh_events(sender_role);
  CREATE INDEX idx_msg ON mesh_events(msg_type);

  CREATE TABLE IF NOT EXISTS pnl_snapshots (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      ts_unix INTEGER NOT NULL,
      daemon TEXT NOT NULL,
      jlp_lamports INTEGER,
      hedge_notional_usdc INTEGER,
      deposited_usdc INTEGER,
      current_ltv_bps INTEGER,
      raw_json TEXT
  );
  CREATE INDEX idx_pnl_ts ON pnl_snapshots(ts_unix DESC);
  ```

- [ ] **Step 3: Log tailer** — `notify`-based file watcher that tails N JSONL log files (one per daemon) and emits parsed lines to a tokio channel. Skip non-JSON lines (tracing's text formatter), only consume `--log-json` formatted lines. Resilient to file rotation.

- [ ] **Step 4: Envelope decoder** — pure fn `LogLine -> Option<MeshEvent>`:
  - Match on `tracing` event message strings: `"BEACON emitted"`, `"AssignMultiply received"`, `"Report sent"`, `"EscalateRisk emitted"`, `"MarketSignal emitted"`, etc.
  - Extract role from log target prefix (`hedgedjlp_daemon::dispatch` → role=hedgedjlp)
  - Extract direction from msg keywords (`emitted`/`sent` = out, `received` = in, default = internal)
  - Decoder produces `(msg_type, summary, payload_json)` — see "Sentence rendering rules" below
  - Insert into mesh_events table

- [ ] **Step 5: PnL JSONL ingestor** — same `notify` pattern but for `*-pnl.jsonl` files. Parse each line as JSON, insert into pnl_snapshots.

- [ ] **Step 6: Smoke test**
  - Start a single daemon (multiply) writing to a known log path
  - Boot fleet-dashboard-server pointing at it
  - Send an Assign via fleet-pm-stub
  - Query `sqlite3 dashboard.sqlite 'SELECT * FROM mesh_events ORDER BY ts_unix DESC LIMIT 10;'` — at least 5 rows expected (Beacon out × N + Assign in + Report out + maybe Escalate)

- [ ] **Step 7: Commit `dashboard: Day 1 — log ingest + SQLite store`**

### Day 2 — REST + WS API + chain reads (whole day, ~6-8h)

**Files:**
- Create: `tools/fleet-dashboard-server/src/api/{mod.rs,events.rs,state.rs}`, `src/chain/{mod.rs,kamino.rs,jupiter_perps.rs,balance.rs}`

- [ ] **Step 1: REST endpoints**
  - `GET /events?since=<unix_ms>&limit=200&role=<filter>&type=<filter>` → JSON array of MeshEvents from SQLite
  - `GET /aum` → `{ total_usdc, per_strategy: { multiply: <usdc>, stable_yield: <usdc>, hedgedjlp_jlp: <usdc>, idle_usdc: <usdc> } }`
  - `GET /pnl?window=1h|24h|all` → `{ window, start_aum, end_aum, delta_usdc, percent_bps }`
  - `GET /positions` → per-daemon position state (Kamino obligations, JLP balance, hedge positions)
  - `GET /daemons` → health pills: `[{ role, last_heartbeat_ms_ago, status }]` (status = green/yellow/red based on last beacon age <30s/<120s/>120s)

- [ ] **Step 2: WebSocket /events/live**
  - On connect: send the last 50 events
  - Then stream new events as they're inserted (broadcast channel)
  - Frontend opens this on dashboard load and keeps it open

- [ ] **Step 3: Chain reads**
  - `chain::kamino::read_obligation(rpc, obligation_pubkey)` → caches 30s
  - `chain::jupiter_perps::read_position(rpc, position_pubkey)` → caches 30s
  - `chain::balance::read_wallet_balances(rpc, wallet_pubkey)` → USDC + JLP + SOL
  - These power /aum and /positions

- [ ] **Step 4: Smoke** — `curl localhost:7700/events?limit=5` returns JSON; `curl /aum` returns plausible numbers; `wscat -c ws://localhost:7700/events/live` shows live events

- [ ] **Step 5: Commit `dashboard: Day 2 — REST + WS API + chain reads`**

### Day 3 — Orchestrator inline + risk-mapper (~½ day) + frontend scaffold (~½ day)

**Files:**
- Create: `tools/fleet-dashboard-server/src/api/orchestrate.rs`
- Create: `frontend/` directory with Next.js scaffold

- [ ] **Step 1: Lift orchestrator from fleet-pm-stub**
  - The dashboard server needs to be able to ALSO emit Assign envelopes (so the demo can show "we're issuing a deposit now" if needed during dress rehearsal)
  - Lift the libp2p mesh peer code from fleet-pm-stub into `tools/fleet-dashboard-server/src/orchestrate.rs`
  - Endpoints:
    - `POST /assign/moderate` `{ amount_usdc }` → splits 60/40 across stable-yield/multiply, sends Assigns, returns conv_ids
    - `POST /withdraw/all` → emits Withdraw to each daemon with active position
  - These can be hidden in demo (operator triggered the Assign 24h earlier)

- [ ] **Step 2: Frontend scaffold**
  - `npx create-next-app@latest frontend --typescript --tailwind --app --no-eslint --no-src-dir --import-alias '@/*'`
  - `npx shadcn-ui@latest init` (Slate theme)
  - Add components: `npx shadcn-ui@latest add card badge button scroll-area separator`
  - Verify `cd frontend && npm run dev` opens localhost:3000

- [ ] **Step 3: API client `frontend/lib/api.ts`**
  ```typescript
  export const API_BASE = "http://localhost:7700";
  export async function fetchEvents(since?: number, limit = 200) { ... }
  export async function fetchAum() { ... }
  export async function fetchPnl(window: "1h" | "24h" | "all") { ... }
  export function openEventStream(onEvent: (e: MeshEvent) => void): WebSocket { ... }
  ```

- [ ] **Step 4: Commit `dashboard: Day 3 — orchestrator inline + frontend scaffold`**

### Day 4 — Frontend dashboard panels (whole day, ~6-8h)

**Files:**
- Create: `frontend/components/{NumbersPanel,MeshFeed,EventCard,BehaviorTimeline,DaemonPill}.tsx`, `frontend/lib/decode.ts`, `frontend/app/page.tsx`

- [ ] **Step 1: Sentence-rendering rules in `decode.ts`**
  Rules table — every MeshEvent decodes to one sentence with template + extracted fields:

  ```typescript
  const TEMPLATES: Record<string, (e: MeshEvent) => string> = {
    "Beacon": (e) => `${e.sender_role} announced presence (nonce ${e.payload.nonce})`,
    "Assign:multiply": (e) => `orchestrator asked multiply to lever to ${pct(e.payload.target_ltv_bps)} LTV (max ${bps(e.payload.max_slippage_bps)} slippage)`,
    "Assign:stable_lend": (e) => `orchestrator asked stable-yield to supply $${usdc(e.payload.usdc_lamports)} to Kamino`,
    "Assign:hedgedjlp": (e) => `orchestrator asked hedgedjlp to deploy $${usdc(e.payload.usdc_lamports)} (target delta ${e.payload.target_delta_bps}bps, borrow ceiling ${pct(e.payload.max_borrow_rate_bps)})`,
    "Approve": (e) => `orchestrator approved conv ${e.conv_id.slice(0,8)}…`,
    "Report:multiply:ok": (e) => `multiply reported ok — resulting LTV ${pct(e.payload.resulting_ltv_bps)}, tx ${e.tx_signature?.slice(0,8)}…`,
    "Report:stable_lend:ok": (e) => `stable-yield deposited $${usdc(e.payload.deposited_usdc_lamports)}`,
    "Report:hedgedjlp:ok": (e) => `hedgedjlp opened ${e.payload.tx_signatures?.length ?? 0} positions, hedge notional $${usdc(e.payload.hedge_notional_usdc)}`,
    "Report:err": (e) => `${e.sender_role} reported error_code=${e.payload.header.error_code} on conv ${e.conv_id.slice(0,8)}…`,
    "EscalateRisk:Notice:DeltaDrift": (e) => `riskwatcher noticed ${e.payload.subject_short} drifting (distance ${bps(e.payload.measurement)})`,
    "EscalateRisk:Critical:LiquidationDistance": (e) => `riskwatcher CRITICAL — ${e.payload.subject_short} liquidation distance ${bps(e.payload.measurement)}`,
    "EscalateRisk:Notice:NeedsApproval": (e) => `${e.sender_role} queued an Assign and is waiting for approval`,
    "MarketSignal:LendingBorrowRateAbove": (e) => `researcher saw ${e.payload.asset} Kamino borrow rate hit ${pct(e.payload.measurement_bps)} (${e.payload.severity})`,
    "MarketSignal:PriceMovedBps": (e) => `researcher saw ${e.payload.asset} move ${signedPct(e.payload.measurement_bps)} over 1h`,
    "MarketSignal:JlpYieldChanged": (e) => `researcher saw JLP 7d yield change to ${pct(e.payload.measurement_bps)} APR`,
    "MarketSignal:JlpCompositionShifted": (e) => `researcher saw JLP composition shift in ${e.payload.asset} (${signedBps(e.payload.measurement_bps)})`,
    "MarketSignal:StableDepegBps": (e) => `researcher saw ${e.payload.asset} ${e.payload.measurement_bps > 0 ? "above" : "below"} peg by ${bps(Math.abs(e.payload.measurement_bps))}`,
    "Internal:rebalance:check": (e) => `${e.sender_role} checked rebalance — ${e.payload.note}`,
    "Internal:poll:heartbeat": (e) => `${e.sender_role} polled (${e.payload.tick_count} watchers, ${e.payload.signals_emitted} signals)`,
  };
  ```

  Fallback: `${role} ${msg_type} ${conv_id?.slice(0,8) ?? ""}` for unmatched.

- [ ] **Step 2: `MeshFeed.tsx`** — vertically-scrolling list, newest at top, auto-scroll on new events from WS. Each row uses `EventCard`. Pull initial 200 from `/events`, then append from WS. Color-code background by msg_type family (signal=blue, assign=purple, report=green, escalate=red, internal=gray). Click row → expand inline showing payload_json pretty-printed.

- [ ] **Step 3: `NumbersPanel.tsx`** — top of page. Three cards:
  - **Total AUM**: big animated number (use `react-countup` or hand-rolled requestAnimationFrame), poll `/aum` every 5s
  - **24h P&L**: signed dollar amount + percent, color-coded green/red, poll `/pnl?window=24h` every 30s
  - **Allocation**: pie chart (`recharts`), per-strategy USDC value
  - Below: 5 daemon pills (`DaemonPill` per daemon) reading `/daemons` every 10s

- [ ] **Step 4: `BehaviorTimeline.tsx`** — bottom panel, narrower height. Two charts side-by-side:
  - AUM over 24h (line chart, recharts)
  - Event-density heatmap (24 hourly buckets, color intensity = event count)
  - Below: grouped event labels ("06:00 — Deposit", "08:23 — Rebalance", "14:11 — Signal Burst") clickable to scroll the mesh feed to that timestamp

- [ ] **Step 5: `app/page.tsx`** — composes the three panels with proper layout (grid, sticky top). Open WS on mount; close on unmount.

- [ ] **Step 6: Smoke** — boot dashboard server pointing at the fleet running locally, open localhost:3000, verify all three panels render with live data.

- [ ] **Step 7: Commit `dashboard: Day 4 — frontend panels with mesh feed`**

### Day 5 — Integration + start 24h soak (whole day, ~6h)

- [ ] **Step 1: Wire all daemons to write JSON-formatted tracing logs**
  - For each daemon: pass `RUST_LOG_FORMAT=json` (or wire via tracing-subscriber's `with_format(JsonFormat)` if not already)
  - Verify each daemon writes structured events the decoder can parse
  - Update each daemon's existing tracing event strings if needed for consistent matching (e.g., `info!("BEACON emitted role=hedgedjlp nonce=X")` becomes a structured event we can match on)

- [ ] **Step 2: Boot script `scripts/run-fleet-with-dashboard.sh`**
  ```bash
  # Boots all 5 daemons + dashboard server in one shell
  # Each daemon writes to ~/01fi-soak/logs/<daemon>.log
  # Dashboard tails them all
  # Wallet keypair: ~/01fi-soak/secrets/solana-wallet.json
  ```

- [ ] **Step 3: Mainnet config**
  - Generate fresh wallet, fund with $500 USDC + 0.1 SOL
  - Boot fleet on mainnet with --network mainnet --i-understand-this-is-mainnet
  - Issue moderate-risk Assign: $300 to stable-yield, $200 to multiply, hedgedjlp in --simulate-only mode (live custody loader still pending)
  - Verify on Solscan: 2-3 real txs land
  - Dashboard shows the activity

- [ ] **Step 4: Soak start (target Day 6 evening for Day 7 morning demo)**
  - Leave fleet running
  - Researcher's watchers will tick every minute and accumulate signals
  - Beacons every 5s give the mesh feed visible heartbeat
  - Multiply's liq monitor polls every N seconds
  - Telemetry JSONL accumulates

- [ ] **Step 5: Commit `dashboard: Day 5 — integration + soak boot script`**

### Day 6 — Polish + dress rehearsal (whole day, fleet soaking in background)

- [ ] **Step 1: Error states** — what does the UI show if:
  - Dashboard server is down (frontend retries WS, shows "reconnecting…" banner)
  - A daemon dies (pill goes red, mesh feed shows last events frozen)
  - RPC rate-limited (chain reads stale, show "last updated 47s ago")
  - SQLite locked (don't crash; back off + retry)

- [ ] **Step 2: Demo narration cards** — optional small overlay that appears on key event types ("This is researcher detecting a SOL move →") to help the audience parse the feed

- [ ] **Step 3: Pacing tweaks** — if the soak shows long quiet stretches, pace BEACON intervals shorter on demo morning for visual activity. Add a low-frequency "research heartbeat" event every 60s that says "5 watchers polled, market stable" — fills dead air while still being honest

- [ ] **Step 4: Demo script (in this file, append below)**
  ```
  0:00-0:30  Intro: 5 autonomous agents, 24h on mainnet, real USDC
  0:30-1:30  Mesh feed walkthrough: pick last 5 minutes, narrate one
              researcher signal → strategy daemon reaction → riskwatcher
              observation. Click a Report event to expand the CBOR payload.
  1:30-2:15  Numbers: AUM, 24h P&L, allocation pie. Pull up Solscan
              for the most recent tx — "this happened 20 minutes ago,
              on chain, real money."
  2:15-2:45  Architecture: compile-time authority isolation
              (cargo-tree on riskwatcher = no wallet dep), per-instruction
              whitelist, role-keyed Ed25519 identity.
  2:45-3:00  Roadmap: investor capital next month, institutional
              deployment Q3.
  ```

- [ ] **Step 5: Dress rehearsal** — run the demo script against the live dashboard. Time it. Cut anything that drags.

- [ ] **Step 6: Pre-flight checklist for demo morning:**
  - [ ] Fleet alive (all 5 daemons green pills)
  - [ ] Recent events visible in last 5min on mesh feed
  - [ ] Most recent on-chain tx still verifiable on Solscan
  - [ ] AUM number displays plausibly (not zero, not infinity)
  - [ ] WS connection live (no "reconnecting" banner)
  - [ ] Browser zoom set for projection legibility

- [ ] **Step 7: Commit `dashboard: Day 6 — polish + demo script`**

### Day 7 — Demo

- [ ] **0700**: pre-flight checklist
- [ ] **0730**: dress rehearsal #2 against final state
- [ ] **0830**: leave fleet alone, no risky changes
- [ ] **Demo time**: 3 minutes per the script

---

## Sentence rendering rules (reference)

For every MeshEvent the decoder produces a one-line human sentence. Rules:

1. **Subject**: who acted (`researcher`, `multiply`, `orchestrator`, etc.)
2. **Verb**: what they did (`asked`, `reported`, `noticed`, `saw`, `announced`, `paused`)
3. **Object**: what was affected (asset, daemon, conv_id, amount)
4. **Quantity**: numeric specifics ($X, Y bps, Z%)

Bad: `"Envelope MsgType=Report sender=08a3 conv=000000... ok=true"`
Good: `"multiply reported ok — resulting LTV 70% (tx 5jKZ…)"`

Bad: `"MarketSignal kind=PerpFundingAbove asset=SOL measurement_bps=2500"`
Good: `"researcher saw SOL perp funding hit 25% APR (Notice)"`

Run every template through the "would a non-engineer institutional buyer parse this in <2s?" filter.

---

## Color palette

- **Beacon**: gray (#9CA3AF) — heartbeats, low-attention
- **Assign / Approve**: purple (#A855F7) — orchestrator commands
- **Report ok**: green (#10B981) — strategy success
- **Report err**: amber (#F59E0B) — strategy failure
- **EscalateRisk Notice**: blue (#3B82F6) — informational
- **EscalateRisk Warning**: orange (#F97316) — attention
- **EscalateRisk Critical**: red (#EF4444) — alarm
- **MarketSignal Info**: light cyan (#67E8F9)
- **MarketSignal Notice**: cyan (#06B6D4)
- **MarketSignal Important**: dark cyan / teal (#0E7490)
- **Internal**: muted gray (#6B7280)

---

## What we deliberately skip

- Authentication (localhost only)
- Multi-tenancy (one operator, one wallet)
- Wallet adapter / Phantom integration (operator manages their own keypair)
- Vault Anchor program (custody = operator's wallet)
- Risk-level slider (hardcoded moderate)
- Withdraw flow visibility in UI (button shows "demo locked")
- Hosted deployment (no Vercel, no domain)
- KYC / compliance flows
- Transaction submission via the frontend (orchestrator commands optional, hidden by default)
- Hedgedjlp live submit (sim-only until live custody loader lands — separate post-demo work)

---

## What can be cut if running short

If by Day 5 you're behind:

1. Drop the **WebSocket** → poll every 3s instead. ~1 day saved.
2. Drop the **behavior timeline** → just numbers + mesh feed. ~½ day saved.
3. Drop the **chain reads** → use only JSONL telemetry for AUM/positions. ~½ day saved.
4. Drop **hedgedjlp from demo entirely** → simpler narrative. ~0 days saved (just removes risk).
5. Drop **error state polish** → defaults are usable. ~½ day saved.

Each cut applied in order, save ~3 days total.

---

## Risk register

| Risk | Mitigation |
|---|---|
| Fleet crashes mid-soak | Dashboard shows last-known state; restart script; demo last 12h |
| Mainnet position loses money during soak | Demo narrative: "watch the strategy work" — losses are real and instructive too. Cap at $500 to bound loss. |
| Demo machine network flakes | Run dashboard + fleet on the same laptop, no network round-trip required for the demo path |
| Hedgedjlp live submit accidentally enabled | M9-M11 hard-stop already prevents this; `--simulate-only=true` is the default |
| Researcher signal flood drowns the mesh feed | Dedup is already on a 60s cooldown per (kind, asset); shouldn't be a problem |
| Audience doesn't grok the mesh feed | Demo script narrates 2-3 specific events; don't expect them to read the whole feed |

---

## Self-review

After Day 6, look at the dashboard with fresh eyes. Spot-check:

1. **Can a stranger watching the screen tell what's happening without you narrating?** If no, the sentences need work.
2. **Is the AUM number changing visibly within a 30s window?** If no, fleet is too quiet — pace beacons or salt with internal heartbeats.
3. **Can you click an event and verify it on Solscan in under 10s?** Tx signatures must be linked or copyable.
4. **Does the demo machine boot the whole stack in <60s?** If no, optimize startup or pre-warm before demo.
5. **Are there any "TODO" overlays in the UI?** If yes, hide behind feature flags — don't show in demo.

---

## Three-minute demo script (locked once Day 6 dress rehearsal passes)

```
[0:00-0:15]
"This is an institutional treasury operator's dashboard.
Five autonomous agents have been managing $500 USDC for 24 hours
on Solana mainnet. Real money. No human approvals."

[0:15-0:45]
"Watch them talk to each other."
[Scroll mesh feed through last 10 minutes. Highlight:]
  - "Here, researcher detected a SOL price move."
  - "Multiply received the signal, checked its position, decided
     no rebalance was needed."
  - "Riskwatcher polled the Kamino obligation and observed
     liquidation distance is 487 bps — comfortable."
"Every line is a signed CBOR envelope. Cryptographically
attributable. We can rewind 24 hours of conversation."

[0:45-1:30]
"The numbers."
[Point to NumbersPanel]
"Total AUM: $501.47. 24h P&L: +$1.47. Allocation:
60% Kamino USDC supply, 40% Kamino-leveraged JitoSOL."
[Click most recent Report event, expand to show payload]
"This transaction landed on chain 23 minutes ago."
[Click tx_signature → opens Solscan]
"Real."

[1:30-2:15]
"Three things make this load-bearing."
[1] "The riskwatcher daemon — `cargo tree -p riskwatcher` shows
    zero wallet crate dependency. Compile-time authority isolation.
    A compromised riskwatcher can spam alerts, cannot move funds."
[2] "Per-instruction whitelist before signing. Every transaction
    passes through SigningWhitelist::verify_ixns. If a future bug
    injects an unexpected program ID, signing refuses."
[3] "Role identity decoupled from host. The libp2p peer-id is
    ephemeral; the role-key is the durable cryptographic identity.
    Any of these daemons can move to a new host in 30 seconds."

[2:15-2:45]
"Roadmap. We're shipping mainnet today on multiply and stable-yield.
Hedgedjlp lands next week. Investor capital onboards next month.
Institutional deployment Q3 — same software, scales to $50M."

[2:45-3:00]
"Questions?"
```

---

## Open implementation questions to resolve Day 1

- Should the dashboard server be in the `01fi/` workspace or `node-enterprise/`? Recommend `01fi/tools/` since that's where fleet-pm-stub lives.
- What's the daemon log path convention? Recommend each daemon writes to `~/01fi-soak/logs/<daemon>.log` for the soak; dashboard tails the directory.
- JSON tracing format: structured fields or rendered strings? Recommend structured (use `tracing-subscriber::fmt::json()`); decoder works on field map, not regex.
- Where's the operator's wallet pubkey passed to the dashboard? Recommend `--solana-wallet ~/01fi-soak/secrets/solana-wallet.json` so balance reads work.
