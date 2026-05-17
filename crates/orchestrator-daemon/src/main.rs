//! Orchestrator daemon — Phase 1, dry-run only.
//!
//! Joins the 0x01 enterprise mesh as a long-lived `Role::Orchestrator`
//! identity, polls the dashboard REST API every tick, and writes the
//! pure-`decide` recommendation to an append-only JSONL audit log. No
//! envelope emission, no wallet.
//!
//! See `crates/orchestrator-daemon/src/lib.rs` for the phase context and
//! `ROADMAP.md` Phase 1 for the milestone breakdown.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use tracing::{info, warn};

use fleet_pm_stub::allocator_runner::config_from_cli;

use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::secrets::{load_role_identity, FileSource};
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

use orchestrator_daemon::telemetry::AuditLog;
use orchestrator_daemon::tick::{self, TickCtx};

#[derive(Parser, Debug)]
struct Args {
    /// Stable fleet identifier (logged for cross-cutting introspection).
    #[arg(long, env = "ZX_FLEET_ID", default_value = "01fi-dev")]
    fleet_id: String,

    /// Directory holding role secret files. The daemon reads
    /// `orchestrator-role.key` (32 raw bytes) from this directory.
    #[arg(long, env = "ZX_SECRETS_DIR", default_value = "/etc/01fi/secrets")]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr for the embedded node.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/9310")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable).
    #[arg(long, env = "ZX_BOOTSTRAP")]
    bootstrap: Vec<String>,

    /// Beacon emit interval, seconds.
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,

    /// Dashboard REST base URL (the `/strategies` + `/aum` endpoints).
    /// Defaults match the in-tree `fleet-dashboard-server` boot.
    #[arg(long, env = "ZX_DASHBOARD_API_BASE", default_value = "http://127.0.0.1:7700")]
    api_base: String,

    /// How often to re-poll the dashboard and run `decide`, in seconds.
    /// Default 60s — the dashboard's own poll cadence is 5s but the
    /// allocator does not need more granular than per-minute observations.
    #[arg(long, env = "ZX_TICK_INTERVAL_SECS", default_value_t = 60)]
    tick_interval_secs: u64,

    /// Append-only JSONL audit log path. One record per tick.
    #[arg(
        long,
        env = "ZX_AUDIT_LOG",
        default_value = "orchestrator-audit.jsonl"
    )]
    audit_log: PathBuf,

    /// Risk premium (bps) `multiply` must beat `stable_yield` by.
    #[arg(long, env = "ZX_RISK_PREMIUM_MULTIPLY_BPS", default_value_t = 200)]
    risk_premium_multiply_bps: i32,

    /// Risk premium (bps) `hedgedjlp` must beat `stable_yield` by.
    #[arg(long, env = "ZX_RISK_PREMIUM_HEDGEDJLP_BPS", default_value_t = 300)]
    risk_premium_hedgedjlp_bps: i32,

    /// Minimum USD action size — actions smaller than this become NoAction.
    #[arg(long, env = "ZX_MIN_ACTION_USD", default_value_t = 5.0)]
    min_action_usd: f64,

    /// Maximum fraction of total AUM that any single action may move.
    /// Caps blast radius — defaults to 0.5 (half of AUM in one tick).
    #[arg(long, env = "ZX_MAX_ACTION_FRACTION", default_value_t = 0.5)]
    max_action_fraction: f64,

    /// Opt into envelope emission. Defaults `false` (Phase 1 dry-run).
    /// When `true`, requires `--targets-json` and signs/sends
    /// Assign/Withdraw envelopes to the strategy daemons.
    #[arg(long, env = "ZX_EXECUTE", default_value_t = false)]
    execute: bool,

    /// Path to targets.json — required when `--execute` is set. Maps
    /// each strategy to its recipient agent_id (and market/reserve for
    /// stable_yield's Kamino reserve). See fleet-pm-stub's docs for
    /// the schema.
    #[arg(long, env = "ZX_TARGETS_JSON")]
    targets_json: Option<PathBuf>,

    /// Per-strategy cooldown between consecutive dispatches, seconds.
    /// Prevents hot-looping. Default 300 (5 min).
    #[arg(long, env = "ZX_COOLDOWN_SECS", default_value_t = 300)]
    cooldown_secs: u64,

    /// Bounded wait for the recipient peer to become reachable before
    /// sending an envelope. Seconds; clamped to 60.
    #[arg(long, env = "ZX_WAIT_FOR_PEER_SECS", default_value_t = 30)]
    wait_for_peer_secs: u64,

    /// Stale-snapshot guard slack factor. Before emit, the daemon
    /// re-fetches the snapshot and rejects any action whose USD
    /// amount exceeds `idle × slack` (deposit) or
    /// `deployed × slack` (withdraw). Default 1.10 = 10% slack.
    #[arg(long, env = "ZX_STALE_SLACK", default_value_t = 1.10)]
    stale_slack: f64,
}

struct Orchestrator {
    args: Args,
}

#[async_trait]
impl Daemon for Orchestrator {
    fn name(&self) -> &'static str {
        "orchestrator"
    }
    fn signs_transactions(&self) -> bool {
        // The orchestrator NEVER signs Solana transactions itself. In
        // execute mode it signs *mesh envelopes* (Assign/Withdraw)
        // addressed to the strategy daemons, who in turn sign the
        // on-chain transactions. The wallet crate is intentionally not
        // in this binary's dependency graph.
        false
    }

    async fn run(self: Box<Self>) -> Result<()> {
        let mode_label = if self.args.execute {
            "execute"
        } else {
            "dry-run"
        };
        info!(
            fleet = %self.args.fleet_id,
            api_base = %self.args.api_base,
            tick_secs = self.args.tick_interval_secs,
            mode = mode_label,
            "orchestrator starting",
        );

        // Pre-flight: `--execute` requires `--targets-json`. Fail at boot,
        // not on the first action.
        let exec_targets_path = if self.args.execute {
            Some(
                self.args
                    .targets_json
                    .clone()
                    .context("--execute requires --targets-json <path>")?,
            )
        } else {
            None
        };

        let secrets = FileSource::new(&self.args.secrets_dir);
        let role_id = load_role_identity(&secrets, Role::Orchestrator, "orchestrator-role.key")
            .await
            .context("loading orchestrator role key")?;

        let node_config = build_node_config(&self.args, &role_id)?;
        let service = NodeService::build(node_config).await?;
        let handle = service.handle();

        let beacon_interval = Duration::from_secs(self.args.beacon_interval_secs);
        let beacon_handle = handle.clone();
        let beacon_role = role_id.clone();
        let outbound_nonce = Arc::new(AtomicU64::new(1));
        let beacon_nonce = outbound_nonce.clone();

        let audit = Arc::new(AuditLog::open(self.args.audit_log.clone())?);
        info!(path = %audit.path().display(), "audit log open");

        let cfg = config_from_cli(
            self.args.risk_premium_multiply_bps,
            self.args.risk_premium_hedgedjlp_bps,
            self.args.min_action_usd,
            self.args.max_action_fraction,
        );

        // Build the execute-mode pack, if requested.
        let execute = if let Some(targets_path) = exec_targets_path {
            let targets = fleet_pm_stub::allocator_runner::ExecuteTargets::load(&targets_path)
                .context("loading --targets-json")?;
            info!(
                cooldown_secs = self.args.cooldown_secs,
                stale_slack = self.args.stale_slack,
                "execute mode enabled — orchestrator will sign + dispatch envelopes",
            );
            Some(orchestrator_daemon::tick::ExecuteCtx {
                targets,
                handle: handle.clone(),
                role_id: role_id.clone(),
                nonce: outbound_nonce.clone(),
                cooldown_secs: self.args.cooldown_secs,
                wait_for_peer_secs: self.args.wait_for_peer_secs,
                stale_slack: self.args.stale_slack,
            })
        } else {
            None
        };

        let cooldown = Arc::new(tokio::sync::Mutex::new(
            orchestrator_daemon::cooldown::CooldownTracker::new(),
        ));

        let tick_ctx = Arc::new(TickCtx {
            api_base: self.args.api_base.clone(),
            cfg,
            audit: audit.clone(),
            mode: mode_label,
            execute,
            cooldown,
        });
        let tick_interval = Duration::from_secs(self.args.tick_interval_secs);

        tokio::select! {
            r = service.run() => {
                warn!(?r, "node loop exited");
                r
            }
            r = emit_beacons(beacon_handle, beacon_role, beacon_interval, beacon_nonce) => {
                warn!(?r, "beacon emitter exited");
                r
            }
            r = tick::run(tick_ctx, tick_interval) => {
                warn!(?r, "tick loop exited");
                r
            }
        }
    }
}

fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-orchestrator");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["orchestrator".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push(format!("orchestrator-{}", args.fleet_id));
    for boot in &args.bootstrap {
        argv.push("--bootstrap".into());
        argv.push(boot.clone());
    }

    NodeConfig::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
}

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
            Ok(()) => info!(role = %role_id.role().as_str(), nonce = n, "BEACON emitted"),
            Err(e) => warn!(?e, "beacon send failed"),
        }

        tokio::time::sleep(interval).await;
    }
}

fn build_beacon_payload(
    role_id: &RoleIdentity,
    signing_key: &ed25519_dalek::SigningKey,
) -> Vec<u8> {
    let vk = signing_key.verifying_key().to_bytes();
    let name = role_id.role().as_str().as_bytes();
    let mut buf = Vec::with_capacity(32 + 32 + name.len());
    buf.extend_from_slice(&vk);
    buf.extend_from_slice(&vk);
    buf.extend_from_slice(name);
    buf
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let json_mode = std::env::var("RUST_LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false);
    if json_mode {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 2 })?;
    rt.block_on(Box::new(Orchestrator { args }).run())
}
