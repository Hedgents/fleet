# Riskwatcher Devnet Runbook (M8)

End-to-end smoke for the riskwatcher daemon: boot multiply +
riskwatcher + a one-shot orchestrator stub on three localhost peers,
trigger a synthetic Critical-band breach, and verify multiply pauses
and rejects subsequent Assigns with `error_code=4`.

This runbook is the deliverable for milestone M8 of
`docs/superpowers/plans/2026-05-06-riskwatcher-daemon.md` and was
authored from a verified live run (commit `0b6e77d` + M8 changes).

## What this exercises

1. **Boot.** Three peers come up on loopback and find each other via mDNS.
2. **Report CC.** Multiply emits a `ReportMultiply` whose primary
   recipient is the orchestrator (the `Assign` sender) AND a CC copy
   to the configured `--riskwatcher` pubkey. Riskwatcher's observer
   sees it.
3. **Synthetic Critical.** Riskwatcher's poll loop classifies an
   injected position as Critical, dedups, and emits `EscalateRisk`
   envelopes to both the orchestrator and the multiply (subject)
   pubkey.
4. **Pause.** Multiply receives the Critical+LiquidationDistance
   Escalate from its trusted riskwatcher and flips to a 300-second
   pause window.
5. **Assign rejection.** A subsequent `AssignMultiply` is rejected
   without leverage execution; the Report carries `ok=false`,
   `error_code=4`.

## Prerequisites

- The repo is built: `cargo build --workspace` from
  `/Users/tobiasd/Desktop/zerox1/01fi-riskwatcher/` (or your worktree).
- `python3` with the `cryptography` package on your PATH (used to
  derive ed25519 verifying keys from raw seeds).
- `openssl` for generating 32-byte seeds.
- A reachable Solana JSON-RPC URL for **multiply only**. Multiply
  performs a `getGenesisHash` check at boot and refuses to start
  against an unreachable URL. Riskwatcher does NOT need a real RPC
  in this smoke — the synthetic-injection path bypasses Kamino — so
  riskwatcher is wired to `http://127.0.0.1:1` deliberately.
- An empty workspace dir at `/tmp/m8-smoke`. If you've run before:
  `rm -rf /tmp/m8-smoke`.

## Layout

| Peer            | Listen multiaddr            | Role                          | Long-running |
|-----------------|-----------------------------|-------------------------------|:------------:|
| fleet-pm-stub   | `/ip4/127.0.0.1/tcp/9300`   | orchestrator (one-shot Assign)| no           |
| multiply-daemon | `/ip4/127.0.0.1/tcp/9301`   | leverage executor             | yes          |
| riskwatcher     | `/ip4/127.0.0.1/tcp/9302`   | risk monitor + soft-veto      | yes          |

Peers find each other via mDNS on the local interface, so explicit
`--bootstrap` is only required from the smaller peers (riskwatcher,
fleet-pm-stub) into multiply. mDNS handles the reverse direction.

## Step 1 — Generate seeds

```bash
mkdir -p /tmp/m8-smoke/{orchestrator,multiply,riskwatcher,logs}
openssl rand -out /tmp/m8-smoke/orchestrator/orchestrator-role.key 32
openssl rand -out /tmp/m8-smoke/multiply/multiply-role.key       32
openssl rand -out /tmp/m8-smoke/riskwatcher/riskwatcher-role.key 32
chmod 600 /tmp/m8-smoke/*/*-role.key
```

The role-key file naming is fixed by the daemon binaries
(`<role>-role.key`); do not rename them.

## Step 2 — Derive ed25519 verifying keys

The CLI flags `--riskwatcher`, `--orchestrator`,
`--inject-test-position`, and `--recipient-agent-id` all expect
32-byte hex pubkeys, which are the ed25519 verifying keys derived
from the raw seeds.

```bash
python3 - <<'EOF'
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization
for name, p in {
    "orchestrator": "/tmp/m8-smoke/orchestrator/orchestrator-role.key",
    "multiply":     "/tmp/m8-smoke/multiply/multiply-role.key",
    "riskwatcher":  "/tmp/m8-smoke/riskwatcher/riskwatcher-role.key",
}.items():
    seed = open(p, "rb").read()
    pk = (Ed25519PrivateKey.from_private_bytes(seed)
          .public_key()
          .public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw))
    open(f"/tmp/m8-smoke/{name}.pubhex", "w").write(pk.hex())
    print(f"{name}={pk.hex()}")
EOF
```

The pubkey for each role is also printed in its first boot log line
(`Loaded identity ... agent_id=<hex>`), which is a fine fallback when
Python+cryptography isn't available.

Pin shell vars for the rest of the runbook:

```bash
ORCH_PUBKEY=$(cat /tmp/m8-smoke/orchestrator.pubhex)
MULT_PUBKEY=$(cat /tmp/m8-smoke/multiply.pubhex)
RW_PUBKEY=$(cat /tmp/m8-smoke/riskwatcher.pubhex)
```

## Step 3 — Generate multiply's Solana wallet

The multiply daemon refuses to boot without a valid Solana keypair
file. In simulate-only mode it never actually signs; an empty,
unfunded account is fine.

```bash
solana-keygen new --no-bip39-passphrase --silent \
  --outfile /tmp/m8-smoke/multiply/wallet.json
chmod 600 /tmp/m8-smoke/multiply/wallet.json
```

## Step 4 — Boot multiply

```bash
cd <worktree-root>   # the 01fi-riskwatcher worktree
RUST_LOG=info ./target/debug/multiply-daemon run \
  --fleet-id m8-smoke \
  --wallet      /tmp/m8-smoke/multiply/wallet.json \
  --secrets-dir /tmp/m8-smoke/multiply \
  --journal     /tmp/m8-smoke/multiply/journal.sqlite \
  --listen      /ip4/127.0.0.1/tcp/9301 \
  --rpc-url     https://api.devnet.solana.com \
  --simulate-only true \
  --require-approval false \
  --network devnet \
  --beacon-interval-secs 5 \
  --pnl-log /tmp/m8-smoke/multiply/pnl.jsonl \
  --riskwatcher "$RW_PUBKEY" \
  > /tmp/m8-smoke/logs/multiply.log 2>&1 &
```

Expected lines in `multiply.log` after a few seconds:

```
multiply args validated network=devnet ... riskwatcher_configured=true
rpc network verified network="devnet" genesis=EtWTRABZaYq6...
multiply starting fleet=m8-smoke role=multiply
Loaded identity ... peer_id=12D3KooW... agent_id=<MULT_PUBKEY>
0x01 enterprise bootstrap multiaddr: /ip4/127.0.0.1/tcp/9301/p2p/12D3KooW...
beacon emitted role=multiply nonce=1
```

Capture multiply's libp2p peer_id for the `--bootstrap` flag in the
next step:

```bash
MULT_PEER_ID=$(grep "0x01 enterprise bootstrap multiaddr" /tmp/m8-smoke/logs/multiply.log \
  | head -1 | sed 's/.*p2p\///' | awk '{print $1}')
echo "MULT_BOOT=/ip4/127.0.0.1/tcp/9301/p2p/$MULT_PEER_ID"
```

## Step 5 — Boot riskwatcher with synthetic injection

The `--inject-test-position` flag is a hidden M8 test fixture: it
pre-populates the registry with one synthetic entry whose
`obligation_pubkey == Pubkey::default()` AND `last_ltv_bps > 0`. The
poller short-circuits the Kamino fetch for this combination and
synthesises a `DecodedObligation` with liquidation distance ≈ 10 bps
— well below the Critical threshold of 50 bps.

```bash
RUST_LOG=info,riskwatcher_daemon=debug ./target/debug/riskwatcher-daemon \
  --fleet-id m8-smoke \
  --secrets-dir /tmp/m8-smoke/riskwatcher \
  --listen      /ip4/127.0.0.1/tcp/9302 \
  --bootstrap   /ip4/127.0.0.1/tcp/9301/p2p/$MULT_PEER_ID \
  --rpc-url     http://127.0.0.1:1 \
  --network     devnet \
  --beacon-interval-secs 5 \
  --poll-interval-secs   5 \
  --orchestrator         "$ORCH_PUBKEY" \
  --inject-test-position "$MULT_PUBKEY:9500" \
  > /tmp/m8-smoke/logs/riskwatcher.log 2>&1 &
```

The `9500` after the colon is the LTV in bps; the **distance** is
hard-coded to ≈ 10 bps inside `synth_critical_obligation` in
`crates/riskwatcher-daemon/src/poller.rs`. Any nonzero LTV
≤ 10000 trips Critical via the synthesis math.

## Step 6 — Verify mesh + synthetic Critical → pause

Within ~10 seconds you should see, on `riskwatcher.log`:

```
TEST FIXTURE — synthetic position injected; poller will short-circuit Kamino fetch
  subject=<MULT_PUBKEY> ltv_bps=9500
kamino poll updated subject=<MULT_PUBKEY> obligation=HUntD4TV... ltv_bps=9500
band breach — emitting Escalate (dedup-aware) severity=Critical ...
Escalate emitted severity=Critical kind=LiquidationDistance recipient=<ORCH_PUBKEY>
Escalate emitted severity=Critical kind=LiquidationDistance recipient=<MULT_PUBKEY>
poll tick complete n_total=1 n_ok=1 n_skipped=0
```

And on `multiply.log`:

```
ESCALATE from <RW_PUBKEY> for conversation 00000000000000000000000000000000 - human decision required
PAUSED by riskwatcher veto for 300s until=<unix> subject=<MULT_PUBKEY>
```

The "until" timestamp is `now + 300`. The pause is self-clearing on
the next inbound Assign whose timestamp ≥ `until`.

## Step 7 — Verify Assign rejection with error_code=4

```bash
./target/debug/fleet-pm-stub \
  --secrets-dir /tmp/m8-smoke/orchestrator \
  --listen      /ip4/127.0.0.1/tcp/9300 \
  --bootstrap   /ip4/127.0.0.1/tcp/9301/p2p/$MULT_PEER_ID \
  --recipient-agent-id "$MULT_PUBKEY" \
  --timeout-secs 25 \
  assign-multiply --target-ltv-bps 5500 --max-slippage-bps 50
```

Expected stdout from the stub:

```
Report received: msg_type=Report sender=<MULT_PUBKEY> conv=...
Report payload (decoded): ReportMultiply { header: ReportHeader { ... ok: false, error_code: Some(4) }, resulting_ltv_bps: 0, tx_signature: None }
```

And on `multiply.log`:

```
Assign rejected — paused by riskwatcher veto conv=...
report sent conv=... ok=false
```

`error_code=4` is `dispatch::ERR_PAUSED_BY_RISKWATCHER` — the soft-veto
return code (M7).

## Step 8 — (Optional) Verify Report CC observation

To watch the Report-CC end-to-end (separate from the pause path),
boot riskwatcher WITHOUT `--inject-test-position` and run an Assign.
Riskwatcher's observer will log at debug level:

```
ReportMultiply ok=false; out of scope for M3 registry sender=<MULT_PUBKEY> conv=...
```

The presence of this log line on the riskwatcher proves multiply CC'd
the Report — without the M8 fanout, the Report is bilateral to the
orchestrator only and the third peer never sees it. To see the
"happy-path" `Report observed` log instead, you'd need a real Kamino
position so multiply replies with `ok=true`, `resulting_ltv_bps>0`.

## Step 9 — (Optional) Verify auto-clear

Wait 300s after the pause was set, then re-issue the AssignMultiply
from Step 7. Expected: multiply executes leverage normally (or fails
on its usual leverage-execution error in this fixture-less smoke,
either way the Report's `error_code` will NOT be 4).

## Cleanup

```bash
kill $(pgrep -f multiply-daemon)
kill $(pgrep -f riskwatcher-daemon)
rm -rf /tmp/m8-smoke
```

## Troubleshooting

- **multiply exits with `get_genesis_hash` connection error.** The
  `--rpc-url` is unreachable. Use `https://api.devnet.solana.com`
  for the smoke. Riskwatcher is the daemon that tolerates an
  unreachable RPC (the synthetic-injection path skips the fetch).
- **fleet-pm-stub times out waiting for a Report.** Check the
  `--recipient-agent-id` value matches multiply's agent_id from its
  boot log. Mismatched agent_id silently drops the bilateral
  envelope. Also verify the `--bootstrap` multiaddr's peer-id
  matches the actual multiply peer-id (a stale value from a prior
  run will fail to dial).
- **riskwatcher Escalate not delivered to multiply.** Verify
  multiply was started with `--riskwatcher <RW_PUBKEY>`. Without
  this flag, multiply silently drops Escalate envelopes
  (`debug!("Escalate received but no --riskwatcher configured")`);
  the daemon is in observe-only mode. Conversely, if `--riskwatcher`
  is set to the wrong pubkey, multiply will log
  `Escalate REJECTED — sender does not match configured riskwatcher pubkey`
  at warn level.
- **Synthetic injection not tripping Critical.** Check `ltv_bps > 0`
  in the `--inject-test-position` argument. The short-circuit
  triggers on `obligation_pubkey == default() AND last_ltv_bps > 0`;
  a zero LTV would route through the real Kamino path. The
  synthesised distance is ≈ 10 bps regardless of the LTV value, so
  any nonzero LTV ≤ 10000 produces Critical.
- **`poll tick complete n_total=1 n_ok=0 n_skipped=1` after the
  first successful tick.** Expected. After the first synthetic
  refresh the entry's `obligation_pubkey` has been replaced with the
  real PDA via `state.upsert`, so subsequent ticks attempt the
  Kamino fetch (which fails against `http://127.0.0.1:1` and is
  treated as `Skipped`). The Escalate is already in flight from the
  first tick and is dedup-suppressed for 60s anyway.
- **Decode failure / version skew.** All three binaries must be
  built from the same workspace. The `zerox1-protocol` crate version
  is shared via the workspace `Cargo.toml`; running a stale binary
  against a freshly-built peer will surface as `Escalate payload
  decode failed` on the receiving side.
