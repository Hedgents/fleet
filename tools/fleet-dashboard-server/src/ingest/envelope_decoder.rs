//! Pure-fn decoder mapping a `RawLogLine` (one JSON tracing event) to a
//! `MeshEvent` row for the dashboard feed.
//!
//! The matching is non-exhaustive — daemons emit many internal events
//! beyond what's mesh-relevant; we only decode the ones surfaced in the
//! demo. Unmatched lines yield `None` and are dropped by the caller.

use chrono::DateTime;
use serde_json::Value;

use crate::types::{Direction, MeshEvent, RawLogLine};

/// Try to decode one JSON tracing log line into a `MeshEvent`.
pub fn decode_log_line(raw: &RawLogLine) -> Option<MeshEvent> {
    let obj = raw.raw.as_object()?;
    let fields = obj.get("fields").and_then(Value::as_object)?;
    let message = fields.get("message").and_then(Value::as_str)?;

    let target = obj.get("target").and_then(Value::as_str).unwrap_or("");
    let role_field = fields.get("role").and_then(Value::as_str);
    let sender_role = role_field
        .map(|s| normalize_role(s).to_string())
        .or_else(|| role_from_target(target).map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    let (ts_unix, ts_ms) = parse_ts(obj.get("timestamp").and_then(Value::as_str));

    let conv_id = fields.get("conv").and_then(|v| {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| Some(v.to_string()))
    });
    // Daemons emit on-chain signatures under several tracing field names
    // depending on emitter call-site convention:
    //   - `sig`            (multiply seed/leverage, hedgedjlp JLP buy/hedge legs)
    //   - `tx`             (legacy / pre-fleet-v0.2 emitters)
    //   - `tx_signature`   (explicit form, used by stable-yield)
    // Accept any of them — pick the first non-empty value found.
    let tx_signature = fields
        .get("sig")
        .or_else(|| fields.get("tx"))
        .or_else(|| fields.get("tx_signature"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let (msg_type, direction, summary) = match_event(message, &sender_role, fields)?;

    Some(MeshEvent {
        id: None,
        ts_unix,
        ts_ms,
        sender_role,
        direction,
        msg_type: msg_type.to_string(),
        payload_summary: summary,
        payload_json: Some(Value::Object(fields.clone()).to_string()),
        conv_id,
        tx_signature,
    })
}

/// Returns `(msg_type, direction, summary)` if the message matches a
/// known mesh-relevant event, else `None`.
fn match_event(
    message: &str,
    role: &str,
    fields: &serde_json::Map<String, Value>,
) -> Option<(&'static str, Direction, String)> {
    let lower = message.to_ascii_lowercase();

    if message == "BEACON emitted" {
        let nonce = fields.get("nonce").and_then(Value::as_u64).unwrap_or(0);
        return Some((
            "Beacon",
            Direction::Out,
            format!("{role} announced presence (nonce {nonce})"),
        ));
    }

    if message == "AssignMultiply received" {
        let target_ltv = fields
            .get("target_ltv_bps")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let slip = fields
            .get("max_slippage_bps")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return Some((
            "Assign",
            Direction::In,
            format!("multiply received Assign — target_ltv={target_ltv}bps, slippage={slip}bps"),
        ));
    }

    if message == "AssignStableLend received" {
        let lamports = fields
            .get("usdc_lamports")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return Some((
            "Assign",
            Direction::In,
            format!(
                "stable-yield received Assign — supply {} USDC",
                usdc(lamports)
            ),
        ));
    }

    if message == "AssignHedgedJlp received" {
        let lamports = fields
            .get("usdc_lamports")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let delta = fields
            .get("target_delta_bps")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return Some((
            "Assign",
            Direction::In,
            format!(
                "hedgedjlp received Assign — deploy {} USDC, delta target {}bps",
                usdc(lamports),
                delta
            ),
        ));
    }

    if message == "WithdrawStableLend received" {
        let amount = fields
            .get("amount")
            .or_else(|| fields.get("usdc_lamports"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return Some((
            "Withdraw",
            Direction::In,
            format!("stable-yield received Withdraw — {}", usdc(amount)),
        ));
    }

    if message == "WithdrawHedgedJlp received" {
        let jlp = fields
            .get("jlp")
            .or_else(|| fields.get("jlp_lamports"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return Some((
            "Withdraw",
            Direction::In,
            format!("hedgedjlp received Withdraw — {} JLP", jmt(jlp)),
        ));
    }

    if message == "Approve received" {
        let conv = fields
            .get("conv")
            .and_then(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| Some(v.to_string()))
            })
            .unwrap_or_default();
        return Some((
            "Approve",
            Direction::In,
            format!("{role} received Approve for conv {}", conv_short(&conv)),
        ));
    }

    // "report sent", "assign report sent", "withdraw report sent",
    // "approve report sent" — every daemon emits a Report acknowledgment
    // with the same shape (`ok`, `conv`).
    if lower == "report sent" || lower.ends_with("report sent") {
        let ok = fields.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let conv = fields
            .get("conv")
            .and_then(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| Some(v.to_string()))
            })
            .unwrap_or_default();
        return Some((
            "Report",
            Direction::Out,
            format!(
                "{role} reported {} on conv {}",
                ok_or_err(ok),
                conv_short(&conv)
            ),
        ));
    }

    if message == "EscalateRisk emitted" {
        let severity = fields
            .get("severity")
            .and_then(field_str)
            .unwrap_or_else(|| "?".to_string());
        let kind = fields
            .get("kind")
            .and_then(field_str)
            .unwrap_or_else(|| "?".to_string());
        return Some((
            "Escalate",
            Direction::Out,
            format!("{role} escalated {severity} {kind}"),
        ));
    }

    if message == "MarketSignal emitted" {
        let kind = fields
            .get("kind")
            .and_then(field_str)
            .unwrap_or_else(|| "?".to_string());
        let asset = fields
            .get("asset")
            .and_then(field_str)
            .unwrap_or_else(|| "?".to_string());
        let severity = fields
            .get("severity")
            .and_then(field_str)
            .unwrap_or_else(|| "?".to_string());
        return Some((
            "MarketSignal",
            Direction::Out,
            format!("researcher signaled {kind} on {asset} ({severity})"),
        ));
    }

    if message == "Approve REJECTED" {
        return Some((
            "Internal",
            Direction::Internal,
            format!("{role} REJECTED Approve from wrong sender"),
        ));
    }

    // ── On-chain confirmation events ──────────────────────────────────
    //
    // These are the daemon log lines that carry a real on-chain
    // transaction signature in `fields.sig`. Matching them here causes
    // the decoder to mint a `MeshEvent` whose `tx_signature` column is
    // populated, which is what `/onchain/activity` and the per-strategy
    // `last_sig` query rely on.

    if message == "deposit confirmed on-chain" {
        let apr = fields.get("apr_bps").and_then(Value::as_i64).unwrap_or(0);
        return Some((
            "OnChain",
            Direction::Out,
            format!("stable-yield deposit confirmed on-chain (apr {apr}bps)"),
        ));
    }

    if message == "seed committed" {
        return Some((
            "OnChain",
            Direction::Out,
            format!("{role} seed tx committed on-chain"),
        ));
    }

    if message == "round committed" {
        let round = fields.get("round").and_then(Value::as_u64);
        let ix_count = fields.get("ix_count").and_then(Value::as_u64);
        let summary = match (round, ix_count) {
            (Some(r), _) => format!("multiply round {r} committed on-chain"),
            (None, Some(n)) => format!("multiply round committed on-chain ({n} ixns)"),
            _ => "multiply round committed on-chain".to_string(),
        };
        return Some(("OnChain", Direction::Out, summary));
    }

    if message == "JLP buy confirmed on-chain (via Jupiter)" {
        return Some((
            "OnChain",
            Direction::Out,
            "hedgedjlp JLP buy confirmed on-chain (Jupiter)".to_string(),
        ));
    }

    if message == "hedge short-open request submitted" {
        let asset = fields
            .get("asset")
            .and_then(field_str)
            .unwrap_or_else(|| "?".to_string());
        return Some((
            "OnChain",
            Direction::Out,
            format!("hedgedjlp short-open submitted on {asset}"),
        ));
    }

    if message == "rebalance tick" {
        let note = fields
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("ticked")
            .to_string();
        return Some((
            "Internal",
            Direction::Internal,
            format!("{role} rebalance tick — {note}"),
        ));
    }

    None
}

/// Normalize a role name from a daemon's `role=` tracing field to the
/// dashboard's canonical role name. The runtime `Role` enum uses
/// `stablefloor` as its `as_str()` value, but the dashboard tracks
/// `stable_yield` (matching the daemon binary name + the `/daemons`
/// route fixture). Other names pass through unchanged.
pub fn normalize_role(role: &str) -> &str {
    match role {
        "stablefloor" => "stable_yield",
        other => other,
    }
}

/// Map a `tracing` event's `target` to a daemon role.
pub fn role_from_target(target: &str) -> Option<&'static str> {
    target.split("::").next().and_then(|first| match first {
        "multiply_daemon" => Some("multiply"),
        "stable_yield_daemon" => Some("stable_yield"),
        "hedgedjlp_daemon" => Some("hedgedjlp"),
        "riskwatcher_daemon" => Some("riskwatcher"),
        "researcher_daemon" => Some("researcher"),
        "orchestrator_daemon" => Some("orchestrator"),
        _ => None,
    })
}

/// First 8 hex characters of a conv id, robust against non-hex
/// debug-formatted bytes (e.g. `[1, 2, 3, ...]`).
pub fn conv_short(s: &str) -> String {
    let trimmed: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect();
    if trimmed.is_empty() {
        s.chars().take(8).collect()
    } else {
        trimmed
    }
}

/// Format USDC lamports (6 decimals) as `$N.NN`.
pub fn usdc(lamports: u64) -> String {
    let dollars = lamports / 1_000_000;
    let cents = (lamports % 1_000_000) / 10_000;
    format!("${}.{:02}", dollars, cents)
}

/// Format JLP lamports (6 decimals) without a $ prefix.
fn jmt(lamports: u64) -> String {
    let units = lamports / 1_000_000;
    let frac = (lamports % 1_000_000) / 10_000;
    format!("{}.{:02}", units, frac)
}

pub fn ok_or_err(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "ERROR"
    }
}

/// Extract a string from either a JSON string or a tracing-debug-formatted
/// value (e.g. `?severity` renders as `"Critical"` once with quotes,
/// sometimes without quotes — we accept both).
fn field_str(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.trim_matches('"').to_string());
    }
    Some(v.to_string().trim_matches('"').to_string())
}

/// Parse RFC3339 timestamp into (seconds, milliseconds). Falls back to 0
/// on failure (caller can rely on log_tailer to drop rows it can't trust).
fn parse_ts(ts: Option<&str>) -> (i64, i64) {
    let Some(s) = ts else { return (0, 0) };
    match DateTime::parse_from_rfc3339(s) {
        Ok(dt) => (dt.timestamp(), dt.timestamp_millis()),
        Err(_) => (0, 0),
    }
}
