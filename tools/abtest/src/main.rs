mod compare;
mod deploy;
mod manifest;
mod market;
mod market_call;
mod market_explorer;
mod market_guides;
mod market_history;
mod market_query;
mod market_status;
mod observe;
mod registry;
mod release;
mod report;
mod run;
mod scenario;
mod stellar;
mod timeline;
mod tui;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};

use crate::deploy::{
    plan_network, publication_is_unsettled, read_deployment_plan, DeployAct, DeployEvent,
    MarketDeployOptions, MarketDeployer, MarketExecutionContext, MarketJournal, MARKET_PLAN_FILE,
};
use crate::manifest::{
    expand_home, MarketConfig, MarketManifest, MarketNetwork, PairConfig,
    PairManifest, MARKET_NETWORK_LABEL, MARKET_TESTNET_PASSPHRASE,
};

use crate::market::{MarketAct, MarketEvent, MarketRunner};
use crate::stellar::{
    Capture, ContractId, Net, Policy, RealExecutor, Safe, Stellar, MARKET_CAPTURE_STDERR_CAP,
    MARKET_CAPTURE_STDOUT_CAP, MARKET_MUTATION_WALL, MARKET_READ_WALL,
};

#[derive(Parser)]
#[command(
    name = "precept-abtest",
    about = "Deploy or explore Stellar testnet markets through the stellar CLI"
)]
struct Cli {
    /// Path to the stellar CLI (>= 23 for pairs, >= 26.1.0 for market commands)
    #[arg(long, env = "STELLAR_BIN", default_value = "stellar")]
    stellar_bin: PathBuf,
    #[arg(
        long,
        env = "STELLAR_RPC_URL",
        default_value = "https://soroban-testnet.stellar.org"
    )]
    rpc_url: String,
    #[arg(
        long,
        env = "STELLAR_NETWORK_PASSPHRASE",
        default_value = "Test SDF Network ; September 2015"
    )]
    passphrase: String,
    /// Run directory. Defaults to the most recent one under ./runs
    #[arg(long)]
    out: Option<PathBuf>,
    /// Identity directory holding the operator's `stellar network add`
    /// entries. A completed mainnet run is read through the entry for its
    /// own frozen network, never through `--rpc-url`, whose value could
    /// carry a provider key into argv.
    #[arg(long, global = true, default_value = "~/.config/stellar")]
    operator_config_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Deploy both sides and write manifest.toml
    Deploy {
        #[arg(long)]
        pair: PathBuf,
    },
    /// Run scenarios against the deployed pair
    Run {
        /// Comma-separated canonical subset of exactly one isolated scenario
        /// family, on a fresh deployment for that family; there is
        /// deliberately no "all"
        #[arg(long)]
        scenario: Option<String>,
        /// Resume from this step label (skips earlier steps)
        #[arg(long)]
        from_step: Option<String>,
        /// Emit one JSON object per event on stdout instead of progress lines
        #[arg(long)]
        json: bool,
    },
    /// Write report.md from retained evidence
    Report,
    /// List run directories, newest first
    List,
    /// Operator terminal: drive a run, or watch one read-only
    Tui {
        /// Driving requires this; a comma-separated canonical subset of one
        /// family. `--watch` may omit it
        #[arg(long)]
        scenario: Option<String>,
        #[arg(long)]
        from_step: Option<String>,
        /// Never take the run lock; browse or follow read-only
        #[arg(long)]
        watch: bool,
    },
    /// Single protected testnet market: deploy, resume, TUI, play, or explore contracts
    Market {
        #[command(subcommand)]
        cmd: MarketCmd,
    },
}

/// The exact 64-hex SHA-256 of a run's frozen `deployment-plan.toml`.
/// Parsed once, at the argv boundary, so no later path can carry an
/// approval that is not a digest at all.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlanDigest(String);

impl FromStr for PlanDigest {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let digest = raw.to_ascii_lowercase();
        if digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(digest))
        } else {
            Err("--approve-plan needs the 64-hex sha256 of the run's deployment-plan.toml".into())
        }
    }
}

/// Non-interactive drive switches, shared by `market deploy` and `market
/// resume`. Approval binds to the frozen plan file, never to a `y` line:
/// `--approve-plan` is the digest `--robot` printed for that same file, and
/// it is re-derived before every send.
#[derive(clap::Args, Clone, Debug, Default)]
struct RobotArgs {
    /// Never read stdin: print the frozen plan and stop before the first
    /// confirmation, or run it under --approve-plan
    #[arg(long)]
    robot: bool,
    /// 64-hex sha256 of the run's frozen deployment-plan.toml; every
    /// confirmation is bound to that file and re-checked before each send
    #[arg(long, requires = "robot", value_name = "SHA256")]
    approve_plan: Option<PlanDigest>,
}

#[derive(Subcommand)]
enum MarketCmd {
    /// Deploy one market from a market config into a fresh run directory
    Deploy {
        /// Path to the market config TOML (the lab stack); exclusive with --config
        #[arg(long, conflicts_with = "config", required_unless_present = "config")]
        market: Option<PathBuf>,
        /// A precept pool config (registry/configs/<pool>.json): deploy a real market
        #[arg(long, requires = "network")]
        config: Option<PathBuf>,
        /// The network a real market deploys to; refused unless the config names it too
        #[arg(long, value_enum, requires = "config")]
        network: Option<MarketNetworkArg>,
        /// End a real run with the pool activated (set_status 0) instead of leaving it for review
        #[arg(long, requires = "config")]
        activate: bool,
        /// Directory holding the Wasm bytes of a digest-pinned launch profile
        #[arg(long, requires = "config")]
        wasm_dir: Option<PathBuf>,
        /// Continue a real run in the existing --out directory: its frozen plan
        /// must match this config, and every open step is reconciled before
        /// anything is resent. Without it, a real deploy needs a fresh directory
        #[arg(long, requires = "config")]
        resume: bool,
        /// Existing stellar CLI identity alias used as the market administrator
        #[arg(long)]
        admin_identity: String,
        /// Explicit stellar CLI config directory holding that identity
        #[arg(long)]
        admin_config_dir: PathBuf,
        /// Override a release-form role's version: <role>=<version|latest>.
        /// Applies only to roles configured in release form
        #[arg(long = "artifact-version", value_name = "ROLE=VERSION")]
        artifact_version: Vec<release::VersionOverride>,
        /// Path to a precept checkout to record this deployment into
        #[arg(long, conflicts_with = "no_registry")]
        registry: Option<PathBuf>,
        /// Do not record this deployment anywhere
        #[arg(long)]
        no_registry: bool,
        #[command(flatten)]
        robot: RobotArgs,
    },
    /// Resume a partial market deployment under its declared reconciliation proofs
    Resume {
        #[arg(long)]
        market: PathBuf,
        #[arg(long)]
        admin_identity: String,
        #[arg(long)]
        admin_config_dir: PathBuf,
        /// Re-request a version for a release-form role; a resumed run is
        /// frozen on its resolved version, so only that one (or `latest`)
        /// is accepted
        #[arg(long = "artifact-version", value_name = "ROLE=VERSION")]
        artifact_version: Vec<release::VersionOverride>,
        /// Path to a precept checkout to record this deployment into
        #[arg(long, conflicts_with = "no_registry")]
        registry: Option<PathBuf>,
        /// Do not record this deployment anywhere
        #[arg(long)]
        no_registry: bool,
        #[command(flatten)]
        robot: RobotArgs,
    },
    /// Drive deployment, browse completed markets, or open the guarded explorer
    Tui {
        #[arg(long, value_enum)]
        mode: MarketTuiMode,
        #[arg(long)]
        market: Option<PathBuf>,
        #[arg(long)]
        admin_identity: Option<String>,
        #[arg(long)]
        admin_config_dir: Option<PathBuf>,
        /// Optional signing alias for explore mode; omitted means reads only
        #[arg(long)]
        signer: Option<String>,
        /// Explicit Stellar identity directory when --signer is provided
        #[arg(long)]
        signer_config_dir: Option<PathBuf>,
    },
    /// Headless counterpart to post-deploy TUI market actions
    Play {
        #[arg(long)]
        market: PathBuf,
        #[arg(long)]
        admin_identity: String,
        #[arg(long)]
        admin_config_dir: PathBuf,
        /// Read-only snapshot of the deployed market
        #[arg(long)]
        snapshot: bool,
        /// Acknowledge the current halt; no chain call
        #[arg(long = "continue")]
        cont: bool,
        /// Stop the market runner
        #[arg(long)]
        halt: bool,
        /// Run one market action by its palette step index
        #[arg(long)]
        step: Option<usize>,
    },
    /// List the contract roles and ids frozen in a completed market manifest
    Contracts,
    /// Inspect a bound market contract's live interface
    Inspect {
        role: String,
        method: Option<String>,
    },
    /// Attach a caller-provided contract to a new non-reserved market role
    Attach { role: String, contract_id: String },
    /// Read market contract state through the pinned market CLI seam
    Query {
        /// Contract role bound in the completed market manifest
        role: String,
        /// Function visible in the target contract's live interface
        method: String,
        /// Read argument as name=value; repeat once per argument
        #[arg(long = "arg")]
        args: Vec<String>,
    },
    /// Simulate any live-ABI function, or explicitly confirm one journaled send
    Call {
        role: String,
        method: String,
        #[arg(long = "arg")]
        args: Vec<String>,
        #[arg(long)]
        signer: String,
        #[arg(long)]
        signer_config_dir: PathBuf,
        /// After preview, require a fresh exact confirmation on stdin
        #[arg(long)]
        execute: bool,
    },
    /// List optional recipes; generic call is never restricted to this list
    Guides,
    /// Prepare a manifest-bound recipe through the same generic guarded call
    Guide {
        category: String,
        action: String,
        #[arg(long)]
        asset: Option<String>,
        #[arg(long)]
        source: Option<usize>,
        #[arg(long = "arg")]
        args: Vec<String>,
        #[arg(long)]
        signer: String,
        #[arg(long)]
        signer_config_dir: PathBuf,
        #[arg(long)]
        execute: bool,
    },
    /// Read the market health and price readiness report
    Status,
    /// Settle an open invoke intent proven never applied by consumed account sequences
    Settle {
        /// Journal coordinate, PHASE-SLOT (for example 900-45)
        coordinate: String,
        /// Unrelated transaction hash from the signer's contiguous sequence history
        #[arg(long = "consumed-by", required = true)]
        consumed_by: Vec<String>,
    },
    /// Replay a real-market coordinate proven never applied by consumed account sequences
    Replay {
        /// Journal coordinate, PHASE-SLOT (for example 3-07)
        coordinate: String,
        /// Unrelated transaction hash from the signer's contiguous sequence history
        #[arg(long = "consumed-by", required = true)]
        consumed_by: Vec<String>,
        /// Identity directory holding the operator's network entry
        #[arg(long)]
        admin_config_dir: PathBuf,
    },
    /// Read market events for a bound role from exactly one position
    Events {
        /// Contract role bound in the completed market manifest
        role: String,
        /// Opaque continuation cursor handed back as `next_cursor` by a
        /// prior page; exactly one of `--cursor` or `--start-ledger`
        #[arg(long)]
        cursor: Option<String>,
        /// First ledger sequence to read; exactly one of `--cursor` or
        /// `--start-ledger`
        #[arg(long)]
        start_ledger: Option<u32>,
        /// Number of events to read, 1 through 100; defaults to 100
        #[arg(long)]
        count: Option<u32>,
    },
    /// Read a transaction receipt by its 64-hex-character hash
    Receipt {
        /// Transaction hash, 64 hex characters
        hash: String,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum MarketTuiMode {
    Deploy,
    Resume,
    Watch,
    Explore,
}

/// The network a real market deploy targets. The labels are the registry's;
/// the resolution is `MarketNetwork::from_label`, so a `--network` value and
/// a pool config's own `network` field go through the same parser.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum MarketNetworkArg {
    Testnet,
    Mainnet,
}

impl MarketNetworkArg {
    fn network(self) -> anyhow::Result<MarketNetwork> {
        MarketNetwork::from_label(match self {
            Self::Testnet => MARKET_NETWORK_LABEL,
            Self::Mainnet => MarketNetwork::Mainnet.label(),
        })
    }
}

/// Run-directory markers owned by a pair run. Their presence says a
/// directory is spoken for by the A/B machinery.
pub(crate) const PAIR_RUN_MARKERS: [&str; 4] =
    ["manifest.toml", "state.json", "pending", "evidence"];

/// Run-directory markers owned by a protected-market run.
pub(crate) const MARKET_RUN_MARKERS: [&str; 5] = [
    "deployment-plan.toml",
    "market-state.json",
    "market-manifest.toml",
    "market-pending",
    "market-evidence",
];

fn marker_conflict(out: &Path, markers: &[&'static str]) -> Option<&'static str> {
    markers.iter().copied().find(|m| out.join(m).exists())
}

/// Market run directories are not pair run directories, and vice versa.
/// Every market command calls this before any executor or context is
/// constructed; a directory holding either shape of prior run state is
/// rejected outright.
pub(crate) fn reject_pair_markers(out: &Path) -> anyhow::Result<()> {
    if let Some(marker) = marker_conflict(out, &PAIR_RUN_MARKERS) {
        anyhow::bail!(
            "selected run directory holds a pair deployment ({}); market commands need a market run directory",
            marker
        );
    }
    Ok(())
}

/// The other direction of the same wall: pair deploy/run/report/tui/list
/// reject any directory carrying market markers, checked before any
/// executor call.
pub(crate) fn reject_market_markers(out: &Path) -> anyhow::Result<()> {
    if let Some(marker) = marker_conflict(out, &MARKET_RUN_MARKERS) {
        anyhow::bail!(
            "run dir {} holds a protected-market deployment ({}); pair commands need a fresh pair run directory",
            out.display(),
            marker
        );
    }
    Ok(())
}

fn main() {
    std::process::exit(match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    })
}

/// Run directories under ./runs, newest first. Market run directories
/// carry no `manifest.toml`, so a pair `list` never even reports them —
/// part of the two-way directory rejection required before any executor
/// use; the explicit check below covers `--out` pointing at one.
fn find_runs() -> Vec<PathBuf> {
    let mut runs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir("runs")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("manifest.toml").exists())
        .map(|e| {
            let t = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (t, e.path())
        })
        .collect();
    runs.sort_by_key(|a| std::cmp::Reverse(a.0));
    runs.into_iter().map(|(_, p)| p).collect()
}

fn list_runs() -> anyhow::Result<i32> {
    let runs = find_runs();
    if runs.is_empty() {
        println!("no runs under ./runs (deploy one first)");
        return Ok(0);
    }
    println!("{:<22} {:>6}  {:<12}  OWNER", "RUN", "STEPS", "PROFILE");
    for r in runs {
        let steps = walkdir_count(&r.join("evidence"));
        let profile = manifest::PairManifest::load(&r)
            .map(|m| format!("{:?}", m.profile))
            .unwrap_or_else(|_| "?".into());
        let owner = match manifest::RunLock::owner(&r) {
            Some(pid) => format!("running (pid {pid})"),
            None => "idle".into(),
        };
        println!(
            "{:<22} {:>6}  {:<12}  {}",
            r.file_name().unwrap_or_default().to_string_lossy(),
            steps,
            profile,
            owner
        );
    }
    Ok(0)
}

fn walkdir_count(evidence: &std::path::Path) -> usize {
    std::fs::read_dir(evidence)
        .into_iter()
        .flatten()
        .flatten()
        .map(|sc| {
            std::fs::read_dir(sc.path())
                .into_iter()
                .flatten()
                .flatten()
                .count()
        })
        .sum()
}

fn real_main() -> anyhow::Result<i32> {
    let cli = Cli::parse();
    // Two-way marker wall: pair commands reject market dirs here (market
    // commands reject pair dirs inside their own handlers — see
    // `market_out`/`reject_pair_markers`). Market play/watch must be able
    // to name their own completed run directory, so the market-marker
    // rejection applies only on the pair path below.
    let is_market = matches!(cli.cmd, Cmd::Market { .. });
    if !is_market {
        if let Some(p) = &cli.out {
            reject_market_markers(p)?;
        }
    }
    if matches!(cli.cmd, Cmd::List) {
        return list_runs();
    }
    if let Cmd::Market { cmd } = &cli.cmd {
        return market_main(&cli, cmd);
    }
    let chosen = match &cli.out {
        Some(p) => p.clone(),
        None => find_runs().into_iter().next().context(
            "no --out given and no run found under ./runs; deploy one first, or pass --out",
        )?,
    };
    std::fs::create_dir_all(&chosen)?;
    let out = chosen.canonicalize()?;
    // Two-way run-directory wall, before any executor is constructed: a
    // market deployment owns this directory or nothing here speaks for it.
    reject_market_markers(&out)?;
    let net = Net {
        rpc_url: cli.rpc_url,
        passphrase: cli.passphrase,
        config_dir: out.join(".stellar"),
        headers: Vec::new(),
    };
    let exec = RealExecutor {
        bin: cli.stellar_bin.clone(),
        xdg: out.join(".xdg"),
        cwd: out.clone(),
        capture: Capture::Pair,
        // Pair transport stays argv: no endpoint env for pair children.
        rpc_env: Vec::new(),
    };
    let st = Stellar::new(&net, &exec, Policy::Pair);
    match cli.cmd {
        Cmd::Deploy { pair } => {
            // A run dir with prior state would "resume" steps against contracts that
            // never ran them: deploy only into a fresh dir.
            anyhow::ensure!(
                !out.join("manifest.toml").exists()
                    && !out.join("state.json").exists()
                    && !out.join("pending").exists()
                    && !out.join("evidence").exists(),
                "run dir {} already holds a deployment; deploy to a new --out (e.g. runs/$(date +%s))",
                out.display()
            );
            st.assert_version()?;
            let cfg = PairConfig::load(&pair)?;
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let m = deploy::deploy_pair(&st, &cfg, nonce)?;
            m.save(&out)?;
            println!(
                "deployed: a={} b={} manifest={}",
                m.a.contracts.pool,
                m.b.contracts.pool,
                PairManifest::path(&out).display()
            );
        }
        Cmd::Tui {
            scenario,
            from_step,
            watch,
        } => {
            if !watch && scenario.as_deref().unwrap_or("").is_empty() {
                anyhow::bail!(
                    "driving the TUI requires --scenario; use --watch to browse a run owned \
                     elsewhere; {}",
                    scenario::SELECTION_GUIDANCE
                );
            }
            return tui::main(
                &out,
                cli.stellar_bin,
                net,
                scenario.unwrap_or_default(),
                from_step,
                watch,
            );
        }
        Cmd::Run {
            scenario,
            from_step,
            json,
        } => {
            let _lock = manifest::RunLock::acquire(&out)?.with_context(|| {
                format!(
                    "run {} is owned by pid {}",
                    out.display(),
                    manifest::RunLock::owner(&out).unwrap_or(0)
                )
            })?;
            let m = PairManifest::load(&out).context("no manifest; run deploy first")?;
            let mut r = run::Runner::new(&st, &m, &out)?;
            let emit = |ev: run::Ev| -> run::Act {
                println!("{}", run::json_line(Policy::Pair, &ev));
                // No operator exists in --json mode: a halt is final, and it must
                // exit 1 when it was a tool failure rather than a semantic stop.
                match ev {
                    run::Ev::Halt { .. } => run::Act::Abort,
                    _ => run::Act::Continue,
                }
            };
            if json {
                r.hook = Some(&emit);
            }
            r.run(scenario.as_deref(), from_step.as_deref())?;
            let steps = report::load_steps(&out, &m)?;
            let verdict = report::write_markdown(&out, &m, &steps)?;
            println!("{verdict}: {}", out.join("report.md").display());
            if let Some(reason) = &r.fatal {
                eprintln!("tool failure: {reason}");
                return Ok(1);
            }
            return Ok(report::verdict_code(&verdict));
        }
        Cmd::List => unreachable!("handled above"),
        Cmd::Report => {
            let m = PairManifest::load(&out)?;
            let steps = report::load_steps(&out, &m)?;
            let verdict = report::write_markdown(&out, &m, &steps)?;
            println!("{verdict}: {}", out.join("report.md").display());
            return Ok(report::verdict_code(&verdict));
        }
        Cmd::Market { .. } => unreachable!("handled above"),
    }
    Ok(0)
}

fn market_out(cli: &Cli) -> anyhow::Result<PathBuf> {
    let chosen = cli
        .out
        .clone()
        .context("market commands require an explicit --out run directory")?;
    // The two-way wall is a create-new style check on the chosen path
    // before any directory is created, executor built, or context made.
    reject_pair_markers(&chosen)?;
    std::fs::create_dir_all(&chosen)?;
    let out = chosen.canonicalize()?;
    reject_pair_markers(&out)?;
    Ok(out)
}

fn market_context(
    cli: &Cli,
    out: &Path,
    alias: &str,
    admin_config_dir: &Path,
) -> anyhow::Result<MarketExecutionContext> {
    market_context_with_rpc(cli, out, alias, admin_config_dir, cli.rpc_url.clone(), Vec::new())
}

/// A real market's RPC comes from the operator's own `stellar network add`
/// entry for the frozen network, never from `--rpc-url`: a mainnet URL may
/// carry a provider key that must not reach argv, and the entry has to quote
/// exactly the network the config is frozen to.
fn real_market_context(
    cli: &Cli,
    out: &Path,
    alias: &str,
    admin_config_dir: &Path,
    network: MarketNetwork,
) -> anyhow::Result<MarketExecutionContext> {
    let entry = stellar::cli_network(admin_config_dir, network.label())?;
    anyhow::ensure!(
        entry.passphrase == network.passphrase(),
        "the operator's '{}' network entry quotes a different network passphrase",
        network.label()
    );
    Ok(
        market_context_with_rpc(cli, out, alias, admin_config_dir, entry.rpc_url, entry.headers)?
            .with_network(network),
    )
}

fn market_context_with_rpc(
    cli: &Cli,
    out: &Path,
    alias: &str,
    admin_config_dir: &Path,
    rpc_url: String,
    rpc_headers: Vec<(String, String)>,
) -> anyhow::Result<MarketExecutionContext> {
    // SEC001: deployment, resumption, play, and TUI mutations must use the
    // canonical resolved CLI executable and a controlled, dotenv-free
    // working directory — never the caller-selected binary or a
    // caller-supplied run directory that a planted `.env` could hijack.
    // The executable is resolved from the invocation context before any
    // child could start; a planted `.env` anywhere on the boundary refuses
    // the whole context before executor construction (fail closed).
    let bin = market_explorer_bin(&cli.stellar_bin)?;
    anyhow::ensure!(
        std::fs::symlink_metadata(Path::new("/").join(".env")).is_err()
            && std::fs::symlink_metadata(out.join(".env")).is_err(),
        "{}",
        Safe::literal(MARKET_CWD_UNSAFE)
    );
    MarketExecutionContext::new(
        rpc_url,
        rpc_headers,
        alias.to_string(),
        admin_config_dir.to_path_buf(),
        out.join(".stellar"),
        bin,
        out.join(".xdg"),
        PathBuf::from("/"),
    )
}

/// The network a completed manifest froze. Read surfaces take their
/// network from the run's own bytes: a manifest frozen to either lawful
/// network is readable, anything else refuses. Rejections are input-free.
fn market_manifest_network(manifest: &MarketManifest) -> anyhow::Result<MarketNetwork> {
    MarketNetwork::from_pair(
        &manifest.network.passphrase.to_string(),
        &manifest.network.label.to_string(),
    )
    .map_err(|_| anyhow::anyhow!("{}", Safe::literal("market manifest network is unknown")))
}

/// Fixed market-execution rejections. Both are input-free: the caller's
/// path or network bytes are never echoed back.
const MARKET_CLI_UNSAFE: &str = "market stellar CLI could not be resolved safely";
const MARKET_CWD_UNSAFE: &str = "market execution cwd is not a controlled dotenv-free directory";

/// The alias a read-only market surface passes to the CLI. Reads never
/// sign, so no identity by this name need exist; the CLI only requires the
/// flag to be present, and this value is never used or displayed.
const MARKET_OPERATOR_ALIAS: &str = "market-read-only";

/// Resolve the caller-supplied market CLI once, before changing to the
/// controlled execution directory. Relative paths are relative to the
/// invocation directory; bare names search only absolute PATH entries.
/// Every market child receives an absolute regular-file executable.
fn market_explorer_bin(requested: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    anyhow::ensure!(
        !requested.as_os_str().is_empty(),
        "{}",
        Safe::literal(MARKET_CLI_UNSAFE)
    );
    let selected = if requested.is_absolute()
        || requested
            .to_string_lossy()
            .contains(std::path::MAIN_SEPARATOR)
    {
        requested.to_path_buf()
    } else {
        std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .filter(|dir| dir.is_absolute())
                    .map(|dir| dir.join(requested))
                    .find(|candidate| {
                        candidate.metadata().is_ok_and(|meta| {
                            meta.is_file() && meta.permissions().mode() & 0o111 != 0
                        })
                    })
            })
            .ok_or_else(|| anyhow::anyhow!("{}", Safe::literal(MARKET_CLI_UNSAFE)))?
    };
    let resolved = selected
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("{}", Safe::literal(MARKET_CLI_UNSAFE)))?;
    anyhow::ensure!(
        resolved
            .metadata()
            .is_ok_and(|meta| { meta.is_file() && meta.permissions().mode() & 0o111 != 0 }),
        "{}",
        Safe::literal(MARKET_CLI_UNSAFE)
    );
    Ok(resolved)
}

/// The CLI auto-loads `.env` from its working directory, and a
/// `STELLAR_CONTRACT_ID` in one hijacks the `--id` that binds every
/// market read to the completed manifest. Market reads therefore run
/// from the controlled root, never a caller-supplied run directory,
/// and fail before any network call when that isolation is not real.
///
/// The network is the one the completed run's manifest froze, never the
/// caller's choice: a mainnet run's endpoint comes from the operator's own
/// `stellar network add` entry for mainnet — the same path a mainnet
/// deploy takes — because a mainnet URL may carry a provider key that must
/// not reach argv.
fn market_explorer_net(
    cli: &Cli,
    out: &Path,
    network: MarketNetwork,
) -> anyhow::Result<(Net, RealExecutor)> {
    if network == MarketNetwork::Mainnet {
        let dir = expand_home(&cli.operator_config_dir);
        let ctx = real_market_context(cli, out, MARKET_OPERATOR_ALIAS, &dir, network)?;
        ctx.assert_network(network)?;
        return Ok(ctx.read_net());
    }
    anyhow::ensure!(
        cli.passphrase == MARKET_TESTNET_PASSPHRASE,
        "{}",
        Safe::literal("market execution network is not the exact testnet")
    );
    let bin = market_explorer_bin(&cli.stellar_bin)?;
    anyhow::ensure!(
        std::fs::symlink_metadata(std::path::Path::new("/").join(".env")).is_err(),
        "{}",
        Safe::literal(MARKET_CWD_UNSAFE)
    );
    let net = Net {
        rpc_url: cli.rpc_url.clone(),
        passphrase: cli.passphrase.clone(),
        config_dir: out.join(".stellar"),
        headers: Vec::new(),
    };
    let exec = RealExecutor {
        bin,
        xdg: out.join(".xdg"),
        cwd: PathBuf::from("/"),
        capture: Capture::Market {
            stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
            stderr_cap: MARKET_CAPTURE_STDERR_CAP,
            wall: Some(MARKET_READ_WALL),
        },
        rpc_env: crate::stellar::rpc_env_for(&net),
    };
    Ok((net, exec))
}

/// Read surfaces require an existing completed run, never a new directory.
fn market_read_context(cli: &Cli) -> anyhow::Result<(PathBuf, MarketManifest, Net, RealExecutor)> {
    let chosen = cli
        .out
        .as_ref()
        .context("market commands require an explicit --out run directory")?;
    reject_pair_markers(chosen)?;
    let out = chosen
        .canonicalize()
        .context("market reads require an existing run directory")?;
    reject_pair_markers(&out)?;
    // Defense in depth on the dotenv boundary: market execution already
    // runs from the controlled root, so a run-directory `.env` cannot
    // reach the CLI. A market run directory is tool-owned and nothing in
    // this tool ever writes one, so its presence means foreign state was
    // planted where the CLI's `.env` auto-load and `STELLAR_CONTRACT_ID`
    // hijack would have found it. Reject it before any executor exists.
    anyhow::ensure!(
        std::fs::symlink_metadata(out.join(".env")).is_err(),
        "{}",
        Safe::literal(MARKET_CWD_UNSAFE)
    );
    let manifest =
        MarketManifest::load(&out).context("market reads need a completed market run directory")?;
    let network = market_manifest_network(&manifest)?;
    let (net, exec) = market_explorer_net(cli, &out, network)?;
    Ok((out, manifest, net, exec))
}
/// A market read context bound to the network its own manifest froze: the
/// identity proof on every read is that network's, whatever argv says.
fn market_read_stellar<'a>(
    net: &'a Net,
    exec: &'a RealExecutor,
) -> Stellar<'a, RealExecutor> {
    Stellar::new(net, exec, Policy::Market).with_market_network(&net.passphrase)
}

/// Action contexts reuse the read-only testnet/dotenv boundary, then bind
/// native CLI signing to one explicitly selected identity directory.
fn market_action_context(
    cli: &Cli,
    signer_config_dir: &Path,
) -> anyhow::Result<(PathBuf, MarketManifest, Net, RealExecutor)> {
    let (out, manifest, mut net, mut exec) = market_read_context(cli)?;
    let dir = signer_config_dir.canonicalize().map_err(|_| {
        anyhow::anyhow!(
            "{}",
            Safe::literal("market signer directory is unavailable")
        )
    })?;
    anyhow::ensure!(
        dir.is_dir(),
        "{}",
        Safe::literal("market signer directory is unavailable")
    );
    net.config_dir = dir;
    exec.capture = Capture::Market {
        stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
        stderr_cap: MARKET_CAPTURE_STDERR_CAP,
        wall: Some(MARKET_MUTATION_WALL),
    };
    Ok((out, manifest, net, exec))
}

/// A single owner drives deployment, play, or a confirmed action per market
/// run. Watch and read-only exploration deliberately never acquire it.
/// Intentional exclusion: a deploy/resume/play driver must finish or be
/// stopped before another process can prepare an explorer action. Even an
/// emergency governance call cannot interleave its journal writes.
fn market_drive_lock(out: &Path) -> anyhow::Result<manifest::RunLock> {
    manifest::RunLock::acquire(out)
        .map_err(|_| anyhow::anyhow!("{}", Safe::literal("market run lock is unavailable")))?
        .ok_or_else(|| anyhow::anyhow!("{}", Safe::literal("another process owns this market run")))
}

fn market_explorer_call<T>(result: anyhow::Result<T>) -> anyhow::Result<T> {
    result.map_err(|e| anyhow::anyhow!("{}", Safe::error_chain(Policy::Market, &e)))
}

/// Print one deployment event. Headless deployment and the TUI observe the
/// same event grammar; only the presentation differs. A stopped step's
/// violations are printed with its failure: the event's reason says the run
/// stopped, and these say what the step's proof objected to.
fn print_deploy_event(ev: &DeployEvent, violations: &[Safe]) {
    match ev {
        DeployEvent::Phase { id, label } => println!("phase {id}: {label}"),
        DeployEvent::Confirm { summary, .. } => {
            println!("CONFIRM REQUIRED: {summary}");
            println!("nothing proceeds until the operator confirms; type exactly y");
        }
        DeployEvent::Custody { unresolved, note } => {
            println!("custody unresolved={unresolved}: {note}")
        }
        DeployEvent::Done(manifest) => {
            print_market_summary(manifest);
        }
        DeployEvent::Failed(reason) => {
            eprintln!("deployment failed: {reason}");
            for violation in violations {
                eprintln!("  violation: {violation}");
            }
        }
    }
}

fn print_market_summary(m: &MarketManifest) {
    println!(
        "market deployment complete; the market is active on Stellar {} only",
        m.network.label
    );
    println!("network: {}", m.network.label);
    println!("pool administrator (public G-address): {}", m.admin);
    println!("rpc endpoint (redacted): {}", m.rpc_host);
    println!("proxy runtime: {}", m.oracle.runtime);
    if let Some(governance) = &m.oracle.governance {
        println!("proxy governance: {governance}");
    }
    println!("oracle adapter (immutable): {}", m.oracle.adapter);
    for (n, source) in m.oracle.sources.iter().enumerate() {
        println!("oracle source {}: {source}", n + 1);
    }
    println!("oracle base: {}", m.oracle.base);
    println!(
        "single-authority synthetic quorum: {} sources, min_sources {}",
        m.oracle.sources.len(),
        m.oracle.min_sources
    );
    for (tok, id) in &m.tokens {
        println!("token {tok:?}: {id}");
    }
    println!("comet: {}", m.comet);
    println!("emitter: {}", m.emitter);
    println!("backstop: {}", m.backstop);
    println!("factory: {}", m.factory);
    println!("pool: {}", m.pool);
    for (tok, r) in &m.reserves {
        println!("reserve {tok:?}: index {} decimals {}", r.index, r.decimals);
    }
    for (who, id) in &m.identities {
        println!("identity {who:?}: {id}");
    }
    println!("deploy ledger height: {}", m.deploy_ledger);
}

/// Headless pump over the shared deployment state machine. Identical
/// engine to `market tui`; the only difference is that the operator's
/// confirmation arrives on stdin here instead of a TUI key.
fn market_pump(deployer: &mut MarketDeployer) -> anyhow::Result<i32> {
    let mut act = DeployAct::Init;
    loop {
        let ev = match deployer.step(act) {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!(
                    "deployment stopped: {}",
                    stellar::Safe::error_chain(Policy::Market, &e)
                );
                return Ok(1);
            }
        };
        if matches!(&ev, DeployEvent::Confirm { .. }) {
            use std::io::IsTerminal;
            anyhow::ensure!(
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                "market deployment needs an interactive terminal for exact confirmation"
            );
            drain_market_confirmation_input().map_err(|_| {
                anyhow::anyhow!("market deployment not sent: terminal input could not be cleared")
            })?;
        }
        print_deploy_event(&ev, deployer.stopped_violations());
        match ev {
            DeployEvent::Done(_) => return Ok(0),
            DeployEvent::Failed(_) => return Ok(1),
            DeployEvent::Custody {
                unresolved: true, ..
            } => return Ok(1),
            DeployEvent::Confirm { token, .. } => {
                if !confirm_on_stdin()? {
                    let stop = deployer.step(DeployAct::Halt);
                    if let Ok(ev) = stop {
                        print_deploy_event(&ev, deployer.stopped_violations());
                    }
                    return Ok(1);
                }
                act = DeployAct::Confirm { token };
                continue;
            }
            _ => {}
        }
        act = DeployAct::Continue;
    }
}

/// Exit code of `--robot` without an approval: the plan was printed and
/// nothing was sent, so the operator can approve exactly that plan.
const ROBOT_PLAN_EXIT: i32 = 3;
/// Exit code of a refused approval: the frozen plan on disk is not the one
/// that was approved. Nothing was sent.
const ROBOT_REFUSED_EXIT: i32 = 4;

/// Drive one deployment through the shared state machine. Human surfaces
/// keep the interactive pump; a robot surface never looks at stdin.
fn market_drive(deployer: &mut MarketDeployer, robot: &RobotArgs) -> anyhow::Result<i32> {
    match (robot.robot, robot.approve_plan.as_ref()) {
        (false, _) => market_pump(deployer),
        (true, None) => market_pump_robot(deployer, None),
        (true, Some(digest)) => market_pump_robot(deployer, Some(digest)),
    }
}

/// Non-interactive pump. Approval is the frozen plan file's own digest, so
/// the operator authorizes one exact plan rather than a sequence of `y`
/// lines: without `--approve-plan` the run stops at the first confirmation
/// after publishing the plan and its digest and sends nothing; with it, each
/// confirmation is released only while the file on disk still hashes to the
/// approved digest. The token never reaches stdout.
fn market_pump_robot(
    deployer: &mut MarketDeployer,
    approved: Option<&PlanDigest>,
) -> anyhow::Result<i32> {
    let mut act = DeployAct::Init;
    loop {
        let ev = match deployer.step(act) {
            Ok(ev) => ev,
            Err(e) => {
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "stopped",
                        "reason": stellar::Safe::error_chain(Policy::Market, &e).to_string(),
                    })
                );
                return Ok(1);
            }
        };
        let code = match &ev {
            DeployEvent::Done(_) => Some(0),
            DeployEvent::Failed(_) => Some(1),
            DeployEvent::Custody {
                unresolved: true, ..
            } => Some(1),
            _ => None,
        };
        if let Some(code) = code {
            println!("{}", robot_event(&ev, deployer.stopped_violations()));
            return Ok(code);
        }
        match ev {
            DeployEvent::Confirm { token, summary } => {
                let run_dir = deployer.run_dir();
                let current = deployment_plan_digest(&run_dir).ok();
                let Some(approved) = approved else {
                    let Some(digest) = current else {
                        println!(
                            "{}",
                            serde_json::json!({
                                "event": "stopped",
                                "reason": "the frozen deployment plan is unreadable",
                            })
                        );
                        return Ok(1);
                    };
                    println!("{}", plan_line(deployer, &digest)?);
                    println!("{}", robot_confirm(&summary.to_string()));
                    return Ok(ROBOT_PLAN_EXIT);
                };
                if current.as_deref() != Some(approved.0.as_str()) {
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "approval_refused",
                            "run_dir": run_dir.display().to_string(),
                            "approved_sha256": approved.0,
                            "plan_sha256": current,
                            "reason": "the frozen deployment plan is not the approved plan",
                        })
                    );
                    return Ok(ROBOT_REFUSED_EXIT);
                }
                println!("{}", robot_confirm(&summary.to_string()));
                act = DeployAct::Confirm { token };
                continue;
            }
            other => println!(
                "{}",
                robot_event(&other, deployer.stopped_violations())
            ),
        }
        act = DeployAct::Continue;
    }
}

/// One deployment event as a single-line JSON object. The confirmation
/// token is deliberately absent: a robot run is authorized by the plan
/// digest, and no bound token reaches stdout, a log, or a capture. A failure
/// carries the violations that stopped the step, so a consumer sees what the
/// step's own proof objected to rather than a bare exit code.
fn robot_event(ev: &DeployEvent, violations: &[Safe]) -> serde_json::Value {
    match ev {
        DeployEvent::Phase { id, label } => {
            serde_json::json!({"event": "phase", "phase": id, "label": label.to_string()})
        }
        DeployEvent::Confirm { summary, .. } => robot_confirm(&summary.to_string()),
        DeployEvent::Custody { unresolved, note } => serde_json::json!({
            "event": "custody",
            "unresolved": unresolved,
            "note": note.to_string(),
        }),
        DeployEvent::Done(manifest) => serde_json::json!({
            "event": "done",
            "network": manifest.network.label,
            "admin": manifest.admin.to_string(),
            "pool": manifest.pool.to_string(),
            "comet": manifest.comet.to_string(),
            "emitter": manifest.emitter.to_string(),
            "backstop": manifest.backstop.to_string(),
            "factory": manifest.factory.to_string(),
            "deploy_ledger": manifest.deploy_ledger,
        }),
        DeployEvent::Failed(reason) => serde_json::json!({
            "event": "failed",
            "reason": reason.to_string(),
            "violations": violations.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }),
    }
}

fn robot_confirm(summary: &str) -> serde_json::Value {
    serde_json::json!({"event": "confirm", "summary": summary})
}

/// The plan listing a robot approves: the run's directory, the digest of
/// the frozen plan file itself, and every coordinate of the frozen graph
/// with the role it acts on and whether the journal already settled it.
fn plan_line(deployer: &MarketDeployer, digest: &str) -> anyhow::Result<serde_json::Value> {
    let steps: Vec<serde_json::Value> = deployer
        .plan_steps()?
        .into_iter()
        .map(|step| {
            serde_json::json!({
                "phase": step.phase,
                "slot": step.slot,
                "action": step.action,
                "mutation_kind": step.kind,
                "target_role": step.target_role,
                "settled": step.settled,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "event": "plan",
        "run_dir": deployer.run_dir().display().to_string(),
        "plan_sha256": digest,
        "steps": steps,
    }))
}

/// sha256 of the run's frozen `deployment-plan.toml`, over exactly the bytes
/// on disk. The same figure `--robot` prints and `--approve-plan` re-checks
/// before every send: an approval names one file, never a step list.
fn deployment_plan_digest(run_dir: &Path) -> anyhow::Result<String> {
    let path = run_dir.join(MARKET_PLAN_FILE);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("the frozen deployment plan {} is unreadable", path.display()))?;
    Ok(crate::market::sha256_hex(&bytes))
}

/// Exactly one `y` line confirms a deployment gate; anything else —
/// including an empty line (Enter alone) — refuses it. The binding
/// confirmation summary is already on screen via `print_deploy_event`; the
/// token itself is never echoed, so no bound authorization material leaks
/// to the terminal, CI logs, or the PTY capture. The gate re-checks the
/// token, so a stale refusal cannot satisfy a later phase.
fn confirm_on_stdin() -> anyhow::Result<bool> {
    println!("type exactly y then Enter to confirm; anything else refuses this mutation");
    let mut line = String::new();
    let read = std::io::stdin()
        .read_line(&mut line)
        .context("reading confirmation")?;
    let answer = line.trim_end_matches(['\r', '\n']);
    Ok(read > 0 && answer == "y")
}

/// Ceiling for renders of parsed market values, matching the explorer's
/// own bounded render cap. Every market read passes through this cap.
const MARKET_READ_RENDER_CAP: usize = 65_536;

/// Inclusive ceiling for `market events --count`.
const MARKET_EVENTS_MAX_COUNT: u32 = 100;

/// Trust-boundary guard for raw `market query --arg` inputs. Each input
/// is split exactly once at its first `=` into owned name and value, and
/// the collected pairs pass through the shared `market_query` validator
/// before any run context or invoke. Every rejection here is input-free:
/// the caller's bytes are never echoed.
fn parse_market_query_args(args: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    anyhow::ensure!(
        args.len() <= market_query::MAX_QUERY_ARGS,
        "{}",
        Safe::literal("market query accepts at most 32 --arg inputs")
    );
    let mut parsed = Vec::with_capacity(args.len());
    for raw in args {
        let Some((name, value)) = raw.split_once('=') else {
            anyhow::bail!(
                "{}",
                Safe::literal("market query arguments must use --arg name=value")
            );
        };
        parsed.push((name.to_string(), value.to_string()));
    }
    market_query::validate_args(&parsed)?;
    Ok(parsed)
}

/// Discard typeahead before displaying a market send preview. Raw mode makes
/// even an unfinished line readable. Restore line input on failure as well.
fn drain_market_confirmation_input() -> std::io::Result<()> {
    use ratatui::crossterm::{event, terminal};
    terminal::enable_raw_mode()?;
    let drained = (|| {
        while event::poll(std::time::Duration::ZERO)? {
            event::read()?;
        }
        Ok(())
    })();
    let restored = terminal::disable_raw_mode();
    drained.and(restored)
}

/// CLI and TUI use the same move-only action capability. This presenter
/// neither builds transactions nor interprets a failed simulation as consent.
#[allow(clippy::too_many_arguments)]
fn market_action_pump(
    st: &Stellar<'_, RealExecutor>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
    method: &str,
    args: &[(String, String)],
    signer: &str,
    execute: bool,
) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    anyhow::ensure!(
        !execute || (std::io::stdin().is_terminal() && std::io::stdout().is_terminal()),
        "{}",
        Safe::literal("market send requires an interactive terminal for exact confirmation")
    );
    let prepared = market_explorer_call(market_call::prepare(
        st, out, manifest, role, method, args, signer,
    ))?;
    if execute {
        drain_market_confirmation_input().map_err(|_| {
            anyhow::anyhow!(
                "{}",
                Safe::literal("market action not sent: terminal input could not be cleared")
            )
        })?;
    }
    println!("{}", prepared.summary());
    if !execute {
        return Ok(0);
    }
    if !confirm_on_stdin()? {
        eprintln!("market action not sent");
        return Ok(1);
    }
    let token: [u8; 64] =
        prepared.confirmation().as_bytes().try_into().map_err(|_| {
            anyhow::anyhow!("{}", Safe::literal("market confirmation is malformed"))
        })?;
    let token = std::str::from_utf8(&token)
        .map_err(|_| anyhow::anyhow!("{}", Safe::literal("market confirmation is malformed")))?;
    let result = market_explorer_call(market_call::execute(st, out, manifest, prepared, token))?;
    println!("outcome: {}", result.outcome);
    if let Some(value) = result.value {
        let rendered =
            market_explorer_call(Safe::render_market_json(&value, MARKET_READ_RENDER_CAP))?;
        println!("value: {rendered}");
    }
    Ok(if result.accepted { 0 } else { 1 })
}

/// Resolve every release-form pin to verified cache bytes before any chain
/// call, once, at deploy start. A run dir that already holds a frozen plan
/// resumes on the plan's exact versions and never consults the catalog. A
/// path-form configuration takes no network dependency here at all.
fn resolve_market_artifacts(
    cfg: &mut MarketConfig,
    out: &Path,
    overrides: &[release::VersionOverride],
) -> anyhow::Result<()> {
    if overrides.is_empty()
        && !cfg
            .artifacts
            .values()
            .any(|p| matches!(p, crate::manifest::ArtifactPin::Release(_)))
    {
        return Ok(());
    }
    let cache = release::default_cache_dir()?;
    release::resolve_market(cfg, out, overrides, &release::HttpSource::new(), &cache)
}

/// A relative `--wasm-dir` means the directory the operator stood in when
/// they typed it. Resolving it here, once, is what keeps the qualified bytes
/// they supplied addressable for the whole run: the run's own paths are
/// absolute, and a path left relative would be looked up against whatever
/// directory a later step happens to work in — so the run would go looking on
/// the network for code it was handed locally. A directory that cannot be read
/// is refused here, before any send.
fn resolve_wasm_dir(wasm_dir: Option<&Path>) -> anyhow::Result<Option<PathBuf>> {
    wasm_dir
        .map(|dir| {
            dir.canonicalize().map_err(|error| {
                anyhow::anyhow!("--wasm-dir {} is not readable: {error}", dir.display())
            })
        })
        .transpose()
}

/// One-command real market deployment: the reviewed pool config is the
/// authority for the pool, its reserves, and its oracle, and the run's
/// graph comes from that config alone. Nothing here sends: the summary
/// below is printed first, and the first send waits for the same exact
/// `y` every other gated mutation waits for.
#[allow(clippy::too_many_arguments)]
fn market_deploy_real(
    cli: &Cli,
    config: &Path,
    network: Option<&MarketNetworkArg>,
    activate: bool,
    wasm_dir: Option<&Path>,
    resume: bool,
    admin_identity: &str,
    admin_config_dir: &Path,
    registry: Option<&Path>,
    no_registry: bool,
    artifact_version: &[release::VersionOverride],
    robot: &RobotArgs,
) -> anyhow::Result<i32> {
    let Some(network) = network else {
        anyhow::bail!("a real market deploy needs --network: the pool config was reviewed for one");
    };
    let wasm_dir = resolve_wasm_dir(wasm_dir)?;
    let declared = network.network()?;
    let out = market_out(cli)?;
    let _lock = market_drive_lock(&out)?;
    if resume {
        anyhow::ensure!(
            out.join("deployment-plan.toml").exists(),
            "no deployment plan in {}: --resume only continues a real deploy that started",
            out.display()
        );
        if out.join("market-manifest.toml").exists() && !publication_is_unsettled(&out)? {
            // A finished run has nothing left to send; resuming it only
            // performs the registry record a skipped or failed one missed.
            return Ok(registry::record_completed_market(
                registry,
                no_registry,
                &out,
                robot.robot,
            ));
        }
    } else if let Some(marker) = marker_conflict(&out, &MARKET_RUN_MARKERS) {
        anyhow::bail!(
            "run dir {} already holds market state ({}); a real deploy needs a fresh directory, or --resume to continue it",
            out.display(),
            marker
        );
    }
    // The launch profile's release-form roles resolve through the same
    // catalog path the lab branch uses; a digest-pinned role (the shared
    // Comet and emitter, and every other `Live` pin) is never fetched, it
    // is uploaded by hash or found in --wasm-dir.
    let mut cfg = MarketConfig::from_pool_config(config, Some(declared))?;
    resolve_market_artifacts(&mut cfg, &out, artifact_version)?;
    anyhow::ensure!(
        cfg.is_real(),
        "the pool config does not describe a real market"
    );
    let pool = cfg.pool.as_ref().expect("a real config has a pool");
    // A robot run owns stdout: every line it prints there is one JSON event,
    // so the human preamble goes to stderr instead. Everything else is
    // unchanged.
    let say = |line: String| {
        if robot.robot {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    say(if robot.robot {
        "real market deploy: robot mode; the plan and its digest are printed as JSON".to_string()
    } else {
        "real market deploy; nothing is sent until each step is confirmed with an exact y"
            .to_string()
    });
    say(format!("network: {}", declared.label()));
    say(format!("pool: {}", pool.name));
    say(format!(
        "pool administrator (public G-address): {}",
        pool.admin
    ));
    say(format!("operator identity: {admin_identity}"));
    if activate {
        say("activation: the run ends with the pool at set_status(0), where the \
             pool checks the backstop threshold against the deposit"
            .to_string());
    }
    say(format!("reserves: {}", pool.reserves.len()));
    for (index, reserve) in pool.reserves.iter().enumerate() {
        say(format!(
            "reserve {}: {} asset {} decimals {}",
            index, reserve.label, reserve.asset, reserve.decimals
        ));
    }
    match &pool.adapter {
        Some(adapter) => say(format!("oracle adapter: reuse {adapter}")),
        None => say("oracle adapter: deploy".to_string()),
    }
    match &pool.runtime {
        Some(runtime) => say(format!("oracle runtime: reuse {runtime}")),
        None => say("oracle runtime: deploy".to_string()),
    }
    let ctx = real_market_context(cli, &out, admin_identity, admin_config_dir, declared)?;
    ctx.assert_network(declared)?;
    for role in cfg.artifacts.keys() {
        say(format!(
            "code: deploy {role} (uploaded first only if {} lacks it)",
            declared.label()
        ));
    }
    let journal = MarketJournal::open(out.as_path());
    let mut deployer = MarketDeployer::with_options(
        cfg,
        ctx,
        journal,
        MarketDeployOptions {
            activate,
            wasm_dir,
        },
    )?;
    // The operator authorizes the whole graph, so the run states it before
    // its first confirmation rather than leaving the scope inferred.
    say(format!(
        "planned coordinates: {}",
        deployer.planned_coordinates()
    ));
    let code = market_drive(&mut deployer, robot)?;
    if code != 0 {
        return Ok(code);
    }
    Ok(registry::record_completed_market(
        registry,
        no_registry,
        &out,
        robot.robot,
    ))
}
fn market_main(cli: &Cli, cmd: &MarketCmd) -> anyhow::Result<i32> {
    match cmd {
        MarketCmd::Deploy {
            market,
            config,
            network,
            activate,
            wasm_dir,
            resume,
            admin_identity,
            admin_config_dir,
            artifact_version,
            registry,
            no_registry,
            robot,
        } => {
            let out = market_out(cli)?;
            let Some(market) = market.as_ref() else {
                return market_deploy_real(
                    cli,
                    config.as_ref().expect("--config or --market is required"),
                    network.as_ref(),
                    *activate,
                    wasm_dir.as_deref(),
                    *resume,
                    admin_identity,
                    admin_config_dir,
                    registry.as_deref(),
                    *no_registry,
                    artifact_version,
                    robot,
                );
            };
            let _lock = market_drive_lock(&out)?;
            // Fresh-directory rule: no market or pair markers may exist
            // before the first executor call.
            if let Some(marker) = marker_conflict(&out, &MARKET_RUN_MARKERS) {
                anyhow::bail!(
                    "run dir {} already holds market state ({}); market deploy needs a fresh directory",
                    out.display(),
                    marker
                );
            }
            let mut cfg = MarketConfig::load(market)?;
            resolve_market_artifacts(&mut cfg, &out, artifact_version)?;
            let ctx = market_context(cli, &out, admin_identity, admin_config_dir)?;
            ctx.assert_testnet()?;
            let journal = MarketJournal::open(out.as_path());
            let mut deployer = MarketDeployer::new(cfg, ctx, journal)?;
            let code = market_drive(&mut deployer, robot)?;
            if code != 0 {
                return Ok(code);
            }
            // The registry step of a COMPLETED deployment: a refusal names
            // the records file and flips the exit code, never the market.
            Ok(registry::record_completed_market(
                registry.as_deref(),
                *no_registry,
                &out,
                robot.robot,
            ))
        }
        MarketCmd::Resume {
            market,
            admin_identity,
            admin_config_dir,
            artifact_version,
            registry,
            no_registry,
            robot,
        } => {
            let out = market_out(cli)?;
            let _lock = market_drive_lock(&out)?;
            anyhow::ensure!(
                out.join("deployment-plan.toml").exists(),
                "no deployment plan in {}: market resume only reconciles a market deploy that started",
                out.display()
            );
            anyhow::ensure!(
                !out.join("market-manifest.toml").exists() || publication_is_unsettled(&out)?,
                "{} is already a completed market run; market resume needs a partial deployment",
                out.display()
            );
            let mut cfg = MarketConfig::load(market)?;
            resolve_market_artifacts(&mut cfg, &out, artifact_version)?;
            let ctx = market_context(cli, &out, admin_identity, admin_config_dir)?;
            ctx.assert_testnet()?;
            let journal = MarketJournal::open(out.as_path());
            let mut deployer = MarketDeployer::new(cfg, ctx, journal)?;
            let code = market_drive(&mut deployer, robot)?;
            if code != 0 {
                return Ok(code);
            }
            // A completed resume completing records like the deploy would.
            Ok(registry::record_completed_market(
                registry.as_deref(),
                *no_registry,
                &out,
                robot.robot,
            ))
        }
        MarketCmd::Tui {
            mode,
            market,
            admin_identity,
            admin_config_dir,
            signer,
            signer_config_dir,
        } => {
            let out = market_out(cli)?;
            match mode {
                MarketTuiMode::Watch => {
                    anyhow::ensure!(
                        market.is_none()
                            && admin_identity.is_none()
                            && admin_config_dir.is_none()
                            && signer.is_none()
                            && signer_config_dir.is_none(),
                        "market tui --mode watch takes no signing or deployment flags; it is read-only"
                    );
                    let manifest = MarketManifest::load(&out).context(
                        "no completed market manifest; watch needs a finished market run",
                    )?;
                    tui::main_market_watch(manifest)
                }
                MarketTuiMode::Explore => {
                    anyhow::ensure!(
                        market.is_none()
                            && admin_identity.is_none()
                            && admin_config_dir.is_none()
                            && signer.is_some() == signer_config_dir.is_some(),
                        "{}",
                        Safe::literal("market explore accepts either both --signer and --signer-config-dir or neither, and no deployment flags")
                    );
                    let (out, manifest, net, exec) = match signer_config_dir {
                        Some(dir) => market_action_context(cli, dir)?,
                        None => market_read_context(cli)?,
                    };
                    tui::market_explorer::main_market_explorer(
                        out,
                        manifest,
                        net,
                        exec,
                        signer.clone(),
                    )
                }
                MarketTuiMode::Deploy | MarketTuiMode::Resume => {
                    anyhow::ensure!(
                        signer.is_none() && signer_config_dir.is_none(),
                        "market deployment TUI uses --admin-identity and --admin-config-dir, not explorer signing flags"
                    );
                    let mode_is_deploy = *mode == MarketTuiMode::Deploy;
                    let market = market.clone().context("market tui requires --market")?;
                    let identity = admin_identity
                        .clone()
                        .context("market tui requires --admin-identity")?;
                    let config_dir = admin_config_dir
                        .clone()
                        .context("market tui requires --admin-config-dir")?;
                    if mode_is_deploy {
                        if let Some(marker) = marker_conflict(&out, &MARKET_RUN_MARKERS) {
                            anyhow::bail!(
                                "run dir {} already holds market state ({}); market tui --mode deploy needs a fresh directory",
                                out.display(),
                                marker
                            );
                        }
                    } else {
                        anyhow::ensure!(
                            out.join("deployment-plan.toml").exists(),
                            "no deployment plan in {}; market tui --mode resume only reconciles a market deploy that started",
                            out.display()
                        );
                        anyhow::ensure!(
                            !out.join("market-manifest.toml").exists()
                                || publication_is_unsettled(&out)?,
                            "{} is already a completed market run; market tui --mode resume needs a partial deployment",
                            out.display()
                        );
                    }
                    let _lock = market_drive_lock(&out)?;
                    let mut cfg = MarketConfig::load(&market)?;
                    resolve_market_artifacts(&mut cfg, &out, &[])?;
                    let ctx = market_context(cli, &out, &identity, &config_dir)?;
                    ctx.assert_testnet()?;
                    let journal = MarketJournal::open(out.as_path());
                    let deployer = MarketDeployer::new(cfg, ctx, journal)?;
                    tui::main_market_deploy(deployer)
                }
            }
        }
        MarketCmd::Play {
            market,
            admin_identity,
            admin_config_dir,
            snapshot,
            cont,
            halt,
            step,
        } => {
            let chosen = cli
                .out
                .clone()
                .context("market play requires --out naming the completed market run")?;
            reject_pair_markers(&chosen)?;
            let out = chosen
                .canonicalize()
                .with_context(|| format!("cannot open run dir {}", chosen.display()))?;
            reject_pair_markers(&out)?;
            let _lock = market_drive_lock(&out)?;
            let manifest = MarketManifest::load(&out)
                .context("market play needs a completed market run directory")?;
            let acts_chosen = [*snapshot, *cont, *halt, step.is_some()]
                .into_iter()
                .filter(|x| *x)
                .count();
            anyhow::ensure!(
                acts_chosen == 1,
                "market play takes exactly one action: --snapshot, --continue, --halt, or --step <index>"
            );
            MarketConfig::load(market)?;
            let ctx = market_context(cli, &out, admin_identity, admin_config_dir)?;
            ctx.assert_testnet()?;
            let journal = MarketJournal::open(out.as_path());
            let mut runner = MarketRunner::new(manifest, ctx, journal)?;
            let act = if *snapshot {
                MarketAct::Snapshot
            } else if *cont {
                MarketAct::Continue
            } else if *halt {
                MarketAct::Halt
            } else {
                MarketAct::Run((*step).context("internal: step index already validated")?)
            };
            let ev = runner.act(act).map_err(|e| {
                // Market action failures are surfaced sanitized: the chain
                // is composed under the market policy, never raw.
                anyhow::anyhow!("{}", stellar::Safe::error_chain(Policy::Market, &e))
            })?;
            match ev {
                MarketEvent::Phase { label } => println!("phase: {label}"),
                MarketEvent::Cli { argv } => println!("cli: {argv}"),
                MarketEvent::Step { label, ok } => {
                    println!("step {label}: {}", if ok { "ok" } else { "failed" });
                    if !ok {
                        return Ok(1);
                    }
                }
                MarketEvent::Halt { reason } => {
                    eprintln!("market runner halted: {reason}");
                    return Ok(1);
                }
                MarketEvent::Failed(reason) => {
                    eprintln!("market action failed: {reason}");
                    return Ok(1);
                }
            }
            Ok(0)
        }
        MarketCmd::Contracts => {
            let out = market_out(cli)?;
            let manifest = MarketManifest::load(&out)
                .context("market contracts needs a completed market run directory")?;
            market_manifest_network(&manifest)?;
            for (role, contract_id) in market_explorer::roles(&manifest) {
                println!("{role} {contract_id}");
            }
            Ok(0)
        }
        MarketCmd::Inspect { role, method } => {
            let out = market_out(cli)?;
            let manifest = MarketManifest::load(&out)
                .context("market inspect needs a completed market run directory")?;
            let network = market_manifest_network(&manifest)?;
            let (net, exec) = market_explorer_net(cli, &out, network)?;
            let st = market_read_stellar(&net, &exec);
            let output = market_explorer_call(market_explorer::inspect(
                &st,
                &out,
                &manifest,
                role,
                method.as_deref(),
            ))?;
            println!("{output}");
            Ok(0)
        }
        MarketCmd::Attach { role, contract_id } => {
            let out = market_out(cli)?;
            let manifest = MarketManifest::load(&out)
                .context("market attach needs a completed market run directory")?;
            let network = market_manifest_network(&manifest)?;
            let contract_id = ContractId::from_str(contract_id)
                .map_err(|_| anyhow::anyhow!("invalid contract id"))?;
            let (net, exec) = market_explorer_net(cli, &out, network)?;
            let st = market_read_stellar(&net, &exec);
            market_explorer_call(market_explorer::attach(
                &st,
                &out,
                &manifest,
                role,
                contract_id,
            ))?;
            println!("market contract attached");
            Ok(0)
        }
        MarketCmd::Query { role, method, args } => {
            let args = parse_market_query_args(args)?;
            let (out, manifest, net, exec) = market_read_context(cli)?;
            let st = Stellar::new(&net, &exec, Policy::Market);
            // The query service self-gates the network the run froze and
            // the pinned CLI before composing under the market policy.
            let value = market_explorer_call(market_query::query(
                &st, &out, &manifest, role, method, &args,
            ))?;
            let output =
                market_explorer_call(Safe::render_market_json(&value, MARKET_READ_RENDER_CAP))?;
            println!("{output}");
            Ok(0)
        }
        MarketCmd::Call {
            role,
            method,
            args,
            signer,
            signer_config_dir,
            execute,
        } => {
            let args = parse_market_query_args(args)?;
            let (out, manifest, net, exec) = market_action_context(cli, signer_config_dir)?;
            let st = Stellar::new(&net, &exec, Policy::Market);
            market_action_pump(&st, &out, &manifest, role, method, &args, signer, *execute)
        }
        MarketCmd::Guides => {
            for (category, action, role, method, help) in market_guides::catalog() {
                println!("{category} {action} [{role}.{method}]: {help}");
            }
            Ok(0)
        }
        MarketCmd::Guide {
            category,
            action,
            asset,
            source,
            args,
            signer,
            signer_config_dir,
            execute,
        } => {
            let args = parse_market_query_args(args)?;
            let (out, manifest, net, exec) = market_action_context(cli, signer_config_dir)?;
            let st = Stellar::new(&net, &exec, Policy::Market);
            let (role, method, args) = market_explorer_call(market_guides::guide(
                &st,
                &out,
                &manifest,
                category,
                action,
                asset.as_deref(),
                *source,
                &args,
            ))?;
            market_action_pump(
                &st, &out, &manifest, &role, &method, &args, signer, *execute,
            )
        }
        MarketCmd::Status => {
            let (out, manifest, net, exec) = market_read_context(cli)?;
            let st = Stellar::new(&net, &exec, Policy::Market);
            market_explorer_call(st.assert_market_cli_version().map(|_| ()))?;
            let (report, ready) =
                market_explorer_call(market_status::status(&st, &out, &manifest))?;
            println!("{report}");
            anyhow::ensure!(ready, "{}", Safe::literal("market prices are not ready"));
            Ok(0)
        }
        MarketCmd::Settle {
            coordinate,
            consumed_by,
        } => {
            let (out, _manifest, net, exec) = market_read_context(cli)?;
            let _lock = market_drive_lock(&out)?;
            let st = Stellar::new(&net, &exec, Policy::Market);
            let summary =
                market_explorer_call(market_history::settle(&st, &out, coordinate, consumed_by))?;
            println!("{summary}");
            Ok(0)
        }
        MarketCmd::Replay {
            coordinate,
            consumed_by,
            admin_config_dir,
        } => {
            let out = market_out(cli)?;
            let _lock = market_drive_lock(&out)?;
            // The network is the one the run's frozen deployment plan names,
            // and the RPC comes from the operator's own network entry for it
            // — the same path `market deploy --network` takes, never argv.
            let plan = read_deployment_plan(&out)?
                .context("a real market replay needs a partial run with a frozen deployment plan")?;
            anyhow::ensure!(
                !out.join("market-manifest.toml").exists(),
                "a real market replay needs a partial deployment; this run is already published"
            );
            let declared = plan_network(&plan)?;
            let config_dir = admin_config_dir.canonicalize().map_err(|_| {
                anyhow::anyhow!("the operator's identity directory is unavailable")
            })?;
            let ctx =
                real_market_context(cli, &out, MARKET_OPERATOR_ALIAS, &config_dir, declared)?;
            ctx.assert_network(declared)?;
            let (net, exec) = ctx.read_net();
            let st = market_read_stellar(&net, &exec);
            let summary = market_explorer_call(market_history::settle_unapplied(
                &st,
                &out,
                coordinate,
                consumed_by,
            ))?;
            println!("{summary}");
            Ok(0)
        }
        MarketCmd::Events {
            role,
            cursor,
            start_ledger,
            count,
        } => {
            // Exactly one continuation position, and every argument rule is
            // settled before any run directory, executor, or network call.
            anyhow::ensure!(
                cursor.is_some() ^ start_ledger.is_some(),
                "{}",
                Safe::literal("market events requires exactly one of --cursor or --start-ledger")
            );
            let count = count.unwrap_or(MARKET_EVENTS_MAX_COUNT);
            anyhow::ensure!(
                (1..=MARKET_EVENTS_MAX_COUNT).contains(&count),
                "{}",
                Safe::literal("market events --count must be between 1 and 100")
            );
            // A continuation handle is checked against the RPC event-id
            // grammar here, so a malformed cursor never reaches the role
            // lookup or the pinned CLI.
            if let Some(cursor) = cursor.as_deref() {
                market_explorer_call(crate::stellar::validate_event_cursor(cursor))?;
            }
            let (out, manifest, net, exec) = market_read_context(cli)?;
            let st = Stellar::new(&net, &exec, Policy::Market);
            // The attachment role is validated against the live spec digest
            // before any history read is composed.
            let target = market_explorer_call(market_explorer::validated_target(
                &st, &out, &manifest, role,
            ))?;
            let events = match (cursor.as_deref(), start_ledger) {
                (Some(cursor), None) => {
                    market_explorer_call(market_history::events_after(&st, &target, cursor, count))?
                }
                (None, Some(start_ledger)) => market_explorer_call(market_history::events(
                    &st,
                    &target,
                    *start_ledger,
                    count,
                ))?,
                _ => anyhow::bail!(
                    "{}",
                    Safe::literal(
                        "market events requires exactly one of --cursor or --start-ledger"
                    )
                ),
            };
            println!("{events}");
            Ok(0)
        }
        MarketCmd::Receipt { hash } => {
            let (_out, _manifest, net, exec) = market_read_context(cli)?;
            let st = Stellar::new(&net, &exec, Policy::Market);
            // The receipt service statically validates the hash before
            // composing; a rejected input is never echoed.
            let receipt = market_explorer_call(market_history::receipt(&st, hash))?;
            println!("{receipt}");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abtest-market-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("harness directory");
        dir
    }

    /// `--wasm-dir` names qualified bytes the operator holds locally. Left
    /// relative, it would be resolved against whatever directory a later step
    /// works in, and the run would go to the network for code it was handed.
    /// A directory that cannot be read is refused at the flag, not after a
    /// send.
    #[test]
    fn the_wasm_directory_is_resolved_once_at_the_flag() {
        let here = std::env::current_dir().expect("the run has a working directory");
        let resolved = resolve_wasm_dir(Some(std::path::Path::new(".")))
            .expect("the working directory is readable");
        assert_eq!(
            resolved.as_deref(),
            Some(here.as_path()),
            "a relative --wasm-dir must arrive at the run as the absolute path it names"
        );
        assert!(resolve_wasm_dir(None).expect("no directory is no directory").is_none());
        let missing = here.join("no-such-wasm-dir");
        let Err(error) = resolve_wasm_dir(Some(&missing)) else {
            panic!("an unreadable --wasm-dir must be refused before any send");
        };
        assert!(
            error.to_string().contains("--wasm-dir"),
            "the refusal must name the flag the operator typed: {error}"
        );
    }

    fn cli(bin: &str, out: Option<PathBuf>) -> Cli {
        Cli {
            stellar_bin: PathBuf::from(bin),
            rpc_url: "https://soroban-testnet.stellar.org".into(),
            passphrase: MARKET_TESTNET_PASSPHRASE.into(),
            out,
            operator_config_dir: PathBuf::from("~/.config/stellar"),
            cmd: Cmd::Market {
                cmd: MarketCmd::Status,
            },
        }
    }

    /// `--registry` and `--no-registry` answer the same question, so the CLI
    /// refuses to guess which one the operator meant.
    #[test]
    fn the_registry_flags_cannot_both_be_asked() {
        use clap::Parser;
        let parse = |extra: &[&str]| {
            Cli::try_parse_from(
                [
                    "precept-abtest",
                    "market",
                    "deploy",
                    "--market",
                    "m.toml",
                    "--admin-identity",
                    "a",
                    "--admin-config-dir",
                    "/cfg",
                ]
                .into_iter()
                .chain(extra.iter().copied()),
            )
            .map(|_| ())
        };
        assert!(
            parse(&["--registry", "/precept", "--no-registry"]).is_err(),
            "both flags at once is not an answer the CLI accepts"
        );
        for solo in [vec!["--registry", "/precept"], vec!["--no-registry"]] {
            assert!(parse(&solo).is_ok(), "{solo:?} alone must parse");
        }
    }

    /// A planted run-directory `.env` is refused before any manifest read
    /// or executor exists, and the refusal never echoes what was planted.
    #[test]
    fn a_run_directory_carrying_dotenv_is_refused_before_any_read() {
        let run = harness("dotenv");
        std::fs::write(
            run.join(".env"),
            "STELLAR_CONTRACT_ID=CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM\n",
        )
        .expect("planted dotenv");
        let err = market_read_context(&cli("stellar", Some(run.clone())))
            .err()
            .expect("a planted dotenv cannot steer a market read");
        assert_eq!(err.to_string(), MARKET_CWD_UNSAFE);
        assert!(
            !err.to_string().contains("STELLAR_CONTRACT_ID") && !err.to_string().contains("CAAAAA"),
            "the refusal never echoes the planted bytes: {err}"
        );
        let _ = std::fs::remove_dir_all(&run);
    }

    #[test]
    fn an_unsafe_cli_path_is_refused_without_echoing_it() {
        for requested in ["", "/", "./absent-market-stellar/stellar"] {
            let err = market_explorer_bin(Path::new(requested))
                .expect_err("a missing path or a directory cannot be the market CLI");
            assert_eq!(err.to_string(), MARKET_CLI_UNSAFE);
            assert!(
                !err.to_string().contains("absent-market-stellar"),
                "the refusal never echoes the caller's path: {err}"
            );
        }
    }

    /// The completed run's own manifest and frozen plan, as the run wrote
    /// them: this test resumes a finished deployment, so the run directory
    /// has to look exactly like one.
    fn completed_market_run(out: &Path) {
        let admin = "GCNQ4ZLK45KOYNIQLIYI7GT6TYWGK2OZEVZEY7A3W5L7QSPBNCBWSJAQ";
        let manifest = serde_json::json!({
            "network": {"label": "testnet", "passphrase": MARKET_TESTNET_PASSPHRASE},
            "admin": admin,
            "rpc_host": "{\"host\":\"soroban-testnet.stellar.org\"}",
            "oracle": {
                "runtime": "CDZIWBTV3S4R6534NJESRCQAXKYWJSCXDHVN6LIHSBCAFOOHTRKE6JEE",
                "governance": "CASM5GWRAUID4XTG4KO3R4L6OJUC755ZNKVI4PFHBIRMAOBGSCGRM5SW",
                "adapter": "CAL44IMB252XGNFY3O3B6HGN6OGLOUVPNGS6SZ7D64P7LJGGN7IPYKVI",
                "sources": [
                    "CBAAHPSQWF7YLQQO3FJFFRX26HIGHHYUVELLM3DSOZ5RSCJNLLOVPSNM",
                    "CAIPFFPYKDJ7YXYYGQ7RPXEBOTV2YGP6O63RM36WXS33CJRTFYA2JCTJ",
                    "CD5PLK3V6KIM5KRXQ5R2GYCYYMUVXJCE74IDUFT2DSHYJQXATL76UD6Y",
                ],
                "min_sources": 3, "max_age_secs": 3600, "max_clock_drift_secs": 60,
                "decimals": 7, "resolution": 300, "base": "USD"
            },
            "tokens": {
                "blnd": "CBYC26I2DE6KLFXPON3AD2OJJY5ZKCU2OWBWFGMJCC43S3Z4X5KBOIPV",
                "usdc": "CDXPSFVFELVSWIHICL2O5BJXQVX47YGIMHXPCLDWNR2XEYIUONR5F75D",
                "weth": "CBOX3I2GVXHHQ74RORKRJVVWMCYZ2JWTE3JYCVKYP7EPN465IQVK4CBI",
                "xlm": "CAWUCL3J4SRFE3MNUHDF5JH4S6MUVUMYWXACPYCBUBRQX4KILKUZAECH",
                "stable": "CB3F4EWMKGBPDRZWXKBFEXK2WVFDD65QUECNKURAUZ372DUGZROC3PX7"
            },
            "pool": "CABLQSAUOCKDXREA7CE55J4PRSHOHVDXNLPA54NUH4GCSLOUDP6CG7NQ",
            "backstop": "CDG62BXK5R7SCGZT4PCJI3OXQW5P4JQIZGBIIEVAXVZGY6QY3O5LNVDQ",
            "factory": "CACHT3KSTIULMHLZ6HPIJINGKMKIXFMICI3OVMLUDOR6KIYTXDXAXB7A",
            "comet": "CDPAWAR5GLCZNKWARR3HRJKRYQ6BKELCF5H5NNYUUZTL4OFX7IJYW4DB",
            "emitter": "CCCTBGR6PXFJVT577NIFM6ITAM552SRLH3OLDLMFE32DRHUQCUOZ3TDL",
            "reserves": {}, "identities": {}, "deploy_ledger": 4951021
        });
        let manifest: MarketManifest =
            serde_json::from_value(manifest).expect("completed market manifest");
        std::fs::write(
            out.join("market-manifest.toml"),
            toml::to_string_pretty(&manifest).expect("render the manifest"),
        )
        .expect("plant the manifest");
        let digest = |byte: u8| format!("{byte:02x}").repeat(32);
        let plan = serde_json::json!({
            "schema": 1, "config_digest": "a".repeat(64), "nonce": "7",
            "network_label": MARKET_NETWORK_LABEL, "passphrase": MARKET_TESTNET_PASSPHRASE,
            "admin": admin,
            "artifacts": {
                "adapter": digest(1), "backstop": digest(2), "comet": digest(3),
                "emitter": digest(4), "factory": digest(5), "pool": digest(6),
                "proxy-governance": digest(7), "proxy-runtime": digest(8),
                "source": digest(9), "token": digest(10)
            },
            "salt_roles": [], "slots": []
        });
        let plan: crate::deploy::DeploymentPlan =
            serde_json::from_value(plan).expect("frozen deployment plan");
        std::fs::write(
            out.join("deployment-plan.toml"),
            toml::to_string_pretty(&plan).expect("render the plan"),
        )
        .expect("plant the plan");
    }

    /// A registry checkout that records the argv it was handed, and a
    /// `stellar` that records any call it is asked to make.
    fn recording_stellar(dir: &Path) -> (PathBuf, PathBuf) {
        let stellar = dir.join("stellar");
        let calls = dir.join("stellar.calls");
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            &stellar,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
                calls.display()
            ),
        )
        .expect("stub stellar");
        let mut permissions = std::fs::metadata(&stellar).expect("stub").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&stellar, permissions).expect("stub executable");
        (stellar, calls)
    }

    /// `--resume` on a run that already published its manifest has nothing
    /// left to send: it asks the network nothing and only performs the
    /// registry record the finished run missed.
    #[test]
    fn resuming_a_completed_market_run_sends_nothing_and_only_records() {
        let dir = harness("resume-complete");
        let out = dir.join("market-live-20260930");
        std::fs::create_dir_all(&out).expect("run directory");
        completed_market_run(&out);
        let (stellar, calls) = recording_stellar(&dir);
        let checkout = dir.join("precept");
        std::fs::create_dir_all(checkout.join("registry")).expect("registry dir");
        std::fs::write(
            checkout.join("registry").join("registry.py"),
            "import json, os, sys\n\
             here = os.path.dirname(os.path.abspath(__file__))\n\
             with open(os.path.join(here, '.calls'), 'a') as made:\n\
             \x20   made.write(json.dumps(sys.argv[1:]) + '\\n')\n",
        )
        .expect("stub registry");
        std::fs::write(checkout.join("registry").join("testnet.jsonl"), "").expect("table");
        let code = market_deploy_real(
            &cli(stellar.to_str().expect("stub path"), Some(out.clone())),
            Path::new("the-pool-config-is-never-read.toml"),
            Some(&MarketNetworkArg::Testnet),
            false,
            None,
            true,
            "operator",
            &dir,
            Some(&checkout),
            false,
            &[],
            &RobotArgs::default(),
        )
        .expect("resuming a finished run is not an error");
        assert_eq!(code, 0);
        assert!(!calls.exists(), "a finished run sends nothing to the network");
        // The one thing it does is the registry record.
        assert!(out.join("registry-records.jsonl").is_file());
        let added: Vec<serde_json::Value> =
            std::fs::read_to_string(out.join("registry-add.jsonl"))
            .expect("the add file")
            .lines()
            .map(|line| serde_json::from_str(line).expect("row"))
            .collect();
        assert!(!added.is_empty(), "an unrecorded deployment is offered to the registry");
        let made: Vec<Vec<String>> = std::fs::read_to_string(checkout.join("registry").join(".calls"))
            .expect("the registry was asked")
            .lines()
            .map(|line| serde_json::from_str(line).expect("argv"))
            .collect();
        assert_eq!(made.len(), 1, "exactly one registry call: {made:?}");
        assert_eq!(made[0][0], "add");
        assert_eq!(made[0][1], "--network");
        assert_eq!(made[0][2], "testnet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A robot failure carries the violations the step reported: an exit code
    /// alone would not say what the step objected to. The confirmation token
    /// is never part of any event.
    #[test]
    fn a_robot_failure_carries_the_violations_and_never_the_token() {
        let event = robot_event(
            &DeployEvent::Failed(Safe::literal(
                "the confirmed step reported a violation",
            )),
            &[Safe::literal("no Accepted price")],
        );
        assert_eq!(event["event"], "failed");
        assert_eq!(
            event["violations"],
            serde_json::json!(["no Accepted price"]),
            "the violations are part of the event: {event}"
        );
        assert!(
            event.get("token").is_none(),
            "no confirmation token is ever printed: {event}"
        );
    }

}
