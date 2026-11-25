use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct BuildArgs {
    /// Crate to build; builds the whole workspace when omitted.
    #[arg(short, long)]
    pub package: Option<String>,
    #[arg(short, long, default_value_t = false)]
    pub release: bool,
    #[arg(short, long, default_value_t = false)]
    pub all_targets: bool,
    /// Build release WASM for Soroban contracts using wasm32v1-none.
    #[arg(long, default_value_t = false)]
    pub wasm: bool,
    /// Check built WASM artifacts against the 150,000-byte contract budget (implies --wasm).
    #[arg(long, default_value_t = false)]
    pub check_size: bool,
}

#[derive(Args, Debug, Clone)]
pub struct LintArgs {
    #[arg(short, long, default_value_t = false)]
    pub fix: bool,
}

#[derive(Args, Debug, Clone)]
pub struct TestArgs {
    /// Crate to test; tests the whole workspace when omitted.
    #[arg(short, long)]
    pub package: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DeployArgs {
    pub wasm: String,
    #[arg(short, long, default_value = "testnet")]
    pub network: String,
    #[arg(short, long)]
    pub source: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct NewArgs {
    /// Contract name (alphanumeric, hyphens, or underscores).
    pub name: String,
    /// Destination directory path for the new contract.
    #[arg(short, long)]
    pub path: Option<std::path::PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct ScaffoldValidateArgs {
    /// Path to a scaffolded contract directory; defaults to the current directory.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct VerifyArgs {
    /// Path to a local WASM artifact to verify against a deterministic rebuild.
    #[arg(long)]
    pub wasm: Option<String>,
    /// Expected SHA-256 of the rebuilt artifact (hex, lowercase).
    #[arg(long)]
    pub expected: Option<String>,
    /// Provenance manifest to validate; defaults to `provenance-manifest.json`.
    #[arg(long, default_value = "provenance-manifest.json")]
    pub manifest: String,
    /// Crate to rebuild; inferred from the WASM file name when omitted.
    #[arg(short, long)]
    pub package: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct InvokeArgs {
    /// Contract address or alias
    #[arg(long)]
    pub contract: String,
    /// Function name to invoke
    #[arg(long)]
    pub function: String,
    /// Function arguments (key=value format)
    #[arg(long = "arg", action = clap::ArgAction::Append)]
    pub args: Vec<String>,
    /// Network to use
    #[arg(long, default_value = "testnet")]
    pub network: String,
    /// Source account
    #[arg(long)]
    pub source: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct EventsArgs {
    /// Contract address or alias
    #[arg(long)]
    pub contract: String,
    /// Starting ledger number
    #[arg(long)]
    pub since: Option<u64>,
    /// Event type filter
    #[arg(long = "type")]
    pub event_type: Option<String>,
    /// Network to use
    #[arg(long, default_value = "testnet")]
    pub network: String,
}

#[derive(Args, Debug, Clone)]
pub struct InspectArgs {
    /// Contract address or alias.
    pub contract: String,
    /// Specific DataKey entries to inspect (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub keys: Vec<String>,
    /// Output as JSON for programmatic use.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Network to use.
    #[arg(long, default_value = "testnet")]
    pub network: String,
    /// Source account.
    #[arg(long)]
    pub source: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    /// Optional WASM artifact to validate as a regular, non-empty file.
    #[arg(long, value_name = "PATH")]
    pub wasm: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct PlanArgs {
    /// Path to the plan JSON file to execute.
    #[arg(value_name = "PLAN")]
    pub plan: PathBuf,
    /// Preview the plan without executing it; writes a `.dryrun.json` report.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Execute without an interactive confirmation prompt.
    #[arg(long, default_value_t = false)]
    pub force: bool,
    /// Write a JSON execution receipt to this path instead of `<plan>.receipt.json`.
    #[arg(long, value_name = "PATH")]
    pub receipt: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct TtlAuditArgs {
    /// Contract ID to audit.
    #[arg(long)]
    pub contract: String,
    /// Network profile to use (e.g. `testnet`, `mainnet`).
    #[arg(long, default_value = "testnet")]
    pub network: String,
    /// JSON file listing persistent storage key specifications to audit.
    #[arg(long, value_name = "FILE")]
    pub keys: Option<PathBuf>,
    /// Minimum remaining TTL in ledgers; entries below this value are flagged.
    #[arg(long, default_value_t = 100_000)]
    pub threshold: u32,
    /// Output format.
    #[arg(long, value_name = "FORMAT", default_value = "table")]
    pub format: OutputFormat,
    /// Write an extend-TTL plan JSON file to this path for all expiring entries.
    #[arg(long, value_name = "FILE")]
    pub plan: Option<PathBuf>,
}

/// Output format selection.
#[derive(Debug, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}
