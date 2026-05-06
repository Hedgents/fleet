# Multiply daemon — devnet smoke runbook

End-to-end Assign → Report round-trip on Solana devnet, sim-only.

## Prerequisites

- Build the workspace once: `cargo build --release -p multiply-daemon -p fleet-pm-stub`
- Devnet RPC URL: `https://api.devnet.solana.com` (or your private endpoint)
- A Solana keypair (sim-only doesn't require funding, but `Wallet::load` rejects empty keypair files)

## Generate role keys + Solana wallet

```bash
mkdir -p /tmp/m5-smoke/{multiply,orch}
openssl rand 32 > /tmp/m5-smoke/multiply/multiply-role.key
openssl rand 32 > /tmp/m5-smoke/orch/orchestrator-role.key
chmod 600 /tmp/m5-smoke/{multiply,orch}/*.key
solana-keygen new --outfile /tmp/m5-smoke/multiply/solana-wallet.json \
    --no-bip39-passphrase --force
```

## Boot multiply-daemon (background)

```bash
RUST_LOG=info cargo run --release -p multiply-daemon -- run \
    --secrets-dir /tmp/m5-smoke/multiply \
    --wallet /tmp/m5-smoke/multiply/solana-wallet.json \
    --rpc-url https://api.devnet.solana.com \
    --listen /ip4/127.0.0.1/tcp/19302 \
    --beacon-interval-secs 5 \
    > /tmp/m5-multiply.log 2>&1 &
sleep 10
grep "Listening on /ip4/127.0.0.1/tcp/19302" /tmp/m5-multiply.log
```

Note: `RUST_LOG=info` is required — without it, `tracing_subscriber::fmt::init()`
defaults to ERROR-level filtering and the boot/dispatch INFO logs won't appear.

Defaults: `--simulate-only true`, `--require-approval false` (devnet),
`--max-position-usdc-lamports 100000000` ($100). Mainnet defaults differ; see
`multiply-mainnet-tiny.md` for that path (M10).

The daemon boot log lists the bootstrap multiaddr including its peer-id, e.g.

```
0x01 enterprise bootstrap multiaddr: /ip4/127.0.0.1/tcp/19302/p2p/12D3Koo...
```

It also logs its `agent_id` (a 32-byte hex string). Capture this — the stub
needs it to route an `Assign` (which is bilateral, not broadcast).

```bash
MULTIPLY_AGENT_ID=$(grep -oE 'agent_id=[0-9a-f]{64}' /tmp/m5-multiply.log | head -1 | cut -d= -f2)
echo "multiply agent_id: $MULTIPLY_AGENT_ID"
```

## Send AssignMultiply

First, extract the multiply daemon's agent_id from its log (if not already done):

```bash
MULTIPLY_AGENT_ID=$(grep -oE 'agent_id=[0-9a-f]{64}' /tmp/m5-multiply.log | head -1 | cut -d= -f2)
```

Then send the Assign with the recipient flag:

```bash
RUST_LOG=info cargo run --release -p fleet-pm-stub -- \
    --secrets-dir /tmp/m5-smoke/orch \
    --listen /ip4/127.0.0.1/tcp/19399 \
    --bootstrap /ip4/127.0.0.1/tcp/19302 \
    --recipient-agent-id "$MULTIPLY_AGENT_ID" \
    --timeout-secs 15 \
    assign-multiply --target-ltv-bps 6000
```

## Expected output

`fleet-pm-stub` exits 0 and prints a Report:
```
Report received: msg_type=Report sender=<hex> conv=<hex>
Report payload (decoded): ReportMultiply { header: ReportHeader { conversation_id: ..., ok: true, error_code: None }, resulting_ltv_bps: 0, tx_signature: None }
```

`multiply-daemon` log shows the dispatch path firing:
```
INFO  AssignMultiply received target_ltv_bps=6000 max_slippage_bps=50
INFO  leverage::run_or_simulate (M4 placeholder — M6 implements) simulate_only=true target_ltv_bps=6000
INFO  report sent ok=true
```

`resulting_ltv_bps=0` is correct for the M4 placeholder — M6 makes the loop
actually advance LTV.

## Tear down

```bash
pkill -f multiply-daemon
```

## Troubleshooting

- **"Failed to dial bootstrap peer"**: multiply-daemon hasn't bound its listen
  port yet. Wait longer or check `/tmp/m5-multiply.log`.
- **"No Report received: timed out" despite correct --recipient-agent-id**: the
  stub now emits a BEACON before sending Assigns to register its pubkey in the
  recipient's peer_states. If you see drops nonetheless, increase the propagation
  sleep (currently 3s) or check that the stub's BEACON broadcast is reaching the
  daemon via gossipsub.
- **"--recipient-agent-id must be 32 bytes"**: the agent_id from the log should
  be 64 hex characters. Double-check the grep command output.
- **"AssignMultiply rejected: target_ltv_bps exceeds hard cap"**: you asked for
  >8000 bps. Pass `--target-ltv-bps 6000` instead.
- **"require_approval is true and Approve flow is not yet wired"**: somehow
  `--require-approval` resolved to true on devnet. Check args passing; default
  is false on devnet.
- **Daemon log is silent / no INFO lines**: you forgot `RUST_LOG=info`. The
  default subscriber filters at ERROR.

## What this proves

- The mesh delivers bilateral envelopes between two libp2p peers (orchestrator
  stub ↔ multiply daemon) via agent_id-based unicast routing
- AssignMultiply CBOR payload encodes/decodes correctly
  (`zerox1-protocol::fleet::multiply` round-trip)
- Daemon dispatches on `MsgType::Assign`, validates caps, builds + signs
  `ReportMultiply`, broadcasts back
- Stub receives, conv-id-filters, decodes, prints

This is the architectural foundation for real-money operations. M6 makes the
leverage loop actually do something on chain; M9 adds telemetry; M10 promotes
to mainnet. Long-term (M7), `runtime::role_registry` will replace the hand-passed
`--recipient-agent-id` flag with automatic role-to-peer-id resolution.
