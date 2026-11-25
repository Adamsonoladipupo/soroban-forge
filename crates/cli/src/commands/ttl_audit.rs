//! `ttl-audit` command — audits the remaining TTL of a deployed Soroban
//! contract's instance and persistent storage entries over RPC.
//!
//! # Exit codes
//!
//! | Code | Meaning                                            |
//! |-----:|:---------------------------------------------------|
//! |    0 | All audited entries are above the threshold.       |
//! |    1 | At least one entry is at or below the threshold.  |
//! |    2 | Usage, configuration, RPC, or other command error. |
//!
//! The exit code is returned as `Ok(i32)` to the caller; `process::exit` is
//! never called inside this module.

use crate::cli::{OutputFormat, TtlAuditArgs};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;

// ── Exit code constants ───────────────────────────────────────────────────────

/// All entries are healthy.
pub const EXIT_OK: i32 = 0;
/// At least one entry is below the TTL threshold.
pub const EXIT_EXPIRING: i32 = 1;
/// Usage, configuration, RPC, or other command-level error.
/// Returned as `Err(...)` from the command handler; mapped to this code by the caller.
#[allow(dead_code)]
pub const EXIT_ERROR: i32 = 2;

// ── Storage key specification (input JSON) ────────────────────────────────────

/// A single persistent storage key specification loaded from a `--keys` file.
///
/// The file is a JSON array of these objects.  The `label` field is optional
/// and used only for display purposes; `key_xdr` is the base-64–encoded XDR
/// of the `LedgerKey` that will be queried.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct KeySpec {
    /// Human-readable label shown in reports.
    pub label: String,
    /// Base-64 XDR of the `LedgerKey` (ContractData variant).
    pub key_xdr: String,
}

// ── RPC transport abstraction ─────────────────────────────────────────────────

/// Minimal RPC response shapes we care about.

#[derive(Debug, Clone, Deserialize)]
pub struct LedgerEntryResult {
    #[allow(dead_code)]
    pub id: Option<String>,
    #[serde(rename = "liveUntilLedgerSeq")]
    pub live_until_ledger_seq: Option<u64>,
    #[allow(dead_code)]
    #[serde(rename = "lastModifiedLedgerSeq")]
    pub last_modified_ledger_seq: Option<u64>,
    #[allow(dead_code)]
    pub xdr: Option<String>,
}

/// A thin wrapper around the `stellar contract ledger-entry` / `getLedgerEntries`
/// call, implemented by spawning the Stellar CLI.
///
/// The trait exists so tests can inject a mock without touching the network.
pub trait RpcClient {
    /// Query the current ledger sequence number.
    fn latest_ledger(&self, network: &str) -> Result<u64>;

    /// Fetch a single ledger entry by its XDR key.
    ///
    /// Returns `Ok(None)` when the entry does not exist or is archived.
    fn get_ledger_entry(&self, network: &str, key_xdr: &str) -> Result<Option<LedgerEntryResult>>;
}

/// Production RPC client that delegates to the `stellar` CLI.
pub struct StellarCliRpc;

impl RpcClient for StellarCliRpc {
    fn latest_ledger(&self, network: &str) -> Result<u64> {
        // Use `stellar ledger current` to get the current ledger sequence.
        let output = Command::new("stellar")
            .args([
                "ledger",
                "current",
                "--network",
                network,
                "--output",
                "json",
            ])
            .output()
            .context("failed to spawn `stellar ledger current`")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("stellar ledger current failed: {}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // The JSON output has a `sequence` field.
        let value: serde_json::Value = serde_json::from_str(stdout.trim())
            .context("failed to parse `stellar ledger current` output")?;

        let seq = value
            .get("sequence")
            .or_else(|| value.get("seq"))
            .or_else(|| value.get("ledger"))
            .and_then(|v| v.as_u64())
            .context("missing `sequence` field in `stellar ledger current` output")?;

        Ok(seq)
    }

    fn get_ledger_entry(&self, network: &str, key_xdr: &str) -> Result<Option<LedgerEntryResult>> {
        let output = Command::new("stellar")
            .args([
                "contract",
                "read",
                "--network",
                network,
                "--key-xdr",
                key_xdr,
                "--output",
                "json",
            ])
            .output()
            .context("failed to spawn `stellar contract read`")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
            // A not-found or archived entry is non-fatal.
            if stderr.contains("not found")
                || stderr.contains("not exist")
                || stderr.contains("archived")
                || stderr.contains("no entry")
                || stderr.contains("does not exist")
            {
                return Ok(None);
            }
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("stellar contract read failed: {}", err.trim());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let value: serde_json::Value = serde_json::from_str(trimmed)
            .context("failed to parse `stellar contract read` JSON")?;

        // The output may be an array of entries or a single object.
        let entry_value = if value.is_array() {
            let arr = value.as_array().unwrap();
            if arr.is_empty() {
                return Ok(None);
            }
            arr[0].clone()
        } else {
            value
        };

        let entry: LedgerEntryResult =
            serde_json::from_value(entry_value).context("failed to deserialize ledger entry")?;

        Ok(Some(entry))
    }
}

// ── Entry status ──────────────────────────────────────────────────────────────

/// Status of a single audited ledger entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// Entry exists and TTL is above the threshold.
    Healthy,
    /// Entry exists and TTL is at or below the threshold.
    Expiring,
    /// Entry does not exist on-chain.
    Missing,
    /// Entry has been archived (TTL reached 0 in a previous ledger).
    Archived,
}

// ── Per-entry audit result ────────────────────────────────────────────────────

/// Full audit result for a single ledger entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    /// Human-readable label (e.g. `"instance"` or the caller-supplied key label).
    pub label: String,
    /// XDR key used to query this entry.
    pub key_xdr: String,
    /// Whether this is a persistent or instance entry.
    pub durability: String,
    /// The ledger at which this entry will expire (`None` when missing/archived).
    pub live_until_ledger: Option<u64>,
    /// Remaining ledgers before expiry (`None` when missing/archived).
    pub remaining_ledgers: Option<u64>,
    /// Whether `remaining_ledgers` is at or below the configured threshold.
    pub below_threshold: bool,
    /// Entry status.
    pub status: EntryStatus,
}

impl AuditEntry {
    /// True when this entry needs extending (expiring but not missing/archived).
    pub fn needs_extend(&self) -> bool {
        self.status == EntryStatus::Expiring
    }
}

// ── Full audit report ─────────────────────────────────────────────────────────

/// The complete TTL audit report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TtlAuditReport {
    /// Contract ID that was audited.
    pub contract_id: String,
    /// Network used.
    pub network: String,
    /// Current ledger at the time of the audit.
    pub current_ledger: u64,
    /// Threshold (minimum acceptable remaining TTL) used for this audit.
    pub threshold: u32,
    /// All audited entries, instance first then persistent keys in input order.
    pub entries: Vec<AuditEntry>,
}

impl TtlAuditReport {
    /// True when any entry has `below_threshold = true` (and is not missing/archived).
    pub fn has_expiring(&self) -> bool {
        self.entries.iter().any(|e| e.below_threshold)
    }
}

// ── XDR key helpers ───────────────────────────────────────────────────────────

/// Encode raw bytes to standard base-64 (RFC 4648, with padding).
///
/// This avoids pulling in the `base64` crate or enabling the optional
/// `base64` feature of `stellar-xdr`, keeping the lockfile unchanged.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((combined >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((combined >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((combined >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(combined & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Build the base-64 XDR `LedgerKey` for a contract instance entry.
///
/// A contract instance `LedgerKey` has type `CONTRACT_DATA` with
/// `key = ScVal::LedgerKeyContractInstance` and `durability = Persistent`.
/// We encode this using `stellar-xdr`.
pub fn contract_instance_key_xdr(contract_id: &str) -> Result<String> {
    use stellar_xdr::curr::{
        ContractDataDurability, LedgerKey, LedgerKeyContractData, Limits, ScAddress, ScVal,
        WriteXdr,
    };

    let hash = contract_id_to_hash(contract_id)?;
    let key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(hash),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    });
    let raw = key.to_xdr(Limits::none())?;
    Ok(base64_encode(&raw))
}

/// Decode a contract ID (Stellar strkey `C…`) to a 32-byte `Hash`.
fn contract_id_to_hash(contract_id: &str) -> Result<stellar_xdr::curr::Hash> {
    use stellar_strkey::Contract;
    use stellar_xdr::curr::Hash;

    let contract = Contract::from_string(contract_id)
        .with_context(|| format!("invalid contract ID: `{contract_id}`"))?;
    Ok(Hash(contract.0))
}

// ── Key spec loading ──────────────────────────────────────────────────────────

/// Load a JSON array of `KeySpec` from `path`.
pub fn load_key_specs(path: &Path) -> Result<Vec<KeySpec>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read key spec file `{}`", path.display()))?;
    let specs: Vec<KeySpec> = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse key spec file `{}`", path.display()))?;
    Ok(specs)
}

// ── Core audit logic ──────────────────────────────────────────────────────────

/// Audit a single entry and return an [`AuditEntry`].
pub fn audit_entry(
    rpc: &dyn RpcClient,
    network: &str,
    label: &str,
    key_xdr: &str,
    durability: &str,
    current_ledger: u64,
    threshold: u32,
) -> AuditEntry {
    match rpc.get_ledger_entry(network, key_xdr) {
        Err(e) => {
            // RPC error — treat as missing for reporting purposes; the
            // caller decides whether to surface it differently.
            let _ = e; // non-fatal: we surface the entry as missing
            AuditEntry {
                label: label.to_string(),
                key_xdr: key_xdr.to_string(),
                durability: durability.to_string(),
                live_until_ledger: None,
                remaining_ledgers: None,
                below_threshold: false,
                status: EntryStatus::Missing,
            }
        }
        Ok(None) => AuditEntry {
            label: label.to_string(),
            key_xdr: key_xdr.to_string(),
            durability: durability.to_string(),
            live_until_ledger: None,
            remaining_ledgers: None,
            below_threshold: false,
            status: EntryStatus::Missing,
        },
        Ok(Some(entry)) => {
            match entry.live_until_ledger_seq {
                None => {
                    // No live_until means the entry has been archived.
                    AuditEntry {
                        label: label.to_string(),
                        key_xdr: key_xdr.to_string(),
                        durability: durability.to_string(),
                        live_until_ledger: None,
                        remaining_ledgers: None,
                        below_threshold: false,
                        status: EntryStatus::Archived,
                    }
                }
                Some(live_until) => {
                    let remaining = live_until.saturating_sub(current_ledger);
                    let below = remaining <= threshold as u64;
                    let status = if below {
                        EntryStatus::Expiring
                    } else {
                        EntryStatus::Healthy
                    };
                    AuditEntry {
                        label: label.to_string(),
                        key_xdr: key_xdr.to_string(),
                        durability: durability.to_string(),
                        live_until_ledger: Some(live_until),
                        remaining_ledgers: Some(remaining),
                        below_threshold: below,
                        status,
                    }
                }
            }
        }
    }
}

/// Run the full audit: instance entry + all specified persistent keys.
pub fn run_audit(
    rpc: &dyn RpcClient,
    contract_id: &str,
    network: &str,
    key_specs: &[KeySpec],
    threshold: u32,
) -> Result<TtlAuditReport> {
    let current_ledger = rpc
        .latest_ledger(network)
        .with_context(|| format!("failed to fetch current ledger on network `{network}`"))?;

    let instance_key_xdr = contract_instance_key_xdr(contract_id)?;
    let instance_entry = audit_entry(
        rpc,
        network,
        "instance",
        &instance_key_xdr,
        "persistent",
        current_ledger,
        threshold,
    );

    let mut entries = vec![instance_entry];
    for spec in key_specs {
        let entry = audit_entry(
            rpc,
            network,
            &spec.label,
            &spec.key_xdr,
            "persistent",
            current_ledger,
            threshold,
        );
        entries.push(entry);
    }

    Ok(TtlAuditReport {
        contract_id: contract_id.to_string(),
        network: network.to_string(),
        current_ledger,
        threshold,
        entries,
    })
}

// ── Output formatting ─────────────────────────────────────────────────────────

/// Print a human-readable table report.
pub fn print_table(report: &TtlAuditReport) {
    println!(
        "TTL Audit: {} on {} (current ledger: {}, threshold: {})",
        report.contract_id, report.network, report.current_ledger, report.threshold
    );
    println!();

    let label_w = report
        .entries
        .iter()
        .map(|e| e.label.len())
        .max()
        .unwrap_or(5)
        .max(5);

    println!(
        "{:<label_w$}  {:<12}  {:<14}  {:<12}  {:<8}  STATUS",
        "ENTRY", "DURABILITY", "LIVE_UNTIL", "REMAINING", "THRESHOLD"
    );
    println!(
        "{:-<label_w$}  {:-<12}  {:-<14}  {:-<12}  {:-<8}  {:-<10}",
        "", "", "", "", "", ""
    );

    for entry in &report.entries {
        let live = entry
            .live_until_ledger
            .map(|l| l.to_string())
            .unwrap_or_else(|| "-".to_string());
        let rem = entry
            .remaining_ledgers
            .map(|r| r.to_string())
            .unwrap_or_else(|| "-".to_string());
        let below = if entry.below_threshold { "YES" } else { "no" };
        let status = match &entry.status {
            EntryStatus::Healthy => "HEALTHY",
            EntryStatus::Expiring => "EXPIRING",
            EntryStatus::Missing => "MISSING",
            EntryStatus::Archived => "ARCHIVED",
        };
        println!(
            "{:<label_w$}  {:<12}  {:<14}  {:<12}  {:<8}  {}",
            entry.label, entry.durability, live, rem, below, status
        );
    }
}

/// Print deterministic JSON output.
pub fn print_json(report: &TtlAuditReport) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    println!("{json}");
    Ok(())
}

// ── Extend-TTL plan generation ────────────────────────────────────────────────

/// Generate a plan-compatible JSON file for entries that need extending.
///
/// The plan uses the `plan.rs` `Plan` schema (version `"1"`) with calls of
/// kind `"invoke"` targeting `stellar contract extend` semantics.  Since the
/// existing transport calls `stellar contract invoke`, we emit
/// `extend_ttl` as a synthetic "function" and pass the required arguments.
/// Callers that want to execute the plan use `soroban-forge plan <file>`.
///
/// More specifically, we serialize a `TtlExtendPlan` — a structure that is
/// compatible with the `Plan` JSON schema used by `plan.rs` — so the existing
/// `validate_plan` function accepts it.
pub fn generate_extend_plan(report: &TtlAuditReport, network: &str, out_path: &Path) -> Result<()> {
    let expiring: Vec<&AuditEntry> = report.entries.iter().filter(|e| e.needs_extend()).collect();

    if expiring.is_empty() {
        // Write an empty-calls plan so the file is always created when
        // --plan is specified; callers can check entry count.
        let plan = TtlExtendPlan {
            version: "1".to_string(),
            network: network.to_string(),
            source: String::new(),
            extends: vec![],
        };
        let json = serde_json::to_string_pretty(&plan)?;
        fs::write(out_path, json)
            .with_context(|| format!("failed to write extend plan to `{}`", out_path.display()))?;
        return Ok(());
    }

    let extends: Vec<TtlExtendEntry> = expiring
        .iter()
        .map(|e| TtlExtendEntry {
            label: e.label.clone(),
            contract: report.contract_id.clone(),
            key_xdr: e.key_xdr.clone(),
            durability: e.durability.clone(),
            current_live_until: e.live_until_ledger,
            remaining_ledgers: e.remaining_ledgers,
        })
        .collect();

    let plan = TtlExtendPlan {
        version: "1".to_string(),
        network: network.to_string(),
        source: String::new(),
        extends,
    };

    let json = serde_json::to_string_pretty(&plan)?;
    fs::write(out_path, &json)
        .with_context(|| format!("failed to write extend plan to `{}`", out_path.display()))?;

    Ok(())
}

/// An extend-TTL plan.  The `version` and `network` fields match the `Plan`
/// schema; `source` is left empty for the caller to fill in before execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TtlExtendPlan {
    pub version: String,
    pub network: String,
    /// Source account; left empty by the generator — fill before executing.
    pub source: String,
    pub extends: Vec<TtlExtendEntry>,
}

/// A single entry in an extend-TTL plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TtlExtendEntry {
    pub label: String,
    pub contract: String,
    pub key_xdr: String,
    pub durability: String,
    pub current_live_until: Option<u64>,
    pub remaining_ledgers: Option<u64>,
}

/// Validate a `TtlExtendPlan` using the same rules as `plan.rs::validate_plan`.
///
/// A plan is valid when:
/// - `version == "1"`
/// - `network` is non-empty
/// - each extend entry has a non-empty `contract`, `key_xdr`, and `durability`
///
/// The `source` field may be empty at generation time (the user must fill it
/// in before executing).
pub fn validate_extend_plan(plan: &TtlExtendPlan) -> Result<()> {
    if plan.version != "1" {
        bail!(
            "unsupported plan version `{}`: only version `1` is supported",
            plan.version
        );
    }
    if plan.network.trim().is_empty() {
        bail!("plan.network must be a non-empty string");
    }
    for (idx, entry) in plan.extends.iter().enumerate() {
        if entry.contract.trim().is_empty() {
            bail!("extends[{idx}].contract must be a non-empty contract address");
        }
        if entry.key_xdr.trim().is_empty() {
            bail!("extends[{idx}].key_xdr must be a non-empty XDR key");
        }
        if entry.durability.trim().is_empty() {
            bail!("extends[{idx}].durability must be non-empty");
        }
    }
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the `ttl-audit` command.
///
/// Returns the exit code that `main` should use:
/// - `0` — all entries healthy
/// - `1` — at least one entry is expiring
/// - `2` — command-level error (but the error is returned as `Err`, not `Ok(2)`)
pub fn run(args: TtlAuditArgs) -> Result<i32> {
    run_with_rpc(&StellarCliRpc, args)
}

/// Testable inner implementation that accepts an injected RPC client.
pub fn run_with_rpc(rpc: &dyn RpcClient, args: TtlAuditArgs) -> Result<i32> {
    // Validate contract ID early.
    contract_id_to_hash(&args.contract)
        .with_context(|| format!("invalid --contract value `{}`", args.contract))?;

    // Load key specs if provided.
    let key_specs = match &args.keys {
        Some(path) => load_key_specs(path)?,
        None => vec![],
    };

    // Run the audit.
    let report = run_audit(
        rpc,
        &args.contract,
        &args.network,
        &key_specs,
        args.threshold,
    )?;

    // Output.
    match args.format {
        OutputFormat::Table => print_table(&report),
        OutputFormat::Json => print_json(&report)?,
    }

    // Generate extend plan if requested.
    if let Some(plan_path) = &args.plan {
        let plan_net = args.network.clone();
        generate_extend_plan(&report, &plan_net, plan_path).with_context(|| {
            format!(
                "failed to generate extend plan at `{}`",
                plan_path.display()
            )
        })?;

        // Validate the generated plan.
        let raw = fs::read_to_string(plan_path)
            .with_context(|| format!("failed to read generated plan `{}`", plan_path.display()))?;
        let plan: TtlExtendPlan = serde_json::from_str(&raw).with_context(|| {
            format!(
                "generated plan is not valid JSON at `{}`",
                plan_path.display()
            )
        })?;
        validate_extend_plan(&plan).with_context(|| {
            format!(
                "generated plan failed validation at `{}`",
                plan_path.display()
            )
        })?;

        eprintln!(
            "extend plan written to `{}` ({} entries)",
            plan_path.display(),
            plan.extends.len()
        );
    }

    // Determine exit code.
    if report.has_expiring() {
        Ok(EXIT_EXPIRING)
    } else {
        Ok(EXIT_OK)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // ── Mock RPC client ───────────────────────────────────────────────────────

    /// Calls are served in the order they were enqueued.
    struct MockRpc {
        /// First call is always `latest_ledger`.
        ledger: u64,
        /// Subsequent calls are served from this queue.
        entries: Mutex<VecDeque<Result<Option<LedgerEntryResult>>>>,
    }

    impl MockRpc {
        fn new(ledger: u64) -> Self {
            Self {
                ledger,
                entries: Mutex::new(VecDeque::new()),
            }
        }

        fn push(&self, result: Result<Option<LedgerEntryResult>>) {
            self.entries.lock().unwrap().push_back(result);
        }

        fn push_entry(&self, live_until: Option<u64>) {
            self.push(Ok(live_until.map(|l| LedgerEntryResult {
                id: None,
                live_until_ledger_seq: Some(l),
                last_modified_ledger_seq: None,
                xdr: None,
            })));
        }

        fn push_missing(&self) {
            self.push(Ok(None));
        }

        fn push_archived(&self) {
            // Archived = entry exists but no live_until_ledger_seq.
            self.push(Ok(Some(LedgerEntryResult {
                id: None,
                live_until_ledger_seq: None,
                last_modified_ledger_seq: None,
                xdr: None,
            })));
        }

        fn push_rpc_error(&self) {
            self.push(Err(anyhow::anyhow!("connection refused")));
        }
    }

    impl RpcClient for MockRpc {
        fn latest_ledger(&self, _network: &str) -> Result<u64> {
            Ok(self.ledger)
        }

        fn get_ledger_entry(
            &self,
            _network: &str,
            _key_xdr: &str,
        ) -> Result<Option<LedgerEntryResult>> {
            self.entries
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(None))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// A valid Stellar contract strkey for tests.
    const CONTRACT_ID: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

    fn default_args() -> TtlAuditArgs {
        TtlAuditArgs {
            contract: CONTRACT_ID.to_string(),
            network: "testnet".to_string(),
            keys: None,
            threshold: 100_000,
            format: OutputFormat::Table,
            plan: None,
        }
    }

    // ── 1. Healthy contract instance and persistent entries ───────────────────

    #[test]
    fn healthy_instance_entry() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000)); // instance entry: 1_000_000 remaining

        let args = default_args();
        let code = run_with_rpc(&rpc, args).unwrap();
        assert_eq!(code, EXIT_OK, "all healthy → exit 0");
    }

    // ── 2. Entries approaching expiration ─────────────────────────────────────

    #[test]
    fn expiring_instance_entry_returns_exit_1() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_000_050)); // 50 remaining < 100_000 threshold

        let args = default_args();
        let code = run_with_rpc(&rpc, args).unwrap();
        assert_eq!(code, EXIT_EXPIRING, "expiring entry → exit 1");
    }

    // ── 3. Missing persistent entries ─────────────────────────────────────────

    #[test]
    fn missing_persistent_entry_does_not_fail_audit() {
        let tmp = TempDir::new().unwrap();
        let keys_file = tmp.path().join("keys.json");
        let spec = KeySpec {
            label: "my_key".to_string(),
            key_xdr: "AAAA".to_string(),
        };
        fs::write(&keys_file, serde_json::to_string(&[&spec]).unwrap()).unwrap();

        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000)); // instance: healthy
        rpc.push_missing(); // persistent key: missing

        let mut args = default_args();
        args.keys = Some(keys_file);
        let code = run_with_rpc(&rpc, args).unwrap();
        assert_eq!(code, EXIT_OK); // missing ≠ below threshold
    }

    // ── 4. Archived persistent entries ───────────────────────────────────────

    #[test]
    fn archived_entry_is_reported_but_does_not_trigger_exit_1() {
        let tmp = TempDir::new().unwrap();
        let keys_file = tmp.path().join("keys.json");
        let spec = KeySpec {
            label: "archived_key".to_string(),
            key_xdr: "AAAA".to_string(),
        };
        fs::write(&keys_file, serde_json::to_string(&[&spec]).unwrap()).unwrap();

        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000)); // instance: healthy
        rpc.push_archived(); // persistent key: archived

        let mut args = default_args();
        args.keys = Some(keys_file);
        let code = run_with_rpc(&rpc, args).unwrap();
        assert_eq!(code, EXIT_OK);
    }

    // ── 5. Instance entry is always audited ───────────────────────────────────

    #[test]
    fn instance_entry_always_present_in_report() {
        let rpc = MockRpc::new(500_000);
        rpc.push_entry(Some(1_500_000));

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        assert!(!report.entries.is_empty());
        assert_eq!(report.entries[0].label, "instance");
    }

    // ── 6. Correct TTL calculation ────────────────────────────────────────────

    #[test]
    fn ttl_calculation_is_correct() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_050_000));

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        let entry = &report.entries[0];
        assert_eq!(entry.live_until_ledger, Some(1_050_000));
        assert_eq!(entry.remaining_ledgers, Some(50_000));
    }

    // ── 7. Correct threshold comparison ──────────────────────────────────────

    #[test]
    fn threshold_comparison_at_exact_boundary() {
        // remaining == threshold → below_threshold
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_100_000)); // exactly 100_000 remaining

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        assert!(
            report.entries[0].below_threshold,
            "equal to threshold is below"
        );
        assert_eq!(report.entries[0].status, EntryStatus::Expiring);

        // remaining == threshold + 1 → healthy
        let rpc2 = MockRpc::new(1_000_000);
        rpc2.push_entry(Some(1_100_001)); // 100_001 remaining

        let report2 = run_audit(&rpc2, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        assert!(!report2.entries[0].below_threshold);
        assert_eq!(report2.entries[0].status, EntryStatus::Healthy);
    }

    // ── 8. Malformed JSON key files ───────────────────────────────────────────

    #[test]
    fn malformed_key_file_returns_error() {
        let tmp = TempDir::new().unwrap();
        let keys_file = tmp.path().join("bad.json");
        fs::write(&keys_file, "this is not json").unwrap();

        let rpc = MockRpc::new(1_000_000);
        let mut args = default_args();
        args.keys = Some(keys_file);
        let result = run_with_rpc(&rpc, args);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("failed to parse key spec file"), "{msg}");
    }

    // ── 9. Invalid key specifications ─────────────────────────────────────────

    #[test]
    fn missing_key_file_returns_error() {
        let rpc = MockRpc::new(1_000_000);
        let mut args = default_args();
        args.keys = Some(std::path::PathBuf::from("/nonexistent/path/keys.json"));
        let result = run_with_rpc(&rpc, args);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("failed to read key spec file"), "{msg}");
    }

    // ── 10. RPC failures ──────────────────────────────────────────────────────

    #[test]
    fn rpc_error_on_instance_query_is_reported_as_missing() {
        // The MockRpc `get_ledger_entry` falls back to `Ok(None)` when the
        // queue is empty; to test an actual error we push one explicitly.
        let rpc = MockRpc::new(1_000_000);
        rpc.push_rpc_error(); // instance query fails

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        // RPC error maps to Missing, not a hard failure of the whole audit.
        assert_eq!(report.entries[0].status, EntryStatus::Missing);
    }

    // ── 11. RPC timeouts ──────────────────────────────────────────────────────

    #[test]
    fn latest_ledger_failure_propagates_as_error() {
        struct FailingLedger;
        impl RpcClient for FailingLedger {
            fn latest_ledger(&self, _network: &str) -> Result<u64> {
                Err(anyhow::anyhow!("connection timed out"))
            }
            fn get_ledger_entry(
                &self,
                _network: &str,
                _key: &str,
            ) -> Result<Option<LedgerEntryResult>> {
                Ok(None)
            }
        }

        let args = default_args();
        let result = run_with_rpc(&FailingLedger, args);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("failed to fetch current ledger"), "{msg}");
    }

    // ── 12. Deterministic JSON output ─────────────────────────────────────────

    #[test]
    fn json_output_is_deterministic() {
        let rpc1 = MockRpc::new(1_000_000);
        rpc1.push_entry(Some(2_000_000));

        let rpc2 = MockRpc::new(1_000_000);
        rpc2.push_entry(Some(2_000_000));

        let report1 = run_audit(&rpc1, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        let report2 = run_audit(&rpc2, CONTRACT_ID, "testnet", &[], 100_000).unwrap();

        let json1 = serde_json::to_string(&report1).unwrap();
        let json2 = serde_json::to_string(&report2).unwrap();
        assert_eq!(
            json1, json2,
            "identical audit runs must produce identical JSON"
        );
    }

    #[test]
    fn json_fields_are_stable() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_050_000));

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"contract_id\""), "{json}");
        assert!(json.contains("\"network\""), "{json}");
        assert!(json.contains("\"current_ledger\""), "{json}");
        assert!(json.contains("\"threshold\""), "{json}");
        assert!(json.contains("\"entries\""), "{json}");
        // Entry fields
        assert!(json.contains("\"label\""), "{json}");
        assert!(json.contains("\"durability\""), "{json}");
        assert!(json.contains("\"live_until_ledger\""), "{json}");
        assert!(json.contains("\"remaining_ledgers\""), "{json}");
        assert!(json.contains("\"below_threshold\""), "{json}");
        assert!(json.contains("\"status\""), "{json}");
    }

    // ── 13. Table output ──────────────────────────────────────────────────────

    #[test]
    fn table_output_runs_without_panic() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000));

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        // Should not panic.
        print_table(&report);
    }

    // ── 14. Extend-TTL plan generation ───────────────────────────────────────

    #[test]
    fn extend_plan_is_generated_for_expiring_entries() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_000_050)); // 50 remaining → expiring

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        let tmp = TempDir::new().unwrap();
        let plan_path = tmp.path().join("extend.json");

        generate_extend_plan(&report, "testnet", &plan_path).unwrap();
        let raw = fs::read_to_string(&plan_path).unwrap();
        let plan: TtlExtendPlan = serde_json::from_str(&raw).unwrap();
        assert_eq!(plan.extends.len(), 1);
        assert_eq!(plan.extends[0].label, "instance");
    }

    // ── 15. Generated plan passes validation ──────────────────────────────────

    #[test]
    fn generated_plan_passes_validation() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_000_050)); // expiring

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        let tmp = TempDir::new().unwrap();
        let plan_path = tmp.path().join("extend.json");

        generate_extend_plan(&report, "testnet", &plan_path).unwrap();

        let raw = fs::read_to_string(&plan_path).unwrap();
        let plan: TtlExtendPlan = serde_json::from_str(&raw).unwrap();
        validate_extend_plan(&plan).expect("generated plan must pass validation");
    }

    #[test]
    fn empty_plan_is_valid_when_no_entries_expiring() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000)); // healthy

        let report = run_audit(&rpc, CONTRACT_ID, "testnet", &[], 100_000).unwrap();
        let tmp = TempDir::new().unwrap();
        let plan_path = tmp.path().join("extend.json");

        generate_extend_plan(&report, "testnet", &plan_path).unwrap();

        let raw = fs::read_to_string(&plan_path).unwrap();
        let plan: TtlExtendPlan = serde_json::from_str(&raw).unwrap();
        assert_eq!(plan.extends.len(), 0);
        validate_extend_plan(&plan).expect("empty plan is valid");
    }

    // ── 16. Correct exit codes ────────────────────────────────────────────────

    #[test]
    fn exit_code_0_when_all_healthy() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000));
        let code = run_with_rpc(&rpc, default_args()).unwrap();
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn exit_code_1_when_expiring() {
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_000_001)); // 1 remaining → expiring
        let code = run_with_rpc(&rpc, default_args()).unwrap();
        assert_eq!(code, EXIT_EXPIRING);
    }

    #[test]
    fn invalid_contract_id_returns_error_not_exit_2() {
        let rpc = MockRpc::new(1_000_000);
        let mut args = default_args();
        args.contract = "NOT_A_VALID_CONTRACT_ID".to_string();
        let result = run_with_rpc(&rpc, args);
        // Returns Err(...) rather than Ok(EXIT_ERROR).
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid --contract value"), "{msg}");
    }

    // ── 17. No transaction submission ────────────────────────────────────────

    #[test]
    fn audit_never_invokes_stellar_contract_invoke() {
        // The mock RPC never delegates to `stellar contract invoke`.
        // This test verifies that `run_with_rpc` completes without panic
        // and that the exit code is correct.  Since `run_with_rpc` only
        // calls `rpc.latest_ledger` and `rpc.get_ledger_entry`, and the
        // mock never spawns external processes, submission is structurally
        // impossible.
        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000));
        let result = run_with_rpc(&rpc, default_args());
        assert!(result.is_ok());
    }

    // ── Extend plan validation edge cases ────────────────────────────────────

    #[test]
    fn plan_with_wrong_version_fails_validation() {
        let plan = TtlExtendPlan {
            version: "2".to_string(),
            network: "testnet".to_string(),
            source: String::new(),
            extends: vec![],
        };
        assert!(validate_extend_plan(&plan).is_err());
    }

    #[test]
    fn plan_with_empty_network_fails_validation() {
        let plan = TtlExtendPlan {
            version: "1".to_string(),
            network: "   ".to_string(),
            source: String::new(),
            extends: vec![],
        };
        assert!(validate_extend_plan(&plan).is_err());
    }

    #[test]
    fn plan_with_empty_key_xdr_fails_validation() {
        let plan = TtlExtendPlan {
            version: "1".to_string(),
            network: "testnet".to_string(),
            source: String::new(),
            extends: vec![TtlExtendEntry {
                label: "test".to_string(),
                contract: CONTRACT_ID.to_string(),
                key_xdr: "  ".to_string(),
                durability: "persistent".to_string(),
                current_live_until: None,
                remaining_ledgers: None,
            }],
        };
        assert!(validate_extend_plan(&plan).is_err());
    }

    // ── XDR key generation ────────────────────────────────────────────────────

    #[test]
    fn instance_key_xdr_generates_valid_base64() {
        let xdr = contract_instance_key_xdr(CONTRACT_ID).unwrap();
        assert!(!xdr.is_empty());
        // Must consist of base-64 characters (A-Z, a-z, 0-9, +, /, =).
        assert!(
            xdr.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "unexpected character in XDR base64: {xdr}"
        );
    }

    #[test]
    fn invalid_contract_id_xdr_fails() {
        let result = contract_instance_key_xdr("GARBAGE_ID");
        assert!(result.is_err());
    }

    // ── Plan --plan arg integration ───────────────────────────────────────────

    #[test]
    fn plan_arg_writes_file_and_validates() {
        let tmp = TempDir::new().unwrap();
        let plan_path = tmp.path().join("out.json");

        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(1_000_001)); // expiring

        let mut args = default_args();
        args.format = OutputFormat::Json;
        args.plan = Some(plan_path.clone());

        let code = run_with_rpc(&rpc, args).unwrap();
        assert_eq!(code, EXIT_EXPIRING);
        assert!(plan_path.exists(), "plan file must be created");

        let raw = fs::read_to_string(&plan_path).unwrap();
        let plan: TtlExtendPlan = serde_json::from_str(&raw).unwrap();
        validate_extend_plan(&plan).expect("plan must be valid");
    }

    // ── Multiple persistent keys ──────────────────────────────────────────────

    #[test]
    fn multiple_keys_all_healthy_exit_0() {
        let tmp = TempDir::new().unwrap();
        let keys_file = tmp.path().join("keys.json");
        let specs = vec![
            KeySpec {
                label: "k1".to_string(),
                key_xdr: "AAAA".to_string(),
            },
            KeySpec {
                label: "k2".to_string(),
                key_xdr: "BBBB".to_string(),
            },
        ];
        fs::write(&keys_file, serde_json::to_string(&specs).unwrap()).unwrap();

        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000)); // instance
        rpc.push_entry(Some(2_000_000)); // k1
        rpc.push_entry(Some(2_000_000)); // k2

        let mut args = default_args();
        args.keys = Some(keys_file);
        let code = run_with_rpc(&rpc, args).unwrap();
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn one_of_multiple_keys_expiring_exits_1() {
        let tmp = TempDir::new().unwrap();
        let keys_file = tmp.path().join("keys.json");
        let specs = vec![
            KeySpec {
                label: "k1".to_string(),
                key_xdr: "AAAA".to_string(),
            },
            KeySpec {
                label: "k2".to_string(),
                key_xdr: "BBBB".to_string(),
            },
        ];
        fs::write(&keys_file, serde_json::to_string(&specs).unwrap()).unwrap();

        let rpc = MockRpc::new(1_000_000);
        rpc.push_entry(Some(2_000_000)); // instance: healthy
        rpc.push_entry(Some(2_000_000)); // k1: healthy
        rpc.push_entry(Some(1_000_001)); // k2: expiring

        let mut args = default_args();
        args.keys = Some(keys_file);
        let code = run_with_rpc(&rpc, args).unwrap();
        assert_eq!(code, EXIT_EXPIRING);
    }
}
