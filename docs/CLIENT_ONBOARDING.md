# Hedgents Fleet — self-host onboarding

Guided onboarding for a client running the fleet on their own hardware,
their own keys. First pilot: CaveBroDAO. This doc is the reusable runbook;
no client secrets live here.

The fleet is an autonomous, role-based agent system that manages a USDC
treasury across vetted Solana yield strategies (stable-yield, hedged-JLP,
ONyc). It runs entirely on the client's box. Hedgents never holds the
client's keys and operates no server in the path.

**Read this first — safety posture.** The fleet signs transactions and
moves real USDC. Do NOT start on mainnet with size. The supported path is
staged: (1) devnet smoke to prove it runs, (2) a mainnet-tiny pilot with
conservative caps, (3) scale only after the client is comfortable. A
third-party audit is in progress; until it lands, keep mainnet exposure
small and treat this as a supervised pilot.

---

## 0. Prerequisites (client provides)

- **A Linux box they control** — x86_64 or arm64, ~2+ cores, ~4GB RAM,
  ~20GB disk. A VPS or an on-prem server. Root/sudo.
- **A Solana RPC endpoint** — a Helius (or similar) mainnet key strongly
  recommended; the public RPC is rate-limited and will make the fleet slow.
- **USDC** for the treasury + a **small SOL gas buffer** (keep 0.05–0.15
  SOL idle for transaction fees; top up when low).
- Outbound HTTPS (Solana RPC, price feeds) and UDP for the libp2p mesh
  between role daemons (loopback by default for a single box).

## 1. Custody model (read before installing)

- The trading wallet keypair is generated **on the client's box** and
  never leaves it. The client owns it. Hedgents cannot move funds.
- The client can **withdraw or stop at any time** (Section 7). This is the
  escape hatch and it is always theirs.
- Safety comes from: client self-custody + on-chain/config **caps** that
  bound every action + role separation (the executor is the only signer;
  the risk role cannot trade). Set caps conservatively for the pilot.

## 2. Install

On the box, as root, pin a specific release (don't use `latest` for a
client, pin the audited/known-good tag):

```
curl -sSL https://github.com/Hedgents/fleet/releases/download/<TAG>/install-hedgents.sh \
  | sudo TAG=<TAG> RPC_URL="https://mainnet.helius-rpc.com/?api-key=<KEY>" bash
```

This creates the `hedgents` system user, installs binaries to
`/opt/hedgents/bin`, systemd units in `/etc/systemd/system`, config in
`/etc/hedgents/hedgents.env`, data + secrets in `/var/lib/hedgents`. It
does NOT auto-start live trading (safety).

## 3. Generate keys (on the client's box)

- **Trading wallet**: generate into `/var/lib/hedgents/secrets/solana-wallet.json`
  (0600, owned by `hedgents`). The client keeps the seed. This is the only
  account that holds funds.
- **Role identities**: the role daemons each get their own libp2p
  identity/keypair under `/var/lib/hedgents/secrets/` (the installer/setup
  provisions these). The executor role is the designated signer.
- Print the wallet pubkey and give it to the client — that's the address
  they fund and can audit on-chain.

## 4. Configure `/etc/hedgents/hedgents.env`

Set, at minimum:
- `RPC_URL` — the client's Helius endpoint.
- Which **strategies** are enabled (start with stable-yield only for the
  pilot; add hedged-JLP / ONyc once trust is established).
- **Caps**: per-action max, 24h cumulative max, target allocations, risk
  thresholds. Start conservative (e.g. small per-action cap, low total
  allocation) — these bound what any agent can do.

## 5. Stage 1 — devnet smoke (no real money)

Prove the fleet runs end-to-end on the client's box before any mainnet
funds. Follow `docs/runbooks/orchestrator-devnet-smoke.md` (+ the per-role
devnet runbooks). Expect: daemons start, form the mesh, the orchestrator
assigns, a strategy executes round-trip on devnet, the dashboard renders.
Do not proceed until this is clean.

## 6. Stage 2 — mainnet-tiny pilot

Switch to mainnet, fund a **small** USDC amount + SOL gas. Enable one
strategy with conservative caps. Follow `docs/runbooks/stable-yield-mainnet-tiny.md`
(and the hedged-JLP / ONyc mainnet-tiny runbooks when adding those).
Start the fleet:

```
sudo systemctl daemon-reload
sudo systemctl enable --now hedgents.target
```

Watch a full cycle (deposit → allocate → earn → and a test withdraw) at
tiny size. Confirm caps hold and nothing exceeds them. Only after this is
comfortable do you raise size or add strategies.

## 7. Monitoring + escape hatch

- **Dashboard**: the dashboard service renders AUM, positions, APR, and
  the role activity on a local port. The client watches it live.
- **Logs**: `/var/lib/hedgents/logs/<daemon>.log` (structured JSON). Note:
  the fleet writes here directly, not journald.
- **Stop**: `sudo systemctl stop hedgents.target` halts all daemons. No
  agent can act when stopped.
- **Withdraw**: the client controls the wallet keypair and can withdraw
  USDC from the strategies / wallet at any time, independent of the fleet.
- **Gas**: keep 0.05–0.15 SOL idle; a fleet with no SOL cannot rebalance
  or unwind. Top up when low.

## 8. What Hedgents provides (the subscription)

- Install + configuration support, the staged bring-up above.
- Release updates (pin + install a new tag, restart the affected unit).
- Incident support (e.g. a stuck position, an RPC outage, a gas top-up
  reminder).
- Risk/cap review. The client always retains custody and the stop/withdraw
  controls.

## 9. Risk disclosure (state plainly to the client)

- DeFi strategies can lose money; yields are variable and not guaranteed.
- The software signs transactions; a bug or a market/RPC event can cause
  loss. Caps bound but do not eliminate this.
- Pre-audit, keep mainnet size small. This is a supervised pilot, not a
  finished, audited custody product.
- Self-custody means the client is responsible for their wallet seed and
  box security.

---

## Appendix A — concrete commands + config

**Key generation (on the client's box).** The installer auto-generates the
six role keypairs and derives their pubkeys into the env. The ONLY key you
generate by hand is the trading wallet:

```
sudo -u hedgents solana-keygen new --no-bip39-passphrase \
  -o /var/lib/hedgents/secrets/solana-wallet.json
sudo chmod 600 /var/lib/hedgents/secrets/solana-wallet.json
# re-run the installer (or its key step) so SOLANA_WALLET_PUBKEY + role
# pubkeys land in /etc/hedgents/hedgents.env, then:
sudo -u hedgents solana-keygen pubkey /var/lib/hedgents/secrets/solana-wallet.json
# ^ give this address to the client. It's what they fund and can audit.
```

**The env (`/etc/hedgents/hedgents.env`).** The installer writes everything
except `RPC_URL`; the pubkeys are auto-derived, do not hand-edit them. The
client only sets the RPC:

```
RPC_URL=https://mainnet.helius-rpc.com/?api-key=<CAVEBRO_HELIUS_KEY>
# --- auto-derived by the installer, do not edit ---
ORCHESTRATOR_PUBKEY=…   STABLE_YIELD_PUBKEY=…   HEDGEDJLP_PUBKEY=…
ONYC_PUBKEY=…   RISKWATCHER_PUBKEY=…   SOLANA_WALLET_PUBKEY=…
```

**Strategy selection = which units you enable.** Start the pilot with
stable-yield only; add the others later. Don't `enable --now hedgents.target`
(which starts everything) for the pilot — enable units explicitly:

```
sudo systemctl enable --now hedgents-researcher hedgents-riskwatcher \
  hedgents-orchestrator hedgents-stable-yield-live
# add later: hedgents-hedgedjlp-live, hedgents-onyc-live
```

**Caps** are enforced per-strategy on the daemons (e.g. single-action and
24h cumulative limits, cooldowns). Set them conservatively for the pilot
per the matching `docs/runbooks/<strategy>-mainnet-tiny.md`; verify the
exact flags there before going live.

## Pre-flight checklist (per client)

- [ ] Box provisioned, arch confirmed (x64/arm64), root access
- [ ] Helius RPC key in `RPC_URL`
- [ ] Release `<TAG>` pinned and installed
- [ ] Trading wallet generated on the box; client holds the seed; pubkey shared
- [ ] Role identities provisioned; executor is the only signer
- [ ] Caps set conservatively; strategies limited (stable-yield first)
- [ ] Stage 1 devnet smoke: clean
- [ ] Stage 2 mainnet-tiny: funded small, full cycle + test withdraw verified
- [ ] Dashboard reachable; logs flowing
- [ ] Client knows: stop = `systemctl stop hedgents.target`, withdraw = their keys
- [ ] Gas buffer (0.05–0.15 SOL) in place
- [ ] Risk disclosure acknowledged
