# Riskwatcher Mainnet Runbook (M10)

WARNING: THIS GUIDE GOVERNS A DAEMON THAT TOUCHES REAL FUNDS, REAL
LIQUIDATIONS, AND CAN PAUSE LIVE TRADING. Skipping the co-location
discipline in §6 or the 24h watch protocol in §7 will silently degrade
the safety guarantee that justifies running riskwatcher at all.

This runbook is the M10 deliverable of
`docs/superpowers/plans/2026-05-06-riskwatcher-daemon.md`. It supersedes
the devnet runbook (`riskwatcher-devnet.md`) for production. The devnet
guide remains the right reference for smoke testing.

## 1. Purpose

You are deploying the riskwatcher daemon to Solana mainnet for the first
time. Riskwatcher is a read-only observer: it polls Kamino obligations
for every multiply position it has heard a `ReportMultiply` for,
classifies liquidation distance into Notice / Warning / Critical bands,
and emits signed `EscalateRisk` envelopes onto the 0x01 mesh. A Critical
escalation causes every multiply daemon configured with this
riskwatcher's pubkey to enter a 300-second pause window. The wallet
crate is intentionally NOT in the dependency graph; the daemon cannot
sign transactions.

Mainnet-specific risks this runbook addresses:

- **Real liquidations.** A missed Critical breach is a missed safety
  net. Co-location with the daemon being watched undoes the entire
  point of the architecture.
- **Real RPC budget.** Public RPC will rate-limit you under volatility,
  exactly when classification accuracy matters most.
- **Real Escalate spam.** Mis-tuned dedup or misconfigured peers can
  cause repeated pauses on healthy positions. The 24h watch verifies
  the dedup window is doing its job.
- **Real key custody.** The role-key seed authenticates every Escalate;
  losing or leaking it is a recoverable but disruptive event (see §9).

## 2. Prerequisites

- A separate host from every multiply daemon you intend to watch. See
  §6 — this is non-negotiable.
- A Solana **mainnet** JSON-RPC URL with adequate rate budget. Use a
  paid provider (Helius, QuickNode, Triton). Kamino obligation polling
  is bursty: a healthy steady state is `1 RPC × n_positions /
  poll_interval_secs`. With the default 30-second interval and 32
  registered positions that is ~64 req/min, but volatility events that
  reset the dedup window can spike that briefly. Public-rpc.solana.com
  will return 429s exactly when you cannot afford them.
- A release build: `cargo build --release -p riskwatcher-daemon` from
  the repo root, producing `target/release/riskwatcher-daemon`. Install
  it as `/usr/local/bin/riskwatcher-daemon` (or your supervisor's
  expected path).
- A secrets directory at `/var/lib/01fi/riskwatcher/`, mode `0700`,
  owned by the UID the daemon runs under.
- A telemetry log directory (e.g. `/var/log/01fi/`) writable by the
  same UID, with logrotate configured (the daemon does not rotate; see
  §10).
- Network reachability:
  - Outbound TCP to your RPC provider.
  - Inbound TCP on the public IP/port chosen for `--listen`.
  - Outbound TCP to the orchestrator's libp2p multiaddr.
  - Outbound TCP to each multiply daemon's libp2p multiaddr (Critical
    Escalates fan out directly to each subject in addition to the
    orchestrator).
- `python3` with the `cryptography` package, for deriving the
  verifying key (§3). PyNaCl works as a drop-in alternative.

## 3. Role key generation

Mainnet should ultimately back the role key with hardware. For the v0
software-key deployment:

```bash
# Create the secrets dir.
sudo install -d -m 0700 -o 01fi -g 01fi /var/lib/01fi/riskwatcher

# 32 random bytes from /dev/urandom (NOT /dev/random — slow on Linux).
sudo dd if=/dev/urandom of=/var/lib/01fi/riskwatcher/riskwatcher-role.key \
        bs=32 count=1 status=none
sudo chmod 0600 /var/lib/01fi/riskwatcher/riskwatcher-role.key
sudo chown 01fi:01fi /var/lib/01fi/riskwatcher/riskwatcher-role.key
```

The file name is fixed by the daemon: `riskwatcher-role.key`. Do not
rename it.

Derive the ed25519 verifying key. This 32-byte hex value is what every
other peer references as the riskwatcher's pubkey (orchestrator config,
multiply's `--riskwatcher` flag).

```bash
python3 - <<'EOF'
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization
seed = open("/var/lib/01fi/riskwatcher/riskwatcher-role.key", "rb").read()
pk = (Ed25519PrivateKey.from_private_bytes(seed)
      .public_key()
      .public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw))
print(pk.hex())
EOF
```

PyNaCl one-liner (equivalent, if `cryptography` is unavailable):

```bash
python3 -c "import nacl.signing, sys; \
seed = open('/var/lib/01fi/riskwatcher/riskwatcher-role.key', 'rb').read(); \
sys.stdout.write(nacl.signing.SigningKey(seed).verify_key.encode().hex())"
```

The verifying key is also printed in the daemon's first boot log line
(`Loaded identity ... agent_id=<hex>`).

**Back up the role-key seed to a separate, audited offline store.**
Loss = the daemon cannot be restarted under that pubkey without
re-distributing a new pubkey to every peer (§9 covers rotation).

## 4. Mainnet boot command

Replace the placeholders (`YOUR_KEY`, `<ORCHESTRATOR_PUBLIC_IP>`,
`<PORT>`, `<ORCHESTRATOR_PEER_ID>`, `<ORCHESTRATOR_PUBKEY_HEX_64_CHARS>`)
with values from your fleet:

```bash
ZX_FLEET_ID=01fi-mainnet \
ZX_RPC_URL=https://mainnet.helius-rpc.com/?api-key=YOUR_KEY \
ZX_NETWORK=mainnet \
ZX_SECRETS_DIR=/var/lib/01fi/riskwatcher \
ZX_LISTEN=/ip4/0.0.0.0/tcp/9302 \
ZX_BOOTSTRAP=/ip4/<ORCHESTRATOR_PUBLIC_IP>/tcp/<PORT>/p2p/<ORCHESTRATOR_PEER_ID> \
ZX_BEACON_INTERVAL_SECS=30 \
ZX_POLL_INTERVAL_SECS=30 \
ZX_ORCHESTRATOR=<ORCHESTRATOR_PUBKEY_HEX_64_CHARS> \
ZX_TELEMETRY_LOG=/var/log/01fi/riskwatcher-pnl.jsonl \
ZX_METRICS_LISTEN=127.0.0.1:9091 \
/usr/local/bin/riskwatcher-daemon
```

Flag-by-flag, with mainnet emphasis:

- `--fleet-id` (`ZX_FLEET_ID`): operator-chosen string, logged for
  cross-cutting introspection. Use a stable per-environment value
  (`01fi-mainnet`) so log aggregation can group cleanly.
- `--rpc-url` (`ZX_RPC_URL`): MUST be a paid provider on mainnet.
  `https://api.mainnet-beta.solana.com` is rate-limited and returns
  429s under exactly the volatility conditions where classifier
  accuracy matters most. The daemon validates network identity
  against this RPC at boot via genesis-hash check.
- `--network mainnet` (`ZX_NETWORK`): selects mainnet program IDs and
  market addresses. The default is `devnet`; mainnet must be opted in
  explicitly.
- `--secrets-dir` (`ZX_SECRETS_DIR`): directory containing
  `riskwatcher-role.key`. Mode `0700`. The daemon also writes
  `.runtime-keypair-riskwatcher` (a copy of the seed in the format the
  embedded node expects) inside this directory; do not interfere with
  it.
- `--listen` (`ZX_LISTEN`): public libp2p multiaddr. `0.0.0.0/tcp/9302`
  binds all interfaces; firewall accordingly.
- `--bootstrap` (`ZX_BOOTSTRAP`): repeatable. At minimum, include the
  orchestrator's multiaddr so the daemon can dial in. Without any
  bootstrap, the riskwatcher only sees BEACONs from peers that dial
  IT — functional, but mesh convergence is slow and one-sided.
- `--beacon-interval-secs` (`ZX_BEACON_INTERVAL_SECS`): how often the
  daemon emits its own BEACON to the mesh. 30s is a good default.
- `--poll-interval-secs` (`ZX_POLL_INTERVAL_SECS`): how often the
  Kamino obligation poller runs. 30s is the default and matches the
  M4 spec; lowering increases RPC cost linearly.
- `--orchestrator` (`ZX_ORCHESTRATOR`): 64-char lowercase hex of the
  orchestrator's verifying key. **Required** — the daemon refuses to
  boot without it. Validated at startup; an invalid value bails the
  process rather than failing silently at the first band breach.
- `--telemetry-log` (`ZX_TELEMETRY_LOG`): path to the JSONL log of
  per-poll telemetry. The daemon refuses to boot if the path is not
  openable. One line per registered position per tick.
- `--metrics-listen` (`ZX_METRICS_LISTEN`): bind for the Prometheus
  `GET /metrics` endpoint. **Bind loopback only.** The endpoint has no
  authentication. Do NOT expose it publicly. Bind failure is fatal.

## 5. Cross-config flags on the orchestrator and multiply

The riskwatcher's verifying key has to be wired into two other
peer configurations:

- **Orchestrator config.** Whatever your orchestrator uses to filter
  inbound `EscalateRisk` envelopes, it must accept this riskwatcher's
  pubkey. The orchestrator must also be able to dial the riskwatcher's
  libp2p address (the orchestrator's `--bootstrap` should include
  `<RW_LISTEN>/p2p/<RW_PEER_ID>`).
- **Each multiply daemon's `--riskwatcher` flag.** Without this flag
  set on a multiply daemon, that daemon silently drops Escalate
  envelopes (logs `Escalate received but no --riskwatcher
  configured` at debug level). With the wrong pubkey set, multiply
  rejects them at warn level (`Escalate REJECTED — sender does not
  match configured riskwatcher pubkey`). Either way the soft-veto
  does not activate.

Each multiply daemon also needs a libp2p route to this riskwatcher.
Critical Escalates fan out to BOTH the orchestrator AND the position
subject; if the multiply's mesh cannot reach the riskwatcher's pubkey,
the Critical pause does not trigger.

The symmetric configuration on each multiply:

```
multiply-daemon ... \
  --riskwatcher <RISKWATCHER_PUBKEY_HEX> \
  --bootstrap   <RISKWATCHER_LISTEN>/p2p/<RISKWATCHER_PEER_ID>
```

Capture the riskwatcher's libp2p `peer_id` from its boot log line
(`0x01 enterprise bootstrap multiaddr: /ip4/.../p2p/12D3KooW...`).

## 6. Co-location: SEPARATE HOST from multiply

A riskwatcher running on the same host as a multiply daemon has
correlated failure with the very thing it is watching. Host crash =
both daemons gone = liquidation goes undetected. Kernel panic, OOM
killer firing on the wrong process, disk failure, network partition,
provider-side incident — every one of these takes out both daemons
at once. The whole point of riskwatcher is independent oversight; if
that independence is not real, the safety guarantee is theatre.

Recommended layout:

- **Multiply daemons:** each on its own bare-metal box or VM,
  separated by region/AZ.
- **Riskwatcher:** one dedicated low-cost host (1 vCPU / 2 GB RAM is
  plenty), in a different region/AZ from any multiply. The daemon's
  steady-state CPU and memory are dominated by libp2p, not RPC.
- **Orchestrator:** a third independent host.

If running in cloud, three different cloud providers OR three
different regions of the same provider is acceptable. Three VMs on
the same hypervisor is NOT acceptable — they share fate on host
hardware and on the provider's control plane.

## 7. 24h watch protocol

What an operator does for the first 24 hours after bringing
riskwatcher up.

### 7.1 Telemetry log

```bash
tail -f /var/log/01fi/riskwatcher-pnl.jsonl
```

Expected shape: one JSONL line per registered position per tick.
Empty file after several minutes means the registry is empty —
multiply has not sent any `ReportMultiply` envelopes yet, OR the
riskwatcher's `--orchestrator` and each multiply's `--riskwatcher`
are not paired correctly. Cross-check the pubkeys.

### 7.2 Prometheus scrape

```bash
curl -s http://127.0.0.1:9091/metrics | grep riskwatcher_escalates_total
```

Expected baseline on a healthy mainnet position: notice / warning /
critical counters all `0`. A typical Kamino position with a 60% LTV
target and 80% liquidation threshold has a distance of ~2500 bps —
well above the Notice band at 500 bps — so 0 escalates is the steady
state. Any nonzero counter on day one warrants investigation.

### 7.3 Dedup verification

If a position oscillates around a band threshold (e.g. distance
flapping between 200 and 220 bps across the Warning boundary), the
dedup logic must fire ONE Warning escalate per `(subject, severity)`
per 60-second window, not one per poll. Verify:

```bash
grep "Escalate emitted severity=Warning" /var/log/01fi/riskwatcher.log \
  | wc -l
```

Over a duration `T` of oscillation, the count should be roughly
`T / 60s`, NOT `T / poll_interval_secs`. If you see two
`Escalate emitted` lines for the same `(subject, severity)` closer
than 60s apart, dedup is broken — file an incident and capture the
log around the duplicates.

### 7.4 Latency check

For a position that does cross a band, time the gap between the
on-chain Kamino state change (visible via Pyth oracle reads) and the
corresponding `severity=...` Escalate emission line. Should be
`< poll_interval_secs + 5s` (one poll plus RPC roundtrip). If it is
much higher, check RPC provider status — the daemon does not yet
expose poll-latency metrics (see §10).

### 7.5 Restart drill

At hour 12 of the 24-hour window, gracefully restart the riskwatcher:

```bash
kill -SIGTERM <pid>
# wait for the process to exit
/usr/local/bin/riskwatcher-daemon &  # or systemd restart
```

Verify:

- The daemon rejoins the mesh within ~30 seconds (look for
  `0x01 enterprise bootstrap multiaddr: ...` and inbound BEACON
  log lines).
- The registry rebuilds from incoming `ReportMultiply` envelopes.
  The registry is not persisted across restarts (known gap, §10);
  the next multiply Report fanout repopulates it.
- No false-positive Escalates fired during the gap.

## 8. Teardown

Graceful shutdown is just `SIGTERM`. The daemon does not need a special
drain — the only async state is short-lived Mutex holds on the
telemetry log, and the libp2p stack handles connection close cleanly.

```bash
kill -SIGTERM <pid>
# Confirm exit:
pgrep -f riskwatcher-daemon || echo "stopped"
```

For a permanent decommission:

```bash
# Shred the role-key seed (irreversible).
sudo shred -u /var/lib/01fi/riskwatcher/riskwatcher-role.key
sudo shred -u /var/lib/01fi/riskwatcher/.runtime-keypair-riskwatcher

# Remove the config dir.
sudo rm -rf /var/lib/01fi/riskwatcher

# Optionally archive then remove the telemetry log.
sudo gzip /var/log/01fi/riskwatcher-pnl.jsonl
sudo mv /var/log/01fi/riskwatcher-pnl.jsonl.gz /path/to/archive/
```

After teardown, every multiply with this riskwatcher's pubkey in its
`--riskwatcher` flag will silently drop into "no riskwatcher
configured" mode and stop respecting the soft-veto. Plan accordingly:
remove the `--riskwatcher` flag from each multiply, OR bring up a
replacement riskwatcher (with the rotation procedure in §9) before
shredding.

## 9. Role-key rotation

Rotate when:

- A compromise is suspected.
- A scheduled rotation interval (e.g. 90 days) elapses.
- The host is being migrated and the seed file moves with it (treat
  this as a rotation rather than an in-place copy).

Procedure:

1. Generate a new key under a temporary name:

   ```bash
   sudo dd if=/dev/urandom \
        of=/var/lib/01fi/riskwatcher/riskwatcher-role.key.new \
        bs=32 count=1 status=none
   sudo chmod 0600 /var/lib/01fi/riskwatcher/riskwatcher-role.key.new
   ```

2. Derive the new verifying key (same Python snippet as §3, but
   reading `riskwatcher-role.key.new`).

3. Update the orchestrator config to accept the NEW pubkey.

4. Update each multiply daemon's `--riskwatcher` flag to the NEW
   pubkey, then restart each multiply.

5. Stop the OLD riskwatcher: `kill -SIGTERM <pid>`.

6. Activate the new key:

   ```bash
   sudo mv /var/lib/01fi/riskwatcher/riskwatcher-role.key.new \
           /var/lib/01fi/riskwatcher/riskwatcher-role.key
   ```

7. Start the riskwatcher under the new identity. Verify mesh
   reconvergence: each multiply's first inbound Escalate from the
   NEW pubkey is accepted; any in-flight envelopes signed by the OLD
   pubkey are rejected at warn level (intended behaviour).

8. Shred the OLD key seed if you kept a backup of it:
   `sudo shred -u <path-to-old-backup>`.

### Ordering matters

There is a brief window during the rotation where:

- The orchestrator is configured for the NEW pubkey.
- One or more multiplies are still configured for the OLD pubkey.

During this window, the OLD daemon (still running) emits Escalates
that multiply still trusts; the orchestrator drops them as
unauthorised, but multiply pauses on Critical. To minimise pauses
caused by stale-key Escalates, sequence the rollout as:

1. Update orchestrator (accepts new pubkey).
2. Update multiplies one at a time (each respects new pubkey).
3. Stop OLD daemon.
4. Start NEW daemon.

If you reverse steps 1 and 2, the orchestrator briefly rejects valid
Escalates from the OLD daemon — survivable but noisy.

## 10. Known gaps for v0

This runbook does NOT yet cover the following, by design:

- **No registry persistence.** The observed-positions registry is
  in-memory only. A restart loses the entries until each multiply's
  next Report fanout repopulates them. Plan restarts during low
  volatility.
- **No escalation-history persistence beyond the JSONL log.** The
  JSONL is append-only; rotation is the operator's responsibility
  (logrotate with copytruncate is recommended; the daemon reopens on
  next write).
- **No metrics for poll latency, RPC error rate, or registry-size
  churn.** The Prometheus endpoint currently exposes only escalate
  counters. Future milestones will fill these in.
- **No `--rotate-key` admin command.** Rotation is the manual
  procedure in §9. Do not assume an in-process rotation flag exists.
- **The synthetic-injection test fixture (`--inject-test-position`)
  MUST NOT be set on a mainnet host.** The flag is hidden from
  `--help` output, but ensure your systemd unit file or process
  supervisor config does NOT inherit a `ZX_INJECT_TEST_POSITION`
  environment variable from a development shell. The synthetic
  short-circuit produces a hard-coded ~10 bps liquidation distance
  that trips Critical on the first poll, which would unconditionally
  pause every multiply that trusts this riskwatcher. **DO NOT USE
  ON MAINNET.**

## Troubleshooting

- **Daemon refuses to boot with "expected 64-char hex string".** The
  `--orchestrator` value is mis-shaped. Check for a stray newline or
  whitespace; the value is validated as exactly 64 lowercase hex
  characters.
- **Daemon refuses to boot on `--telemetry-log`.** The path is not
  writable by the daemon's UID. Verify ownership and parent-dir
  permissions.
- **Daemon refuses to boot on `--metrics-listen`.** Port already in
  use. `ss -ltn | grep 9091` to find the conflict; either change the
  port or stop the conflicting process. Bind failure is intentionally
  fatal (better than running blind to escalation rates).
- **No `riskwatcher-pnl.jsonl` lines after several minutes.** The
  registry is empty. Either no multiply has emitted a Report yet, or
  the riskwatcher's `--orchestrator` and the multiply's
  `--riskwatcher` are paired against different pubkeys. Re-derive
  both pubkeys from the seed files and compare.
- **Critical Escalate emitted but no multiply pause.** The multiply
  cannot reach this riskwatcher's libp2p address, OR multiply was
  not started with `--riskwatcher <RW_PUBKEY>`. Check multiply's log
  for `Escalate received but no --riskwatcher configured` (no flag
  set) or `Escalate REJECTED — sender does not match configured
  riskwatcher pubkey` (wrong pubkey).
- **Genesis-hash mismatch at boot.** The `--rpc-url` is pointing at a
  different network than `--network mainnet`. Reconcile.
