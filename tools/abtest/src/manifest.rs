//! Pair configuration (TOML in), deployed manifests (TOML out), run state (JSON).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};

use crate::stellar::{AccountId, ContractId, Invocation, Net, Safe};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    StockVsFork,
    ForkVsFork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    Admin,
    Frodo,
    Sam,
    User,
}

impl Who {
    pub const ALL: [Who; 4] = [Who::Admin, Who::Frodo, Who::Sam, Who::User];
    pub fn key_name(self) -> String {
        format!("abtest-{}", self.as_str())
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Who::Admin => "admin",
            Who::Frodo => "frodo",
            Who::Sam => "sam",
            Who::User => "user",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tok {
    Blnd,
    Usdc,
    Weth,
    Xlm,
    Stable,
    Lp,
}

impl Tok {
    /// Mock sep-41 tokens deployed per side, in deploy order.
    pub const MOCK: [Tok; 5] = [Tok::Blnd, Tok::Usdc, Tok::Weth, Tok::Xlm, Tok::Stable];
    pub const ALL: [Tok; 6] = [
        Tok::Blnd,
        Tok::Usdc,
        Tok::Weth,
        Tok::Xlm,
        Tok::Stable,
        Tok::Lp,
    ];
    /// Reserve assets in the fixture's oracle order.
    pub const ORACLE_ORDER: [Tok; 4] = [Tok::Weth, Tok::Usdc, Tok::Xlm, Tok::Stable];
    pub const RESERVES: [Tok; 3] = [Tok::Stable, Tok::Xlm, Tok::Weth];
    pub fn decimals(self) -> u32 {
        match self {
            Tok::Weth => 9,
            Tok::Stable => 6,
            _ => 7,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Tok::Blnd => "blnd",
            Tok::Usdc => "usdc",
            Tok::Weth => "weth",
            Tok::Xlm => "xlm",
            Tok::Stable => "stable",
            Tok::Lp => "lp",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedWasm {
    pub token_wasm: PathBuf,
    pub oracle_wasm: PathBuf,
    pub comet_wasm: PathBuf,
    pub emitter_wasm: PathBuf,
}

/// Per-reserve overrides (7-decimal fixed point except supply_cap, which is a
/// base-unit i128 string). Pair runs may set any subset; a market deploy must
/// set every field explicitly (`MarketConfig::require_explicit_reserves`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReserveOverride {
    pub c_factor: Option<u32>,
    pub l_factor: Option<u32>,
    pub util: Option<u32>,
    pub max_util: Option<u32>,
    pub supply_cap: Option<String>,
    pub r_base: Option<u32>,
    pub r_one: Option<u32>,
    pub r_two: Option<u32>,
    pub r_three: Option<u32>,
    pub reactivity: Option<u32>,
}

impl ReserveOverride {
    /// Names of the fields still falling back to a fixture default.
    pub fn missing_fields(&self) -> Vec<&'static str> {
        [
            (self.c_factor.is_some(), "c_factor"),
            (self.l_factor.is_some(), "l_factor"),
            (self.util.is_some(), "util"),
            (self.max_util.is_some(), "max_util"),
            (self.supply_cap.is_some(), "supply_cap"),
            (self.r_base.is_some(), "r_base"),
            (self.r_one.is_some(), "r_one"),
            (self.r_two.is_some(), "r_two"),
            (self.r_three.is_some(), "r_three"),
            (self.reactivity.is_some(), "reactivity"),
        ]
        .into_iter()
        .filter(|(set, _)| !set)
        .map(|(_, name)| name)
        .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SideSpec {
    pub label: String,
    pub pool_wasm: PathBuf,
    pub backstop_wasm: PathBuf,
    pub factory_wasm: PathBuf,
    #[serde(default)]
    pub reserves: BTreeMap<Tok, ReserveOverride>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    pub profile: Profile,
    /// What this pair is comparing, shown in the TUI's info overlay.
    #[serde(default)]
    pub description: Option<String>,
    pub shared: SharedWasm,
    pub a: SideSpec,
    pub b: SideSpec,
}

impl PairConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: PairConfig =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        for p in [
            &mut cfg.shared.token_wasm,
            &mut cfg.shared.oracle_wasm,
            &mut cfg.shared.comet_wasm,
            &mut cfg.shared.emitter_wasm,
            &mut cfg.a.pool_wasm,
            &mut cfg.a.backstop_wasm,
            &mut cfg.a.factory_wasm,
            &mut cfg.b.pool_wasm,
            &mut cfg.b.backstop_wasm,
            &mut cfg.b.factory_wasm,
        ] {
            *p = expand_home(p);
            // Pin to absolute now: existence is checked here, but the upload happens
            // later with the run dir as the child's cwd.
            if !p.is_absolute() {
                *p = std::env::current_dir()
                    .context("resolve relative wasm path")?
                    .join(&*p);
            }
            anyhow::ensure!(p.is_file(), "wasm not a file: {}", p.display());
        }
        anyhow::ensure!(
            !cfg.a.label.trim().is_empty()
                && !cfg.b.label.trim().is_empty()
                && cfg.a.label != cfg.b.label,
            "side labels must be non-empty and distinct (a={:?}, b={:?})",
            cfg.a.label,
            cfg.b.label
        );
        Ok(cfg)
    }
}

pub fn expand_home(p: &Path) -> PathBuf {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Contracts {
    pub blnd: String,
    pub usdc: String,
    pub weth: String,
    pub xlm: String,
    pub stable: String,
    pub lp: String,
    pub emitter: String,
    pub backstop: String,
    pub pool_factory: String,
    pub oracle: String,
    pub pool: String,
}

impl Contracts {
    pub fn token(&self, t: Tok) -> &str {
        match t {
            Tok::Blnd => &self.blnd,
            Tok::Usdc => &self.usdc,
            Tok::Weth => &self.weth,
            Tok::Xlm => &self.xlm,
            Tok::Stable => &self.stable,
            Tok::Lp => &self.lp,
        }
    }
    pub fn set_token(&mut self, t: Tok, id: String) {
        match t {
            Tok::Blnd => self.blnd = id,
            Tok::Usdc => self.usdc = id,
            Tok::Weth => self.weth = id,
            Tok::Xlm => self.xlm = id,
            Tok::Stable => self.stable = id,
            Tok::Lp => self.lp = id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReserveRef {
    pub index: u32,
    pub decimals: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SideManifest {
    pub label: String,
    pub deploy_ledger: u32,
    /// wasm role -> hash
    pub wasm: BTreeMap<String, String>,
    pub contracts: Contracts,
    pub reserves: BTreeMap<Tok, ReserveRef>,
    /// Total minted per mock token, for conservation checks (fits i64 by construction).
    pub minted: BTreeMap<Tok, i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairManifest {
    pub profile: Profile,
    #[serde(default)]
    pub description: Option<String>,
    pub net: Net,
    /// identity -> G address (shared by both sides)
    pub identities: BTreeMap<Who, String>,
    pub a: SideManifest,
    pub b: SideManifest,
}

impl PairManifest {
    pub fn path(out: &Path) -> PathBuf {
        out.join("manifest.toml")
    }
    pub fn save(&self, out: &Path) -> anyhow::Result<()> {
        // Atomic, like `RunState::save`: a kill mid-write would otherwise
        // truncate the only record of deployed contracts, and the deployed
        // value is already spent — the run dir becomes unrecoverable.
        std::fs::create_dir_all(out)?;
        let p = Self::path(out);
        let tmp = out.join("manifest.toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?).context("staging manifest")?;
        fsync(&tmp, "staged manifest")?;
        std::fs::rename(&tmp, &p).context("publishing manifest")?;
        fsync(out, "run directory")?;
        Ok(())
    }
    pub fn load(out: &Path) -> anyhow::Result<Self> {
        let p = Self::path(out);
        let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        Ok(toml::from_str(&text)?)
    }
    pub fn addr(&self, who: Who) -> &str {
        &self.identities[&who]
    }
}

/// Mutable run state persisted between `run` invocations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunState {
    /// scenario -> last completed step label, per side pair
    pub completed: BTreeMap<String, Vec<String>>,
    /// Exact requested labels, including waits; operator actions are not a campaign.
    #[serde(default)]
    pub requested: BTreeMap<String, Vec<String>>,
    /// side label -> extra minted since deploy (conservation)
    pub minted: BTreeMap<String, BTreeMap<Tok, i64>>,
    /// side label -> auction owner -> auction start block from `new_auction`
    pub auction_blocks: BTreeMap<String, BTreeMap<Who, u32>>,
    pub frozen: bool,
    /// Scenarios in the order they were executed; the report replays cross-step
    /// context (delta mode, pool-loss custody) in this order.
    #[serde(default)]
    pub scenario_order: Vec<String>,
}

impl RunState {
    pub fn path(out: &Path) -> PathBuf {
        out.join("state.json")
    }
    pub fn load(out: &Path) -> anyhow::Result<Self> {
        let p = Self::path(out);
        if !p.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
    }
    pub fn save(&self, out: &Path) -> anyhow::Result<()> {
        // Atomic: a kill between write and completion must leave the previous
        // resumable state readable, never a half-written JSON document.
        let p = Self::path(out);
        let tmp = out.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).context("staging run state")?;
        fsync(&tmp, "staged run state")?;
        std::fs::rename(&tmp, &p).context("publishing run state")?;
        fsync(out, "run directory")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideOutcome {
    Declined,
    Unmutating,
    Invocation(Invocation),
    ToolError(String),
}

impl SideOutcome {
    fn settled(&self) -> bool {
        matches!(self, Self::Declined | Self::Unmutating)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingIntent {
    pub scenario: String,
    pub index: usize,
    pub label: String,
    pub side: char,
}

pub fn pending_step_dir(out: &Path, scenario: &str, index: usize, label: &str) -> PathBuf {
    out.join("pending")
        .join(scenario)
        .join(format!("{index:03}-{label}"))
}

fn fsync(path: &Path, what: &str) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {what} {} for sync", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {what} {}", path.display()))
}

/// Durably reserve one side's pre-invocation intent. `create_new` makes a second
/// attempt fail before it can reach the chain, even if a prior process died after
/// opening the intent but before recording its outcome.
pub fn begin_pending(out: &Path, intent: &PendingIntent) -> anyhow::Result<()> {
    use std::io::Write;
    let side = intent.side;
    let dir = pending_step_dir(out, &intent.scenario, intent.index, &intent.label);
    std::fs::create_dir_all(&dir).context("creating pending step directory")?;
    let result = dir.join(format!("{side}.result.json"));
    anyhow::ensure!(
        !result.exists(),
        "pending step {} {} already has side {} outcome {}",
        intent.scenario,
        intent.label,
        side,
        result.display()
    );
    let path = dir.join(format!("{side}.intent.json"));
    let bytes = serde_json::to_vec_pretty(intent)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("reserving side {side} intent {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing side {side} intent {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing side {side} intent {}", path.display()))?;
    fsync(&dir, "pending step directory")
}

pub fn finish_pending(
    out: &Path,
    scenario: &str,
    index: usize,
    label: &str,
    side: char,
    outcome: SideOutcome,
) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = pending_step_dir(out, scenario, index, label);
    let path = dir.join(format!("{side}.result.json"));
    let bytes = serde_json::to_vec_pretty(&outcome)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("reserving side {side} outcome {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing side {side} outcome {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing side {side} outcome {}", path.display()))?;
    fsync(&dir, "pending outcome directory")
}

pub fn clear_pending(out: &Path, scenario: &str, index: usize, label: &str) -> anyhow::Result<()> {
    let step = pending_step_dir(out, scenario, index, label);
    if step.exists() {
        std::fs::remove_dir_all(&step)
            .with_context(|| format!("clearing pending step {}", step.display()))?;
    }
    let scenario_dir = out.join("pending").join(scenario);
    if scenario_dir.exists() && scenario_dir.read_dir()?.next().is_none() {
        let _ = std::fs::remove_dir(&scenario_dir);
    }
    let root = out.join("pending");
    if root.exists() && root.read_dir()?.next().is_none() {
        let _ = std::fs::remove_dir(&root);
    }
    Ok(())
}

/// Remove attempts proved to have touched no chain state. Anything else is an
/// unresolved attempt and is returned rather than silently dropped.
pub fn reap_safe_pending(out: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let root = out.join("pending");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut unsafe_steps = Vec::new();
    for scenario in std::fs::read_dir(&root)?.flatten() {
        let scenario_dir = scenario.path();
        if !scenario_dir.is_dir() {
            unsafe_steps.push(scenario_dir);
            continue;
        }
        for step in std::fs::read_dir(&scenario_dir)?.flatten() {
            let step_dir = step.path();
            if !step_dir.is_dir() {
                unsafe_steps.push(step_dir);
                continue;
            }
            let mut intents = std::fs::read_dir(&step_dir)?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".intent.json"))
                })
                .collect::<Vec<_>>();
            intents.sort();
            let settled = !intents.is_empty()
                && intents.iter().all(|intent| {
                    let name = intent
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    let result = step_dir.join(name.replace(".intent.json", ".result.json"));
                    std::fs::read(&result)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<SideOutcome>(&bytes).ok())
                        .is_some_and(|outcome| outcome.settled())
                });
            if settled {
                let _ = std::fs::remove_dir_all(&step_dir);
            } else {
                unsafe_steps.push(step_dir);
            }
        }
        if scenario_dir.exists() && scenario_dir.read_dir()?.next().is_none() {
            let _ = std::fs::remove_dir(&scenario_dir);
        }
    }
    if root.exists() && root.read_dir()?.next().is_none() {
        let _ = std::fs::remove_dir(&root);
    }
    unsafe_steps.sort();
    Ok(unsafe_steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_toml() {
        let side = SideManifest {
            label: "fork".into(),
            deploy_ledger: 42,
            wasm: [("pool".to_string(), "ab".to_string())].into(),
            contracts: Contracts {
                pool: "CPOOL".into(),
                ..Default::default()
            },
            reserves: [(
                Tok::Stable,
                ReserveRef {
                    index: 0,
                    decimals: 6,
                },
            )]
            .into(),
            minted: [(Tok::Stable, 100_000_000_000i64)].into(),
        };
        let m = PairManifest {
            profile: Profile::StockVsFork,
            description: Some("test pair".into()),
            net: Net {
                rpc_url: "u".into(),
                passphrase: "p".into(),
                config_dir: "/c".into(),
                headers: Vec::new(),
            },
            identities: [(Who::Admin, "GADMIN".into())].into(),
            a: side.clone(),
            b: side,
        };
        let text = toml::to_string_pretty(&m).unwrap();
        let back: PairManifest = toml::from_str(&text).unwrap();
        assert_eq!(back.a.reserves[&Tok::Stable].decimals, 6);
        assert_eq!(back.addr(Who::Admin), "GADMIN");
        assert_eq!(back.profile, Profile::StockVsFork);
    }

    #[test]
    fn pair_config_parses_overrides() {
        let text = r#"
profile = "fork-vs-fork"
[shared]
token_wasm = "/x/t.wasm"
oracle_wasm = "/x/o.wasm"
comet_wasm = "/x/c.wasm"
emitter_wasm = "/x/e.wasm"
[a]
label = "a"
pool_wasm = "/x/p.wasm"
backstop_wasm = "/x/b.wasm"
factory_wasm = "/x/f.wasm"
[b]
label = "b"
pool_wasm = "/x/p.wasm"
backstop_wasm = "/x/b.wasm"
factory_wasm = "/x/f.wasm"
[b.reserves.xlm]
c_factor = 6000000
"#;
        let cfg: PairConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.b.reserves[&Tok::Xlm].c_factor, Some(6_000_000));
        assert!(cfg.a.reserves.is_empty());
    }

    /// A typo'd key used to be dropped silently, deploying a pair the operator
    /// did not configure.
    #[test]
    fn unknown_toml_keys_are_rejected_not_dropped() {
        let text = r#"profile = "fork-vs-fork"
[shared]
token_wasm = "/x/t.wasm"
oracle_wasm = "/x/o.wasm"
comet_wasm = "/x/c.wasm"
emitter_wasm = "/x/e.wasm"
[a]
label = "a"
pool_wasm = "/x/p.wasm"
backstop_wasm = "/x/b.wasm"
factory_wasm = "/x/f.wasm"
typo_field = true
[b]
label = "b"
pool_wasm = "/x/p.wasm"
backstop_wasm = "/x/b.wasm"
factory_wasm = "/x/f.wasm"
"#;
        let err = toml::from_str::<PairConfig>(text).unwrap_err();
        assert!(
            err.to_string().contains("typo_field"),
            "error should name the unknown key: {err}"
        );
    }

    /// An empty path absolutizes to the cwd directory, which `exists()` accepts;
    /// only an `is_file` check rejects the typo before any chain work starts.
    #[test]
    fn an_empty_wasm_path_is_rejected_before_deploy() {
        let dir = std::env::temp_dir().join(format!("abtest-emptywasm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = r#"profile = "fork-vs-fork"
[shared]
token_wasm = ""
oracle_wasm = "/x/o.wasm"
comet_wasm = "/x/c.wasm"
emitter_wasm = "/x/e.wasm"
[a]
label = "a"
pool_wasm = "/x/p.wasm"
backstop_wasm = "/x/b.wasm"
factory_wasm = "/x/f.wasm"
[b]
label = "b"
pool_wasm = "/x/p.wasm"
backstop_wasm = "/x/b.wasm"
factory_wasm = "/x/f.wasm"
"#;
        let path = dir.join("pair.toml");
        std::fs::write(&path, text).unwrap();
        let err = PairConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("wasm not a file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lock_stamped_with_our_recycled_pid_reads_as_stale() {
        let dir = std::env::temp_dir().join(format!("abtest-ownpid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Our pid with a start time that cannot be ours: the pid was recycled.
        std::fs::write(
            dir.join("run.lock"),
            format!("{} 9999999999999", std::process::id()),
        )
        .unwrap();
        assert_eq!(
            RunLock::owner(&dir),
            None,
            "own recycled pid must not read live"
        );
        assert!(RunLock::acquire(&dir).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A racing reclaim can unlink our file between create and stamp; only a
    /// file still carrying our stamp may be trusted, and Drop must never release
    /// someone else's lock.
    #[test]
    fn drop_does_not_release_a_reclaimed_lock() {
        let dir = std::env::temp_dir().join(format!("abtest-reclaim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = RunLock::acquire(&dir).unwrap().unwrap();
        // Another "process" reclaims the lock file under us.
        std::fs::write(dir.join("run.lock"), "4294967294 1").unwrap();
        drop(lock);
        assert!(
            dir.join("run.lock").exists(),
            "Drop must not unlink a lock we no longer hold"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `save` writes via tmp+rename, so an interrupted write can never leave
    /// half-JSON where the previous resumable state used to be.
    #[test]
    fn state_round_trips_with_no_temp_residue() {
        let dir = std::env::temp_dir().join(format!("abtest-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = RunState::default();
        st.completed.insert("lending".into(), vec!["borrow".into()]);
        st.requested.insert("lending".into(), vec!["borrow".into()]);
        st.scenario_order.push("lending".into());
        st.save(&dir).unwrap();
        let back = RunState::load(&dir).unwrap();
        assert_eq!(back.scenario_order, vec!["lending"]);
        assert_eq!(back.completed["lending"], vec!["borrow"]);
        assert_eq!(back.requested["lending"], vec!["borrow"]);
        assert!(!dir.join("state.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Our own stamp always carries a nonzero start tick; a `start == 0` file
    /// was written by an external tool and its bare pid must never be trusted
    /// as a live owner (pid 1 is always alive).
    #[test]
    fn externally_written_lock_is_foreign() {
        let dir = std::env::temp_dir().join(format!("abtest-foreign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.lock"), "1 0").unwrap();
        assert_eq!(RunLock::owner(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A malformed market manifest fails with a fixed input-free message: the
    /// rejected value and any terminal controls planted inside it must never
    /// reach an error display, not even through the source chain.
    #[test]
    fn malformed_market_manifest_errors_never_echo_the_rejected_value() {
        let dir = std::env::temp_dir().join(format!("abtest-badmanifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("market-manifest.toml"),
            "pool = \"SENTINELVALUE\u{1b}[2Jpaid\u{7}\"\n",
        )
        .unwrap();
        let err = MarketManifest::load(&dir).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("market manifest is malformed"),
            "refusal must carry the fixed message: {rendered:?}"
        );
        for source in err.chain() {
            let text = source.to_string();
            assert!(
                !text.contains("SENTINELVALUE"),
                "error chain leaked the planted value: {text:?}"
            );
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'),
                "error chain leaked terminal controls: {text:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single-source bridge manifest carries no governance contract: the
    /// absence parses as absence, round-trips as absence, and serializes as
    /// absence. The absence must never gain an address on its own.
    #[test]
    fn a_market_manifest_may_omit_governance_without_fabricating_one() {
        use std::str::FromStr;
        let c = || {
            ContractId::from_str("CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
                .unwrap()
        };
        let manifest = MarketManifest {
            real: None,
            network: MarketNetworkRecord {
                label: Safe::literal(MARKET_NETWORK_LABEL),
                passphrase: Safe::literal(MARKET_TESTNET_PASSPHRASE),
            },
            admin: AccountId::from_str("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF")
                .unwrap(),
            rpc_host: Safe::literal("rpc.testnet.local"),
            oracle: OracleStackManifest {
                runtime: c(),
                governance: None,
                adapter: c(),
                sources: vec![c()],
                min_sources: 1,
                max_age_secs: 3600,
                max_clock_drift_secs: 60,
                decimals: 7,
                resolution: 300,
                base: Safe::literal("USD"),
            },
            tokens: BTreeMap::new(),
            pool: c(),
            backstop: c(),
            factory: c(),
            comet: c(),
            emitter: c(),
            reserves: BTreeMap::new(),
            identities: BTreeMap::new(),
            deploy_ledger: 1,
        };
        assert!(manifest.oracle.governance.is_none());
        let dir = std::env::temp_dir().join(format!("abtest-nogov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("market-manifest.toml"),
            toml::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let back = MarketManifest::load(&dir).unwrap();
        assert!(
            back.oracle.governance.is_none(),
            "an omitted governance field must load back as none"
        );
        let rendered = toml::to_string(&back).unwrap();
        assert!(
            !rendered.contains("governance"),
            "governance must not serialize when absent: {rendered}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
// --------------------------------------------------------------------------
// Run lock
//
// Two runners on one output directory interleave writes to `state.json`, so the
// directory is owned by at most one live process. The TUI uses the same lock to
// decide between driving and watching.
// --------------------------------------------------------------------------

pub struct RunLock {
    path: PathBuf,
}

impl RunLock {
    fn file(out: &Path) -> PathBuf {
        out.join("run.lock")
    }

    /// `pid start_ticks`; the start time disambiguates a recycled pid.
    fn stamp() -> String {
        format!(
            "{} {}",
            std::process::id(),
            proc_start_ticks(std::process::id()).unwrap_or(0)
        )
    }

    /// `Ok(None)` when a live process already owns this directory.
    /// `create_new` alone does not exclude a racing reclaim (unlink-then-recreate),
    /// so after stamping we only trust a file still carrying our own stamp.
    pub fn acquire(out: &Path) -> anyhow::Result<Option<RunLock>> {
        let path = Self::file(out);
        let mine = Self::stamp();
        for _ in 0..3 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(mine.as_bytes())?;
                    drop(f);
                    if Self::read_stamp(&path).as_deref() == Some(mine.as_str()) {
                        return Ok(Some(RunLock { path }));
                    }
                    // A racing reclaim replaced our file; the next round sees it.
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::owner(out).is_some() {
                        return Ok(None);
                    }
                    let _ = std::fs::remove_file(&path); // recorded but dead: reclaim
                }
                Err(e) => return Err(e).with_context(|| format!("lock {}", path.display())),
            }
        }
        Ok(None) // lost the race repeatedly
    }

    fn read_stamp(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_owned())
    }

    /// The pid in the lock file, if that process is still the one that wrote it.
    /// A recycled pid has a different `/proc` start time and reads as stale —
    /// including when the recycled pid happens to be ours.
    pub fn owner(out: &Path) -> Option<u32> {
        let text = std::fs::read_to_string(Self::file(out)).ok()?;
        let mut it = text.split_whitespace();
        let pid: u32 = it.next()?.parse().ok()?;
        let start: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        // `start == 0` never comes from our own stamp on Linux (a live pid
        // always has /proc ticks): it marks a file written by an external
        // writer, so it is foreign — stale, not "unknown, trust the pid".
        if start == 0 {
            return None;
        }
        proc_start_ticks(pid).map(|t| t == start)?.then_some(pid)
    }
}

/// 100 Hz process start time (`/proc/<pid>/stat` field 22); `None` when the pid is gone.
fn proc_start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // Never release a lock another holder reclaimed under us.
        let mine = Self::stamp();
        if Self::read_stamp(&self.path).as_deref() == Some(mine.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_then_released() {
        let dir = std::env::temp_dir().join(format!("abtest-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let held = RunLock::acquire(&dir).unwrap().expect("first acquire wins");
        assert_eq!(RunLock::owner(&dir), Some(std::process::id()));
        assert!(
            RunLock::acquire(&dir).unwrap().is_none(),
            "second acquire must observe the live owner"
        );
        drop(held);
        assert_eq!(RunLock::owner(&dir), None);
        assert!(RunLock::acquire(&dir).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let dir = std::env::temp_dir().join(format!("abtest-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.lock"), "4294967294").unwrap(); // no such pid
        assert_eq!(RunLock::owner(&dir), None);
        assert!(RunLock::acquire(&dir).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// --------------------------------------------------------------------------
// Market mode: one disposable protected Blend market on Stellar testnet.
//
// Everything above is pair-only and stays unchanged. Market deployment and
// play get their own strict config, manifests, and custody shapes (plan §2/§4)
// so a single market is never dressed up as an A/B pair. Market records carry
// only rendered `Safe` text and item-7 validated identifiers — there is no
// `Invocation`/`ErrorKind`/`RawText` field here, so raw CLI bytes are
// structurally absent from every market surface.
// --------------------------------------------------------------------------

/// Exact Stellar testnet passphrase: a market-mode invariant that no config or
/// manifest can relax, rechecked before every mutation.
pub const MARKET_TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";

/// Credential-free network label persisted in `MarketManifest` (never a
/// serializable pair `Net`).
pub const MARKET_NETWORK_LABEL: &str = "testnet";

/// Exact Stellar mainnet passphrase, the second network a market run may be
/// frozen to. Same law as the testnet constant: no config or manifest can
/// relax it, and it is rechecked before every mutation.
pub const MARKET_MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";

/// Credential-free network label for mainnet registry writes
/// (`registry/mainnet.jsonl`).
pub const MARKET_MAINNET_LABEL: &str = "mainnet";

/// Synthetic quorum shape required by `validate_proxy_config`
/// (`contracts/contract/proxy-oracle/soroban/common/src/lib.rs:158-191`).
pub const MARKET_SOURCE_COUNT: usize = 3;
pub const MARKET_MIN_SOURCES: u32 = 3;
pub const MARKET_MAX_AGE_SECS: u32 = 3600;
pub const MARKET_MAX_CLOCK_DRIFT_SECS: u32 = 60;
/// SEP-40 feed granularity; deliberately different from the adapter's own
/// resolution — the adapter is its own Blend oracle (plan §4 phase 4).
pub const MARKET_SOURCE_RESOLUTION: u32 = 300;
pub const MARKET_SOURCE_DECIMALS: u32 = 7;
pub const MARKET_ADAPTER_DECIMALS: u32 = 7;
pub const MARKET_ADAPTER_RESOLUTION: u32 = 1;
pub const MARKET_BASE: &str = "USD";

/// Artifact roles whose optimized Wasm digests are required before the first
/// network call. One qualified source digest is intentionally reused for the
/// three same-admin source roles (plan §2).
pub const MARKET_ARTIFACT_ROLES: [&str; 10] = [
    "pool",
    "backstop",
    "factory",
    "token",
    "source",
    "comet",
    "emitter",
    "proxy-runtime",
    "proxy-governance",
    "adapter",
];

/// The artifact role of a Pyth Lazer source instance, pinned by a real
/// market that declares Lazer sources. Not part of the lab stack's role set.
pub const MARKET_LAZER_SOURCE_ROLE: &str = "lazer-source";

/// The roles a real market uploads its own bytes for: its pool stack, its
/// price adapter, and the proxy stack it configures. Comet and the emitter are
/// shared production contracts already live on every network — pinned by hash,
/// never uploaded — and the mock token/publisher roles do not exist here: a
/// real market's legs are classic assets and its prices come from Lazer or a
/// third-party SEP-40 contract. Mainnet upload fees are ~1.5 XLM per KB, so
/// nothing outside this list is ever submitted.
pub const MARKET_REAL_ARTIFACT_ROLES: [&str; 6] = [
    "pool",
    "backstop",
    "factory",
    "adapter",
    "proxy-runtime",
    "proxy-governance",
];

/// One whole unit in the 7-decimal fixed point every Blend parameter and
/// quote uses.
pub const MARKET_7DP_UNIT: u32 = 10_000_000;

/// Upper bound on an interest-rate parameter; the curve's own units are
/// 7-decimal per-year rates with headroom for the kink values.
pub const MARKET_IRM_MAX: u32 = 1_000_000_000;

/// Bounds a real market's config must fall inside. These are sanity bounds on
/// operator input, not the contract's acceptance rules: the contract decides
/// what it accepts, and a run that violates them fails at the first call
/// rather than at a guessed threshold.
pub const MARKET_MAX_POSITIONS_CAP: u32 = 32;
pub const MARKET_MAX_RESERVES: usize = 4;
pub const MARKET_MAX_SOURCES: usize = 8;
pub const MARKET_MAX_AGE_CAP: u32 = 86_400;
pub const MARKET_MAX_DRIFT_CAP: u32 = 3_600;

/// Parse an i128 config amount the way the CLI wants it: a decimal string,
/// positive, no separators. Shared by every i128 field of a real market.
fn positive_i128(value: &str, name: &str) -> anyhow::Result<()> {
    let parsed = value
        .parse::<i128>()
        .map_err(|_| anyhow!("{name} must be a positive integer in base 10"))?;
    anyhow::ensure!(parsed > 0, "{name} must be positive");
    Ok(())
}

/// Variant order is the wire-name order: the `BTreeMap` ordering, the string
/// the deployer looks roles up by (`Borrow<str>`), and the TOML key order all
/// have to agree, so adding a role means inserting it at its alphabetical spot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    Adapter,
    Backstop,
    Comet,
    Emitter,
    Factory,
    #[serde(rename = "lazer-source")]
    LazerSource,
    Pool,
    #[serde(rename = "proxy-governance")]
    ProxyGovernance,
    #[serde(rename = "proxy-runtime")]
    ProxyRuntime,
    Source,
    Token,
}

impl std::borrow::Borrow<str> for ArtifactRole {
    fn borrow(&self) -> &str {
        match self {
            Self::Adapter => "adapter",
            Self::Backstop => "backstop",
            Self::Comet => "comet",
            Self::Emitter => "emitter",
            Self::Factory => "factory",
            Self::LazerSource => "lazer-source",
            Self::Pool => "pool",
            Self::ProxyGovernance => "proxy-governance",
            Self::ProxyRuntime => "proxy-runtime",
            Self::Source => "source",
            Self::Token => "token",
        }
    }
}

impl std::fmt::Display for ArtifactRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(std::borrow::Borrow::<str>::borrow(self))
    }
}

impl std::str::FromStr for ArtifactRole {
    type Err = anyhow::Error;

    /// The one way a role name becomes a role: it has to be the exact wire
    /// name `Borrow<str>` renders, so a launch profile and a market config can
    /// never disagree about what a role is called.
    fn from_str(name: &str) -> anyhow::Result<Self> {
        [
            Self::Adapter,
            Self::Backstop,
            Self::Comet,
            Self::Emitter,
            Self::Factory,
            Self::LazerSource,
            Self::Pool,
            Self::ProxyGovernance,
            Self::ProxyRuntime,
            Self::Source,
            Self::Token,
        ]
        .into_iter()
        .find(|role| std::borrow::Borrow::<str>::borrow(role) == name)
        .ok_or_else(|| anyhow!("'{name}' is not an artifact role"))
    }
}

impl ArtifactRole {
    /// The only catalog artifact key this role may resolve from a contracts
    /// release. `None` means the role has no release form and stays a path
    /// pin — the proxy-oracle stack's other members are deployed by other
    /// tools and are not pinned by a market config.
    pub fn release_key(&self) -> Option<&'static str> {
        match self {
            Self::ProxyRuntime => Some("soroban-proxy-oracle"),
            Self::ProxyGovernance => Some("soroban-proxy-governance"),
            _ => None,
        }
    }
}

/// A persisted SHA-256 digest; parsing never normalizes malformed input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Sha256Hex {
    fn default() -> Self {
        // The canonical custody payload blanks this slot before hashing.
        Self("0".repeat(64))
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = anyhow::Error;

    fn try_from(value: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            is_sha256_hex(&value),
            "SHA-256 digest must be 64 lowercase hex characters"
        );
        Ok(Self(value))
    }
}

impl std::ops::Deref for Sha256Hex {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

/// A market deployment's network, frozen into the plan, the manifest and every
/// persisted custody record. The only two networks the tool can represent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarketNetwork {
    #[serde(rename = "Test SDF Network ; September 2015")]
    Testnet,
    #[serde(rename = "Public Global Stellar Network ; September 2015")]
    Mainnet,
}

impl MarketNetwork {
    /// The exact network passphrase this arm represents.
    #[must_use]
    pub const fn passphrase(self) -> &'static str {
        match self {
            Self::Testnet => MARKET_TESTNET_PASSPHRASE,
            Self::Mainnet => MARKET_MAINNET_PASSPHRASE,
        }
    }

    /// The short registry label (`registry/<label>.jsonl`) for this network.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Testnet => MARKET_NETWORK_LABEL,
            Self::Mainnet => MARKET_MAINNET_LABEL,
        }
    }

    /// Resolve a network from its registry label alone, as a pool config's
    /// `network` field names it.
    pub fn from_label(label: &str) -> anyhow::Result<Self> {
        match label {
            MARKET_NETWORK_LABEL => Ok(Self::Testnet),
            MARKET_MAINNET_LABEL => Ok(Self::Mainnet),
            _ => bail!("unknown network label; a market runs on 'testnet' or 'mainnet'"),
        }
    }

    /// Resolve the network from the exact passphrase/label pair a config
    /// declares, or fail when the pair is not one of the two known networks.
    pub fn from_pair(passphrase: &str, label: &str) -> anyhow::Result<Self> {
        let resolved = if passphrase == MARKET_MAINNET_PASSPHRASE {
            Self::Mainnet
        } else if passphrase == MARKET_TESTNET_PASSPHRASE {
            Self::Testnet
        } else {
            bail!(
                "network passphrase must be an exact known network passphrase \
                 ({MARKET_TESTNET_PASSPHRASE} or {MARKET_MAINNET_PASSPHRASE})"
            )
        };
        anyhow::ensure!(
            resolved.label() == label,
            "network label must be `{}` for passphrase `{passphrase}`",
            resolved.label()
        );
        Ok(resolved)
    }
}

/// A positive signed amount, retaining the existing i64 wire range.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(try_from = "i64")]
pub struct PositiveAmount(i64);

impl TryFrom<i64> for PositiveAmount {
    type Error = anyhow::Error;

    fn try_from(value: i64) -> anyhow::Result<Self> {
        anyhow::ensure!(value > 0, "initial balance amount must be positive");
        Ok(Self(value))
    }
}

impl std::fmt::Display for PositiveAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A local artifact file: path plus its required SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePin {
    pub path: PathBuf,
    pub sha256: Sha256Hex,
}

/// A published contracts release: the catalog artifact key, an optional exact
/// version (absent means the newest catalogued stable release), and the
/// provenance the release resolver froze at deploy start. `path` is where the
/// resolver wrote and verified the bytes — the one location the deploy path
/// reads. It is a runtime fact: never serialized, so the config digest does
/// not depend on which cache directory resolved the release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePin {
    pub release: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<crate::release::Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<crate::release::Provenance>,
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

/// One pinned Wasm artifact: a verified local file, or a published contracts
/// release resolved to verified bytes before any network call.
// `untagged` shapes serialization only: path pins keep the exact
// `[artifacts.<role>] path/sha256` bytes the frozen config digest is bound
// to. Deserialization goes through `RawArtifactPin` for precise errors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ArtifactPin {
    File(FilePin),
    Release(ReleasePin),
    /// An artifact pinned to a Wasm hash that is already live on the target
    /// network (a shared production contract: Comet, the emitter). There are
    /// no local bytes to hash: the run proves the hash is fetchable and
    /// settles its upload slot without submitting anything.
    Live(LivePin),
}

impl ArtifactPin {
    /// The local bytes and their digest for a pin the deploy path can read.
    /// A release pin must be resolved first; its bytes are at the path the
    /// resolver wrote them to. A live pin has no bytes by design.
    pub fn file_location(&self) -> anyhow::Result<(PathBuf, Sha256Hex)> {
        match self {
            Self::File(f) => Ok((f.path.clone(), f.sha256.clone())),
            Self::Release(r) => match (&r.resolved, &r.path) {
                (Some(provenance), Some(path)) => Ok((path.clone(), provenance.sha256.clone())),
                _ => anyhow::bail!("a release artifact pin was never resolved"),
            },
            Self::Live(_) => anyhow::bail!(
                "this artifact is pinned to a hash already live on the network; \
                 it is never uploaded"
            ),
        }
    }

    /// The Wasm hash this pin pins, whichever form it is: the hash the upload
    /// slot compares against the network's answer.
    pub fn wasm_hash(&self) -> anyhow::Result<Sha256Hex> {
        match self {
            Self::File(f) => Ok(f.sha256.clone()),
            Self::Release(r) => r
                .resolved
                .as_ref()
                .map(|p| p.sha256.clone())
                .ok_or_else(|| anyhow!("a release artifact pin was never resolved")),
            Self::Live(l) => Ok(l.wasm_hash.clone()),
        }
    }
}

/// An artifact that is already deployed as the network's own contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LivePin {
    pub wasm_hash: Sha256Hex,
}

/// Parse-level shape of one `[artifacts.<role>]` table: exactly one of the
/// three forms, with each form's fields complete.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtifactPin {
    path: Option<PathBuf>,
    sha256: Option<Sha256Hex>,
    release: Option<String>,
    version: Option<crate::release::Version>,
    wasm_hash: Option<Sha256Hex>,
}

impl TryFrom<RawArtifactPin> for ArtifactPin {
    type Error = anyhow::Error;

    fn try_from(raw: RawArtifactPin) -> anyhow::Result<Self> {
        let RawArtifactPin {
            path,
            sha256,
            release,
            version,
            wasm_hash,
        } = raw;
        match (path, sha256, release, wasm_hash) {
            (Some(path), Some(sha256), None, None) => {
                anyhow::ensure!(version.is_none(), "version belongs to a release pin");
                Ok(Self::File(FilePin { path, sha256 }))
            }
            (None, None, Some(release), None) => Ok(Self::Release(ReleasePin {
                release,
                version,
                resolved: None,
                path: None,
            })),
            (None, None, None, Some(wasm_hash)) => Ok(Self::Live(LivePin { wasm_hash })),
            _ => anyhow::bail!(
                "an artifact pin is {{path, sha256}}, {{release, version?}} \
                 or {{wasm_hash}}"
            ),
        }
    }
}

impl<'de> Deserialize<'de> for ArtifactPin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RawArtifactPin::deserialize(deserializer)
            .and_then(|raw| Self::try_from(raw).map_err(serde::de::Error::custom))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketNetworkConfig {
    pub passphrase: String,
    pub label: String,
}

/// Proxy-oracle stack wiring: the sources, the quorum, and the freshness
/// bounds. For the lab stack these are the frozen synthetic shape; for a real
/// market the sources are declared and the quorum is bounded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleStackConfig {
    pub sources: usize,
    pub min_sources: u32,
    pub max_age_secs: u32,
    pub max_clock_drift_secs: u32,
    pub base: String,
    pub decimals: u32,
    pub resolution: u32,
    /// For a real market, the contract prices are denominated in — the asset
    /// that is the pool's own quote unit. Absent means the lab `USD` base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_asset: Option<String>,
}

/// Immutable `PoolOracleAdapter` metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterConfig {
    pub decimals: u32,
    pub resolution: u32,
    pub base: String,
}

/// Initial fake balance minted to a run-local actor. Test assets only.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialBalance {
    pub who: Who,
    pub token: Tok,
    pub amount: PositiveAmount,
}

// ---------- real-market configuration (mainnet launch, ADR-0008) ----------
//
// A lab market (`markets/protected-testnet.toml`, no `[pool]`) deploys the
// mock token/oracle stack the abtest pairs use. A real market brings its own
// pool parameters, its own reserves — real assets with their own identifiers —
// its own price sources, and the activation shim ADR-0008 requires. The
// difference is one discriminator: `[pool]` present means a real market, and
// every rule below keys off it.

/// The minimum Comet backing value the backstop threshold accepts, in
/// 7-decimal units of each synthetic leg.
pub const MARKET_SHIM_MIN_BACKSTOP_VALUE: u128 = 10u128.pow(25);

/// The seeded balance of each synthetic leg, in stroops, as the runbook
/// derives it: the smallest whole-token amount `u` of the 20%-weight leg with
/// `(4u)^4 * u >= 1e25` (the backstop's fourth-power test on the 80/20 Comet),
/// plus one whole token of margin on each leg. Deterministic, so two runs of
/// the same config seed the same amounts.
#[must_use]
pub fn market_shim_seed() -> (i128, i128) {
    let scale = 10_000_000i128;
    let target = i128::try_from(MARKET_SHIM_MIN_BACKSTOP_VALUE).expect("1e25 fits i128");
    let mut lo: i128 = 1;
    let mut hi: i128 = 1;
    while shim_backstop_value(hi) < target {
        hi *= 2;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if shim_backstop_value(mid) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // `lo` is the smallest whole-token amount of the minority leg; the majority
    // leg is its 4x by the 80/20 weights. One token of margin on each.
    let usdc = (lo + 1) * scale;
    let blnd = (lo * 4 + 1) * scale;
    (blnd, usdc)
}

/// `(4u)^4 * u` for one whole-token amount `u` of the minority leg.
fn shim_backstop_value(u: i128) -> i128 {
    let blnd = u
        .checked_mul(4)
        .and_then(|b| b.checked_pow(4))
        .expect("shim search stays inside i128");
    blnd.checked_mul(u).expect("shim search stays inside i128")
}

/// The two classic legs of the activation shim: synthetic, no-value assets
/// issued by run-local issuer accounts whose only holder is the pool's Comet.
/// Codes are classic asset codes (1-12 alphanumeric) and must differ.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketShim {
    pub blnd_code: String,
    pub usdc_code: String,
}

/// How one price source of a real market's oracle is obtained. The grammar is
/// the pool config's: a Lazer instance is named by its numeric feed id and
/// bound to the market's verifier, and an existing SEP-40 contract is named by
/// its address and the asset key it advertises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// The synthetic single-admin source the lab stack uses. Testnet only.
    Mock,
    /// A Pyth Lazer source instance (the pinned lazer source Wasm), bound to a
    /// Pyth verifier contract at construction.
    Lazer,
    /// A third-party SEP-40 contract that already exists — referenced, never
    /// deployed or reconfigured by this tool.
    #[serde(rename = "sep40")]
    Sep40,
}

/// One source of one reserve's price feed. Sources are per reserve because each
/// asset configures its own ProxyConfig, and the sources behind different assets
/// are pushed on different cadences: Lazer every minute, a RedStone wrapper on
/// a 12-hour heartbeat.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleSource {
    pub kind: SourceKind,
    /// `Lazer`: the Pyth Lazer feed id this entry serves. The proxy keys a
    /// Lazer source's asset `{"Other": "<feed_id>"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_id: Option<String>,
    /// `Sep40`: the contract id of the already-deployed SEP-40 oracle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    /// `Sep40`: the asset key the contract advertises, as the contract's own
    /// `Asset` union. A RedStone mainnet wrapper keys its assets
    /// `{"Stellar": "<token contract id>"}` and reports the base
    /// `{"Other": "USD"}` at 8 decimals; the runtime's own base is
    /// `{"Other": "USD"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<serde_json::Value>,
}

/// One reserve of a real market: a real asset, the complete `ReserveConfig`
/// the pool freezes for it, and the price sources its oracle quorum is built
/// from. Field-for-field the shape of one entry of the `reserves` array of
/// `precept/registry/configs/<pool>.json` together with that file's
/// `oracle.assets` entry for the reserve's label; `index` is the position in
/// this list, never config data.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketReserve {
    pub label: String,
    pub asset: String,
    pub decimals: u32,
    pub c_factor: u32,
    pub l_factor: u32,
    pub util: u32,
    pub max_util: u32,
    pub supply_cap: String,
    pub min_position: String,
    pub r_base: u32,
    pub r_one: u32,
    pub r_two: u32,
    pub r_three: u32,
    pub reactivity: u32,
    pub enabled: bool,
    /// This asset's own quorum and freshness. Each reserve maps to exactly one
    /// ProxyConfig on the runtime, and different assets are pushed on
    /// different cadences — Lazer every minute, a RedStone wrapper on a
    /// 12-hour heartbeat — so no price parameter is shared between assets.
    pub min_sources: u32,
    pub max_age_secs: u32,
    /// The clock drift this asset's ProxyConfig allows; absent means zero.
    #[serde(default)]
    pub max_clock_drift_secs: u32,
    /// This asset's price sources, in the order the proxy registers them.
    /// Every reserve names them: the ProxyConfig that reads them is its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<OracleSource>,
}

/// The arguments of `PoolFactory.deploy` plus the reserves to enable after it,
/// for a market whose pool lends real assets.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolSpec {
    /// The pool's own name, as the factory records it.
    pub name: String,
    /// The pool admin: a G-address. For a launch pool this is the temporary
    /// operator key handed over after the multisig is confirmed.
    pub admin: String,
    pub backstop_take_rate: u32,
    pub max_positions: u32,
    /// Effective collateral requirement in 7-decimal oracle base units.
    pub min_collateral: String,
    /// The status the run leaves the pool in, after the backstop deposit.
    pub target_status: u32,
    pub reserves: Vec<MarketReserve>,
    /// The Pyth Lazer verifier every Lazer source of this market binds. Named
    /// once for the market, as the pool config names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lazer_verifier: Option<String>,
    /// An already-deployed `pool_oracle_adapter` to use instead of deploying
    /// one: the pool config's own `pool.oracle`, which is how a pool is
    /// attached to an oracle the operator owns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// An already-deployed proxy runtime to reuse instead of deploying one.
    /// Oracle runtimes are grouped, not per pool: Templar's runtime serves
    /// xlm-usdc and dejaaa-usdc, Gami's serves savusd-usdc and
    /// earnusdc-usdc. Naming it here means this pool is the second of its
    /// group — the run then uploads and deploys no proxy code at all, verifies
    /// the runtime belongs to the governance this market configures, registers
    /// only the assets it does not already serve, and deploys this pool's own
    /// adapter over it. Re-registering an asset that is already served would
    /// clear its breakers, cache and history, so the run never does it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

/// The pool configuration a market run borrows from the registry's config
/// directory (`precept/registry/configs/<pool>.json`) instead of repeating.
/// The JSON is the authority the proposal review approved, so the tool reads
/// it — pool parameters, reserves, and the oracle's sources — rather than
/// asking the operator to transcribe any of it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryPoolConfig {
    name: String,
    network: String,
    #[serde(default)]
    proposed_by: Option<String>,
    pool: RegistryPoolInner,
    reserves: Vec<RegistryReserve>,
    target_status: u32,
    /// The market's oracle, as the pool config declares it: the verifier the
    /// Lazer instances bind, a runtime this pool shares, and one block per
    /// reserve label with that asset's quorum, freshness and sources.
    #[serde(default)]
    oracle: Option<RegistryOracle>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryPoolInner {
    admin: String,
    name: String,
    /// An existing `pool_oracle_adapter` the pool is attached to. When absent
    /// the run deploys and owns the adapter itself.
    oracle: Option<String>,
    backstop_take_rate: u32,
    max_positions: u32,
    min_collateral: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryOracle {
    /// The Pyth Lazer verifier every Lazer source of this pool binds. Present
    /// whenever any source is a Lazer instance.
    #[serde(default)]
    lazer_verifier: Option<String>,
    /// An existing proxy runtime this pool shares with the pool that first
    /// deployed it: oracle runtimes are grouped, not per pool. The run
    /// verifies it instead of deploying one.
    #[serde(default)]
    runtime: Option<String>,
    /// One block per reserve label. Quorum and freshness are per asset because
    /// the sources behind different assets are pushed on different cadences.
    assets: BTreeMap<String, RegistryAssetOracle>,
}

/// One entry of `oracle.assets[<reserve label>]`: the ProxyConfig the runtime
/// serves that one asset with, and the sources behind it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryAssetOracle {
    min_sources: u32,
    /// This asset's `ProxyConfig.max_age_secs`, in seconds.
    max_age_secs: u32,
    /// Absent means zero, the value every launch pool runs with.
    #[serde(default)]
    max_clock_drift_secs: u32,
    sources: Vec<RegistrySource>,
}

/// One entry of `oracle.assets[<reserve label>].sources`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrySource {
    kind: String,
    #[serde(default)]
    feed_id: Option<u64>,
    #[serde(default)]
    contract: Option<String>,
    #[serde(default)]
    asset: Option<serde_json::Value>,
}

/// One entry of the registry config's `reserves` array: the asset, its label,
/// and its `ReserveConfig` nested under `config`, with the `index` the
/// proposal declared. The declared index must be the array position — a config
/// that orders its reserves differently from its own indices is refused
/// rather than renumbered, because the index is what the pool freezes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryReserve {
    asset: String,
    label: String,
    config: RegistryReserveConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryReserveConfig {
    index: u32,
    decimals: u32,
    c_factor: u32,
    l_factor: u32,
    util: u32,
    max_util: u32,
    supply_cap: String,
    min_position: String,
    r_base: u32,
    r_one: u32,
    r_two: u32,
    r_three: u32,
    reactivity: u32,
    enabled: bool,
}

impl RegistryReserve {
    fn into_reserve(self, position: usize) -> anyhow::Result<MarketReserve> {
        anyhow::ensure!(
            usize::try_from(self.config.index) == Ok(position),
            "reserve '{}' declares index {} but sits at position {position}; \
             the pool freezes that index, so the order is not free",
            self.label,
            self.config.index
        );
        let config = self.config;
        Ok(MarketReserve {
            label: self.label,
            asset: self.asset,
            decimals: config.decimals,
            c_factor: config.c_factor,
            l_factor: config.l_factor,
            util: config.util,
            max_util: config.max_util,
            supply_cap: config.supply_cap,
            min_position: config.min_position,
            r_base: config.r_base,
            r_one: config.r_one,
            r_two: config.r_two,
            r_three: config.r_three,
            reactivity: config.reactivity,
            enabled: config.enabled,
            min_sources: 0,
            max_age_secs: 0,
            max_clock_drift_secs: 0,
            sources: Vec::new(),
        })
    }
}

impl RegistryPoolConfig {
    /// Translate a registry pool config into the market's `[pool]` table,
    /// including each reserve's price sources. The registry config declares the
    /// pool it proposes, so its network must be the run's network. Its
    /// `oracle` block is the market's source configuration; a pool that names
    /// an existing adapter instead declares none, because there is no quorum
    /// for this run to build. `proposed_by` and `notes` are governance prose
    /// and are deliberately not carried: they would enter the config digest
    /// without changing the deployment.
    fn into_pool(self, expected_label: &str) -> anyhow::Result<PoolSpec> {
        anyhow::ensure!(
            self.network == expected_label,
            "the pool config is for network '{}' but this market runs on '{expected_label}'",
            self.network
        );
        anyhow::ensure!(!self.name.trim().is_empty(), "the pool config has no name");
        let (verifier, runtime, declared_assets) = match self.oracle {
            Some(oracle) => (oracle.lazer_verifier, oracle.runtime, oracle.assets),
            None => (None, None, BTreeMap::new()),
        };
        let _ = (self.proposed_by, self.notes);
        let mut reserves = self
            .reserves
            .into_iter()
            .enumerate()
            .map(|(index, r)| r.into_reserve(index))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for reserve in &mut reserves {
            let asset = declared_assets.get(&reserve.label).ok_or_else(|| {
                anyhow!(
                    "the pool config declares no 'oracle.assets' entry for reserve '{}': \
                     each reserve names its own quorum, freshness and sources",
                    reserve.label
                )
            })?;
            reserve.min_sources = asset.min_sources;
            reserve.max_age_secs = asset.max_age_secs;
            reserve.max_clock_drift_secs = asset.max_clock_drift_secs;
            reserve.sources = asset
                .sources
                .clone()
                .into_iter()
                .map(|s| s.into_source())
                .collect::<anyhow::Result<Vec<_>>>()?;
        }
        let declared_labels: std::collections::BTreeSet<&String> = declared_assets.keys().collect();
        let reserve_labels: std::collections::BTreeSet<&String> =
            reserves.iter().map(|r| &r.label).collect();
        if let Some(unknown) = declared_labels.difference(&reserve_labels).next() {
            anyhow::bail!(
                "the pool config declares oracle assets for '{unknown}', which is not a reserve of this pool"
            );
        }
        Ok(PoolSpec {
            name: self.pool.name,
            admin: self.pool.admin,
            backstop_take_rate: self.pool.backstop_take_rate,
            max_positions: self.pool.max_positions,
            min_collateral: self.pool.min_collateral,
            target_status: self.target_status,
            reserves,
            lazer_verifier: verifier,
            adapter: self.pool.oracle,
            runtime,
        })
    }
}

impl RegistrySource {
    /// The market's own source shape, so one validator governs sources whether
    /// they came from the pool config or from a market config that declares
    /// its `[pool]` inline.
    fn into_source(self) -> anyhow::Result<OracleSource> {
        let kind = match self.kind.as_str() {
            "lazer" => SourceKind::Lazer,
            "sep40" => SourceKind::Sep40,
            "mock" => SourceKind::Mock,
            other => {
                bail!("an oracle source kind must be lazer, sep40 or mock, not '{other}'")
            }
        };
        Ok(OracleSource {
            kind,
            feed_id: self.feed_id.map(|id| id.to_string()),
            contract: self.contract,
            asset: self.asset,
        })
    }
}

// ---------- the ADR-0008 launch profile ----------
//
// A launch pool is deployed from the pool config its proposal review approved
// — `precept/registry/configs/<pool>.json` — and from nothing else the
// operator has to write. The code it runs is the profile below: pinned
// digests, so the same bytes deploy on every network, and a digest the
// network already holds (Comet, the emitter) needs no bytes at all.

/// One contract of the launch profile.
pub struct LaunchArtifact {
    /// The market config's artifact role.
    pub role: &'static str,
    /// The pinned Wasm digest.
    pub wasm_hash: &'static str,
    /// The file name the bytes are expected under in the operator's wasm
    /// directory, for the case where the network does not have them yet.
    pub asset: &'static str,
    /// The registry's `wasm` line for these bytes: artifact name, version and
    /// source JSON, as curated for the same digest on testnet. Release-pinned
    /// roles record what the release resolver froze instead.
    pub registry: (&'static str, &'static str, &'static str),
}

/// The code every launch pool runs: the hardened v0.1.1 pool stack, the
/// v0.1.4/v0.1.5 proxy stack, the v0.1.0 Lazer source, and the two shared
/// production contracts the backstop and the shim are built on.
pub const MARKET_LAUNCH_PROFILE: [LaunchArtifact; 9] = [
    LaunchArtifact {
        role: "pool",
        wasm_hash: "acf7a128074d4d9c34b4ac4e8ab46cc1148eb353eb3908ad0faf922851728086",
        asset: "pool.wasm",
        registry: ("pool", "v0.1.1", r#"{"asset":"pool.wasm","repo":"Templar-Protocol/precept","tag":"v0.1.1"}"#),
    },
    LaunchArtifact {
        role: "backstop",
        wasm_hash: "41608bcb5176fdfdc74d4594be7ea538e93bc32bda24bc537cc1cd9e44e650d1",
        asset: "backstop.wasm",
        registry: ("backstop", "v0.1.1", r#"{"asset":"backstop.wasm","repo":"Templar-Protocol/precept","tag":"v0.1.1"}"#),
    },
    LaunchArtifact {
        role: "factory",
        wasm_hash: "47956acea8b2ca37ea83e4153fd592609f430ddc4899eca7d7878c6bf1cfbe65",
        asset: "pool_factory.wasm",
        registry: ("pool_factory", "v0.1.1", r#"{"asset":"pool_factory.wasm","repo":"Templar-Protocol/precept","tag":"v0.1.1"}"#),
    },
    LaunchArtifact {
        role: "adapter",
        wasm_hash: "c5e1f21099f532b84d02571e24d196fbdb76e77286e973239dc47ed926f368c2",
        asset: "pool_oracle_adapter.wasm",
        registry: ("pool_oracle_adapter", "v0.1.1", r#"{"asset":"pool_oracle_adapter.wasm","repo":"Templar-Protocol/precept","tag":"v0.1.1"}"#),
    },
    LaunchArtifact {
        role: "proxy-runtime",
        wasm_hash: "10680d5d880757ca9705bd63f1b08bde9c2ef479f1ba2ccf27afdcf802ec31b6",
        asset: "templar-proxy-oracle-soroban.wasm",
        registry: ("", "", ""),
    },
    LaunchArtifact {
        role: "proxy-governance",
        wasm_hash: "226dae54f2c84b46306331165afc6a4938d22cc5e13227cbacc27f373aaf298c",
        asset: "templar-proxy-governance-soroban.wasm",
        registry: ("", "", ""),
    },
    LaunchArtifact {
        role: MARKET_LAZER_SOURCE_ROLE,
        wasm_hash: "b38a98281f328d02f8eb741bedf27bd8698fd5f69980cb12dee66f92b5dff959",
        asset: "pyth-lazer-source.wasm",
        registry: (
            "soroban-lazer-source",
            "v0.1.0",
            r#"{"asset":"templar_proxy_oracle_soroban_pyth_lazer_source_contract-0.1.0.wasm","repo":"Templar-Protocol/contracts","tag":"templar-proxy-oracle-soroban-pyth-lazer-source-contract-v0.1.0"}"#,
        ),
    },
    LaunchArtifact {
        role: "comet",
        wasm_hash: "8abc28913035c07411ed5d134e6bfeab4723d97ddd4d1a22a0605d35c94d1a36",
        asset: "comet.wasm",
        registry: (
            "comet",
            "blend-v2-production",
            r#"{"note":"Blend v2 production Comet, live on mainnet as Blend's backstop LP CAS3FL6TLZKDGGSISDBWGGPXT3NRR4DYTZD7YOD3HMYO6LTJUVGRVEAM"}"#,
        ),
    },
    LaunchArtifact {
        role: "emitter",
        wasm_hash: "438a5528cff17ede6fe515f095c43c5f15727af17d006971485e52462e7e7b89",
        asset: "emitter.wasm",
        registry: (
            "emitter",
            "blend-contract-sdk 1.22.0",
            r#"{"note":"prebuilt testutils wasm shipped with the blend-contract-sdk 1.22.0 crate (local cargo registry cache)"}"#,
        ),
    },
];

/// The synthetic legs the activation shim mints, named by the runbook.
pub const MARKET_SHIM_BLND_CODE: &str = "SHIMBLND";
pub const MARKET_SHIM_USDC_CODE: &str = "SHIMUSDC";

/// The file name the operator's wasm directory must hold for a profile entry
/// whose bytes the network does not already have.
#[cfg(test)]
pub fn launch_wasm_file(role: &str) -> Option<&'static LaunchArtifact> {
    MARKET_LAUNCH_PROFILE.iter().find(|a| a.role == role)
}

/// The launch profile as artifact pins.
///
/// The proxy stack is pinned by *release*, not by digest: the runtime is
/// re-released (contracts#692 lowers the proxy's source floor to one), so its
/// version is an operator input — `--artifact-version proxy-runtime=<v>`
/// resolves it through the release catalog and the run freezes the resolved
/// version and digest, exactly as the lab market does. Everything else the
/// launch runs is pinned by digest and carries no bytes: where the network
/// already has those bytes the run uploads nothing, and where it does not, the
/// deployer asks for the profile's file name in the operator's wasm directory
/// and verifies it against the same digest before submitting.
pub fn launch_artifact_pins() -> anyhow::Result<BTreeMap<ArtifactRole, ArtifactPin>> {
    let mut pins = BTreeMap::new();
    for entry in MARKET_LAUNCH_PROFILE {
        let role = entry
            .role
            .parse::<ArtifactRole>()
            .map_err(|_| anyhow!("the launch profile names an unknown role"))?;
        let pin = match entry.role {
            "proxy-runtime" | "proxy-governance" => ArtifactPin::Release(ReleasePin {
                release: role
                    .release_key()
                    .ok_or_else(|| anyhow!("the launch profile's proxy role has no release"))?
                    .to_string(),
                version: None,
                resolved: None,
                path: None,
            }),
            _ => {
                let hash = Sha256Hex::try_from(entry.wasm_hash.to_string()).map_err(|_| {
                    anyhow!(
                        "the launch profile digest for '{}' is malformed",
                        entry.role
                    )
                })?;
                ArtifactPin::Live(LivePin { wasm_hash: hash })
            }
        };
        anyhow::ensure!(
            pins.insert(role, pin).is_none(),
            "the launch profile pins '{}' twice",
            entry.role
        );
    }
    Ok(pins)
}

impl MarketConfig {
    /// Turn every digest pin the network does not hold into a file pin on
    /// the bytes in `wasm_dir`, verified against the same digest, so the
    /// upload coordinate submits them; a digest the network already serves
    /// stays a live pin and uploads nothing. Done once, before the plan is
    /// frozen, so the plan records exactly what this run uploads. A first
    /// mainnet deploy needs every launch artifact's bytes here; a later one
    /// finds them live. `live` answers whether the network holds a hash.
    pub fn materialize_missing_code(
        &mut self,
        wasm_dir: Option<&Path>,
        live: impl Fn(&str, &Sha256Hex) -> bool,
    ) -> anyhow::Result<Vec<ArtifactRole>> {
        let mut uploads = Vec::new();
        for (role, pin) in &mut self.artifacts {
            let name = role.to_string();
            // A release pin whose resolved digest the network already holds
            // (a later pool, after the first uploaded it) uploads nothing
            // either: it becomes a live pin and takes no slot.
            if let ArtifactPin::Release(release) = pin {
                if let Some(resolved) = &release.resolved {
                    if live(&name, &resolved.sha256) {
                        *pin = ArtifactPin::Live(LivePin {
                            wasm_hash: resolved.sha256.clone(),
                        });
                    }
                }
                continue;
            }
            let ArtifactPin::Live(LivePin { wasm_hash }) = pin else {
                continue;
            };
            if live(&name, wasm_hash) {
                continue;
            }
            let entry = MARKET_LAUNCH_PROFILE
                .iter()
                .find(|a| a.role == role.to_string())
                .ok_or_else(|| anyhow!("'{role}' is pinned to a hash this network does not hold and the launch profile has no file for it"))?;
            let dir = wasm_dir.ok_or_else(|| {
                anyhow!(
                    "'{role}' ({}) is not on this network yet; pass --wasm-dir <dir> holding {} so this run uploads it",
                    wasm_hash.as_str(),
                    entry.asset
                )
            })?;
            let path = dir.join(entry.asset);
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading {} for '{role}'", path.display()))?;
            let digest = Sha256Hex::try_from(crate::market::sha256_hex(&bytes))?;
            anyhow::ensure!(
                &digest == wasm_hash,
                "{} is not the reviewed '{role}' code: its digest is {}, the pin is {}",
                path.display(),
                digest.as_str(),
                wasm_hash.as_str()
            );
            *pin = ArtifactPin::File(FilePin {
                path,
                sha256: digest,
            });
            uploads.push(*role);
        }
        Ok(uploads)
    }
}

impl MarketConfig {
    /// Build the whole market configuration for a launch pool from its pool
    /// config alone — the one-command path. The reviewed proposal is the
    /// pool's parameters, its reserves and its price sources; the launch
    /// profile is the code. `declared` is the network the operator named on
    /// the command line; when given it must be the network the pool config
    /// was reviewed for.
    pub fn from_pool_config(
        path: &Path,
        declared: Option<MarketNetwork>,
    ) -> anyhow::Result<MarketConfig> {
        let text = std::fs::read_to_string(path).context("reading pool config")?;
        let raw: RegistryPoolConfig =
            serde_json::from_str(&text).context("the pool config is malformed")?;
        let network = MarketNetwork::from_label(&raw.network)?;
        if let Some(declared) = declared {
            anyhow::ensure!(
                declared == network,
                "the pool config was reviewed for {} but this command names {}",
                network.label(),
                declared.label()
            );
        }
        let pool = raw.into_pool(network.label())?;
        // Every leg of this pool is measured against USDC: the prices, the
        // backstop threshold and the shim's majority leg.
        let base_asset = pool
            .reserves
            .iter()
            .find(|r| r.label == "usdc")
            .map(|r| r.asset.clone())
            .ok_or_else(|| {
                anyhow!("a launch pool must have a reserve labelled 'usdc'")
            })?;
        let sources = pool
            .reserves
            .iter()
            .map(|r| r.sources.len())
            .max()
            .unwrap_or(0);
        let cfg = MarketConfig {
            network: MarketNetworkConfig {
                passphrase: network.passphrase().to_string(),
                label: network.label().to_string(),
            },
            artifacts: {
                let mut pins = launch_artifact_pins()?;
                // Reusing an oracle runtime means this run deploys no proxy
                // code, so it pins none.
                if pool.runtime.is_some() {
                    pins.remove(&ArtifactRole::ProxyRuntime);
                    pins.remove(&ArtifactRole::ProxyGovernance);
                }
                pins
            },
            oracle: OracleStackConfig {
                sources,
                // The stack-level fields record the widest values any reserve
                // asks for; what the runtime's ProxyConfigs are configured
                // from are the per-reserve values above.
                min_sources: pool.reserves.iter().map(|r| r.min_sources).max().unwrap_or(0),
                max_age_secs: pool.reserves.iter().map(|r| r.max_age_secs).max().unwrap_or(0),
                max_clock_drift_secs: pool
                    .reserves
                    .iter()
                    .map(|r| r.max_clock_drift_secs)
                    .max()
                    .unwrap_or(0),
                base: MARKET_BASE.to_string(),
                decimals: MARKET_ADAPTER_DECIMALS,
                resolution: MARKET_SOURCE_RESOLUTION,
                base_asset: Some(base_asset),
            },
            adapter: AdapterConfig {
                decimals: MARKET_ADAPTER_DECIMALS,
                resolution: MARKET_ADAPTER_RESOLUTION,
                base: MARKET_BASE.to_string(),
            },
            reserves: BTreeMap::new(),
            balances: Vec::new(),
            pool: Some(pool),
            pool_config: Some(path.to_path_buf()),
            shim: Some(MarketShim {
                blnd_code: MARKET_SHIM_BLND_CODE.to_string(),
                usdc_code: MARKET_SHIM_USDC_CODE.to_string(),
            }),
        };
        cfg.validate()
            .context("validating pool config")?;
        Ok(cfg)
    }
}

/// Strict single-market configuration
/// (`tools/abtest/markets/protected-testnet.toml`). Unknown fields are
/// rejected, never dropped.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketConfig {
    pub network: MarketNetworkConfig,
    pub artifacts: BTreeMap<ArtifactRole, ArtifactPin>,
    pub oracle: OracleStackConfig,
    pub adapter: AdapterConfig,
    #[serde(default)]
    pub reserves: BTreeMap<Tok, ReserveOverride>,
    #[serde(default)]
    pub balances: Vec<InitialBalance>,
    /// A real market's pool parameters and reserves. Their presence is the
    /// one discriminator between the lab stack and a real market.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolSpec>,
    /// Path of a `precept/registry/configs/<pool>.json` to read `[pool]`
    /// from, so the reviewed proposal is the authority. Resolved by `load`;
    /// a config sets either this or `[pool]`, never both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_config: Option<PathBuf>,
    /// The synthetic classic legs of the activation shim; required for a real
    /// market, absent for the lab stack (whose shim is the mock tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shim: Option<MarketShim>,
}

impl MarketConfig {
    /// Load and fully validate a market config. Every failure here happens
    /// before any executor call — no executor is even constructed.
    pub fn load(path: &Path) -> anyhow::Result<MarketConfig> {
        let text = std::fs::read_to_string(path).context("reading market config")?;
        let mut cfg: MarketConfig = toml::from_str(&text).context("parsing market config")?;
        if let Some(borrowed) = cfg.pool_config.clone() {
            anyhow::ensure!(
                cfg.pool.is_none(),
                "a market config sets both [pool] and pool_config"
            );
            let label = MarketNetwork::from_pair(&cfg.network.passphrase, &cfg.network.label)?.label();
            let text = std::fs::read_to_string(expand_home(&borrowed))
                .context("reading the referenced pool config")?;
            let borrowed: RegistryPoolConfig =
                serde_json::from_str(&text).context("the referenced pool config is malformed")?;
            cfg.pool = Some(borrowed.into_pool(label)?);
        }
        cfg.validate().context("validating market config")?;
        cfg.check_artifacts()
            .context("validating market artifacts")?;
        Ok(cfg)
    }

    /// The network this config is frozen to, or the reason it names no known
    /// network.
    pub fn network(&self) -> anyhow::Result<MarketNetwork> {
        MarketNetwork::from_pair(&self.network.passphrase, &self.network.label)
    }

    /// Whether this config describes a real market — its own pool parameters,
    /// its own real reserves — rather than the mock lab stack.
    #[must_use]
    pub fn is_real(&self) -> bool {
        self.pool.is_some()
    }

    /// Structural invariants (plan §2). Rejection text is input-free: config
    /// and CLI bytes are never echoed onto market error surfaces.
    pub fn validate(&self) -> anyhow::Result<()> {
        let network = self.network()?;
        match &self.pool {
            None => self.validate_lab(network),
            Some(pool) => self.validate_real(network, pool),
        }
    }

    /// The lab stack: the mock token set, three synthetic sources, the exact
    /// frozen quorum and freshness, and testnet only — it funds actors from
    /// friendbot and lends tokens that carry no value.
    fn validate_lab(&self, network: MarketNetwork) -> anyhow::Result<()> {
        anyhow::ensure!(
            network == MarketNetwork::Testnet,
            "the mock lab stack is testnet-only: a mainnet market brings real \
             reserves, its own pool parameters and its own price sources"
        );
        anyhow::ensure!(
            self.shim.is_none(),
            "the lab stack's shim is its mock tokens; [shim] belongs to a real market"
        );
        anyhow::ensure!(
            self.oracle.sources == MARKET_SOURCE_COUNT,
            "market config requires exactly three oracle sources"
        );
        anyhow::ensure!(
            self.oracle.min_sources == MARKET_MIN_SOURCES,
            "market config requires min_sources = 3"
        );
        anyhow::ensure!(
            self.oracle.max_age_secs == MARKET_MAX_AGE_SECS,
            "market config requires proxy max_age_secs = 3600"
        );
        anyhow::ensure!(
            self.oracle.max_clock_drift_secs == MARKET_MAX_CLOCK_DRIFT_SECS,
            "market config requires proxy max_clock_drift_secs = 60"
        );
        anyhow::ensure!(
            self.oracle.base == MARKET_BASE,
            "market config requires oracle base USD"
        );
        anyhow::ensure!(
            self.oracle.decimals == MARKET_SOURCE_DECIMALS,
            "market config requires source decimals = 7"
        );
        anyhow::ensure!(
            self.oracle.resolution == MARKET_SOURCE_RESOLUTION,
            "market config requires source resolution = 300"
        );
        anyhow::ensure!(
            self.oracle.base_asset.is_none(),
            "the lab stack quotes USD; base_asset belongs to a real market"
        );
        self.validate_oracle_shape()?;
        Ok(())
    }

    /// The invariants both shapes share: the adapter is the pool's oracle and
    /// reports 7 decimals of USD with resolution 1.
    fn validate_oracle_shape(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.adapter.decimals == MARKET_ADAPTER_DECIMALS,
            "market config requires adapter decimals = 7"
        );
        anyhow::ensure!(
            self.adapter.resolution == MARKET_ADAPTER_RESOLUTION,
            "market config requires adapter resolution = 1"
        );
        anyhow::ensure!(
            self.adapter.base == MARKET_BASE,
            "market config requires adapter base USD"
        );
        Ok(())
    }

    /// A real market: its pool parameters, its reserves, its sources and its
    /// activation shim are all config. Quorum and freshness are bounded, not
    /// equality-locked, because the number of independent sources a market
    /// needs is a property of the market — only the contract's own
    /// `validate_proxy_config` decides what it will accept.
    fn validate_real(&self, network: MarketNetwork, pool: &PoolSpec) -> anyhow::Result<()> {
        // (the network is used by the source rules below)
        anyhow::ensure!(
            !pool.name.trim().is_empty() && pool.name.len() <= 64,
            "a real market requires a pool name of 1-64 characters"
        );
        anyhow::ensure!(
            AccountId::from_str(&pool.admin).is_ok(),
            "the pool admin must be a G-address: a launch pool's admin is the \
             temporary operator key handed over after the multisig is confirmed"
        );
        anyhow::ensure!(
            pool.backstop_take_rate <= MARKET_7DP_UNIT,
            "backstop_take_rate must be a 7-decimal fraction"
        );
        anyhow::ensure!(
            (1..=MARKET_MAX_POSITIONS_CAP).contains(&pool.max_positions),
            "max_positions must be between 1 and {MARKET_MAX_POSITIONS_CAP}"
        );
        // 0 is a pool with no collateral minimum; the factory refuses only negatives
        // (pool_factory.rs:85).
        let min_collateral = pool.min_collateral.parse::<i128>().map_err(|_| {
            anyhow!("min_collateral must be a non-negative integer in base 10")
        })?;
        anyhow::ensure!(min_collateral >= 0, "min_collateral must not be negative");
        anyhow::ensure!(
            pool.target_status == 0,
            "a real market activates its pool with set_status 0, which is where \
             the backstop threshold is checked"
        );
        self.validate_reserves(pool)?;
        self.validate_shim()?;
        self.validate_sources(network, pool)?;
        self.validate_oracle_shape()?;
        Ok(())
    }

    fn validate_reserves(&self, pool: &PoolSpec) -> anyhow::Result<()> {
        anyhow::ensure!(
            !pool.reserves.is_empty() && pool.reserves.len() <= MARKET_MAX_RESERVES,
            "a real market declares between 1 and {MARKET_MAX_RESERVES} reserves"
        );
        let mut labels = std::collections::BTreeSet::new();
        let mut assets = std::collections::BTreeSet::new();
        for (index, reserve) in pool.reserves.iter().enumerate() {
            let at = format!("reserve {index} ('{}')", reserve.label);
            anyhow::ensure!(
                !reserve.label.trim().is_empty() && reserve.label.len() <= 32,
                "{at} needs a label of 1-32 characters"
            );
            anyhow::ensure!(
                labels.insert(reserve.label.clone()),
                "{at} repeats a reserve label"
            );
            anyhow::ensure!(
                ContractId::from_str(&reserve.asset).is_ok(),
                "{at} needs the asset's contract id (a C-address); an asset is \
                 never named by a symbol"
            );
            anyhow::ensure!(
                assets.insert(reserve.asset.clone()),
                "{at} repeats an asset another reserve already uses"
            );
            anyhow::ensure!(
                reserve.decimals > 0 && reserve.decimals <= 18,
                "{at} needs the asset's real decimals (1-18); the pool reads \
                 balances in the asset's own units"
            );
            for (name, value) in [
                ("c_factor", reserve.c_factor),
                ("l_factor", reserve.l_factor),
                ("util", reserve.util),
                ("max_util", reserve.max_util),
            ] {
                anyhow::ensure!(
                    value <= MARKET_7DP_UNIT,
                    "{at} {name} must be a 7-decimal fraction"
                );
            }
            anyhow::ensure!(
                reserve.util <= reserve.max_util,
                "{at} uses util above max_util"
            );
            for name in ["supply_cap", "min_position"] {
                let value = match name {
                    "supply_cap" => &reserve.supply_cap,
                    _ => &reserve.min_position,
                };
                positive_i128(value, &format!("{at} {name}"))?;
            }
            for (name, value) in [
                ("r_base", reserve.r_base),
                ("r_one", reserve.r_one),
                ("r_two", reserve.r_two),
                ("r_three", reserve.r_three),
                ("reactivity", reserve.reactivity),
            ] {
                anyhow::ensure!(value <= MARKET_IRM_MAX, "{at} {name} is out of range");
            }
        }
        Ok(())
    }

    fn validate_shim(&self) -> anyhow::Result<()> {
        let shim = self
            .shim
            .as_ref()
            .ok_or_else(|| anyhow!("a real market declares its activation shim's synthetic asset codes"))?;
        for (name, code) in [("blnd_code", &shim.blnd_code), ("usdc_code", &shim.usdc_code)] {
            anyhow::ensure!(
                !code.is_empty()
                    && code.len() <= 12
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-')),
                "the shim's {name} must be a classic asset code of 1-12 alphanumeric characters"
            );
        }
        anyhow::ensure!(
            shim.blnd_code != shim.usdc_code,
            "the shim's two legs are two distinct assets"
        );
        Ok(())
    }

    /// Sources are config-driven: each is a synthetic mock (testnet only), a
    /// Lazer source instance bound to a verifier the operator names, or an
    /// existing third-party SEP-40 contract this tool only references.
    fn validate_sources(
        &self,
        network: MarketNetwork,
        pool: &PoolSpec,
    ) -> anyhow::Result<()> {
        let base = self.oracle.base_asset.as_deref().ok_or_else(|| {
            anyhow!("a real market quotes a real asset: base_asset must name the contract the \
                    prices are denominated in")
        })?;
        anyhow::ensure!(
            ContractId::from_str(base).is_ok(),
            "base_asset must be a contract id (a C-address)"
        );
        anyhow::ensure!(
            self.oracle.base == MARKET_BASE,
            "market config requires oracle base USD"
        );
        // A source belongs to the reserve whose price it quotes, and a quorum
        // is only a quorum across distinct contracts — so every check below is
        // per reserve, including the distinctness set.
        for reserve in &pool.reserves {
            let mut external = std::collections::BTreeSet::new();
            // Each reserve configures exactly one ProxyConfig on the runtime,
            // so its quorum and freshness are its own: Lazer is pushed every
            // minute, a RedStone wrapper on a 12-hour heartbeat. A quorum of
            // one is lawful on a runtime that accepts it, so the tool never
            // pre-rejects one — the runtime's own error says whether it does.
            anyhow::ensure!(
                reserve.min_sources >= 1
                    && usize::try_from(reserve.min_sources).unwrap_or(usize::MAX)
                        <= MARKET_MAX_SOURCES,
                "reserve '{}' sets a quorum of {}; a quorum is at least 1 and no more \
                 than {MARKET_MAX_SOURCES}",
                reserve.label,
                reserve.min_sources
            );
            anyhow::ensure!(
                (1..=MARKET_MAX_AGE_CAP).contains(&reserve.max_age_secs),
                "reserve '{}' sets max_age_secs {}; it must be between 1 and \
                 {MARKET_MAX_AGE_CAP} seconds",
                reserve.label,
                reserve.max_age_secs
            );
            anyhow::ensure!(
                reserve.max_clock_drift_secs <= MARKET_MAX_DRIFT_CAP,
                "reserve '{}' sets max_clock_drift_secs {}; it must be between 0 and \
                 {MARKET_MAX_DRIFT_CAP} seconds",
                reserve.label,
                reserve.max_clock_drift_secs
            );
            anyhow::ensure!(
                !reserve.sources.is_empty()
                    && usize::try_from(reserve.min_sources).unwrap_or(usize::MAX)
                        <= reserve.sources.len()
                    && reserve.sources.len() <= MARKET_MAX_SOURCES,
                "reserve '{}' must declare at least its quorum of {} price sources and no \
                 more than {MARKET_MAX_SOURCES}",
                reserve.label,
                reserve.min_sources
            );
            for source in &reserve.sources {
                let at = format!("reserve '{}' source {:?}", reserve.label, source.kind);
                match source.kind {
                    // A synthetic source quotes a price the operator chose. It
                    // is a testnet instrument: on mainnet it would stand in for
                    // real data behind a pool that lends real value.
                    SourceKind::Mock => anyhow::ensure!(
                        network == MarketNetwork::Testnet,
                        "{at} is a synthetic mock source; a mainnet market's sources are \
                         Lazer instances and existing SEP-40 contracts"
                    ),
                    SourceKind::Lazer => {
                        let verifier = pool.lazer_verifier.as_deref().ok_or_else(|| {
                            anyhow!("{at} binds no verifier: the pool config names no \
                                    'lazer_verifier'")
                        })?;
                        anyhow::ensure!(
                            ContractId::from_str(verifier).is_ok(),
                            "the pool's lazer_verifier must be a contract id (a C-address)"
                        );
                        let feed_id = source.feed_id.as_deref().ok_or_else(|| {
                            anyhow!("{at} is a Lazer source and must name its feed id")
                        })?;
                        anyhow::ensure!(
                            !feed_id.trim().is_empty()
                                && feed_id.bytes().all(|b| b.is_ascii_digit()),
                            "{at} must name its Lazer feed id in base 10"
                        );
                        anyhow::ensure!(
                            source.contract.is_none() && source.asset.is_none(),
                            "{at} deploys its own contract; it takes no address or asset key"
                        );
                    }
                    SourceKind::Sep40 => {
                        let contract = source.contract.as_deref().ok_or_else(|| {
                            anyhow!("{at} is an existing contract and must name its address")
                        })?;
                        anyhow::ensure!(
                            ContractId::from_str(contract).is_ok(),
                            "{at} address must be a contract id (a C-address)"
                        );
                        anyhow::ensure!(
                            external.insert(contract.to_string()),
                            "{at} repeats an address another source of this reserve uses; a \
                             quorum of one contract is not a quorum"
                        );
                        anyhow::ensure!(
                            source.asset.is_some() && source.feed_id.is_none(),
                            "{at} is referenced, never configured: it names its asset key and \
                             no feed id"
                        );
                    }
                }
            }
        }
        Ok(())
    }


    /// The artifact roles this run must pin and may upload. The lab stack
    /// pins its fixed ten. A real market pins only the code it actually
    /// deploys on the network it is deploying to — mainnet uploads cost about
    /// 1.5 XLM per KB, so the mock token and `set_data` publisher roles are
    /// never part of it (its legs are classic assets, its prices come from
    /// Lazer or a third-party SEP-40 contract), and a market whose every
    /// source is a contract it only references pins no source Wasm at all.
    #[must_use]
    pub fn required_roles(&self) -> Vec<&'static str> {
        if !self.is_real() {
            return MARKET_ARTIFACT_ROLES.to_vec();
        }
        let mut roles = MARKET_REAL_ARTIFACT_ROLES.to_vec();
        // A pool that reuses an existing oracle runtime deploys no runtime or
        // governance of its own.
        if self.pool.as_ref().is_some_and(|p| p.runtime.is_some()) {
            roles.retain(|r| !matches!(*r, "proxy-runtime" | "proxy-governance"));
        }
        if self
            .pool
            .iter()
            .flat_map(|p| p.reserves.iter())
            .flat_map(|r| r.sources.iter())
            .any(|s| s.kind == SourceKind::Lazer)
        {
            roles.push(MARKET_LAZER_SOURCE_ROLE);
        }
        roles.sort_unstable();
        roles
    }

    /// Every required artifact role must be pinned. A path pin must point at
    /// an existing file; a release pin must name its role's catalog artifact
    /// (bytes are verified by the release resolver before any network call); a
    /// live pin asserts no bytes at all — the deployer proves the hash is
    /// already on the network before anything is deployed.
    /// Types enforce role names and digest syntax; deployer preflight checks
    /// the artifact bytes against the digest.
    pub fn check_artifacts(&self) -> anyhow::Result<()> {
        for role in self.required_roles() {
            anyhow::ensure!(
                self.artifacts.contains_key(role),
                "market config is missing required artifact role '{role}'"
            );
        }
        for (role, pin) in &self.artifacts {
            match pin {
                ArtifactPin::File(f) => {
                    anyhow::ensure!(
                        expand_home(&f.path).is_file(),
                        "market config artifact '{role}' file is missing"
                    );
                }
                ArtifactPin::Release(r) => {
                    anyhow::ensure!(
                        Some(r.release.as_str()) == role.release_key(),
                        "market config artifact '{role}' may only resolve \
                         '{}'",
                        role.release_key().unwrap_or("<no release form>")
                    );
                }
                ArtifactPin::Live(_) => {}
            }
        }
        Ok(())
    }

    /// The provenance each resolved release-form pin ran against, keyed by
    /// role wire name. Frozen into the deployment plan.
    pub fn releases(&self) -> BTreeMap<String, crate::release::Provenance> {
        self.artifacts
            .iter()
            .filter_map(|(role, pin)| match pin {
                ArtifactPin::Release(r) => r
                    .resolved
                    .as_ref()
                    .map(|res| (role.to_string(), res.clone())),
                ArtifactPin::File(_) | ArtifactPin::Live(_) => None,
            })
            .collect()
    }

    /// Deploy-time gate: every scheduled reserve must set every parameter
    /// explicitly. Fixture defaults (`deploy::reserve_config`) exist for pair
    /// runs; a market that lends real value may not inherit an unreviewed
    /// cap, factor or rate curve. `load` does not call this so an existing
    /// market's config still attaches; only deploy and resume refuse.
    pub fn require_explicit_reserves(&self) -> anyhow::Result<()> {
        // A real market declares every field of every reserve in its own
        // `[pool]` table (or the registry config it borrows); this gate is the
        // lab stack's, where a reserve may otherwise inherit a fixture default.
        if self.is_real() {
            return Ok(());
        }
        for tok in Tok::RESERVES {
            let missing = self
                .reserves
                .get(&tok)
                .map(ReserveOverride::missing_fields)
                .unwrap_or_else(|| ReserveOverride::default().missing_fields());
            anyhow::ensure!(
                missing.is_empty(),
                "market config reserve '{}' must set every parameter explicitly; missing: {}",
                tok.as_str(),
                missing.join(", ")
            );
        }
        Ok(())
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Network facts a market manifest may persist: the exact testnet passphrase
/// and a credential-free label. Holds `Safe` literals only.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketNetworkRecord {
    pub label: Safe,
    pub passphrase: Safe,
}

/// Deployed proxy-oracle stack. Identifiers are checksum-validated newtypes
/// that bind verbatim; labels are tool-chosen `Safe` literals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OracleStackManifest {
    pub runtime: ContractId,
    /// Proxy governance contract, or absent for a direct testnet bridge that
    /// has no governance contract. Omission is not an empty address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governance: Option<ContractId>,
    pub adapter: ContractId,
    /// Three distinct deployed source contracts — an explicitly single-admin
    /// synthetic quorum, not independent data.
    pub sources: Vec<ContractId>,
    pub min_sources: u32,
    pub max_age_secs: u32,
    pub max_clock_drift_secs: u32,
    pub decimals: u32,
    pub resolution: u32,
    pub base: Safe,
}

/// Deployed single-market manifest (`market-manifest.toml`). Reuses `Tok`,
/// `Who`, and `ReserveRef` from the pair vocabulary but never the
/// serializable pair `Net`: only the exact testnet passphrase, a
/// credential-free RPC host label, the resolved admin G-address, and
/// validated contract identifiers are persisted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketManifest {
    pub network: MarketNetworkRecord,
    /// Resolved external operator identity — never the alias or config path.
    pub admin: AccountId,
    /// Credential-free RPC host label or redacted endpoint identity.
    pub rpc_host: Safe,
    pub oracle: OracleStackManifest,
    pub tokens: BTreeMap<Tok, ContractId>,
    pub pool: ContractId,
    pub backstop: ContractId,
    pub factory: ContractId,
    pub comet: ContractId,
    pub emitter: ContractId,
    pub reserves: BTreeMap<Tok, ReserveRef>,
    /// Run-local actor identities.
    pub identities: BTreeMap<Who, AccountId>,
    /// A real market's own identifiers, absent for the lab stack. The
    /// synthetic legs of the activation shim and the real reserves the pool
    /// enabled: their assets are real contracts, not mock tokens, so they are
    /// never dressed up as `Tok` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real: Option<RealMarketManifest>,
    pub deploy_ledger: u32,
}

/// One synthetic leg of the activation shim, as deployed: the classic asset it
/// is issued as, the run-local issuer account that minted it, and the Stellar
/// Asset Contract that stands for it in the pool's Comet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShimLegManifest {
    pub code: Safe,
    pub issuer: AccountId,
    pub sac: ContractId,
}

/// One real reserve, as enabled on the pool. The asset identifier is the real
/// asset's contract, never a mock token's.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealReserveManifest {
    pub label: Safe,
    pub asset: ContractId,
    pub index: u32,
    pub decimals: u32,
}

/// What a real market's manifest records beyond the lab stack's shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealMarketManifest {
    pub pool_name: Safe,
    pub reserves: Vec<RealReserveManifest>,
    pub blnd: ShimLegManifest,
    pub usdc: ShimLegManifest,
    /// The pool's backstop deposit, in LP units, read back from the pool's
    /// own `pool_data` after the deposit — the proof the activation shim is
    /// what carries the pool's status transition.
    pub backstop_deposit: i64,
    /// The backstop's own fourth-power threshold, as the pool reports it.
    pub q4w_pct: i64,
    /// One price stack per reserve: a real market configures each asset's
    /// proxy on its own, so no single stack describes them all.
    pub proxies: Vec<AssetProxyManifest>,
    /// Every Lazer source instance this run deployed, in instance order — the
    /// contracts whose code is ours. A reserve may also quote an existing
    /// SEP-40 oracle the run did not deploy; that contract is not here, and a
    /// reader that records registry rows must never claim it. Absent in
    /// manifests written before this field existed, where the per-reserve
    /// source identities are the only evidence.
    #[serde(default)]
    pub lazer_sources: Vec<ContractId>,
}

/// One real asset's own price stack, as configured on the runtime. Sources
/// are the identities the configuration named (`lazer:<feed id>` or
/// `sep40:<contract id>`), never a secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetProxyManifest {
    pub label: Safe,
    pub asset: ContractId,
    pub adapter: ContractId,
    pub runtime: ContractId,
    pub sources: Vec<Safe>,
    pub min_sources: u32,
    pub max_age_secs: u32,
    pub max_clock_drift_secs: u32,
    pub decimals: u32,
    pub base: Safe,
}

impl MarketManifest {
    /// Load the completed market manifest for a run directory. Parsing the
    /// strict shape is the whole contract: a partial or malformed file is
    /// an error, never a best-effort market.
    pub fn load(out: &Path) -> anyhow::Result<Self> {
        let p = out.join("market-manifest.toml");
        let text = std::fs::read_to_string(&p).context("read market manifest failed")?;
        toml::from_str(&text).map_err(|_| anyhow::anyhow!("market manifest is malformed"))
    }
}

/// Market evidence for one executed action: rendered text and validated
/// identifiers only, produced by the pinned `MarketRunner` conversion seam.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketStepRecord {
    /// Numbered deployment/play phase.
    pub phase: u32,
    /// Checked operation label: a tool literal for deployment/play or a
    /// bounded rendered role/method for a generic call. Never a path.
    pub action: Safe,
    /// Sanitized command summary, or the same checked generic-call label.
    pub command: Safe,
    /// Rendered outcome classification.
    pub outcome: Safe,
    /// Mutated or read contract where applicable.
    pub target: Option<ContractId>,
    pub violations: Vec<Safe>,
}

/// Mutation kinds, each with a distinct reconciliation proof (plan §4 resume law).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketMutationKind {
    Upload,
    Deploy,
    Invoke,
    FundKey,
    Config,
    Governance,
}

/// Pre-send intent and settled custody for one market mutation: rendered
/// `Safe` text, checksum-validated identifiers, and shape-validated hex
/// digests. Never an `Invocation`, `ErrorKind`, or `RawText`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketCustodyRecord {
    pub phase: u32,
    pub kind: MarketMutationKind,
    /// Checked operation label: tool literal for deployment/play; generic
    /// actions store bounded rendered role/method from the live ABI. Journal
    /// directories use numeric phase/slot coordinates, never this field.
    pub action: Safe,
    /// Network this mutation is bound to, frozen from the plan; it
    /// serializes as the exact passphrase and deserializes only from one of
    /// the two known market passphrases.
    pub network: MarketNetwork,
    /// Frozen signer for this mutation.
    pub signer: AccountId,
    /// Redacted normalized payload digest.
    pub payload_digest: Sha256Hex,
    /// Mutated contract where applicable.
    pub target: Option<ContractId>,
    /// Artifact or constructor digest where applicable.
    pub artifact_digest: Option<Sha256Hex>,
    /// Deterministic predicted contract id where applicable.
    pub predicted: Option<ContractId>,
    /// Rendered pre-state anchors.
    pub pre_state: Vec<Safe>,
    /// Rendered tool-failure text; `None` unless the tool itself failed.
    pub tool_error: Option<Safe>,
}

/// Settled result for one market mutation: exactly one record set — the
/// custody record plus its rendered step. A pair `SideOutcome` never reaches
/// a market journal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketPendingResult {
    pub custody: MarketCustodyRecord,
    pub step: MarketStepRecord,
}

/// Test-only coordinate path for asserting journal effects. Production opens
/// pinned descriptors and never trusts a caller-supplied path to a coordinate.
#[cfg(test)]
pub fn market_pending_dir(out: &Path, phase: u32, slot: u32) -> PathBuf {
    out.join("market-pending")
        .join(format!("{phase:03}-{slot:02}"))
}

/// Fixed input-free refusal for a record the pinned reader could never
/// return. Enforced before any journal directory or file is created.
pub const MARKET_JOURNAL_RECORD_OVERSIZE: &str =
    "market journal record exceeds the reader cap; nothing was written";

/// Enforce the shared reader cap before any journal write. This runs
/// before create_new for intent and result records alike, so no journal
/// write can strand an unreadable oversized record, and it never truncates
/// or fabricates content: the whole record is refused whole.
fn ensure_market_record_within_cap(bytes: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes.len() <= crate::market_status::PENDING_RECORD_CAP,
        "{}",
        Safe::literal(MARKET_JOURNAL_RECORD_OVERSIZE)
    );
    Ok(())
}

/// Linux procfs supplies descriptor-relative paths without unsafe code; libc
/// supplies the per-architecture `O_*` values (x86_64 literals differ on arm).
/// Keep every directory pinned until the exclusive write and directory syncs
/// finish, then refuse a detectable rename/replacement before permitting a send.
fn write_market_pending(
    out: &Path,
    phase: u32,
    slot: u32,
    settled: bool,
    bytes: &[u8],
) -> anyhow::Result<()> {
    use std::io::{Error, ErrorKind, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let write = || -> std::io::Result<()> {
        let open_dir = |path: &Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                .open(path)
        };
        let create_dir = |path: &Path| match std::fs::create_dir(path) {
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
            result => result,
        };
        if !settled {
            std::fs::create_dir_all(out)?;
        }
        let out_dir = open_dir(out)?;
        let root_path = PathBuf::from(format!(
            "/proc/self/fd/{}/market-pending",
            out_dir.as_raw_fd()
        ));
        if !settled {
            create_dir(&root_path)?;
        }
        let root = open_dir(&root_path)?;
        let coordinate_path = PathBuf::from(format!(
            "/proc/self/fd/{}/{phase:03}-{slot:02}",
            root.as_raw_fd()
        ));
        if !settled {
            create_dir(&coordinate_path)?;
        }
        let dir = open_dir(&coordinate_path)?;
        let pinned = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        if !settled {
            match std::fs::symlink_metadata(pinned.join("result.json")) {
                Ok(_) => return Err(ErrorKind::AlreadyExists.into()),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let name = if settled {
            "result.json"
        } else {
            "intent.json"
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(pinned.join(name))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        dir.sync_all()?;
        root.sync_all()?;
        out_dir.sync_all()?;
        for (path, held) in [
            (out, &out_dir),
            (root_path.as_path(), &root),
            (coordinate_path.as_path(), &dir),
        ] {
            let current = std::fs::symlink_metadata(path)?;
            let original = held.metadata()?;
            if !current.is_dir()
                || current.dev() != original.dev()
                || current.ino() != original.ino()
            {
                return Err(ErrorKind::Other.into());
            }
        }
        Ok(())
    };
    // Preserve AlreadyExists for the play runner's occupied-slot classifier,
    // but never expose paths, symlink targets, or operating-system text.
    write().map_err(|error| Error::new(error.kind(), "market pending journal unavailable").into())
}

/// Durably reserve a market mutation intent before any executor call.
/// `create_new` mirrors the pair journal: a replayed attempt fails before it
/// can reach the chain, even if a prior process died after opening the
/// intent but before recording its outcome.
pub fn begin_market_pending(
    out: &Path,
    phase: u32,
    slot: u32,
    record: &MarketCustodyRecord,
) -> anyhow::Result<()> {
    anyhow::ensure!(record.phase == phase, "market intent phase mismatch");
    // The reader re-derives this digest, so an unbound one (e.g. the
    // `Sha256Hex::default()` placeholder) would strand the coordinate.
    anyhow::ensure!(
        crate::market::custody_payload_digest(record)? == record.payload_digest,
        "market intent payload digest is not bound to its record"
    );
    let bytes = serde_json::to_vec_pretty(record)?;
    ensure_market_record_within_cap(&bytes)?;
    write_market_pending(out, phase, slot, false, &bytes)
}

/// Write the settled result for a market mutation after a definite outcome.
/// Persists exactly one record set (`MarketCustodyRecord` + `MarketStepRecord`).
pub fn finish_market_pending(
    out: &Path,
    phase: u32,
    slot: u32,
    result: &MarketPendingResult,
) -> anyhow::Result<()> {
    // The same pinned reader and settlement law used on resume must accept
    // these bytes before an append-only result can be written. Otherwise a
    // valid-looking run could become permanently unreadable at first send.
    let Some((intent, None)) = crate::market_status::read_journal_coordinate(out, phase, slot)?
    else {
        anyhow::bail!("market settlement requires an open pinned intent");
    };
    anyhow::ensure!(
        crate::market_status::verify_settled_custody((phase, slot), &intent, result),
        "market settlement does not match the frozen intent"
    );
    let bytes = serde_json::to_vec_pretty(result)?;
    ensure_market_record_within_cap(&bytes)?;
    write_market_pending(out, phase, slot, true, &bytes)
}

/// Read a journaled market intent for reconciliation. This delegates to the
/// strict pinned journal reader: validation runs on the stored bytes and the
/// canonical payload digest is re-derived, so an intent written by an older
/// build or edited on disk cannot be reconciled against a relaxed network or
/// a malformed digest. An absent coordinate is an error, not a default.
pub fn read_market_intent(
    out: &Path,
    phase: u32,
    slot: u32,
) -> anyhow::Result<MarketCustodyRecord> {
    crate::market_status::read_journal_coordinate(out, phase, slot)?
        .map(|(intent, _result)| intent)
        .ok_or_else(|| anyhow::anyhow!("market journal coordinate is unreserved"))
}

#[cfg(test)]
mod market_tests {
    use super::*;
    use crate::market::custody_payload_digest;
    use crate::market_status::journal_clear_for_action;
    use std::str::FromStr;

    /// Canonical zero-key StrKeys: checksum-valid so the item-7 newtypes
    /// accept them. Swap fixtures only if the boundary lane's `FromStr`
    /// disagrees — the journal logic under test is filesystem custody.
    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const C_POOL: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

    /// A temp dir following the existing lock-test convention.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abtest-market-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A complete, exact config fixture with one dummy artifact file per role.
    /// Writes the artifact files into `dir` and returns the TOML text.
    fn exact_config(dir: &Path) -> String {
        let mut text = String::from(
            "[network]\n\
             passphrase = \"Test SDF Network ; September 2015\"\n\
             label = \"testnet\"\n\n",
        );
        for role in MARKET_ARTIFACT_ROLES {
            let file = dir.join(format!("{role}.wasm"));
            std::fs::write(&file, b"wasm").unwrap();
            text.push_str(&format!(
                "[artifacts.{role}]\n\
                 path = \"{}\"\n\
                 sha256 = \"{}\"\n\n",
                file.display(),
                "0".repeat(64)
            ));
        }
        text.push_str(
            "[oracle]\n\
             sources = 3\n\
             min_sources = 3\n\
             max_age_secs = 3600\n\
             max_clock_drift_secs = 60\n\
             base = \"USD\"\n\
             decimals = 7\n\
             resolution = 300\n\n\
             [adapter]\n\
             decimals = 7\n\
             resolution = 1\n\
             base = \"USD\"\n\n\
             [reserves.stable]\n\
             c_factor = 9000000\n\n\
             [[balances]]\n\
             who = \"frodo\"\n\
             token = \"stable\"\n\
             amount = 100000000000\n",
        );
        text
    }

    /// Load fixture text through the real parser. No executor is constructed
    /// anywhere in this module: every rejection below happens before any
    /// executor call by construction.
    fn parse(dir: &Path, text: &str) -> anyhow::Result<MarketConfig> {
        let file = dir.join("config.toml");
        std::fs::write(&file, text).unwrap();
        MarketConfig::load(&file)
    }

    #[test]
    fn market_config_parses_exact_testnet_fixture() {
        let dir = scratch("exact");
        let cfg = parse(&dir, &exact_config(&dir)).expect("exact fixture must load");
        assert_eq!(cfg.network.passphrase, MARKET_TESTNET_PASSPHRASE);
        assert_eq!(cfg.oracle.sources, 3);
        assert_eq!(cfg.oracle.min_sources, 3);
        assert_eq!(cfg.oracle.max_age_secs, 3600);
        assert_eq!(cfg.oracle.max_clock_drift_secs, 60);
        assert_eq!(cfg.adapter.decimals, 7);
        assert_eq!(cfg.adapter.resolution, 1);
        assert_eq!(cfg.artifacts.len(), MARKET_ARTIFACT_ROLES.len());
        assert_eq!(cfg.reserves[&Tok::Stable].c_factor, Some(9_000_000));
        assert_eq!(cfg.balances[0].who, Who::Frodo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn market_config_rejects_unknown_fields() {
        let dir = scratch("unknown");
        let mut text = exact_config(&dir);
        text.push_str("typo = 1\n");
        assert!(
            parse(&dir, &text).is_err(),
            "unknown top-level key must be rejected"
        );
        let mut nested = exact_config(&dir);
        nested
            .push_str("[[balances]]\nwho = \"frodo\"\ntoken = \"xlm\"\namount = 1\nextra = true\n");
        assert!(
            parse(&dir, &nested).is_err(),
            "unknown nested key must be rejected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn market_config_rejects_missing_artifact_file() {
        let dir = scratch("missing-file");
        let text = exact_config(&dir).replace("pool.wasm", "pool-absent.wasm");
        let err = format!("{:#}", parse(&dir, &text).unwrap_err());
        assert!(err.contains("file is missing"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn market_config_rejects_wrong_digest() {
        let dir = scratch("digest");
        let text = exact_config(&dir).replace(&"0".repeat(64), "not-a-digest");
        let err = format!("{:#}", parse(&dir, &text).unwrap_err());
        assert!(
            err.contains("digest must be 64 lowercase hex characters"),
            "{err}"
        );
        let upper = exact_config(&dir).replace(&"0".repeat(64), &"A".repeat(64));
        assert!(
            parse(&dir, &upper).is_err(),
            "uppercase hex must be rejected"
        );
        // A missing or unknown role is equally fatal before any executor call.
        let short = exact_config(&dir).replace("[artifacts.adapter]", "[artifacts.adaptr]");
        assert!(
            parse(&dir, &short).is_err(),
            "missing role must be rejected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn market_config_refuses_a_wrong_network() {
        // The pair law: the passphrase and the label name one of the two
        // known networks, and they name the same one. A testnet config run
        // under a mainnet label — or the reverse — is refused before any
        // executor call, exactly as a mainnet config once was.
        let dir = scratch("net-pair");
        let text = exact_config(&dir).replace(
            "Test SDF Network ; September 2015",
            "Public Global Stellar Network ; September 2015",
        );
        let err = format!("{:#}", parse(&dir, &text).unwrap_err());
        assert!(err.contains("network label must be `mainnet`"), "{err}");
        let label = exact_config(&dir).replace("label = \"testnet\"", "label = \"public\"");
        let err = format!("{:#}", parse(&dir, &label).unwrap_err());
        assert!(err.contains("network label must be"), "{err}");
        let unknown = exact_config(&dir).replace(
            "Test SDF Network ; September 2015",
            "Future Network ; September 2026",
        );
        let err = format!("{:#}", parse(&dir, &unknown).unwrap_err());
        assert!(err.contains("exact known network passphrase"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn market_config_refuses_the_lab_stack_on_mainnet() {
        // Friendbot-funded actors and mock tokens carry no mainnet meaning:
        // a mainnet market brings real reserves and real sources instead.
        let dir = scratch("lab-mainnet");
        let text = exact_config(&dir)
            .replace(
                "Test SDF Network ; September 2015",
                "Public Global Stellar Network ; September 2015",
            )
            .replace("label = \"testnet\"", "label = \"mainnet\"");
        let err = format!("{:#}", parse(&dir, &text).unwrap_err());
        assert!(err.contains("testnet-only"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn market_config_rejects_invalid_freshness() {
        let dir = scratch("freshness");
        let stale = exact_config(&dir).replace("\nmax_age_secs = 3600\n", "\nmax_age_secs = 60\n");
        assert!(
            parse(&dir, &stale).is_err(),
            "max_age_secs other than 3600 must be rejected before any executor call"
        );
        let drift = exact_config(&dir).replace(
            "\nmax_clock_drift_secs = 60\n",
            "\nmax_clock_drift_secs = 600\n",
        );
        assert!(
            parse(&dir, &drift).is_err(),
            "max_clock_drift_secs other than 60 must be rejected"
        );
        let quorum = exact_config(&dir).replace("\nmin_sources = 3\n", "\nmin_sources = 2\n");
        assert!(
            parse(&dir, &quorum).is_err(),
            "min_sources < 3 must be rejected"
        );
        let sources = exact_config(&dir).replace("\nsources = 3\n", "\nsources = 2\n");
        assert!(
            parse(&dir, &sources).is_err(),
            "source count != 3 must be rejected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn market_config_rejects_invalid_metadata() {
        let dir = scratch("metadata");
        let bad_adapter = exact_config(&dir)
            .replace("\n[adapter]\ndecimals = 7\n", "\n[adapter]\ndecimals = 8\n");
        assert!(
            parse(&dir, &bad_adapter).is_err(),
            "adapter decimals other than 7 must be rejected"
        );
        let zero_resolution =
            exact_config(&dir).replace("\nresolution = 1\nbase", "\nresolution = 0\nbase");
        assert!(
            parse(&dir, &zero_resolution).is_err(),
            "zero adapter resolution must be rejected"
        );
        let bad_base = exact_config(&dir).replace(
            "\nresolution = 1\nbase = \"USD\"\n",
            "\nresolution = 1\nbase = \"EUR\"\n",
        );
        assert!(
            parse(&dir, &bad_base).is_err(),
            "adapter base other than USD must be rejected"
        );
        let bad_source_base = exact_config(&dir).replace(
            "\nbase = \"USD\"\ndecimals = 7\n",
            "\nbase = \"EUR\"\ndecimals = 7\n",
        );
        assert!(
            parse(&dir, &bad_source_base).is_err(),
            "source base other than USD must be rejected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn custody(phase: u32) -> MarketCustodyRecord {
        let mut record = MarketCustodyRecord {
            phase,
            kind: MarketMutationKind::Deploy,
            action: Safe::literal("deploy-pool"),
            network: MarketNetwork::Testnet,
            signer: AccountId::from_str(G_ADMIN).unwrap(),
            payload_digest: Sha256Hex::default(),
            target: Some(ContractId::from_str(C_POOL).unwrap()),
            artifact_digest: Some("1".repeat(64).try_into().unwrap()),
            predicted: Some(ContractId::from_str(C_POOL).unwrap()),
            pre_state: vec![Safe::literal("pool status = 6")],
            tool_error: None,
        };
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        record
    }

    fn test_safe(text: String) -> Safe {
        serde_json::from_value(serde_json::Value::String(text)).expect("test Safe text")
    }

    #[test]
    fn market_journal_reserves_intent_once_and_settles() {
        let dir = scratch("journal");
        let record = custody(9);
        begin_market_pending(&dir, 9, 0, &record).expect("first intent reserves");
        assert!(
            begin_market_pending(&dir, 9, 0, &record).is_err(),
            "a replayed attempt must fail before it can reach the chain"
        );
        assert!(
            !journal_clear_for_action(&dir),
            "an unsettled reservation must block admission"
        );
        assert!(market_pending_dir(&dir, 9, 0).join("intent.json").exists());
        assert!(!market_pending_dir(&dir, 9, 0).join("result.json").exists());
        let unbound_result = MarketPendingResult {
            custody: record.clone(),
            step: MarketStepRecord {
                phase: 9,
                action: record.action.clone(),
                command: Safe::literal("stellar contract deploy"),
                outcome: Safe::literal("ok"),
                target: None,
                violations: vec![],
            },
        };
        assert!(
            finish_market_pending(&dir, 9, 0, &unbound_result).is_err(),
            "writer must refuse a result the pinned reader could not settle"
        );
        assert!(
            !market_pending_dir(&dir, 9, 0).join("result.json").exists()
                && !journal_clear_for_action(&dir),
            "the refused result cannot close or alter the open intent"
        );
        finish_market_pending(
            &dir,
            9,
            0,
            &MarketPendingResult {
                custody: record.clone(),
                step: MarketStepRecord {
                    phase: 9,
                    action: Safe::literal("deploy-pool"),
                    command: Safe::literal("stellar contract deploy"),
                    outcome: Safe::literal("ok"),
                    target: record.target.clone(),
                    violations: vec![],
                },
            },
        )
        .expect("definite outcome settles");
        assert!(
            journal_clear_for_action(&dir),
            "definite settlement must clear admission"
        );
        // Custody is append-only: settlement retains the intent and the
        // settled result, and the occupied coordinate refuses reuse.
        let coordinate = market_pending_dir(&dir, 9, 0);
        let intent_path = coordinate.join("intent.json");
        let result_path = coordinate.join("result.json");
        let intent_before =
            std::fs::read(&intent_path).expect("settled custody retains the intent");
        let result_before =
            std::fs::read(&result_path).expect("settlement retains the settled result");
        let err = begin_market_pending(&dir, 9, 0, &record)
            .expect_err("a settled coordinate must refuse a new intent");
        assert_eq!(
            err.downcast_ref::<std::io::Error>().map(|e| e.kind()),
            Some(std::io::ErrorKind::AlreadyExists),
            "reuse of a settled coordinate is refused as an occupied slot"
        );
        assert_eq!(
            std::fs::read(&intent_path).expect("intent retained after refusal"),
            intent_before,
            "a refused reuse cannot alter the settled intent bytes"
        );
        assert_eq!(
            std::fs::read(&result_path).expect("result retained after refusal"),
            result_before,
            "a refused reuse cannot alter the settled result bytes"
        );
        assert!(
            journal_clear_for_action(&dir),
            "retained settlement evidence must keep admission clear"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The journal is addressed only by numbers: nothing a record or an
    /// on-disk name carries can move a write outside `market-pending/`.
    #[test]
    fn market_journal_anomalies_block_admission_and_are_never_removed() {
        let dir = scratch("stray");
        let root = dir.join("market-pending");
        // A malformed step name and a loose file are both custody anomalies:
        // they block play and are retained, never skipped or removed.
        std::fs::create_dir_all(root.join("00a-01")).unwrap();
        std::fs::write(root.join("loose-file"), b"x").unwrap();
        assert!(
            !journal_clear_for_action(&dir),
            "custody anomalies must never be skipped as clear"
        );
        assert!(
            root.join("00a-01").is_dir() && root.join("loose-file").is_file(),
            "a blocking anomaly stays where the operator can inspect it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Malformed persisted custody cannot exist as a value at all: the strict
    /// field types refuse it while deserializing, so no code path can skip a
    /// shape check. The wire shape is unchanged — a plain passphrase string and
    /// lowercase-hex digest strings.
    #[test]
    fn malformed_custody_fails_at_deserialization() {
        let record = custody(2);
        let base = serde_json::to_value(&record).expect("fixture custody serializes");
        assert_eq!(
            base["network"],
            serde_json::json!(MARKET_TESTNET_PASSPHRASE)
        );
        assert_eq!(
            base["artifact_digest"],
            serde_json::json!("1".repeat(64)),
            "digests must still serialize as plain strings"
        );
        // A network the tool cannot represent is refused while parsing. The
        // two known networks are named by their exact passphrase, so a label
        // is not a network either.
        for (field, value) in [
            ("network", serde_json::json!("Future Net ; January 2030")),
            ("network", serde_json::json!("testnet")),
            ("network", serde_json::json!("mainnet")),
            ("network", serde_json::json!(null)),
            ("payload_digest", serde_json::json!("short")),
            ("payload_digest", serde_json::json!("0".repeat(63))),
            ("payload_digest", serde_json::json!("A".repeat(64))),
            ("artifact_digest", serde_json::json!("zz")),
        ] {
            let mut tampered = base.clone();
            tampered[field] = value;
            assert!(
                serde_json::from_value::<MarketCustodyRecord>(tampered).is_err(),
                "{field} must be refused while deserializing, not afterwards"
            );
        }
        let bytes = serde_json::to_vec(&record).expect("custody serializes");
        let again: MarketCustodyRecord =
            serde_json::from_slice(&bytes).expect("a valid custody record parses back");
        assert_eq!(
            serde_json::to_vec(&again).expect("custody re-serializes"),
            bytes,
            "the custody wire format must stay byte-identical"
        );
    }

    /// Artifact roles, pinned digests, and minted amounts are refused while
    /// the config is parsed: an unknown role, a non-hex pin, or a
    /// non-positive amount can never become a `MarketConfig` value.
    #[test]
    fn market_config_refuses_malformed_values_at_parse() {
        let dir = scratch("typed-parse");
        let template = exact_config(&dir);
        let hex = format!("sha256 = \"{}\"", "0".repeat(64));
        let refuse = |note: &str, text: String| {
            assert!(
                parse(&dir, &text).is_err(),
                "{note} must be refused while parsing"
            );
        };
        refuse(
            "a non-hex artifact pin",
            template.replace(&hex, "sha256 = \"TBD\""),
        );
        refuse(
            "an uppercase artifact pin",
            template.replace(&hex, &format!("sha256 = \"{}\"", "A".repeat(64))),
        );
        refuse(
            "an unknown artifact role",
            template.replace("[artifacts.adapter]", "[artifacts.adaptr]"),
        );
        refuse(
            "a zero initial balance",
            template.replace("amount = 100000000000", "amount = 0"),
        );
        refuse(
            "a negative initial balance",
            template.replace("amount = 100000000000", "amount = -1"),
        );
        refuse(
            "a fractional initial balance",
            template.replace("amount = 100000000000", "amount = 1.5"),
        );
        // The accepted shape still serializes as plain TOML table names,
        // strings, and integers, so the frozen config digest is unchanged.
        let cfg = parse(&dir, &template).expect("exact config parses");
        let canonical = toml::to_string_pretty(&cfg).expect("config serializes");
        let bare = canonical.replace('"', "");
        for role in MARKET_ARTIFACT_ROLES {
            assert!(
                bare.contains(&format!("[artifacts.{role}]")),
                "{role} must serialize under its TOML table name: {canonical}"
            );
        }
        assert!(
            bare.contains(&format!("sha256 = {}", "0".repeat(64))),
            "an artifact digest must still serialize as a plain hex string: {canonical}"
        );
        assert!(
            bare.contains("amount = 100000000000"),
            "an initial balance must still serialize as a plain integer: {canonical}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The path-form table text `exact_config` writes for one role.
    fn path_table(dir: &Path, role: &str) -> String {
        format!(
            "[artifacts.{role}]\npath = \"{}\"\nsha256 = \"{}\"\n",
            dir.join(format!("{role}.wasm")).display(),
            "0".repeat(64)
        )
    }

    /// A release-form pin is `{release, version?}` only, may only name its
    /// role's catalog artifact, and needs no file on disk. Every other shape
    /// — mixed forms, a stray `version`, a wrong key, a release form on a
    /// path-only role — is refused while parsing.
    #[test]
    fn market_config_accepts_release_form_pins_only_for_proxy_roles() {
        let dir = scratch("release-pin");
        let template = exact_config(&dir);
        let runtime = path_table(&dir, "proxy-runtime");
        let with =
            |pin: &str| template.replace(&runtime, &format!("[artifacts.proxy-runtime]\n{pin}\n"));
        let load = |pin: &str| parse(&dir, &with(pin));

        let cfg = load("release = \"soroban-proxy-oracle\"").expect("release form parses");
        match &cfg.artifacts[&ArtifactRole::ProxyRuntime] {
            ArtifactPin::Release(r) => {
                assert_eq!(r.release, "soroban-proxy-oracle");
                assert_eq!(r.version, None);
                assert!(r.resolved.is_none(), "load never resolves");
            }
            ArtifactPin::File(_) | ArtifactPin::Live(_) => panic!("must parse as a release pin"),
        }
        let cfg = load("release = \"soroban-proxy-oracle\"\nversion = \"0.1.2\"")
            .expect("pinned version parses");
        match &cfg.artifacts[&ArtifactRole::ProxyRuntime] {
            ArtifactPin::Release(r) => assert_eq!(
                r.version.as_ref().map(|v| v.to_string()).as_deref(),
                Some("0.1.2")
            ),
            ArtifactPin::File(_) | ArtifactPin::Live(_) => panic!("must parse as a release pin"),
        }

        let refuse = |note: &str, pin: &str| {
            assert!(load(pin).is_err(), "{note} must be refused while parsing");
        };
        refuse(
            "a `latest` in config",
            "release = \"soroban-proxy-oracle\"\nversion = \"latest\"",
        );
        refuse(
            "a non-semver version",
            "release = \"soroban-proxy-oracle\"\nversion = \"0.1\"",
        );
        refuse(
            "the other role's key",
            "release = \"soroban-proxy-governance\"",
        );
        refuse("an unknown key", "release = \"soroban-batcher\"");
        let hex = format!("sha256 = \"{}\"", "0".repeat(64));
        refuse(
            "a mixed form",
            &format!("release = \"soroban-proxy-oracle\"\n{hex}"),
        );
        refuse(
            "a path with a release",
            "release = \"soroban-proxy-oracle\"\npath = \"/x/w.wasm\"",
        );
        refuse("path without a digest", "path = \"/x/w.wasm\"");
        refuse(
            "a digest without a path",
            &format!("sha256 = \"{}\"", "0".repeat(64)),
        );
        refuse("a version without a release", "version = \"0.1.2\"");
        // A release form on a path-only role is refused too.
        let pool = path_table(&dir, "pool");
        assert!(parse(
            &dir,
            &template.replace(
                &pool,
                "[artifacts.pool]\nrelease = \"soroban-proxy-oracle\"\n"
            )
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A market intent may not be journaled under coordinates that disagree
    /// with the record it carries, nor with a digest unbound to its payload.
    #[test]
    fn market_journal_rejects_coordinate_mismatch_and_unbound_digest() {
        let dir = scratch("mismatch");
        assert!(begin_market_pending(&dir, 5, 0, &custody(7)).is_err());
        let unbound = MarketCustodyRecord {
            payload_digest: Sha256Hex::default(),
            ..custody(7)
        };
        assert!(begin_market_pending(&dir, 7, 0, &unbound).is_err());
        assert!(
            !dir.join("market-pending").exists(),
            "a rejected intent leaves no reservation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reconciliation reads the stored bytes, so an intent edited on disk —
    /// here onto a non-testnet network — cannot be reconciled as valid.
    #[test]
    fn market_journal_refuses_tampered_intent_on_read_back() {
        let dir = scratch("tamper");
        let record = custody(6);
        begin_market_pending(&dir, 6, 1, &record).expect("intent reserves");
        assert_eq!(
            read_market_intent(&dir, 6, 1).unwrap().phase,
            6,
            "an untouched intent reads back valid"
        );
        let path = dir
            .join("market-pending")
            .join("006-01")
            .join("intent.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replace(
            "Test SDF Network ; September 2015",
            "Public Global Stellar Network ; September 2015",
        );
        assert_ne!(tampered, text, "fixture must actually carry the passphrase");
        std::fs::write(&path, &tampered).unwrap();
        assert!(
            read_market_intent(&dir, 6, 1).is_err(),
            "an edited intent must be refused before reconciliation"
        );
        assert!(
            read_market_intent(&dir, 6, 2).is_err(),
            "reading an absent intent is an error, not a default"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn refused_text(error: &anyhow::Error) -> String {
        format!("{error:#}")
    }

    fn assert_no_leak(error: &anyhow::Error, sentinel: &[u8], outside: &Path) {
        let text = refused_text(error);
        assert!(
            !text.contains(&outside.display().to_string())
                && !text.contains(std::str::from_utf8(sentinel).unwrap()),
            "refusal leaked a path or target: {text}"
        );
    }

    fn hostile_settlement(record: &MarketCustodyRecord) -> MarketPendingResult {
        MarketPendingResult {
            custody: record.clone(),
            step: MarketStepRecord {
                phase: record.phase,
                action: record.action.clone(),
                command: Safe::literal("test settlement"),
                outcome: Safe::literal("settled"),
                target: record.target.clone(),
                violations: Vec::new(),
            },
        }
    }

    /// A planted root symlink must fail before the writer can reserve durable
    /// intent through it. The separate victim proves any followed link would
    /// otherwise be writable.
    #[test]
    fn market_journal_refuses_symlinked_root_before_writing() {
        let dir = scratch("symlink-root-writer");
        let outside = scratch("outside-root-writer");
        let victim = outside.join("market-pending");
        std::fs::create_dir_all(victim.join("901-00")).unwrap();
        let sentinel = victim.join("do-not-touch");
        std::fs::write(&sentinel, b"outside-root-victim").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("market-pending")).unwrap();
        let error = begin_market_pending(&dir, 901, 0, &custody(901)).unwrap_err();
        assert_no_leak(&error, b"outside-root-victim", &outside);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside-root-victim");
        assert!(!victim.join("901-00").join("intent.json").exists());
        assert!(!victim.join("901-00").join("result.json").exists());
        std::fs::remove_file(dir.join("market-pending")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Both writer entry points must refuse a planted coordinate symlink, even
    /// when the custody record and journal root are otherwise valid.
    #[test]
    fn market_journal_refuses_symlinked_coordinate_before_writing() {
        let dir = scratch("symlink-coordinate-writer");
        let outside = scratch("outside-coordinate-writer");
        let victim = outside.join("902-00");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::create_dir_all(dir.join("market-pending")).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("market-pending").join("902-00")).unwrap();
        let record = custody(902);
        let begin_error = begin_market_pending(&dir, 902, 0, &record).unwrap_err();
        assert_no_leak(&begin_error, b"outside-coordinate-victim", &outside);
        let finish_error =
            finish_market_pending(&dir, 902, 0, &hostile_settlement(&record)).unwrap_err();
        assert_no_leak(&finish_error, b"outside-coordinate-victim", &outside);
        assert_eq!(
            std::fs::read_dir(&victim)
                .map(|entries| entries.count())
                .unwrap_or(usize::MAX),
            0,
            "neither writer may create an intent or result in the victim"
        );
        assert!(
            !journal_clear_for_action(&dir),
            "the planted coordinate symlink stays a blocking custody anomaly"
        );
        assert!(!dir
            .join("market-pending")
            .join("902-00")
            .join("intent.json")
            .exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// An intent that the pinned reader could never return must be
    /// refused before any journal directory or file is created.
    #[test]
    fn market_journal_refuses_oversized_intent_before_any_write() {
        let dir = scratch("oversized-intent");
        let mut record = custody(12);
        record.pre_state.push(test_safe("9".repeat(64_953)));
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        let error = begin_market_pending(&dir, 12, 0, &record).unwrap_err();
        assert_eq!(error.to_string(), MARKET_JOURNAL_RECORD_OVERSIZE);
        assert!(!dir.join("market-pending").exists());
        assert!(!journal_clear_for_action(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A realistic escaped/Unicode pre-state that the pinned reader could
    /// never return must be refused whole, input-free, before any write.
    #[test]
    fn market_journal_refuses_escaped_oversized_pre_state_before_any_write() {
        let dir = scratch("oversized-escaped");
        let mut record = custody(13);
        record.pre_state.push(test_safe(format!(
            "pool-status={}\u{1f600}{}\u{1f600}",
            "\\|".repeat(20_311),
            "`".repeat(4_000)
        )));
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        let error = begin_market_pending(&dir, 13, 0, &record).unwrap_err();
        assert_eq!(error.to_string(), MARKET_JOURNAL_RECORD_OVERSIZE);
        assert!(!dir.join("market-pending").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Intent records at the exact shared cap are reserved and remain
    /// reconcilable; the settlement for such an intent is legitimately
    /// larger, so the writer must refuse it and the attempt must stay
    /// unresolved custody (never fabricated, never unreadable).
    #[test]
    fn market_journal_intent_at_cap_stays_readable_and_oversized_result_refuses() {
        let dir = scratch("exact-cap");
        let mut record = custody(14);
        record.pre_state.push(test_safe("0".repeat(64_952)));
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        assert_eq!(
            serde_json::to_vec_pretty(&record).unwrap().len(),
            crate::market_status::PENDING_RECORD_CAP,
            "fixture must sit exactly at the shared cap"
        );
        begin_market_pending(&dir, 14, 0, &record).expect("exact-cap intent reserves");
        assert_eq!(
            read_market_intent(&dir, 14, 0).unwrap().phase,
            14,
            "a record at the cap stays reconcilable"
        );
        let oversized = hostile_settlement(&record);
        assert!(
            serde_json::to_vec_pretty(&oversized).unwrap().len()
                > crate::market_status::PENDING_RECORD_CAP
        );
        let error = finish_market_pending(&dir, 14, 0, &oversized).unwrap_err();
        assert_no_leak(&error, b"0000000", &dir);
        assert!(!dir
            .join("market-pending")
            .join("014-00")
            .join("result.json")
            .exists());
        assert!(
            !journal_clear_for_action(&dir),
            "an unsettled attempt stays blocking custody"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legitimate record whose settlement sits exactly at the shared
    /// cap is reserved, settled, and clears the action gate.
    #[test]
    fn market_journal_accepts_and_settles_record_exactly_at_cap() {
        let dir = scratch("exact-cap-result");
        let mut record = custody(16);
        record.pre_state.push(test_safe("0".repeat(64_687)));
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        let settlement = hostile_settlement(&record);
        assert_eq!(
            serde_json::to_vec_pretty(&settlement).unwrap().len(),
            crate::market_status::PENDING_RECORD_CAP,
            "settlement fixture must sit exactly at the shared cap"
        );
        begin_market_pending(&dir, 16, 0, &record).expect("limit-bound intent reserves");
        finish_market_pending(&dir, 16, 0, &settlement).expect("exact-cap settlement is written");
        assert!(journal_clear_for_action(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Result bytes are outcome-dependent: an oversized settlement is
    /// refused before create_new, and the open intent stays unresolved
    /// custody rather than becoming fabricated or unreadable evidence.
    #[test]
    fn market_journal_refuses_oversized_result_before_any_write() {
        let dir = scratch("oversized-result");
        let record = custody(15);
        begin_market_pending(&dir, 15, 0, &record).expect("open intent reserves");
        let mut stranding = custody(15);
        stranding.pre_state.push(test_safe("0".repeat(64_700)));
        stranding.payload_digest = custody_payload_digest(&stranding).expect("fixture digest");
        let oversized = hostile_settlement(&stranding);
        assert!(
            serde_json::to_vec_pretty(&oversized).unwrap().len()
                > crate::market_status::PENDING_RECORD_CAP
        );
        let error = finish_market_pending(&dir, 15, 0, &oversized).unwrap_err();
        assert_no_leak(&error, b"0000000", &dir);
        assert!(!dir
            .join("market-pending")
            .join("015-00")
            .join("result.json")
            .exists());
        assert!(
            !journal_clear_for_action(&dir),
            "an unsettled attempt stays blocking custody"
        );
        assert_eq!(read_market_intent(&dir, 15, 0).unwrap().phase, 15);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The result is a fixed filename inside a pinned coordinate. A planted
    /// symlink there must not become the settlement target, even though the
    /// record otherwise satisfies custody validation.
    #[test]
    fn market_journal_refuses_symlinked_result_before_writing() {
        let dir = scratch("symlink-result-writer");
        let outside = scratch("outside-result-writer");
        let victim = outside.join("settled.json");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(&victim, b"outside-result-victim").unwrap();
        let result = dir
            .join("market-pending")
            .join("903-00")
            .join("result.json");
        std::fs::create_dir_all(result.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, &result).unwrap();
        let record = custody(903);
        let begin_error = begin_market_pending(&dir, 903, 0, &record).unwrap_err();
        assert_no_leak(&begin_error, b"outside-result-victim", &outside);
        let finish_error =
            finish_market_pending(&dir, 903, 0, &hostile_settlement(&record)).unwrap_err();
        assert_no_leak(&finish_error, b"outside-result-victim", &outside);
        assert_eq!(std::fs::read(&victim).unwrap(), b"outside-result-victim");
        assert!(!dir
            .join("market-pending")
            .join("903-00")
            .join("intent.json")
            .exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Removing all hostile links must restore the ordinary durability path;
    /// refusal cannot become a way to evade journal custody.
    #[test]
    fn market_journal_settles_after_refusing_hostile_links() {
        let dir = scratch("legitimate-after-links");
        let record = custody(904);
        begin_market_pending(&dir, 904, 0, &record).expect("legitimate intent reserves");
        finish_market_pending(&dir, 904, 0, &hostile_settlement(&record))
            .expect("legitimate result settles");
        assert!(dir
            .join("market-pending")
            .join("904-00")
            .join("result.json")
            .exists());
        assert!(
            journal_clear_for_action(&dir),
            "legitimate settlement after hostile refusals must clear admission"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- real market (mainnet launch) configuration ----------

    const MAINNET_USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    const MAINNET_XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const TESTNET_USDC: &str = "CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA";
    const TESTNET_XLM: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
    const VERIFIER: &str = "CAYFT5JE3UQTKT4Q6ZOZK4FXVYVT6RE3MFC7STA4UB6WAEGBT65MRU52";
    const EXTERNAL_ORACLE: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

    /// A complete real-market config: the reviewed xlm-usdc pool parameters,
    /// each reserve's own two Lazer instances and one existing SEP-40 contract
    /// as its sources, and the activation shim's two synthetic legs. Written
    /// for either network.
    fn real_config(dir: &Path, mainnet: bool) -> String {
        let (passphrase, label, usdc, xlm) = if mainnet {
            (
                "Public Global Stellar Network ; September 2015",
                "mainnet",
                MAINNET_USDC,
                MAINNET_XLM,
            )
        } else {
            (
                "Test SDF Network ; September 2015",
                "testnet",
                TESTNET_USDC,
                TESTNET_XLM,
            )
        };
        let mut text = format!(
            "[network]\n\
             passphrase = \"{passphrase}\"\n\
             label = \"{label}\"\n\n"
        );
        // The two shared production contracts are pinned like any other role:
        // a config may pin them by file or by a hash the network already has.
        for role in MARKET_REAL_ARTIFACT_ROLES
            .into_iter()
            .chain(["comet", "emitter", MARKET_LAZER_SOURCE_ROLE])
        {
            let file = dir.join(format!("{role}.wasm"));
            std::fs::write(&file, b"wasm").unwrap();
            text.push_str(&format!(
                "[artifacts.{role}]\n\
                 path = \"{}\"\n\
                 sha256 = \"{}\"\n\n",
                file.display(),
                "0".repeat(64)
            ));
        }
        text.push_str(&format!(
            "[oracle]\n\
             sources = 3\n\
             min_sources = 2\n\
             max_age_secs = 300\n\
             max_clock_drift_secs = 0\n\
             base = \"USD\"\n\
             decimals = 7\n\
             resolution = 300\n\
             base_asset = \"{usdc}\"\n\n\
             [adapter]\n\
             decimals = 7\n\
             resolution = 1\n\
             base = \"USD\"\n\n\
             [shim]\n\
             blnd_code = \"SHIMBLND\"\n\
             usdc_code = \"SHIMUSDC\"\n\n\
             [pool]\n\
             name = \"Precept - XLM/USDC\"\n\
             admin = \"{G_ADMIN}\"\n\
             backstop_take_rate = 0\n\
             max_positions = 4\n\
             min_collateral = \"10000000000\"\n\
             target_status = 0\n\
             lazer_verifier = \"{VERIFIER}\"\n\n"
        ));
        for (index, (asset, lbl, c, l, r1, min_pos, code)) in [
            (usdc, "usdc", 0u32, 10_000_000u32, 799_000u32, "10000000", "USDC"),
            (xlm, "xlm", 8_000_000, 0, 0, "500000000", "XLM"),
        ]
        .into_iter()
        .enumerate()
        {
            let (feed_a, feed_b) = if index == 0 { ("100", "101") } else { ("200", "201") };
            text.push_str(&format!(
                "[[pool.reserves]]\n\
                 label = \"{lbl}\"\n\
                 asset = \"{asset}\"\n\
                 decimals = 7\n\
                 c_factor = {c}\n\
                 l_factor = {l}\n\
                 util = {}\n\
                 max_util = {}\n\
                 supply_cap = \"170141183460469231731687303715884105727\"\n\
                 min_position = \"{min_pos}\"\n\
                 r_base = 1000\n\
                 r_one = {r1}\n\
                 r_two = {r1}\n\
                 r_three = {r1}\n\
                 reactivity = 0\n\
                 enabled = true\n\
                 min_sources = 2\n\
                 max_age_secs = 300\n\n\
                 [[pool.reserves.sources]]\n\
                 kind = \"lazer\"\n\
                 feed_id = \"{feed_a}\"\n\n\
                 [[pool.reserves.sources]]\n\
                 kind = \"lazer\"\n\
                 feed_id = \"{feed_b}\"\n\n\
                 [[pool.reserves.sources]]\n\
                 kind = \"sep40\"\n\
                 contract = \"{EXTERNAL_ORACLE}\"\n\
                 asset = {{ Other = \"{code}\" }}\n",
                if index == 0 { 9_000_000 } else { 8_000_000 },
                if index == 0 { 9_900_000 } else { 9_000_000 },
            ));
        }
        text
    }

    #[test]
    fn real_market_config_freezes_its_network_and_its_parameters() {
        let dir = scratch("real-exact");
        let cfg = parse(&dir, &real_config(&dir, true)).expect("real mainnet fixture must load");
        assert_eq!(cfg.network().unwrap(), MarketNetwork::Mainnet);
        assert!(cfg.is_real());
        let pool = cfg.pool.as_ref().expect("the pool is the config");
        assert_eq!(pool.reserves.len(), 2);
        assert_eq!(pool.reserves[1].label, "xlm");
        assert_eq!(pool.max_positions, 4);
        assert_eq!(pool.target_status, 0);
        // A market with no synthetic source pins no mock token Wasm.
        let roles = cfg.required_roles();
        assert!(roles.contains(&MARKET_LAZER_SOURCE_ROLE));
        assert!(!roles.contains(&"token") && !roles.contains(&"source"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_market_config_requires_the_inputs_it_cannot_know() {
        // An unknown input stays a required field: an empty or placeholder
        // value fails with a message naming it, never a default that passes.
        let dir = scratch("real-required");
        // A pool with a Lazer source and no verifier of its own: the run cannot
        // guess one.
        let no_verifier = real_config(&dir, true)
            .replace(&format!("lazer_verifier = \"{VERIFIER}\"\n\n"), "\n");
        let err = format!("{:#}", parse(&dir, &no_verifier).unwrap_err());
        assert!(err.contains("binds no verifier"), "{err}");
        let bad_verifier = real_config(&dir, true)
            .replace(&format!("lazer_verifier = \"{VERIFIER}\""), "lazer_verifier = \"\"");
        let err = format!("{:#}", parse(&dir, &bad_verifier).unwrap_err());
        assert!(err.contains("lazer_verifier must be a contract id"), "{err}");

        let placeholder_address = real_config(&dir, true)
            .replace(&format!("contract = \"{EXTERNAL_ORACLE}\""), "contract = \"REQUIRED\"");
        let err = format!("{:#}", parse(&dir, &placeholder_address).unwrap_err());
        assert!(err.contains("address must be a contract id"), "{err}");

        let no_base = real_config(&dir, true)
            .replace(&format!("base_asset = \"{MAINNET_USDC}\"\n"), "");
        let err = format!("{:#}", parse(&dir, &no_base).unwrap_err());
        assert!(err.contains("base_asset"), "{err}");

        let bad_asset = real_config(&dir, true).replace(
            &format!("asset = \"{MAINNET_XLM}\""),
            "asset = \"XLM\"",
        );
        let err = format!("{:#}", parse(&dir, &bad_asset).unwrap_err());
        assert!(err.contains("needs the asset's contract id"), "{err}");

        let no_admin = real_config(&dir, true).replace(
            &format!("admin = \"{G_ADMIN}\""),
            "admin = \"\"",
        );
        let err = format!("{:#}", parse(&dir, &no_admin).unwrap_err());
        assert!(err.contains("pool admin must be a G-address"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_market_config_bounds_quorum_and_freshness() {
        // Bounds, not equalities: two of three is a lawful quorum, and a
        // freshness window the operator chose is lawful within the bounds.
        let dir = scratch("real-quorum");
        let two_of_three = parse(&dir, &real_config(&dir, true)).expect("2-of-3 is lawful");
        assert_eq!(two_of_three.oracle.min_sources, 2);
        let above = real_config(&dir, true).replace("min_sources = 2", "min_sources = 4");
        assert!(
            parse(&dir, &above).is_err(),
            "min_sources above the declared sources must be rejected"
        );
        let zero = real_config(&dir, true).replace("min_sources = 2", "min_sources = 0");
        assert!(
            parse(&dir, &zero).is_err(),
            "a quorum of nothing is no quorum"
        );
        let stale = real_config(&dir, true).replace("max_age_secs = 300", "max_age_secs = 999999");
        assert!(
            parse(&dir, &stale).is_err(),
            "a freshness window past the bound must be rejected"
        );
        let bad_feed = real_config(&dir, true).replace("feed_id = \"100\"", "feed_id = \"x\"");
        let err = format!("{:#}", parse(&dir, &bad_feed).unwrap_err());
        assert!(err.contains("Lazer feed id in base 10"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_market_config_refuses_a_mock_source_on_mainnet_and_requires_its_shim() {
        let dir = scratch("real-mock");
        let mock = real_config(&dir, true).replace("kind = \"sep40\"", "kind = \"mock\"");
        let err = format!("{:#}", parse(&dir, &mock).unwrap_err());
        assert!(err.contains("synthetic mock source"), "{err}");
        // The same source list is lawful on testnet, where a rehearsal runs.
        let rehearsal = real_config(&dir, false).replace("kind = \"sep40\"", "kind = \"mock\"");
        parse(&dir, &rehearsal).expect("a testnet rehearsal may quote a mock source");
        let no_shim = real_config(&dir, true).replace(
            "[shim]\nblnd_code = \"SHIMBLND\"\nusdc_code = \"SHIMUSDC\"\n\n",
            "",
        );
        let err = format!("{:#}", parse(&dir, &no_shim).unwrap_err());
        assert!(err.contains("activation shim"), "{err}");
        let same_code = real_config(&dir, true).replace("usdc_code = \"SHIMUSDC\"", "usdc_code = \"SHIMBLND\"");
        assert!(
            parse(&dir, &same_code).is_err(),
            "the shim's two legs are two distinct assets"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_market_pin_may_name_a_hash_that_is_already_live() {
        // Comet and the emitter are the network's own contracts: an operator
        // pins their hash and the run proves them fetchable instead of
        // uploading bytes it does not have.
        let dir = scratch("live-pin");
        let text = real_config(&dir, true).replace(
            &format!(
                "[artifacts.comet]\npath = \"{}\"\nsha256 = \"{}\"\n\n",
                dir.join("comet.wasm").display(),
                "0".repeat(64)
            ),
            "[artifacts.comet]\nwasm_hash = \"8abc28913035c07411ed5d134e6bfeab4723d97ddd4d1a22a0605d35c94d1a36\"\n\n",
        );
        let cfg = parse(&dir, &text).expect("a live pin needs no local bytes");
        let pin = cfg.artifacts.get("comet").expect("comet is pinned");
        assert!(pin.file_location().is_err(), "a live pin has no bytes to read");
        assert_eq!(
            pin.wasm_hash().unwrap().as_str(),
            "8abc28913035c07411ed5d134e6bfeab4723d97ddd4d1a22a0605d35c94d1a36"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_market_config_borrows_the_registrys_reviewed_pool_config() {
        let dir = scratch("pool-config");
        let reviewed = serde_json::json!({
            "name": "xlm-usdc",
            "network": "mainnet",
            "proposed_by": "Templar",
            "pool": {
                "admin": G_ADMIN,
                "name": "Precept - XLM/USDC",
                "oracle": null,
                "backstop_take_rate": 0,
                "max_positions": 4,
                "min_collateral": "10000000000"
            },
            "reserves": [{
                "asset": MAINNET_USDC,
                "label": "usdc",
                "config": {
                    "index": 0, "decimals": 7, "c_factor": 0, "l_factor": 10_000_000,
                    "util": 9_000_000, "max_util": 9_900_000, "r_base": 1000,
                    "r_one": 799_000, "r_two": 1_200_000, "r_three": 1_200_000,
                    "reactivity": 0,
                    "supply_cap": "170141183460469231731687303715884105727",
                    "min_position": "10000000", "enabled": true
                }
            }],
            "oracle": {
                "lazer_verifier": VERIFIER,
                "assets": {"usdc": {
                    "min_sources": 2, "max_age_secs": 300,
                    "sources": [
                        {"kind": "lazer", "feed_id": 100}, {"kind": "lazer", "feed_id": 101}
                    ]
                }}
            },
            "target_status": 0,
            "notes": []
        });
        let file = dir.join("xlm-usdc.json");
        std::fs::write(&file, serde_json::to_string(&reviewed).unwrap()).unwrap();
        let without_pool = real_config(&dir, true)
            .split("\n[pool]\n")
            .next()
            .expect("the pool table is the last section")
            .to_string();
        // A top-level key has to precede every table, so the reference to the
        // reviewed config opens the document.
        let text = format!(
            "pool_config = \"{}\"\n{}",
            file.display(),
            without_pool
        );
        let cfg = parse(&dir, &text).expect("the reviewed config is the authority");
        let pool = cfg.pool.expect("translated from the registry config");
        assert_eq!(pool.name, "Precept - XLM/USDC");
        assert_eq!(pool.reserves.len(), 1);
        assert_eq!(pool.reserves[0].min_position, "10000000");
        // A borrowed config for another network is refused, not adapted.
        let other = reviewed
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>();
        let mut wrong = serde_json::Value::Object(other);
        wrong["network"] = serde_json::Value::String("testnet".into());
        std::fs::write(&file, serde_json::to_string(&wrong).unwrap()).unwrap();
        let err = format!("{:#}", parse(&dir, &text).unwrap_err());
        assert!(err.contains("is for network 'testnet'"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- the one-command path: a launch pool from its pool config ----------

    const MAINNET_DEJAAA: &str = "CC64WBDGS6QQP22QTTIACYIXT3WF7BBQEYOQPLTP7GTKYY7PZ74QYGSL";
    const MAINNET_SAVUSD: &str = "CBW5332J2RUTSOIUESSNBNLESPEIEUG6G4LOQOAM6IP3ANORCMN5CPQB";
    const MAINNET_EARNUSDC: &str = "CCL3WITWFFXIHV2I52ECV5DPIEOFSTU3PBPR53ILPLF2IP5KHECXRUTY";

    /// One entry of a pool config's `reserves` array, as
    /// `precept/registry/configs/<pool>.json` writes it.
    fn registry_reserve(label: &str, asset: &str, index: u32, decimals: u32) -> serde_json::Value {
        serde_json::json!({
            "asset": asset,
            "label": label,
            "config": {
                "index": index, "decimals": decimals,
                "c_factor": 0, "l_factor": 10_000_000,
                "util": 9_000_000, "max_util": 9_900_000,
                "r_base": 1000, "r_one": 799_000, "r_two": 1_200_000, "r_three": 1_200_000,
                "reactivity": 0,
                "supply_cap": "170141183460469231731687303715884105727",
                "min_position": "10000000", "enabled": true
            }
        })
    }

    /// A launch pool in the shape of the reviewed
    /// `precept/registry/configs/<pool>.json`: USDC at index 0, the pool's own
    /// asset after it, and the `oracle` block a launch reads. The asset and its
    /// decimals are the real proposal's; the sources are a test quorum, not a
    /// live feed set.
    fn launch_pool(label: &str, asset: &str, decimals: u32, min_sources: u32) -> serde_json::Value {
        let pair = |base: u32| -> Vec<serde_json::Value> {
            (0..2)
                .map(|i| serde_json::json!({"kind": "lazer", "feed_id": base + i}))
                .collect()
        };
        // Each asset declares its own quorum and window: the sources behind
        // different assets are pushed on different cadences.
        let block = |feeds: u32| {
            serde_json::json!({
                "min_sources": min_sources,
                "max_age_secs": 300,
                "sources": pair(feeds)
            })
        };
        let mut assets = serde_json::Map::new();
        assets.insert("usdc".to_string(), block(100));
        assets.insert(label.to_string(), block(200));
        serde_json::json!({
            "name": format!("{label}-usdc"),
            "network": "mainnet",
            "pool": {
                "admin": G_ADMIN,
                "name": format!("Precept - {}/USDC", label.to_uppercase()),
                "oracle": null,
                "backstop_take_rate": 0,
                "max_positions": 4,
                "min_collateral": "10000000000"
            },
            "reserves": [
                registry_reserve("usdc", MAINNET_USDC, 0, 7),
                registry_reserve(label, asset, 1, decimals)
            ],
            "target_status": 0,
            "oracle": {
                "lazer_verifier": VERIFIER,
                "assets": serde_json::Value::Object(assets)
            },
            "notes": []
        })
    }

    /// The one-command path, through a real file on disk.
    fn from_pool_file(
        dir: &Path,
        spec: &serde_json::Value,
        declared: Option<MarketNetwork>,
    ) -> anyhow::Result<MarketConfig> {
        let file = dir.join("pool.json");
        std::fs::write(&file, spec.to_string()).unwrap();
        MarketConfig::from_pool_config(&file, declared)
    }

    /// The four pools the launch plan names, as their reviewed configs will be
    /// written: the assets and the decimals each token really reports.
    #[test]
    fn from_pool_config_builds_every_reviewed_launch_pool() {
        let dir = scratch("pool-all");
        for (label, asset, decimals) in [
            ("xlm", MAINNET_XLM, 7u32),
            ("dejaaa", MAINNET_DEJAAA, 18),
            ("savusd", MAINNET_SAVUSD, 7),
            ("earnusdc", MAINNET_EARNUSDC, 13),
        ] {
            let cfg = from_pool_file(&dir, &launch_pool(label, asset, decimals, 2), None)
                .unwrap_or_else(|e| panic!("{label}-usdc must load from its pool config: {e:#}"));
            assert_eq!(cfg.network().unwrap(), MarketNetwork::Mainnet, "{label}-usdc");
            let pool = cfg.pool.as_ref().expect("the pool is the config");
            assert_eq!(pool.reserves[0].label, "usdc", "{label}-usdc quotes USDC");
            assert_eq!(pool.reserves[1].label, label);
            assert_eq!(pool.reserves[1].asset, asset);
            assert_eq!(
                pool.reserves[1].decimals, decimals,
                "{label} keeps its own decimals, not the quote's"
            );
            assert_eq!(cfg.oracle.base_asset.as_deref().unwrap(), MAINNET_USDC);
            assert_eq!(pool.runtime, None, "the first of a group names no runtime");
            // Mainnet uploads only the code it deploys: the mock token and the
            // set_data publisher roles do not exist for a real market.
            let roles = cfg.required_roles();
            assert!(!roles.contains(&"token") && !roles.contains(&"source"), "{roles:?}");
            assert!(roles.contains(&MARKET_LAZER_SOURCE_ROLE), "a lazer source needs its wasm");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_pool_config_refuses_a_network_it_was_not_reviewed_for() {
        let dir = scratch("pool-network");
        let spec = launch_pool("xlm", MAINNET_XLM, 7, 2);
        let err = format!(
            "{:#}",
            from_pool_file(&dir, &spec, Some(MarketNetwork::Testnet)).unwrap_err()
        );
        assert!(err.contains("was reviewed for mainnet but this command names testnet"), "{err}");
        let mut testnet = spec.clone();
        testnet["network"] = serde_json::Value::String("testnet".into());
        let err = format!(
            "{:#}",
            from_pool_file(&dir, &testnet, Some(MarketNetwork::Mainnet)).unwrap_err()
        );
        assert!(err.contains("was reviewed for testnet but this command names mainnet"), "{err}");
        // Declaring no network is not a free pass: the config must name one of
        // the two the tool can represent.
        let mut other = spec.clone();
        other["network"] = serde_json::Value::String("futurenet".into());
        let err = format!("{:#}", from_pool_file(&dir, &other, None).unwrap_err());
        assert!(err.contains("unknown network label"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_pool_config_refuses_a_reserve_whose_index_is_not_its_position() {
        let dir = scratch("pool-index");
        // The proposal's own indices, swapped: the pool would freeze an index
        // no one reviewed, so the run stops rather than renumbering.
        let mut spec = launch_pool("xlm", MAINNET_XLM, 7, 2);
        spec["reserves"][0]["config"]["index"] = serde_json::json!(1);
        spec["reserves"][1]["config"]["index"] = serde_json::json!(0);
        let err = format!("{:#}", from_pool_file(&dir, &spec, None).unwrap_err());
        assert!(err.contains("declares index 1 but sits at position 0"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_pool_config_parses_i128_amounts_and_refuses_ill_formed_ones() {
        let dir = scratch("pool-i128");
        let spec = launch_pool("xlm", MAINNET_XLM, 7, 2);
        // i128::MAX is what the reviewed configs write for an uncapped reserve.
        let cfg = from_pool_file(&dir, &spec, None).expect("i128::MAX is a lawful cap");
        assert_eq!(
            cfg.pool.expect("pool").reserves[0].supply_cap,
            "170141183460469231731687303715884105727"
        );
        let mut grouped = spec.clone();
        grouped["reserves"][0]["config"]["supply_cap"] = serde_json::json!("1_000");
        let err = format!("{:#}", from_pool_file(&dir, &grouped, None).unwrap_err());
        assert!(err.contains("supply_cap must be a positive integer in base 10"), "{err}");
        let mut negative = spec.clone();
        negative["pool"]["min_collateral"] = serde_json::json!("-1");
        let err = format!("{:#}", from_pool_file(&dir, &negative, None).unwrap_err());
        assert!(err.contains("min_collateral must not be negative"), "{err}");
        let mut none = spec.clone();
        none["pool"]["min_collateral"] = serde_json::json!("0");
        let cfg = from_pool_file(&dir, &none, None).expect("0 is a pool with no minimum");
        assert_eq!(cfg.pool.expect("pool").min_collateral, "0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_pool_config_validates_the_quorum_of_each_reserve() {
        let dir = scratch("pool-quorum");
        // USDC keeps its two sources; the other reserve cannot form its own
        // quorum, and the error names that reserve.
        let mut thin = launch_pool("xlm", MAINNET_XLM, 7, 2);
        thin["oracle"]["assets"]["xlm"]["sources"] =
            serde_json::json!([{"kind": "lazer", "feed_id": 200}]);
        let err = format!("{:#}", from_pool_file(&dir, &thin, None).unwrap_err());
        assert!(err.contains("reserve 'xlm' must declare at least its quorum of 2"), "{err}");
        // A quorum above what that one asset declares is refused even though
        // the other asset declares enough.
        let mut high = launch_pool("xlm", MAINNET_XLM, 7, 2);
        high["oracle"]["assets"]["xlm"]["min_sources"] = serde_json::json!(3);
        let err = format!("{:#}", from_pool_file(&dir, &high, None).unwrap_err());
        assert!(err.contains("reserve 'xlm' must declare at least its quorum of 3"), "{err}");
        // And a quorum of nothing is no quorum.
        let mut none = launch_pool("xlm", MAINNET_XLM, 7, 2);
        none["oracle"]["assets"]["xlm"]["min_sources"] = serde_json::json!(0);
        let err = format!("{:#}", from_pool_file(&dir, &none, None).unwrap_err());
        assert!(err.contains("reserve 'xlm' sets a quorum of 0"), "{err}");
        // An asset block for a label that is not a reserve is a mistake, not a
        // hint.
        let mut stranger = launch_pool("xlm", MAINNET_XLM, 7, 2);
        stranger["oracle"]["assets"]["wbtc"] = serde_json::json!({
            "min_sources": 1, "max_age_secs": 300,
            "sources": [{"kind": "lazer", "feed_id": 300}]
        });
        let err = format!("{:#}", from_pool_file(&dir, &stranger, None).unwrap_err());
        assert!(err.contains("which is not a reserve of this pool"), "{err}");
        // And the per-asset blocks are never invented by the tool.
        let mut no_oracle = launch_pool("xlm", MAINNET_XLM, 7, 2);
        no_oracle.as_object_mut().expect("object").remove("oracle");
        let err = format!("{:#}", from_pool_file(&dir, &no_oracle, None).unwrap_err());
        assert!(err.contains("declares no 'oracle.assets' entry for reserve 'usdc'"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_pool_config_refuses_a_mock_source_on_mainnet() {
        let dir = scratch("pool-mock");
        let mut spec = launch_pool("xlm", MAINNET_XLM, 7, 2);
        spec["oracle"]["assets"]["xlm"]["sources"] =
            serde_json::json!([{"kind": "mock"}, {"kind": "mock"}]);
        let err = format!("{:#}", from_pool_file(&dir, &spec, None).unwrap_err());
        assert!(err.contains("synthetic mock source"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_pool_config_reuses_a_shared_runtime_without_pinning_proxy_code() {
        let dir = scratch("pool-runtime");
        // The second pool of a group names the runtime its group already
        // deployed: it then uploads and deploys no proxy code at all.
        let mut shared = launch_pool("dejaaa", MAINNET_DEJAAA, 18, 2);
        shared["oracle"]["runtime"] = serde_json::json!(EXTERNAL_ORACLE);
        let cfg = from_pool_file(&dir, &shared, None).expect("a grouped pool reuses its runtime");
        let pool = cfg.pool.as_ref().expect("the pool is the config");
        assert_eq!(pool.runtime.as_deref(), Some(EXTERNAL_ORACLE));
        assert_eq!(pool.adapter, None, "the pool still deploys its own adapter");
        let roles = cfg.required_roles();
        assert!(
            !roles.contains(&"proxy-runtime") && !roles.contains(&"proxy-governance"),
            "{roles:?}"
        );
        assert!(
            roles.contains(&"pool")
                && roles.contains(&"backstop")
                && roles.contains(&"factory")
                && roles.contains(&"adapter"),
            "the pool's own code is still uploaded: {roles:?}"
        );
        assert!(!cfg.artifacts.contains_key(&ArtifactRole::ProxyRuntime));
        assert!(!cfg.artifacts.contains_key(&ArtifactRole::ProxyGovernance));
        // The first pool of a group names no runtime and pins the proxy stack
        // by release: its version is an operator input, not a constant.
        let first = from_pool_file(&dir, &launch_pool("xlm", MAINNET_XLM, 7, 2), None)
            .expect("the first of a group deploys its own runtime");
        let roles = first.required_roles();
        assert!(
            roles.contains(&"proxy-runtime") && roles.contains(&"proxy-governance"),
            "{roles:?}"
        );
        assert!(matches!(
            first.artifacts[&ArtifactRole::ProxyRuntime],
            ArtifactPin::Release(_)
        ));
        // Comet and the emitter are the network's own contracts: pinned by
        // hash, never uploaded.
        assert!(matches!(
            first.artifacts[&ArtifactRole::Comet],
            ArtifactPin::Live(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A first mainnet deploy finds none of the launch code live. Each such
    /// digest pin becomes an upload of the profile's file from --wasm-dir,
    /// verified against the pin; code the network holds stays a live pin.
    #[test]
    fn missing_launch_code_is_uploaded_from_the_wasm_dir_under_its_digest() {
        let dir = scratch("pool-materialize");
        let mut cfg = from_pool_file(&dir, &launch_pool("xlm", MAINNET_XLM, 7, 2), None)
            .expect("a launch pool");
        let pool_entry = launch_wasm_file("pool").expect("the profile names the pool");
        let comet_hash = launch_wasm_file("comet").unwrap().wasm_hash;
        // Two pins stand for the two cases: the Comet, which the network
        // holds, and the pool, which a first deploy must upload. The pool's
        // pin is forged to this test's bytes; on a run it is the release digest.
        let bytes = b"the reviewed pool code";
        let digest = Sha256Hex::try_from(crate::market::sha256_hex(bytes)).unwrap();
        cfg.artifacts
            .retain(|role, _| matches!(role, ArtifactRole::Pool | ArtifactRole::Comet));
        cfg.artifacts.insert(
            ArtifactRole::Pool,
            ArtifactPin::Live(LivePin { wasm_hash: digest.clone() }),
        );
        let live = |_: &str, h: &Sha256Hex| h.as_str() == comet_hash;

        // No directory: refused, naming the flag and the file it wants.
        let err = cfg
            .clone()
            .materialize_missing_code(None, live)
            .expect_err("no bytes anywhere")
            .to_string();
        assert!(err.contains("--wasm-dir") && err.contains(pool_entry.asset), "{err}");

        // Wrong bytes under the right name: refused before anything is pinned.
        let wasm = dir.join("wasm");
        std::fs::create_dir_all(&wasm).unwrap();
        std::fs::write(wasm.join(pool_entry.asset), b"not the pool").unwrap();
        let err = cfg
            .clone()
            .materialize_missing_code(Some(&wasm), live)
            .expect_err("drifted bytes")
            .to_string();
        assert!(err.contains("not the reviewed 'pool' code"), "{err}");

        // The reviewed bytes: a file pin, so the upload coordinate submits
        // exactly them; the Comet stays live and uploads nothing.
        std::fs::write(wasm.join(pool_entry.asset), bytes).unwrap();
        let uploads = cfg
            .materialize_missing_code(Some(&wasm), live)
            .expect("the pool's bytes are present and match");
        assert_eq!(uploads, vec![ArtifactRole::Pool]);
        assert!(matches!(
            &cfg.artifacts[&ArtifactRole::Pool],
            ArtifactPin::File(FilePin { path, sha256 })
                if path == &wasm.join(pool_entry.asset) && *sha256 == digest
        ));
        assert!(matches!(cfg.artifacts[&ArtifactRole::Comet], ArtifactPin::Live(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A later pool finds the proxy stack's release digests already on the
    /// network (the first pool uploaded them): those release pins upload
    /// nothing and take no slot. One the network lacks is left for the
    /// resolver's bytes, exactly as before.
    #[test]
    fn a_release_pin_the_network_already_holds_uploads_nothing() {
        let dir = scratch("pool-release-live");
        let mut cfg = from_pool_file(&dir, &launch_pool("xlm", MAINNET_XLM, 7, 2), None)
            .expect("a launch pool");
        cfg.artifacts.retain(|role, _| {
            matches!(role, ArtifactRole::ProxyRuntime | ArtifactRole::ProxyGovernance)
        });
        let resolved = |digest: &str| crate::release::Provenance {
            artifact: String::new(),
            version: String::new(),
            tag: String::new(),
            asset: String::new(),
            sha256: Sha256Hex::try_from(digest.repeat(64)).unwrap(),
            length: 0,
            catalog_commit: String::new(),
        };
        for (role, digest) in [
            (ArtifactRole::ProxyRuntime, "a"),
            (ArtifactRole::ProxyGovernance, "b"),
        ] {
            let ArtifactPin::Release(release) = cfg.artifacts.get_mut(&role).unwrap() else {
                panic!("the proxy stack is release-pinned");
            };
            release.resolved = Some(resolved(digest));
            release.path = Some(dir.join(format!("{digest}.wasm")));
        }
        // The runtime is live; the governance is not.
        let uploads = cfg
            .materialize_missing_code(None, |_, h| h.as_str().starts_with('a'))
            .expect("release pins never need the wasm dir");
        assert!(uploads.is_empty(), "release pins are not wasm-dir uploads: {uploads:?}");
        assert!(
            matches!(cfg.artifacts[&ArtifactRole::ProxyRuntime], ArtifactPin::Live(_)),
            "a live release digest becomes a live pin"
        );
        assert!(
            matches!(cfg.artifacts[&ArtifactRole::ProxyGovernance], ArtifactPin::Release(_)),
            "an absent release digest stays a release pin for its bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RedStone pushes on a 12-hour heartbeat while Lazer is pushed every
    /// minute, so each asset carries its own quorum and freshness.
    #[test]
    fn from_pool_config_carries_each_assets_quorum_and_freshness() {
        let dir = scratch("pool-per-asset");
        let mut spec = launch_pool("xlm", MAINNET_XLM, 7, 2);
        // The RedStone shape: one existing wrapper, a quorum of one, and a
        // window that matches its heartbeat. The tool accepts it and lets the
        // runtime say whether it allows a quorum of one.
        spec["oracle"]["assets"]["xlm"] = serde_json::json!({
            "min_sources": 1,
            "max_age_secs": 43_200,
            "sources": [{"kind": "sep40", "contract": EXTERNAL_ORACLE,
                         "asset": {"Stellar": MAINNET_XLM}}]
        });
        let cfg = from_pool_file(&dir, &spec, None)
            .expect("a 1-of-1 asset on a 12-hour heartbeat is lawful");
        let pool = cfg.pool.expect("the pool is the config");
        assert_eq!(pool.reserves[1].min_sources, 1);
        assert_eq!(pool.reserves[1].max_age_secs, 43_200);
        assert_eq!(
            pool.reserves[1].max_clock_drift_secs, 0,
            "an absent drift is zero, not a guess"
        );
        assert_eq!(pool.reserves[0].min_sources, 2, "the fast asset keeps its own quorum");
        assert_eq!(pool.reserves[0].max_age_secs, 300);
        // The stack-level record reports the widest values the run configures.
        assert_eq!(cfg.oracle.min_sources, 2);
        assert_eq!(cfg.oracle.max_age_secs, 43_200);
        // A window past a week is refused, naming the asset it belongs to.
        let mut too_old = spec.clone();
        too_old["oracle"]["assets"]["xlm"]["max_age_secs"] = serde_json::json!(604_801);
        let err = format!("{:#}", from_pool_file(&dir, &too_old, None).unwrap_err());
        assert!(err.contains("reserve 'xlm' sets max_age_secs 604801"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn activation_shim_seed_is_the_runbook_minimum() {
        // The runbook's numbers: the smallest whole-token amount u of the
        // 20%-weight leg with (4u)^4 * u >= 1e25, plus one token of margin.
        let (blnd, usdc) = market_shim_seed();
        assert_eq!(usdc, 32_989 * 10_000_000);
        assert_eq!(blnd, 131_953 * 10_000_000);
        // The seeded legs carry one token of margin over the minimum, so the
        // amount one token below the minimum — two below the seed — is what
        // must fail the fourth-power test.
        let one_token = 10_000_000;
        let just_below = shim_backstop_value((usdc - 2 * one_token) / one_token);
        assert!(
            just_below < i128::try_from(MARKET_SHIM_MIN_BACKSTOP_VALUE).unwrap(),
            "the seed must be the smallest value that clears the threshold"
        );
    }
}
