# Hedgents fleet on Oracle Cloud ARM (free tier)

End-to-end guide for running the fleet on Oracle's Always Free Ampere
A1 ARM instance (up to 4 OCPU, 24 GB RAM, 200 GB block storage). The
free tier is large enough to host the entire fleet plus the dashboard
indefinitely.

## 1. Provision the VM

In the Oracle Cloud console:

- **Compute → Instances → Create**
- **Image:** Canonical Ubuntu 24.04 (or Oracle Linux 9)
- **Shape:** `VM.Standard.A1.Flex` — 2 OCPU, 12 GB RAM is plenty (free tier allows up to 4 OCPU / 24 GB total across instances)
- **SSH key:** add yours
- **Boot volume:** 50 GB is fine for the binaries + a year of logs

After it boots, SSH in.

## 2. Open the dashboard port (optional, only if you want browser access from outside the VM)

By default the dashboard binds `127.0.0.1:7700`. To reach it from your
laptop's browser, do **one of**:

- **SSH port-forward (recommended, no public exposure):**
  ```bash
  ssh -L 7700:127.0.0.1:7700 -L 3000:127.0.0.1:3000 ubuntu@<oracle-ip>
  ```
  Then open <http://localhost:7700/daemons> on your laptop.

- **Public exposure (only if you'll add TLS + auth):** Open ports 80/443
  in the Oracle security list, run Caddy in front of 127.0.0.1:7700 with
  Basic Auth. Do not expose the dashboard nakedly.

## 3. One-command install

Once SSH'd into the VM:

```bash
curl -sSL https://github.com/Hedgents/fleet/releases/latest/download/install-hedgents.sh | sudo bash
```

The installer:
- Creates a system user `hedgents`
- Downloads the matching arch tarball, verifies sha256
- Installs binaries to `/opt/hedgents/bin/`
- Installs systemd units to `/etc/systemd/system/`
- Generates role keys + a demo Solana wallet in `/var/lib/hedgents/secrets/`
- Writes config to `/etc/hedgents/hedgents.env`

When it finishes it prints the wallet pubkey and the next steps.

## 4. Configure RPC + (optional) ElevenLabs key

```bash
sudo nano /etc/hedgents/hedgents.env
```

Set:
- `RPC_URL=https://mainnet.helius-rpc.com/?api-key=YOUR_KEY` (recommended; the public Solana RPC is rate-limited)
- `ELEVENLABS_API_KEY=...` (optional, enables hourly voice briefings)

Leave the `*_PUBKEY=...` lines alone — those are derived from the role keys.

## 5. Start the fleet

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now hedgents.target
```

Check status:

```bash
systemctl status hedgents-multiply hedgents-stable-yield hedgents-hedgedjlp \
                 hedgents-riskwatcher hedgents-researcher hedgents-dashboard \
                 hedgents-paper-trade

curl http://127.0.0.1:7700/daemons
curl http://127.0.0.1:7700/paper | python3 -m json.tool
```

After ~60 seconds all five daemons should report green.

## 6. Frontend (optional)

The release tarball does **not** include the Next.js frontend — that's
shipped as a separate `hedgents-frontend-<version>.tar.gz` asset.

To run the dashboard UI on the same VM:

```bash
# Install Node 22
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo bash -
sudo apt-get install -y nodejs

# Fetch the frontend bundle from the same release
VERSION=$(curl -sSL https://github.com/Hedgents/fleet/releases/latest/download/manifest.json | python3 -c "import json,sys; print(json.load(sys.stdin)['version'])")
curl -L "https://github.com/Hedgents/fleet/releases/latest/download/hedgents-frontend-${VERSION}.tar.gz" | sudo tar -xz -C /opt/

# Run it
cd /opt/hedgents-frontend-${VERSION}
PORT=3000 NEXT_PUBLIC_API_BASE=http://localhost:7700 node server.js
```

For persistent operation, write a small systemd unit or run inside `tmux`.

## 7. Maintain the soak

- **Logs:** `/var/lib/hedgents/logs/*.log` (text) and `*.jsonl` (telemetry)
- **SQLite history:** `/var/lib/hedgents/dashboard.sqlite`
- **Restart a single daemon:** `sudo systemctl restart hedgents-multiply`
- **Stop the fleet:** `sudo systemctl stop hedgents.target`
- **View live activity:** `journalctl -fu 'hedgents-*'`

## 8. Update to a new release

```bash
curl -sSL https://github.com/Hedgents/fleet/releases/latest/download/install-hedgents.sh | sudo bash
sudo systemctl restart hedgents.target
```

The installer preserves your `hedgents.env` (so RPC + ElevenLabs key
survive) and your secrets directory.

## 9. Going live (real funds)

The systemd units default to `--simulate-only=true`. To execute real
on-chain transactions:

1. Fund the wallet whose pubkey was printed during install (or replace
   `/var/lib/hedgents/secrets/solana-wallet.json` with your own keypair).
2. Follow `docs/runbooks/stable-yield-mainnet-tiny.md` — do **not** flip
   `--simulate-only=false` for all three execution daemons at once
   without going through the runbook.

For a single-strategy live demo, use the host-side script
`scripts/mainnet-demo-stable-yield.sh` rather than the systemd path.

## Troubleshooting

**Daemon won't start, `systemctl status` shows exit code 101.** Almost
always missing `--i-understand-this-is-mainnet` or a malformed
`RPC_URL`. Check `journalctl -u hedgents-multiply -n 50`.

**multiply is red, others green.** Same Beacon-task issue that affected
local dev — restart multiply: `sudo systemctl restart hedgents-multiply`.
A fix is in progress.

**Oracle reclaims the Always Free instance.** Oracle has historically
reclaimed "idle" ARM instances. The fleet keeps the CPU at ~1% from
RPC polling and Beacon emission, which is usually enough to look
non-idle. If you do get reclaimed, the installer is idempotent on a
fresh box — re-run, your `/var/lib/hedgents/` data is gone but the
binaries reinstall cleanly.

**Free Oracle CPU bursts hit a quota.** Switch the instance shape from
the burstable shape to the dedicated Ampere shape (still within free
tier limits) in the Oracle console.

**ssh tunnel disconnects.** Use `autossh` with `-M 0` and
`ServerAliveInterval 30` in your `~/.ssh/config`.
