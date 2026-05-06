//! Risk Watcher daemon — read-only oracle and health monitor.
//! Mandate: emit alerts; never trade. The wallet crate is intentionally
//! not in the dependency graph.
//!
//! This binary embeds a full `zerox1-node-enterprise` `NodeService`
//! instance and joins the 0x01 mesh as a long-lived role identity. It
//! does not run an HTTP server — every interaction is via signed
//! envelopes on the mesh.
//!
//! M3 wired the inbox to a Report observer that maintains an in-memory
//! registry of multiply-desk positions. Future milestones add the Kamino
//! poller (M4), risk classifier (M5), and Escalate emitter (M6).

mod alerts;
mod streams;

use riskwatcher_daemon::escalate::DedupCache;
use riskwatcher_daemon::observer;
use riskwatcher_daemon::poller::{self, PollerCtx};
use riskwatcher_daemon::state::{ObservedPositions, PositionView, Source};
use riskwatcher_daemon::telemetry::{EscalateMetrics, TelemetryLog};

use solana_sdk::pubkey::Pubkey;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, ValueEnum};
use tracing::{info, warn};

use solana_sdk::commitment_config::CommitmentConfig;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_defi_runtime::secrets::{FileSource, load_role_identity};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

#[derive(Parser, Debug)]
struct Args {
    /// Stable fleet identifier (logged for cross-cutting introspection).
    #[arg(long, env = "ZX_FLEET_ID", default_value = "01fi-dev")]
    fleet_id: String,

    /// Directory holding role secret files. The daemon reads
    /// `riskwatcher-role.key` (32 raw bytes) from this directory.
    #[arg(long, env = "ZX_SECRETS_DIR", default_value = "/etc/01fi/secrets")]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr for the embedded node.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/9300")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable). Empty = no peers; the daemon
    /// still listens but only sees beacons from peers that dial it.
    #[arg(long, env = "ZX_BOOTSTRAP")]
    bootstrap: Vec<String>,

    /// Beacon emit interval, seconds.
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,

    /// Solana JSON-RPC endpoint. Consumed in M4 (Kamino obligation poller)
    /// and M6 (escalate emission). Wired now so later milestones don't
    /// churn the CLI surface again.
    #[arg(long, env = "ZX_RPC_URL", default_value = "http://localhost:8899")]
    rpc_url: String,

    /// Solana network the daemon is observing. Consumed in M4 poller and
    /// M6 escalate (selects program IDs / market addresses). Devnet by
    /// default — mainnet must be opted into explicitly.
    #[arg(long, env = "ZX_NETWORK", value_enum, default_value_t = Network::Devnet)]
    network: Network,

    /// Poll interval (seconds) for the Kamino obligation refresh task.
    /// Each tick snapshots the registry and re-queries on-chain LTV for
    /// every tracked subject. Default 30s matches the M4 spec.
    #[arg(long, env = "ZX_POLL_INTERVAL_SECS", default_value_t = 30)]
    poll_interval_secs: u64,

    /// Orchestrator pubkey (32-byte hex) — primary recipient of every
    /// `EscalateRisk` envelope (M6). Required: riskwatcher refuses to
    /// boot without a configured orchestrator. Validated as a 64-char
    /// lowercase hex string at startup; an invalid value bails the
    /// daemon rather than failing silently at the first band breach.
    #[arg(long, env = "ZX_ORCHESTRATOR")]
    orchestrator: String,

    /// M9: append-only JSONL log of per-poll telemetry. One line per
    /// position per tick; default lives in CWD and is gitignored. The
    /// file is created on first write; the daemon refuses to boot if
    /// the path is not openable.
    #[arg(long, env = "ZX_TELEMETRY_LOG", default_value = "riskwatcher-pnl.jsonl")]
    telemetry_log: PathBuf,

    /// M9: bind address for the Prometheus metrics HTTP endpoint
    /// (`GET /metrics`). Loopback by default; the daemon refuses to
    /// boot if the address is already bound.
    #[arg(long, env = "ZX_METRICS_LISTEN", default_value = "127.0.0.1:9091")]
    metrics_listen: String,

    /// **TEST FIXTURE — M8 devnet smoke only.** Pre-populate the
    /// observed-positions registry at boot with one synthetic entry.
    /// Format: `<subject-hex>:<ltv-bps>` where subject-hex is 64 chars
    /// and ltv-bps is 0..=10000. The poller short-circuits the Kamino
    /// fetch for entries with `obligation_pubkey == Pubkey::default()`
    /// AND `last_ltv_bps > 0` (the M3-stub combination), synthesising
    /// a `DecodedObligation` with Critical-band liquidation distance.
    /// NOT for production use — the synthetic distance is hard-coded
    /// to trip the `Critical` classifier (distance < 50 bps).
    #[arg(long, env = "ZX_INJECT_TEST_POSITION", hide = true)]
    inject_test_position: Option<String>,
}

#[derive(ValueEnum, Clone, Debug)]
enum Network {
    Devnet,
    Mainnet,
}

struct RiskWatcher {
    args: Args,
}

#[async_trait]
impl Daemon for RiskWatcher {
    fn name(&self) -> &'static str { "riskwatcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(
            fleet = %self.args.fleet_id,
            rpc_url = %self.args.rpc_url,
            network = ?self.args.network,
            "riskwatcher starting",
        );

        // Load role identity from the secrets backend. File-based for now;
        // production would swap in a Vault-backed SecretSource here.
        let secrets = FileSource::new(&self.args.secrets_dir);
        let role_id = load_role_identity(&secrets, Role::RiskWatcher, "riskwatcher-role.key")
            .await
            .context("loading riskwatcher role key")?;

        // Build the embedded node from synthetic argv, write the role seed
        // to a keypair file the node loop can mmap as identity.
        let node_config = build_node_config(&self.args, &role_id)?;
        let service = NodeService::build(node_config).await?;
        let handle = service.handle();

        let beacon_interval = Duration::from_secs(self.args.beacon_interval_secs);
        let beacon_handle = handle.clone();
        let beacon_role = role_id.clone();

        let inbox_handle = handle.clone();

        // Shared outbound nonce counter. Today only `emit_beacons` claims
        // from it, but M6 (escalate emitter) will share it too — wiring
        // the Arc<AtomicU64> now mirrors the multiply-daemon pattern and
        // avoids churn when escalate lands.
        let outbound_nonce = Arc::new(AtomicU64::new(1));
        let beacon_nonce = outbound_nonce.clone();

        // Shared observed-positions registry. The M3 inbox observer
        // populates it from `ReportMultiply` envelopes; the M4 poller
        // refreshes the same entries from on-chain Kamino state.
        let observed = Arc::new(ObservedPositions::new());

        // M8 test fixture: pre-populate the registry from
        // --inject-test-position so the smoke can deterministically
        // trigger the Critical band without a real Kamino position.
        if let Some(spec) = self.args.inject_test_position.as_deref() {
            let (subject, ltv_bps) = parse_inject_test_position(spec)
                .context("parsing --inject-test-position")?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let view = PositionView {
                subject,
                obligation_pubkey: Pubkey::default(),
                last_ltv_bps: ltv_bps,
                last_seen_unix: now,
                source: Source::Report,
            };
            observed.upsert(view).await;
            warn!(
                subject = %hex::encode(subject),
                ltv_bps,
                "TEST FIXTURE — synthetic position injected; poller will short-circuit Kamino fetch",
            );
        }

        // Shared RPC context for read-only on-chain queries (M4 poller;
        // M6 escalate will reuse it). Constructed once at boot —
        // `Arc<RpcClient>` internally — and cloned into each consumer.
        let rpc = Arc::new(RpcContext::new(
            self.args.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));
        let poll_interval = Duration::from_secs(self.args.poll_interval_secs);

        // Parse the orchestrator pubkey at boot. Fail-fast on invalid
        // hex / wrong length — better than silently breaking the first
        // band-breach emission an hour into a run.
        let orchestrator = parse_orchestrator(&self.args.orchestrator)
            .context("parsing --orchestrator")?;

        // Shared `(subject, severity)` dedup cache for M6 Escalate
        // emission. Owned by the poller — escalate is its only caller.
        let dedup = Arc::new(DedupCache::new());

        // M9: telemetry sinks. The JSONL log opens lazily-on-first-write
        // semantics by way of `OpenOptions::create(true).append(true)`,
        // but we open it eagerly at boot so the operator gets an
        // immediate error if the path is not writable. The metrics
        // counter is shared between the poller (via PollerCtx → emit)
        // and the metrics HTTP task.
        let telemetry = Arc::new(
            TelemetryLog::open(self.args.telemetry_log.clone())
                .context("opening --telemetry-log")?,
        );
        info!(path = %telemetry.path().display(), "telemetry log open");
        let metrics = Arc::new(EscalateMetrics::new());

        let poller_ctx = Arc::new(PollerCtx {
            rpc: rpc.clone(),
            state: observed.clone(),
            handle: handle.clone(),
            role: role_id.clone(),
            nonce: outbound_nonce.clone(),
            dedup,
            orchestrator,
            telemetry: Some(telemetry.clone()),
            metrics: metrics.clone(),
        });

        let metrics_listen = self.args.metrics_listen.clone();
        let metrics_for_endpoint = metrics.clone();

        tokio::select! {
            r = service.run() => {
                warn!(?r, "node loop exited");
                r
            }
            r = emit_beacons(beacon_handle, beacon_role, beacon_interval, beacon_nonce) => {
                warn!(?r, "beacon emitter exited");
                r
            }
            r = observer::run(inbox_handle, observed.clone()) => {
                warn!(?r, "inbox observer exited");
                r
            }
            r = poller::run(poller_ctx, poll_interval) => {
                warn!(?r, "kamino poller exited");
                r
            }
            r = streams::run() => {
                warn!(?r, "streams loop exited");
                r
            }
            r = run_metrics_endpoint(metrics_for_endpoint, metrics_listen) => {
                warn!(?r, "metrics endpoint exited");
                r
            }
        }
    }
}

/// M9: tiny axum server exposing `GET /metrics` in Prometheus text format.
///
/// Loopback by default (`127.0.0.1:9091`). No middleware, no auth — this
/// endpoint is intentionally a single route on a private interface so
/// dashboards/alerting can scrape it without daemon-internal complexity.
/// Bind failure (e.g. port already in use) is fatal to the daemon: we'd
/// rather refuse to start than run blind to escalation rates.
async fn run_metrics_endpoint(
    metrics: Arc<EscalateMetrics>,
    listen: String,
) -> Result<()> {
    use axum::{response::IntoResponse, routing::get, Router};

    let m = metrics.clone();
    let app = Router::new().route(
        "/metrics",
        get(move || {
            let m = m.clone();
            async move { m.render_prometheus().into_response() }
        }),
    );

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind metrics endpoint on {listen}"))?;
    info!(%listen, "metrics endpoint listening on /metrics");
    axum::serve(listener, app)
        .await
        .context("metrics endpoint serve")?;
    Ok(())
}

/// Translate the daemon's `Args` + role seed into a `NodeConfig`.
///
/// We use `NodeConfig::try_parse_from(synthetic_argv)` so we get the same
/// defaulting behavior as the standalone `zerox1-node-enterprise` binary,
/// without consuming the daemon's own CLI args. The role seed is written
/// to `<secrets_dir>/.runtime-keypair-riskwatcher` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-riskwatcher");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["riskwatcher".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push(format!("riskwatcher-{}", args.fleet_id));
    for boot in &args.bootstrap {
        argv.push("--bootstrap".into());
        argv.push(boot.clone());
    }

    NodeConfig::try_parse_from(&argv)
        .map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
}

/// Write a 32-byte Ed25519 seed to `path` in the raw format expected by
/// `AgentIdentity::load_or_generate` (which calls `std::fs::read` and
/// expects exactly 32 bytes).
fn write_keypair(path: &Path, seed: &[u8; 32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(seed)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, seed)?;
    }
    Ok(())
}

/// Emit a Beacon envelope onto the mesh every `interval`.
///
/// Beacon payload convention: `[agent_id(32)][verifying_key(32)][name(utf-8)]`.
/// For now `agent_id == verifying_key` (no on-chain registration in the
/// enterprise mesh — see `node-enterprise/src/identity.rs`).
async fn emit_beacons(
    handle: NodeHandle,
    role_id: RoleIdentity,
    interval: Duration,
    nonce: Arc<AtomicU64>,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();

    loop {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let payload = build_beacon_payload(&role_id, &signing_key);

        // Claim the next nonce from the shared counter.
        let n = nonce.fetch_add(1, Ordering::Relaxed);

        let env = Envelope::build(
            MsgType::Beacon,
            sender,
            BROADCAST_RECIPIENT,
            now_secs,
            n,
            [0u8; 16],
            payload,
            &signing_key,
        );

        match handle.send(env).await {
            Ok(()) => info!(role = %role_id.role().as_str(), nonce = n, "beacon emitted"),
            Err(e) => warn!(?e, "beacon send failed"),
        }

        tokio::time::sleep(interval).await;
    }
}

/// Decode the `--orchestrator` CLI value into a 32-byte agent id.
///
/// Accepts a 64-char hex string (case-insensitive). Bails with a clear
/// error on wrong length or non-hex bytes. Pre-checking the length
/// keeps the error message specific to a stripped/typo'd input — the
/// generic `hex::decode` error is "Invalid string length" which doesn't
/// tell the operator they wrote 63 chars instead of 64.
/// Parse `--inject-test-position <subject-hex>:<ltv-bps>` into a
/// `(subject, ltv_bps)` tuple. Both fields validated:
///   - subject-hex: exactly 64 lowercase hex chars (32 bytes)
///   - ltv-bps:     0..=10000
///
/// Bails with a specific error on each malformed-input shape so an
/// operator running the M8 smoke gets clear feedback.
fn parse_inject_test_position(s: &str) -> Result<([u8; 32], u16)> {
    let (subject_str, ltv_str) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected '<subject-hex>:<ltv-bps>', got '{s}'"))?;
    if subject_str.len() != 64 {
        anyhow::bail!(
            "subject must be 64 hex chars (got {}). Pass the multiply daemon's 32-byte agent_id.",
            subject_str.len()
        );
    }
    let bytes = hex::decode(subject_str).context("decoding subject hex")?;
    let subject: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("decoded {} bytes, expected 32", v.len()))?;
    let ltv: u16 = ltv_str
        .parse()
        .with_context(|| format!("parsing ltv-bps '{ltv_str}'"))?;
    if ltv > 10_000 {
        anyhow::bail!("ltv-bps must be 0..=10000, got {ltv}");
    }
    if ltv == 0 {
        anyhow::bail!("ltv-bps must be > 0; the synthetic short-circuit triggers on last_ltv_bps > 0");
    }
    Ok((subject, ltv))
}

fn parse_orchestrator(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        anyhow::bail!(
            "expected 64-char hex string (got {} chars). Pass the orchestrator's 32-byte agent_id as hex.",
            s.len()
        );
    }
    let bytes = hex::decode(s).context("decoding hex")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("decoded {} bytes, expected 32", v.len()))?;
    Ok(arr)
}

fn build_beacon_payload(
    role_id: &RoleIdentity,
    signing_key: &ed25519_dalek::SigningKey,
) -> Vec<u8> {
    let vk = signing_key.verifying_key().to_bytes();
    let name = role_id.role().as_str().as_bytes();
    let mut buf = Vec::with_capacity(32 + 32 + name.len());
    buf.extend_from_slice(&vk);          // agent_id (= verifying_key in enterprise mode)
    buf.extend_from_slice(&vk);          // verifying_key
    buf.extend_from_slice(name);         // display name
    buf
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 4 })?;
    rt.block_on(Box::new(RiskWatcher { args }).run())
}
