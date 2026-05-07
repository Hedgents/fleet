//! Researcher daemon — read-only batch worker. No keys, no streams.
//! Pulls jobs from the mesh, produces artefacts, exits when idle.
//!
//! This binary embeds a full `zerox1-node-enterprise` `NodeService`
//! instance and joins the 0x01 mesh as a long-lived role identity. It
//! does not run an HTTP server — every interaction is via signed
//! envelopes on the mesh.
//!
//! Mandate: read-only. The wallet crate is intentionally not in the
//! dependency graph (authority isolation invariant).

mod jobs;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use solana_sdk::commitment_config::CommitmentConfig;
use tracing::{info, warn};

use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_defi_runtime::secrets::{FileSource, load_role_identity};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

use researcher_daemon::dedup::EmissionTracker;
use researcher_daemon::telemetry::{self, TelemetryHandle, TelemetryTally};
use researcher_daemon::watchers;

#[derive(Parser, Debug)]
struct Args {
    /// Stable fleet identifier (logged for cross-cutting introspection).
    #[arg(long, env = "ZX_FLEET_ID", default_value = "01fi-dev")]
    fleet_id: String,

    /// Directory holding role secret files. The daemon reads
    /// `researcher-role.key` (32 raw bytes) from this directory.
    #[arg(long, env = "ZX_SECRETS_DIR", default_value = "/etc/01fi/secrets")]
    secrets_dir: PathBuf,

    /// Output directory for batch artefacts.
    #[arg(long, env = "ZX_ARTEFACTS", default_value = "./researcher-artefacts")]
    artefacts: PathBuf,

    /// Number of batch workers (defaults to host parallelism).
    #[arg(long, env = "ZX_WORKERS", default_value_t = num_cpus())]
    workers: usize,

    /// libp2p listen multiaddr for the embedded node.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/9304")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable). Empty = no peers; the daemon
    /// still listens but only sees beacons from peers that dial it.
    #[arg(long, env = "ZX_BOOTSTRAP")]
    bootstrap: Vec<String>,

    /// Beacon emit interval, seconds.
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,

    /// Solana RPC URL — used by chain-reading watchers (lending_rate
    /// watcher polls Kamino reserves here).
    #[arg(long, env = "ZX_RPC_URL", default_value = "https://api.devnet.solana.com")]
    rpc_url: String,

    /// Network: "devnet" or "mainnet". Mainnet additionally requires
    /// --i-understand-this-is-mainnet. Cross-validated against the RPC
    /// URL via getGenesisHash at boot (audit-fix I1).
    #[arg(long, env = "ZX_NETWORK", default_value = "devnet")]
    network: String,

    /// Required redundant acknowledgment when --network mainnet. No default.
    #[arg(long)]
    i_understand_this_is_mainnet: bool,

    /// Lending watcher tick interval, seconds.
    #[arg(long, default_value_t = 60)]
    lending_poll_interval_secs: u64,

    /// Reserves to watch. Format: `name:base58_pubkey:asset_enum`. Repeat
    /// for multiple. Example: `usdc:DGQRoyx...:USDC`. Empty = lending
    /// watcher disabled.
    #[arg(long)]
    lending_reserve: Vec<String>,

    /// Perp funding watcher tick interval, seconds.
    #[arg(long, default_value_t = 60)]
    funding_poll_interval_secs: u64,

    /// Drift perp markets to watch. Format: `name:base58_pubkey:asset_enum`.
    /// Example: `sol-perp:8UJgxaiQx5nTrdDgph5FiahMmzduuLTLf5WmsPegYA6W:SOL`.
    /// Empty = funding watcher disabled.
    #[arg(long)]
    funding_market: Vec<String>,

    /// Pyth price watcher tick interval, seconds.
    #[arg(long, default_value_t = 60)]
    price_poll_interval_secs: u64,

    /// Pyth price feeds to watch. Format: `name:base58_pubkey:asset_enum`.
    /// Mainnet sponsored SOL/USD: `7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE`.
    /// Empty = price watcher disabled.
    #[arg(long)]
    price_feed: Vec<String>,

    /// Stable-peg watcher tick interval, seconds.
    #[arg(long, default_value_t = 60)]
    peg_poll_interval_secs: u64,

    /// Stablecoin Pyth feeds. Format: `name:base58_pubkey:asset_enum`
    /// where asset_enum is USDC or USDT only. Example:
    /// `usdc:Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD:USDC`.
    /// Empty = peg watcher disabled.
    #[arg(long)]
    peg_feed: Vec<String>,

    /// JLP yield + composition watcher tick interval, seconds. Default
    /// 300 (5 min) — JLP yield doesn't change tick-to-tick, longer
    /// interval reduces RPC load.
    #[arg(long, default_value_t = 300)]
    jlp_poll_interval_secs: u64,

    /// Jupiter Perps pool pubkey (base58). Single pool per fleet.
    /// Mainnet JLP pool: `5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq`.
    /// None = JLP watcher disabled.
    #[arg(long)]
    jlp_pool: Option<String>,

    /// Bags.fm program ID for token-activity log subscription. Default
    /// empty = watcher disabled. v0 ships as a stub: the loop is wired
    /// but the WS `logs_subscribe` + Bags.fm log decoder is deferred.
    #[arg(long)]
    bags_program_id: Option<String>,

    /// Token-activity watcher tick interval, seconds.
    #[arg(long, default_value_t = 30)]
    token_activity_tick_secs: u64,

    /// Initial subscriber list — recipients of MarketSignal envelopes.
    /// Hex-encoded role pubkeys (32 bytes = 64 hex chars). Repeat for
    /// multiple. v0: must be passed explicitly. Future: auto-discover via
    /// BEACON.
    #[arg(long)]
    subscriber: Vec<String>,

    /// Path to the JSONL telemetry log — every emitted MarketSignal
    /// appends one line here. Default `researcher-signals.jsonl` is
    /// gitignored.
    #[arg(long, default_value = "researcher-signals.jsonl")]
    telemetry_log: PathBuf,

    /// Tally summary interval, seconds (default 3600 = 1 hour). The
    /// running count of Info / Notice / Important emissions is logged
    /// at INFO level on each tick, then reset.
    #[arg(long, default_value_t = 3600)]
    tally_interval_secs: u64,
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

struct Researcher {
    args: Args,
    role_identity: RoleIdentity,
}

#[async_trait]
impl Daemon for Researcher {
    fn name(&self) -> &'static str { "researcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        std::fs::create_dir_all(&self.args.artefacts)
            .with_context(|| format!("creating artefacts dir {}", self.args.artefacts.display()))?;
        info!(
            fleet = %self.args.fleet_id,
            workers = self.args.workers,
            artefacts = %self.args.artefacts.display(),
            "researcher starting",
        );

        // Build the embedded node from synthetic argv, write the role seed
        // to a keypair file the node loop can mmap as identity.
        let node_config = build_node_config(&self.args, &self.role_identity)?;
        let service = NodeService::build(node_config).await?;
        let handle = service.handle();

        // Shared outbound nonce for ALL outbound envelopes (BEACONs +
        // MarketSignals). Watchers monotonically increment this so the
        // mesh sees a single consistent stream from the role identity.
        let outbound_nonce = Arc::new(AtomicU64::new(1));

        // Shared dedup tracker — future watchers (M4+) plug in here.
        let dedup = Arc::new(EmissionTracker::default());

        // Shared telemetry handle (M9): every signal emission appends a
        // JSONL line + bumps a per-severity tally. The tally is drained
        // and logged at INFO every `tally_interval_secs`.
        let tally = TelemetryTally::new();
        let telemetry_handle = TelemetryHandle::new(
            self.args.telemetry_log.clone(),
            tally.clone(),
        );
        info!(
            telemetry_log = %self.args.telemetry_log.display(),
            tally_interval_secs = self.args.tally_interval_secs,
            "researcher telemetry initialized"
        );

        // Parse lending reserve specs + subscriber pubkeys from CLI.
        let reserves = parse_reserves(&self.args.lending_reserve)?;
        let perp_markets = parse_perp_markets(&self.args.funding_market)?;
        let price_feeds = parse_price_feeds(&self.args.price_feed)?;
        let peg_feeds = parse_peg_feeds(&self.args.peg_feed)?;
        let jlp_pool_pubkey = parse_jlp_pool(self.args.jlp_pool.as_deref())?;
        let bags_program_pubkey = parse_bags_program(self.args.bags_program_id.as_deref())?;
        let subscribers_vec = parse_subscribers(&self.args.subscriber)?;
        let subscribers = Arc::new(tokio::sync::RwLock::new(subscribers_vec));

        // RpcContext for chain-reading watchers.
        let rpc = Arc::new(RpcContext::new(
            self.args.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        let beacon_interval = Duration::from_secs(self.args.beacon_interval_secs);
        let beacon_handle = handle.clone();
        let beacon_role = self.role_identity.clone();
        let beacon_nonce = outbound_nonce.clone();

        let inbox_handle = handle.clone();

        // Watcher: lending rate. Disabled when no reserves passed.
        let lending_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if reserves.is_empty() {
                info!("lending_rate watcher disabled (no --lending-reserve)");
                Box::pin(std::future::pending())
            } else {
                let lending_rpc = rpc.clone();
                let lending_handle = handle.clone();
                let lending_role = self.role_identity.clone();
                let lending_nonce = outbound_nonce.clone();
                let lending_dedup = dedup.clone();
                let lending_subs = subscribers.clone();
                let lending_telemetry = Some(telemetry_handle.clone());
                let lending_interval =
                    Duration::from_secs(self.args.lending_poll_interval_secs);
                Box::pin(async move {
                    watchers::lending_rate::run(
                        lending_rpc,
                        lending_handle,
                        lending_role,
                        lending_nonce,
                        lending_dedup,
                        reserves,
                        lending_subs,
                        lending_telemetry,
                        lending_interval,
                    )
                    .await
                })
            };

        // Watcher: perp funding. Disabled when no markets passed.
        let funding_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if perp_markets.is_empty() {
                info!("perp_funding watcher disabled (no --funding-market)");
                Box::pin(std::future::pending())
            } else {
                let funding_rpc = rpc.clone();
                let funding_handle = handle.clone();
                let funding_role = self.role_identity.clone();
                let funding_nonce = outbound_nonce.clone();
                let funding_dedup = dedup.clone();
                let funding_subs = subscribers.clone();
                let funding_telemetry = Some(telemetry_handle.clone());
                let funding_interval =
                    Duration::from_secs(self.args.funding_poll_interval_secs);
                Box::pin(async move {
                    watchers::perp_funding::run(
                        funding_rpc,
                        funding_handle,
                        funding_role,
                        funding_nonce,
                        funding_dedup,
                        perp_markets,
                        funding_subs,
                        funding_telemetry,
                        funding_interval,
                    )
                    .await
                })
            };

        // Watcher: price (Pyth). Disabled when no feeds passed.
        let price_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if price_feeds.is_empty() {
                info!("price watcher disabled (no --price-feed)");
                Box::pin(std::future::pending())
            } else {
                let price_rpc = rpc.clone();
                let price_handle = handle.clone();
                let price_role = self.role_identity.clone();
                let price_nonce = outbound_nonce.clone();
                let price_dedup = dedup.clone();
                let price_subs = subscribers.clone();
                let price_telemetry = Some(telemetry_handle.clone());
                let price_interval =
                    Duration::from_secs(self.args.price_poll_interval_secs);
                Box::pin(async move {
                    watchers::price::run(
                        price_rpc,
                        price_handle,
                        price_role,
                        price_nonce,
                        price_dedup,
                        price_feeds,
                        price_subs,
                        price_telemetry,
                        price_interval,
                    )
                    .await
                })
            };

        // Watcher: JLP yield + composition. Disabled when no pool passed.
        let jlp_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if let Some(pool) = jlp_pool_pubkey {
                let jlp_rpc = rpc.clone();
                let jlp_handle = handle.clone();
                let jlp_role = self.role_identity.clone();
                let jlp_nonce = outbound_nonce.clone();
                let jlp_dedup = dedup.clone();
                let jlp_subs = subscribers.clone();
                let jlp_telemetry = Some(telemetry_handle.clone());
                let jlp_interval = Duration::from_secs(self.args.jlp_poll_interval_secs);
                Box::pin(async move {
                    watchers::jlp_yield::run(
                        jlp_rpc,
                        jlp_handle,
                        jlp_role,
                        jlp_nonce,
                        jlp_dedup,
                        pool,
                        jlp_subs,
                        jlp_telemetry,
                        jlp_interval,
                    )
                    .await
                })
            } else {
                info!("jlp_yield watcher disabled (no --jlp-pool)");
                Box::pin(std::future::pending())
            };

        // Watcher: stable peg (Pyth USDC/USDT). Disabled when no feeds passed.
        let peg_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if peg_feeds.is_empty() {
                info!("stable_peg watcher disabled (no --peg-feed)");
                Box::pin(std::future::pending())
            } else {
                let peg_rpc = rpc.clone();
                let peg_handle = handle.clone();
                let peg_role = self.role_identity.clone();
                let peg_nonce = outbound_nonce.clone();
                let peg_dedup = dedup.clone();
                let peg_subs = subscribers.clone();
                let peg_telemetry = Some(telemetry_handle.clone());
                let peg_interval =
                    Duration::from_secs(self.args.peg_poll_interval_secs);
                Box::pin(async move {
                    watchers::stable_peg::run(
                        peg_rpc,
                        peg_handle,
                        peg_role,
                        peg_nonce,
                        peg_dedup,
                        peg_feeds,
                        peg_subs,
                        peg_telemetry,
                        peg_interval,
                    )
                    .await
                })
            };

        // Watcher: token activity (Bags.fm). Always spawned — the inner
        // run() short-circuits to Ok(()) when no program-id is provided.
        let token_activity_fut: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<()>> + Send>,
        > = {
            let ta_rpc = rpc.clone();
            let ta_handle = handle.clone();
            let ta_role = self.role_identity.clone();
            let ta_nonce = outbound_nonce.clone();
            let ta_dedup = dedup.clone();
            let ta_subs = subscribers.clone();
            let ta_telemetry = Some(telemetry_handle.clone());
            let ta_interval =
                Duration::from_secs(self.args.token_activity_tick_secs);
            let ta_program = bags_program_pubkey;
            Box::pin(async move {
                watchers::token_activity::run(
                    ta_rpc,
                    ta_handle,
                    ta_role,
                    ta_nonce,
                    ta_dedup,
                    ta_program,
                    ta_subs,
                    ta_telemetry,
                    ta_interval,
                )
                .await
            })
        };

        // Spawn the tally loop: every `tally_interval_secs` it drains the
        // running counts and logs at INFO. The future is `()` so we wrap
        // with `Ok` for the select! result type.
        let tally_for_loop = tally.clone();
        let tally_interval = self.args.tally_interval_secs;
        let tally_fut = async move {
            telemetry::run_tally_loop(tally_for_loop, tally_interval).await;
            Ok::<(), anyhow::Error>(())
        };

        tokio::select! {
            r = service.run() => {
                warn!(?r, "node loop exited");
                r
            }
            r = emit_beacons(beacon_handle, beacon_role, beacon_interval, beacon_nonce) => {
                warn!(?r, "beacon emitter exited");
                r
            }
            r = tally_fut => {
                warn!(?r, "telemetry tally loop exited");
                r
            }
            r = handle_inbox(inbox_handle) => {
                warn!(?r, "inbox dispatcher exited");
                r
            }
            r = jobs::run() => {
                warn!(?r, "jobs loop exited");
                r
            }
            r = lending_fut => {
                warn!(?r, "lending watcher exited");
                r
            }
            r = funding_fut => {
                warn!(?r, "perp_funding watcher exited");
                r
            }
            r = price_fut => {
                warn!(?r, "price watcher exited");
                r
            }
            r = peg_fut => {
                warn!(?r, "stable_peg watcher exited");
                r
            }
            r = jlp_fut => {
                warn!(?r, "jlp_yield watcher exited");
                r
            }
            r = token_activity_fut => {
                warn!(?r, "token_activity watcher exited");
                r
            }
        }
    }
}

/// Parse `--lending-reserve` strings into `ReserveSpec` values.
fn parse_reserves(specs: &[String]) -> Result<Vec<watchers::lending_rate::ReserveSpec>> {
    specs
        .iter()
        .map(|s| watchers::lending_rate::parse_reserve_spec(s))
        .collect()
}

/// Parse `--funding-market` strings into `PerpMarketSpec` values.
fn parse_perp_markets(specs: &[String]) -> Result<Vec<watchers::perp_funding::PerpMarketSpec>> {
    specs
        .iter()
        .map(|s| watchers::perp_funding::parse_market_spec(s))
        .collect()
}

/// Parse `--price-feed` strings into `PriceFeedSpec` values.
fn parse_price_feeds(specs: &[String]) -> Result<Vec<watchers::price::PriceFeedSpec>> {
    specs
        .iter()
        .map(|s| watchers::price::parse_feed_spec(s))
        .collect()
}

/// Parse `--peg-feed` strings into `StableFeedSpec` values.
fn parse_peg_feeds(specs: &[String]) -> Result<Vec<watchers::stable_peg::StableFeedSpec>> {
    specs
        .iter()
        .map(|s| watchers::stable_peg::parse_feed_spec(s))
        .collect()
}

/// Parse `--jlp-pool` (optional) into a Solana pubkey.
fn parse_jlp_pool(s: Option<&str>) -> Result<Option<solana_sdk::pubkey::Pubkey>> {
    match s {
        None => Ok(None),
        Some(raw) => {
            let pk: solana_sdk::pubkey::Pubkey = raw
                .parse()
                .with_context(|| format!("parsing --jlp-pool {raw:?}"))?;
            Ok(Some(pk))
        }
    }
}

/// Parse `--bags-program-id` (optional) into a Solana pubkey.
fn parse_bags_program(s: Option<&str>) -> Result<Option<solana_sdk::pubkey::Pubkey>> {
    match s {
        None => Ok(None),
        Some(raw) => {
            let pk: solana_sdk::pubkey::Pubkey = raw
                .parse()
                .with_context(|| format!("parsing --bags-program-id {raw:?}"))?;
            Ok(Some(pk))
        }
    }
}

/// Parse `--subscriber` hex strings into 32-byte pubkeys.
fn parse_subscribers(items: &[String]) -> Result<Vec<[u8; 32]>> {
    let mut out = Vec::with_capacity(items.len());
    for s in items {
        let bytes = hex::decode(s)
            .with_context(|| format!("subscriber {s:?} is not valid hex"))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "subscriber {s:?} decodes to {} bytes; expected 32",
                bytes.len()
            );
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        out.push(arr);
    }
    Ok(out)
}

/// Translate the daemon's `Args` + role seed into a `NodeConfig`.
///
/// We use `NodeConfig::try_parse_from(synthetic_argv)` so we get the same
/// defaulting behavior as the standalone `zerox1-node-enterprise` binary,
/// without consuming the daemon's own CLI args. The role seed is written
/// to `<secrets_dir>/.runtime-keypair-researcher` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-researcher");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["researcher".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push(format!("researcher-{}", args.fleet_id));
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
        let nonce_v = nonce.fetch_add(1, Ordering::Relaxed);

        let env = Envelope::build(
            MsgType::Beacon,
            sender,
            BROADCAST_RECIPIENT,
            now_secs,
            nonce_v,
            [0u8; 16],
            payload,
            &signing_key,
        );

        match handle.send(env).await {
            Ok(()) => info!(role = %role_id.role().as_str(), nonce = nonce_v, "beacon emitted"),
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
    buf.extend_from_slice(&vk);          // agent_id (= verifying_key in enterprise mode)
    buf.extend_from_slice(&vk);          // verifying_key
    buf.extend_from_slice(name);         // display name
    buf
}

/// Drain the inbound envelope stream, logging each delivery. The future
/// strategy plan replaces this with per-MsgType dispatch (e.g. ingest
/// FleetResearchRequest, fan out artefact-ready notifications).
async fn handle_inbox(mut handle: NodeHandle) -> Result<()> {
    while let Some(env) = handle.recv().await {
        info!(
            msg_type = ?env.msg_type,
            sender = %hex::encode(env.sender),
            nonce = env.nonce,
            "inbox envelope",
        );
    }
    warn!("inbox channel closed; daemon exiting");
    Ok(())
}

/// Cross-validate that the RPC URL matches the declared network by querying
/// `getGenesisHash` and comparing against the known mainnet/devnet hashes.
/// Returns Err on mismatch — a hard fail before any chain-touching state is
/// constructed. Lifted verbatim from multiply-daemon (audit-fix I3) so all
/// daemons share the same boot-time network gate. (Audit-fix I1.)
async fn verify_network_matches_rpc(network: &str, rpc_url: &str) -> Result<()> {
    const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
    const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";

    let ctx = RpcContext::new(rpc_url.to_string(), CommitmentConfig::confirmed());
    let genesis: String = ctx
        .client
        .get_genesis_hash()
        .await
        .context("get_genesis_hash")?
        .to_string();

    let expected = match network {
        "mainnet" => MAINNET_GENESIS,
        "devnet" => DEVNET_GENESIS,
        _ => anyhow::bail!("unknown network {:?}", network),
    };
    if genesis != expected {
        anyhow::bail!(
            "RPC URL {} returned genesis hash {} but --network {} expects {}",
            rpc_url,
            genesis,
            network,
            expected
        );
    }
    info!(network, %genesis, "rpc network verified");
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let workers = args.workers;

    // Audit-fix I1: network sanity gates. Bail before any runtime cost on
    // unknown network or mainnet-without-ack. The RPC<->network genesis
    // cross-check runs inside the runtime once we can do async I/O.
    if args.network != "devnet" && args.network != "mainnet" {
        anyhow::bail!(
            "--network must be 'devnet' or 'mainnet', got {:?}",
            args.network
        );
    }
    if args.network == "mainnet" && !args.i_understand_this_is_mainnet {
        anyhow::bail!(
            "--network=mainnet requires --i-understand-this-is-mainnet flag \
             (this exists to make mainnet promotion explicit)"
        );
    }

    info!(
        network = %args.network,
        rpc_url = %args.rpc_url,
        "researcher args validated",
    );

    // Load the role identity before constructing the runtime so we can fail
    // fast on missing secrets without paying the cost of spinning up a
    // multi-thread tokio runtime.
    let rt = build_runtime(RuntimeProfile::Batch { workers })?;
    rt.block_on(async move {
        // Audit-fix I1: cross-validate that the RPC URL matches the declared
        // network. Catches the "declared mainnet but pointed at devnet RPC"
        // typo before any chain reads. One extra RPC call at boot.
        verify_network_matches_rpc(&args.network, &args.rpc_url).await?;

        let secrets = FileSource::new(&args.secrets_dir);
        let role_identity = load_role_identity(&secrets, Role::Researcher, "researcher-role.key")
            .await
            .context("loading researcher role key")?;
        Box::new(Researcher { args, role_identity }).run().await
    })
}
