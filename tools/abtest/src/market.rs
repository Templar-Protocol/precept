//! Single-market play for the disposable protected testnet market (plan §6).
//!
//! [`MarketRunner`] is the bounded market-mode executor. It refuses every
//! mutating palette step fail-closed — no market capture facility exists, so
//! zero executor calls are made for them — and implements exactly two
//! journaled mutating journeys against the frozen contracts: price
//! publication (per-asset `set_data` on every source, then refresh-all
//! requiring `Accepted` with readback) and governance manual trip/untrip
//! (create/execute proposal with breaker readback). Every mutation is
//! journaled before and after the call via the market pending custody
//! directory, and every surfaced text value is rendered through the market
//! `Safe` policy. No pair state, pair evidence, or A/B verdict is touched.
//!
//! Invariants enforced here (all rechecked immediately before every call,
//! never only at construction):
//! - Exact testnet passphrase recheck against [`MARKET_TESTNET_PASSPHRASE`]
//!   (plan §Invariants; `manifest.rs` — a manifest cannot relax it).
//! - Signer binding: the admin alias is re-resolved through
//!   `Stellar::keys_public_key` on the external admin config directory and
//!   compared with the frozen [`MarketManifest::admin`] G-address before any
//!   admin-signed call — re-checked before each of the per-asset
//!   publications, not once per journey.
//! - Pinned source (SEC003): every market mutation builder carries the
//!   frozen pinned G as the transaction source together with the CLI26
//!   `--sign-with-key <alias>` flag; the alias is re-resolved immediately
//!   before each mutation and a mismatch refuses before send. A rebound
//!   alias can never sign an unpinned key.
//! - Actor binding (SEC005): actor-signed play (refresh) is pinned to the
//!   frozen manifest user identity and recorded under that same G — never
//!   the operator's — with the alias re-resolved before every mutation.
//! - Mutation gate: no market capture facility exists, so every mutating
//!   `Step` variant is refused with zero executor calls (input-free reason);
//!   the only mutating paths are the publication and governance journeys
//!   implemented here, which carry their own readback evidence. Read-only
//!   classification steps (`Wait*`, `AssertRatesIncreased`) route through
//!   [`crate::run::invoke_side`], which sends nothing for them.
//! - Freshness and acceptance are read from the contracts only — never from
//!   a hand-rolled clock compare: the adapter's `lastprice(asset)` is `Some`
//!   only when the proxy price is accepted AND fresh, `get_cached(asset)`
//!   must carry an `Accepted` status (a `Some` cache alone can hold a
//!   `ResolveFailed` failure), and `get_breaker_set_view(asset)` must be
//!   readable and neither tripped nor blocking. Publication timestamps are
//!   wall-clock `SystemTime` seconds; the proxy's own freshness filters are
//!   the enforcement truth.
//! - Price publication is 12 individually journaled per-asset mutations
//!   (registered assets × three sources, `Who::Admin`-signed, binding
//!   recheck before each), then refresh-all of every registered asset
//!   requiring `Accepted` plus per-asset `get_cached` and `lastprice`
//!   readback; any partial publication leaves unresolved custody and blocks
//!   later value movement until reconciliation.
//! - Governance-only actions (manual trip, untrip) go exclusively through
//!   the governance contract's `create_proposal(caller, id,
//!   {"SetManualTrip":[asset, tripped, null]}, ttl)` / `execute_proposal`
//!   path with the proposal id read back from `next_proposal_id` and the
//!   result verified through `get_breaker_set_view` — never a direct owner
//!   call, never all-reserves, never id-range inference.
//!
//! Upstream seams are referenced by their canonical paths only:
//! [`crate::deploy::MarketExecutionContext`] and [`crate::deploy::MarketJournal`]
//! (deploy lane, landing), and [`crate::run::invoke_side`] with `SideView` and
//! `SideHooks` (evidence lane, landed). The market runner supplies market
//! journaling and sanitized presentation through those hooks.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::deploy::{MarketExecutionContext, MarketJournal};
use crate::manifest::{
    read_market_intent, MarketCustodyRecord, MarketManifest, MarketMutationKind,
    MarketNetwork, MarketPendingResult, MarketStepRecord, Sha256Hex, SideManifest, Tok, Who,
    MARKET_TESTNET_PASSPHRASE,
};
use crate::market_status::journal_clear_for_action;
use crate::run::{invoke_side, SideHooks, SideOutcome, SideView};
use crate::scenario::Step;
use crate::stellar::{
    definite_outcome, AccountId, Capture, ContractId, ErrorKind, Net, Policy, RealExecutor, Safe,
    Stellar, MARKET_CAPTURE_STDERR_CAP, MARKET_CAPTURE_STDOUT_CAP, MARKET_MUTATION_WALL,
};
use crate::tui::palette;

/// Play-phase number for market operator evidence (`MarketStepRecord.phase`).
const PLAY_PHASE: u32 = 1;
/// Two-digit market journal coordinates have slots 00..99; play uses 01..99.
const JOURNAL_SLOT_SCAN_CEILING: u32 = 99;

/// Input-free refusals for the publication journey. Every precondition that
/// fails before the first publish reports one of these two, so the journal
/// never carries an operator-supplied or CLI-supplied string.
const REFUSE_PRECONDITION: &str =
    "price publication blocked: a precondition failed before any mutation";
const REFUSE_CAPACITY: &str = "price publication blocked: the market journal has too few free play coordinates for this journey; preserve evidence and inspect market status";

/// Proxy runtime, adapter, and governance entrypoints used by market play.
/// These match the frozen contracts (`contracts/contract/proxy-oracle/
/// soroban/`); nothing here calls an owner-gated runtime entry directly —
/// breaker movement is governance-proposal only.
const CONTRACT_REFRESH: &str = "refresh";
const CONTRACT_GET_CACHED: &str = "get_cached";
const CONTRACT_GET_BREAKER_VIEW: &str = "get_breaker_set_view";
const CONTRACT_REGISTERED_ASSETS: &str = "registered_assets";
const CONTRACT_LASTPRICE: &str = "lastprice";
/// Synthetic source price publisher: per-asset single price
/// `set_data(asset, price, timestamp)` — one journaled mutation per
/// (asset, source) pair, never a bundled array.
const CONTRACT_SET_PRICE: &str = "set_data";
/// One operator request to the market runner. `Run(usize)` is a palette step
/// index: pair `specs()` indices followed by the fixed market-only actions in
/// numeric order (0 refresh-all, 1 manual trip, 2 manual untrip-and-refresh,
/// 3 extend TTL), refused fail-closed beyond those indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketAct {
    Continue,
    Run(usize),
    Halt,
    Snapshot,
}

/// One sanitized market event. Every field is a policy-rendered [`Safe`];
/// raw CLI bytes, aliases, config paths, and credentials never appear.
#[derive(Clone, Debug)]
pub enum MarketEvent {
    Phase { label: Safe },
    Cli { argv: Safe },
    Step { label: Safe, ok: bool },
    Halt { reason: Safe },
    Failed(Safe),
}

/// Market play executor. Owned per run directory; constructed from a loaded
/// [`MarketManifest`], the non-serializable [`MarketExecutionContext`], and
/// the [`MarketJournal`] custody handle. Mutating palette steps are refused
/// fail-closed (no capture facility); the two journaled mutating journeys —
/// price publication and governance trips — are implemented here against the
/// market manifest contracts only.
pub struct MarketRunner {
    manifest: MarketManifest,
    ctx: MarketExecutionContext,
    journal: MarketJournal,
    halted: AtomicBool,
    /// Run-local actor net: market RPC + frozen identities in the run-local
    /// config dir. Admin-signed calls never use it.
    actor_net: Net,
    /// External operator admin net (admin config dir). Never serialized.
    admin_net: Net,
    /// The shared CLI executor pinned to the operator-supplied binary and
    /// XDG paths (`RealExecutor` clears ambient environment).
    executor: RealExecutor,
    /// The frozen CLI alias for actor-signed play (`abtest-user`), derived
    /// from code and never from a record. Paired with the manifest user G it
    /// forms the pinned signer on every actor-signed builder.
    user_alias: String,
    /// The frozen run-local user G persisted in the market manifest. Every
    /// actor-signed intent is attributed to this address — never to the
    /// operator and never to a freshly resolved alias.
    user_account: AccountId,
    /// In-memory single-side execution view over the market manifest
    /// (plan §6). Never serialized: the market manifest is the durable
    /// record and never re-enters pair shapes.
    market_side: SideManifest,
    market_ids: BTreeMap<Who, String>,
}

impl MarketRunner {
    /// Verifies the manifest and context, refuses play unless the strict
    /// journal integrity gate reports the custody journal clear (parsed,
    /// matching results only), requires the frozen run-local user identity
    /// the play journeys attribute their signatures to, and builds the
    /// market execution state. No chain call happens until [`Self::act`]
    /// is invoked.
    pub fn new(
        manifest: MarketManifest,
        ctx: MarketExecutionContext,
        journal: MarketJournal,
    ) -> Result<Self> {
        ctx.assert_testnet()?;
        if manifest.network.passphrase.to_string() != MARKET_TESTNET_PASSPHRASE {
            return Err(anyhow!(
                "market manifest network is not the exact testnet passphrase"
            ));
        }
        // Strict journal integrity (SEC002): clear only when every journal
        // entry parses, its settled result matches its intent, and the
        // bounded scanner still has room for the next intent. An open,
        // malformed, forged, or over-capacity journal blocks play before
        // any executor exists. Mere existence of a result proves nothing.
        if !journal_clear_for_action(&journal.out()) {
            return Err(anyhow!(
                "market journal is not strictly clear: unresolved or invalid custody or scan capacity blocks play; preserve evidence and inspect market status"
            ));
        }
        let mut executor = RealExecutor {
            bin: ctx.stellar_bin().to_path_buf(),
            xdg: ctx.xdg().to_path_buf(),
            cwd: ctx.cwd().to_path_buf(),
            capture: Capture::Market {
                stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
                stderr_cap: MARKET_CAPTURE_STDERR_CAP,
                wall: Some(MARKET_MUTATION_WALL),
            },
            rpc_env: Vec::new(),
        };
        let actor_net = Net {
            rpc_url: ctx.rpc_url().to_string(),
            passphrase: MARKET_TESTNET_PASSPHRASE.to_string(),
            config_dir: ctx.actor_config_dir().to_path_buf(),
            headers: ctx.rpc_headers().to_vec(),
        };
        let admin_net = Net {
            rpc_url: ctx.rpc_url().to_string(),
            passphrase: MARKET_TESTNET_PASSPHRASE.to_string(),
            config_dir: ctx.admin_config_dir().to_path_buf(),
            headers: ctx.rpc_headers().to_vec(),
        };
        // Both bindings share one endpoint and its headers: the transport
        // reaches the CLI through the child environment, never argv.
        executor.rpc_env = crate::stellar::rpc_env_for(&actor_net);
        // SEC005: every actor-signed play journey (refresh is the only one)
        // is signed and attributed by this frozen manifest identity. Without
        // it there is nothing lawful to pin or attribute, so the runner is
        // never constructed — no path where an actor call silently executes
        // under, or is recorded as, the operator's address. This refusal
        // precedes any executor construction, so no chain call is possible.
        let user_account = manifest
            .identities
            .get(&Who::User)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "market manifest has no frozen user identity to attribute actor-signed play"
                )
            })?;
        Stellar::new(&actor_net, &executor, Policy::Market).assert_rpc_testnet()?;
        let mut runner = Self {
            manifest,
            ctx,
            journal,
            halted: AtomicBool::new(false),
            actor_net,
            admin_net,
            executor,
            market_side: SideManifest {
                label: String::new(),
                deploy_ledger: 0,
                wasm: BTreeMap::new(),
                contracts: Default::default(),
                reserves: BTreeMap::new(),
                minted: BTreeMap::new(),
            },
            market_ids: BTreeMap::new(),
            user_alias: Who::User.key_name(),
            user_account,
        };
        runner.load_market_side();
        Ok(runner)
    }

    /// Fills the in-memory side view from the market manifest (contract ids
    /// and actor identities only). The adapter is the pool's Blend oracle;
    /// publication never routes through this view — sources are addressed
    /// directly by the per-asset publish path.
    fn load_market_side(&mut self) {
        let m = &self.manifest;
        self.market_side.deploy_ledger = m.deploy_ledger;
        for (tok, id) in &m.tokens {
            self.market_side
                .contracts
                .set_token(*tok, id.as_str().to_string());
        }
        self.market_side.contracts.lp = m.comet.as_str().to_string();
        self.market_side.contracts.emitter = m.emitter.as_str().to_string();
        self.market_side.contracts.backstop = m.backstop.as_str().to_string();
        self.market_side.contracts.pool_factory = m.factory.as_str().to_string();
        self.market_side.contracts.oracle = m.oracle.adapter.as_str().to_string();
        self.market_side.contracts.pool = m.pool.as_str().to_string();
        self.market_side.reserves = m.reserves.clone();
        for (who, account) in &m.identities {
            self.market_ids.insert(*who, account.as_str().to_string());
        }
    }

    /// Actor-signed executor builder, pinned to the frozen manifest user
    /// identity (SEC003/SEC005): every market mutation through this handle
    /// must submit with `--source <frozen user G> --sign-with-key <actor
    /// alias>`, with the alias re-resolved by the CLI seam immediately
    /// before the mutation and any mismatch refused. Reads carry no pin.
    fn actor_stellar(&self) -> Stellar<'_, RealExecutor> {
        Stellar::new(&self.actor_net, &self.executor, Policy::Market)
            .with_market_signer(&self.user_alias, &self.user_account)
    }

    /// Operator-signed executor builder, pinned to the frozen manifest
    /// admin G with the operator alias (SEC003): a rebound alias can never
    /// sign an unpinned key because the CLI seam re-resolves the alias
    /// immediately before every mutation and refuses on mismatch.
    fn admin_stellar(&self) -> Stellar<'_, RealExecutor> {
        Stellar::new(&self.admin_net, &self.executor, Policy::Market)
            .with_market_signer(self.ctx.admin_alias(), &self.manifest.admin)
    }

    /// Re-resolves the admin alias through the CLI on the external config
    /// directory and compares it with the frozen manifest G-address. Any
    /// admin-signed call must pass this immediately before the call.
    fn require_admin_binding(&self) -> Result<()> {
        let st = self.admin_stellar();
        let resolved = st
            .keys_public_key(self.ctx.admin_alias())?
            .ok_or_else(|| anyhow!("admin alias does not resolve in the admin config dir"))?;
        if resolved != self.manifest.admin {
            return Err(anyhow!(
                "admin alias does not match the frozen manifest signer"
            ));
        }
        Ok(())
    }

    /// Re-resolves the run-local actor alias through the CLI on the
    /// run-local config directory and compares it with the frozen manifest
    /// user G-address (SEC005 mirror of the admin recheck). Every
    /// actor-signed call — refresh is the only one — must pass this
    /// immediately before the call. An alias that does not resolve to the
    /// frozen identity refuses; it never falls back to the operator.
    fn require_user_binding(&self) -> Result<()> {
        let st = self.actor_stellar();
        let resolved = st
            .keys_public_key(&self.user_alias)?
            .ok_or_else(|| anyhow!("actor alias does not resolve in the actor config dir"))?;
        if resolved != self.user_account {
            return Err(anyhow!(
                "actor alias does not match the frozen manifest identity"
            ));
        }
        Ok(())
    }

    /// Refuses any journey unless strict journal integrity is clear (SEC002):
    /// every intent has a parsed matching result and there is room for one
    /// more intent under the bounded scan. Open, malformed, forged, or
    /// over-capacity journals block play. A result file alone proves nothing.
    fn require_journal_clear(&self) -> Result<()> {
        if !journal_clear_for_action(&self.journal.out()) {
            return Err(anyhow!(
                "market journal is not strictly clear: unresolved or invalid custody or scan capacity blocks this journey; preserve evidence and inspect market status"
            ));
        }
        Ok(())
    }

    /// The registered market assets in `Tok::ORACLE_ORDER`, as the tagged
    /// `{"Stellar": <token contract>}` JSON form every proxy/adapter/governance
    /// entry takes — never a bare contract-id string.
    fn registered_asset_jsons(&self) -> Vec<(Tok, String)> {
        Tok::ORACLE_ORDER
            .iter()
            .filter_map(|tok| {
                self.manifest
                    .tokens
                    .get(tok)
                    .map(|id| (*tok, json!({"Stellar": id.as_str()}).to_string()))
            })
            .collect()
    }

    /// Renders an argv echo for one market call under market policy.
    fn cli_event(&self, target: &str, function: &str) -> MarketEvent {
        MarketEvent::Cli {
            argv: Safe::render_parsed(
                Policy::Market,
                &json!({"contract": target, "function": function}),
                2048,
            ),
        }
    }

    fn actor_key(&self, who: Who) -> Result<&str> {
        self.market_ids
            .get(&who)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("run-local actor identity is not frozen in the manifest"))
    }

    /// Refuses play whenever the operator halted this runner.
    fn require_not_halted(&self) -> Result<()> {
        if self.halted.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "market play halted; a new process must restart play"
            ));
        }
        Ok(())
    }

    /// Free play-phase journal coordinates inside the bounded scan window.
    /// Anything that exists at a coordinate — an open intent, a settled
    /// result, or anything the reader cannot even classify — counts as
    /// occupied, so capacity is never booked against a slot the journal could
    /// not actually hand out.
    fn free_play_slots(&self) -> usize {
        let root = self.journal.out().join("market-pending");
        (1..=JOURNAL_SLOT_SCAN_CEILING)
            .filter(|slot| {
                matches!(
                    std::fs::symlink_metadata(root.join(format!("{PLAY_PHASE:03}-{slot:02}"))),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                )
            })
            .count()
    }

    /// The journaled mutations the publication journey needs: one per
    /// (source, registered asset) publication plus one refresh per registered
    /// asset.
    fn publish_journey_mutations(&self) -> usize {
        let assets = self.registered_asset_jsons().len();
        self.manifest.oracle.sources.len() * assets + assets
    }

    /// A multi-mutation journey must be able to journal *every* mutation it
    /// makes, so it is refused up front whenever the play phase cannot supply
    /// one free coordinate per mutation. Admitting a journey that runs out
    /// halfways would leave custody open with no coordinate left for the rest;
    /// this refusal makes no executor call and reserves nothing.
    fn require_journey_capacity(&self, mutations: usize) -> Result<()> {
        anyhow::ensure!(
            self.free_play_slots() >= mutations,
            "market journal has too few free play coordinates for this journey; preserve evidence and inspect market status before retrying"
        );
        Ok(())
    }

    /// Reserves one journaled market mutation intent before the call (plan §4
    /// custody law: intent before execution, settled result or explicit
    /// unresolved stop after). The strict journal integrity gate must clear
    /// before any intent is written (SEC002): a zero-length, truncated,
    /// forged, or otherwise malformed settled record blocks admission just
    /// as an open intent does, and journal-existence alone never clears it.
    ///
    /// The intent binds the exact ordered call payload (SEC004): the
    /// contract method and every argument — name and value, in send order,
    /// including the source asset, price, precomputed timestamp, proposal
    /// id, operation, and TTL — are canonicalized into the record's
    /// pre-state before the payload digest is computed, so two mutations
    /// that differ in anything the chain will see can never share a custody
    /// digest or be attributed to one another. `method` and `args` are
    /// required parameters: a future callsite cannot omit the payload from
    /// the binding. Digest computation failure aborts before the intent.
    fn begin_mutation(
        &self,
        kind: MarketMutationKind,
        action: Safe,
        signer: AccountId,
        target: Option<ContractId>,
        method: &str,
        args: &[(&str, String)],
    ) -> Result<u32> {
        if !journal_clear_for_action(&self.journal.out()) {
            return Err(anyhow!(
                "market journal is not strictly clear: unresolved or invalid custody or scan capacity blocks this mutation; preserve evidence and inspect market status"
            ));
        }
        Stellar::new(&self.actor_net, &self.executor, Policy::Market).assert_rpc_testnet()?;
        let pre_state = play_call_pre_state(method, args)?;
        let mut record = MarketCustodyRecord {
            phase: PLAY_PHASE,
            kind,
            action,
            network: MarketNetwork::Testnet,
            signer,
            payload_digest: Sha256Hex::default(),
            target,
            artifact_digest: None,
            predicted: None,
            pre_state,
            tool_error: None,
        };
        record.payload_digest = custody_payload_digest(&record)?;
        for slot in 1..=JOURNAL_SLOT_SCAN_CEILING {
            match self.journal.begin(PLAY_PHASE, slot, &record) {
                Ok(()) => return Ok(slot),
                // Occupied coordinate: the create-new reservation itself is
                // the guard — intent.json exists (io AlreadyExists) or a
                // settled result already occupies the slot.
                Err(e) if slot_occupied(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(anyhow!("market journal slot space exhausted"))
    }

    /// Settles one mutation: renders the outcome under market policy and
    /// persists the settled custody record plus the market evidence step.
    /// Only rendered `Safe` text is stored.
    fn settle(
        &self,
        slot: u32,
        action: Safe,
        target: Option<ContractId>,
        outcome: &InvocationOutcome,
    ) -> Result<()> {
        let custody = read_market_intent(&self.journal.out(), PLAY_PHASE, slot)?;
        let (outcome_safe, violations) = outcome.render();
        let step = MarketStepRecord {
            phase: PLAY_PHASE,
            action,
            command: custody.action.clone(),
            outcome: outcome_safe,
            target,
            violations,
        };
        self.journal
            .finish(PLAY_PHASE, slot, &MarketPendingResult { custody, step })
    }

    /// Executes one action against the market manifest, exactly once. The
    /// mutation gate is the driver: every mutating `Step` variant is refused
    /// with zero executor calls (no market capture facility exists), except
    /// the publication journey intercepted here. Read-only classification
    /// steps route through the shared primitive, which sends nothing for
    /// them.
    fn execute_market_step(&mut self, label: Safe, step: &Step) -> Result<MarketEvent> {
        self.require_not_halted()?;
        self.ctx.assert_testnet()?;
        match step {
            // Publication journey: journaled per-asset admin mutations with
            // their own refresh-and-readback evidence.
            Step::SetPrices(prices) => Ok(self.publish_and_refresh(label, prices)),
            // The journey stamps the current time, so an offset (stale or
            // future-dated) probe would silently publish fresh prices.
            Step::SetPricesAt { .. } => Ok(MarketEvent::Failed(Safe::literal(
                "offset price publication is not supported in market mode; refused before any mutation",
            ))),
            // Non-mutating steps: the shared primitive classifies them
            // without any executor call.
            Step::WaitLedgersSince { .. }
            | Step::WaitSeconds(_)
            | Step::AssertRatesIncreased { .. } => {
                let mut hooks = MarketHooks {
                    halted: &self.halted,
                };
                let st = self.actor_stellar();
                let side = SideView {
                    tag: 'm',
                    m: &self.market_side,
                    identities: &self.market_ids,
                };
                match invoke_side(&st, &side, step, &mut hooks) {
                    Ok(SideOutcome::Unmutating) | Ok(SideOutcome::Declined) => {
                        Ok(MarketEvent::Step { label, ok: true })
                    }
                    Ok(SideOutcome::Invocation(_)) => Ok(MarketEvent::Failed(Safe::literal(
                        "market step refused: a non-mutating classification attempted a mutation",
                    ))),
                    Err(_) => Ok(MarketEvent::Failed(Safe::literal(
                        "market step classification failed",
                    ))),
                }
            }
            // Fail-closed mutation gate: no capture facility, no call.
            _ => Ok(MarketEvent::Failed(Safe::literal(
                "mutating market step refused: no market capture facility exists; price publication and governance trips are the only journaled mutating paths",
            ))),
        }
    }

    /// The market publication journey: publish every registered asset's
    /// price to every source contract — one journaled per-asset mutation at
    /// a time (4 assets × 3 sources = 12, `Who::Admin`-signed with the
    /// admin-binding recheck before each) — then refresh every registered
    /// asset requiring `Accepted` plus readback. Prices are indexed by
    /// registered-asset position in `Tok::ORACLE_ORDER`, never by bundled
    /// array position. A partial publication leaves the journal open
    /// (unresolved custody) and blocks value-moving actions until a later
    /// successful refresh-and-readback reconciles.
    fn publish_and_refresh(&mut self, label: Safe, prices: &[i128]) -> MarketEvent {
        if self.require_not_halted().is_err() {
            return MarketEvent::Failed(Safe::literal(REFUSE_PRECONDITION));
        }
        // A journey that cannot journal every mutation it would make is
        // refused before its first publish, so custody can never be left open
        // mid-journey with no coordinate left for the rest.
        if self
            .require_journey_capacity(self.publish_journey_mutations())
            .is_err()
        {
            return MarketEvent::Failed(Safe::literal(REFUSE_CAPACITY));
        }
        if self
            .ctx
            .assert_testnet()
            .and_then(|()| self.require_admin_binding())
            .and_then(|()| self.require_journal_clear())
            .is_err()
        {
            return MarketEvent::Failed(Safe::literal(REFUSE_PRECONDITION));
        }
        if prices.len() != Tok::ORACLE_ORDER.len() {
            return MarketEvent::Failed(Safe::literal(
                "price publication blocked: exactly four prices in oracle order are required",
            ));
        }
        if prices.iter().any(|p| *p <= 0) {
            return MarketEvent::Failed(Safe::literal(
                "price publication blocked: every published price must be positive",
            ));
        }
        let assets: Vec<(Tok, i128, String)> = Tok::ORACLE_ORDER
            .iter()
            .zip(prices.iter().copied())
            .filter_map(|(tok, price)| {
                self.manifest
                    .tokens
                    .get(tok)
                    .map(|id| (*tok, price, json!({"Stellar": id.as_str()}).to_string()))
            })
            .collect();
        let st = self.admin_stellar();
        for source in &self.manifest.oracle.sources {
            for (_tok, price, asset_json) in &assets {
                // Admin-binding recheck immediately before each signed
                // mutation (plan §Invariants).
                if self.require_admin_binding().is_err() {
                    return MarketEvent::Failed(Safe::literal(
                        "price publication blocked: the admin binding recheck failed mid-journey",
                    ));
                }
                // Breaker health is a per-call publication precondition:
                // no publication proceeds while any registered asset
                // reports a tripped or blocking breaker.
                let actor = self.actor_stellar();
                let actor_source = match self.actor_key(Who::User) {
                    Ok(key) => key.to_string(),
                    Err(_) => {
                        return MarketEvent::Failed(Safe::literal(
                            "price publication blocked: the actor identity is not frozen in the manifest",
                        ))
                    }
                };
                if !assets.iter().all(|(_t, _p, json)| {
                    breaker_clear(&actor, &actor_source, &self.manifest.oracle.runtime, json)
                }) {
                    return MarketEvent::Failed(Safe::literal(
                        "price publication refused: a registered asset reports a tripped or blocking breaker",
                    ));
                }
                // SEC004: one timestamp is precomputed for this mutation
                // before its intent is reserved; the same bound value is
                // what gets sent, and nothing re-samples the clock after
                // the intent exists.
                let timestamp = now_epoch_secs().to_string();
                let call_args: Vec<(&str, String)> = vec![
                    ("asset", asset_json.clone()),
                    ("price", price.to_string()),
                    ("timestamp", timestamp.clone()),
                ];
                let slot =
                    match self.begin_mutation(
                        MarketMutationKind::Invoke,
                        Safe::literal("price publish"),
                        self.manifest.admin.clone(),
                        Some(source.clone()),
                        CONTRACT_SET_PRICE,
                        &call_args,
                    ) {
                        Ok(slot) => slot,
                        Err(_) => return MarketEvent::Failed(Safe::literal(
                            "price publication blocked: the custody intent could not be reserved",
                        )),
                    };
                let _cli = self.cli_event(source.as_str(), CONTRACT_SET_PRICE);
                let inv = st.invoke(
                    source.as_str(),
                    self.ctx.admin_alias(),
                    CONTRACT_SET_PRICE,
                    &call_args,
                );
                let inv = match inv {
                    Ok(inv) => inv,
                    Err(_) => {
                        // Tool failure: the intent stays open (unresolved)
                        // for public reconciliation — never a clean close.
                        return MarketEvent::Failed(Safe::literal(
                            "price publication blocked: a source publish did not complete; refresh-all and Accepted are required",
                        ));
                    }
                };
                if unresolved_submission(&inv) {
                    // Ambiguous: custody stays open; the plan §4 stop applies.
                    return MarketEvent::Halt {
                        reason: Safe::literal(
                            "price publication unresolved: public-chain reconciliation required before replay",
                        ),
                    };
                }
                let outcome = InvocationOutcome::of(&inv);
                if self
                    .settle(
                        slot,
                        Safe::literal("price publish"),
                        Some(source.clone()),
                        &outcome,
                    )
                    .is_err()
                {
                    return MarketEvent::Failed(Safe::literal(
                        "price publication blocked: the settled custody could not be recorded",
                    ));
                }
                if !inv.ok {
                    return MarketEvent::Failed(Safe::literal(
                        "price publication blocked: a source publish failed; refresh-all and Accepted are required",
                    ));
                }
            }
        }
        // All per-asset publications landed: refresh every registered asset,
        // then require Accepted plus readback.
        match self.refresh_all_inner() {
            Ok(true) => MarketEvent::Step { label, ok: true },
            Ok(false) => MarketEvent::Failed(Safe::literal(
                "price publication incomplete: refresh-all did not reach Accepted with readback for every registered asset",
            )),
            Err(_) => MarketEvent::Failed(Safe::literal(
                "refresh-all blocked: unresolved market custody or a failed precondition",
            )),
        }
    }

    /// Refresh-all with contract-read evidence only. Each registered
    /// asset's `refresh(asset)` is its own journaled mutation: reserve,
    /// call, classify, settle — in that order, with one retained custody
    /// coordinate per on-chain call (plan §4 per-mutation law). Acceptance
    /// for an asset requires a decoded `Accepted` refresh status AND a
    /// `get_cached` entry whose status is `Accepted` AND a `Some` adapter
    /// `lastprice` (the only read encoding accepted-and-fresh). A
    /// transaction success alone is not acceptance (`Blocked` and
    /// `ResolveFailed` complete normally), and a `Some` cache alone can
    /// hold a failure — both fail closed. A definite outcome (accepted or
    /// refused) retains its matching settled record; a tool failure or
    /// ambiguous outcome leaves that asset's own coordinate open
    /// (unresolved) and stops the journey.
    fn refresh_all_inner(&mut self) -> Result<bool> {
        self.require_not_halted()?;
        self.ctx.assert_testnet()?;
        self.require_journal_clear()?;
        // One journaled refresh per registered asset: a journey that cannot
        // journal them all is refused before the first send.
        self.require_journey_capacity(self.registered_asset_jsons().len())?;
        // SEC005: refresh is actor-signed. The actor alias is re-resolved
        // against the frozen manifest user G immediately before this
        // journey and again before every individual mutation; an alias
        // that does not resolve to the frozen identity refuses the send
        // and is never recorded as the operator.
        self.require_user_binding()?;
        let runtime = self.manifest.oracle.runtime.clone();
        let adapter = self.manifest.oracle.adapter.clone();
        let st = self.actor_stellar();
        let source = self.actor_key(Who::User)?.to_string();
        let mut all_accepted = true;
        for (_tok, asset_json) in self.registered_asset_jsons() {
            self.require_user_binding()?;
            let slot = self.begin_mutation(
                MarketMutationKind::Invoke,
                Safe::literal("refresh asset"),
                self.user_account.clone(),
                Some(runtime.clone()),
                CONTRACT_REFRESH,
                &[("asset", asset_json.clone())],
            )?;
            let _cli = self.cli_event(runtime.as_str(), CONTRACT_REFRESH);
            let inv = match st.invoke(
                runtime.as_str(),
                &self.user_alias,
                CONTRACT_REFRESH,
                &[("asset", asset_json.clone())],
            ) {
                Ok(inv) => inv,
                // Tool failure: this asset's own coordinate stays open
                // (unresolved) for public reconciliation; the journey
                // stops here and value movement stays blocked.
                Err(_) => return Ok(false),
            };
            if unresolved_submission(&inv) {
                // Ambiguous: this asset's own coordinate stays open for
                // public-chain reconciliation; the journey stops here.
                return Ok(false);
            }
            let accepted = inv.ok
                && status_accepted(inv.value.as_ref())
                && cached_accepted(&st, &source, &runtime, &asset_json)
                && lastprice_present(&st, &source, &adapter, &asset_json);
            let outcome = if accepted {
                InvocationOutcome::Ok
            } else {
                InvocationOutcome::Notice(Safe::literal(
                    "refresh did not reach Accepted with readback for this registered asset",
                ))
            };
            self.settle(
                slot,
                Safe::literal("refresh asset"),
                Some(runtime.clone()),
                &outcome,
            )?;
            if !accepted {
                all_accepted = false;
            }
        }
        Ok(all_accepted)
    }

    /// Read-only snapshot battery against the market (no submission): pool
    /// status, registered assets, proxy cached prices and breaker views, and
    /// adapter `lastprice` presence per registered asset. Results are
    /// rendered `Safe` and never compared to anything.
    fn snapshot(&mut self) -> MarketEvent {
        if self
            .require_not_halted()
            .and_then(|()| self.ctx.assert_testnet())
            .is_err()
        {
            return MarketEvent::Failed(Safe::literal(
                "market snapshot blocked: a precondition failed",
            ));
        }
        let st = self.actor_stellar();
        let Ok(source) = self.actor_key(Who::User).map(str::to_string) else {
            return MarketEvent::Failed(Safe::literal(
                "market snapshot blocked: the actor identity is not frozen in the manifest",
            ));
        };
        let runtime = self.manifest.oracle.runtime.as_str().to_string();
        let adapter = self.manifest.oracle.adapter.clone();
        let mut ok = true;
        let probe = st.view(
            self.manifest.pool.as_str(),
            &source,
            "get_status",
            &[(
                "user",
                self.actor_key(Who::User).unwrap_or_default().to_string(),
            )],
        );
        ok &= probe.is_ok();
        let assets = st.view(&runtime, &source, CONTRACT_REGISTERED_ASSETS, &[]);
        ok &= assets.is_ok();
        for (_tok, asset_json) in self.registered_asset_jsons() {
            ok &= st
                .view(
                    &runtime,
                    &source,
                    CONTRACT_GET_CACHED,
                    &[("asset", asset_json.clone())],
                )
                .map(|v| !v.is_null())
                .unwrap_or(false);
            ok &= st
                .view(
                    &runtime,
                    &source,
                    CONTRACT_GET_BREAKER_VIEW,
                    &[("asset", asset_json.clone())],
                )
                .map(|v| !v.is_null())
                .unwrap_or(false);
            ok &= lastprice_present(&st, &source, &adapter, &asset_json);
        }
        MarketEvent::Step {
            label: Safe::literal("market snapshot"),
            ok,
        }
    }

    /// One action, once. `Continue` acknowledges the play phase without a
    /// chain call; `Halt` stops the runner and refuses further work;
    /// `Snapshot` runs the read-only battery; `Run(index)` executes one
    /// market palette entry — pair `specs()` indices first, then the four
    /// market-only entries — with the mutation gate above applied.
    pub fn act(&mut self, act: MarketAct) -> Result<MarketEvent> {
        if let MarketAct::Halt = act {
            self.halted.store(true, Ordering::SeqCst);
            return Ok(MarketEvent::Halt {
                reason: Safe::literal("market play halted by operator"),
            });
        }
        self.require_not_halted()?;
        match act {
            MarketAct::Continue => Ok(MarketEvent::Phase {
                label: Safe::literal("market play ready"),
            }),
            MarketAct::Halt => Ok(MarketEvent::Halt {
                reason: Safe::literal("market play halted by operator"),
            }),
            MarketAct::Snapshot => Ok(self.snapshot()),
            MarketAct::Run(index) => self.run_palette(index),
        }
    }

    /// Dispatches one palette index. Pair indices dispatch through the pair
    /// table with the market-visibility gate; market-only indices dispatch
    /// numerically, and the numeric match refuses every unmatched index
    /// fail-closed.
    fn run_palette(&mut self, index: usize) -> Result<MarketEvent> {
        let pair_len = palette::specs().len();
        if index < pair_len {
            let spec = &palette::specs()[index];
            if palette::market_hidden(spec.name) {
                return Ok(MarketEvent::Failed(Safe::literal(
                    "pair-only probe is hidden in market mode",
                )));
            }
            if !spec.args.is_empty() {
                return Ok(MarketEvent::Failed(Safe::literal(
                    "this action requires operator arguments; drive it from the interactive market palette",
                )));
            }
            let step = (spec.build)(&[]);
            return self.execute_market_step(Safe::literal(spec.name), &step);
        }
        match index - pair_len {
            0 => match self.refresh_all_inner() {
                Ok(true) => Ok(MarketEvent::Step {
                    label: Safe::literal("refresh all"),
                    ok: true,
                }),
                Ok(false) => Ok(MarketEvent::Step {
                    label: Safe::literal("refresh all"),
                    ok: false,
                }),
                Err(_) => Ok(MarketEvent::Failed(Safe::literal(
                    "refresh-all blocked: unresolved market custody or a failed precondition",
                ))),
            },
            1 => Ok(MarketEvent::Failed(Safe::literal(
                "manual trip requires a reserve target and trip state; drive it from the interactive market palette",
            ))),
            2 => Ok(MarketEvent::Failed(Safe::literal(
                "manual untrip and refresh requires a reserve target; drive it from the interactive market palette",
            ))),
            3 => Ok(MarketEvent::Failed(Safe::literal(
                "ttl extension is a mutating market action with no capture facility; refused fail-closed",
            ))),
            _ => Ok(MarketEvent::Failed(Safe::literal(
                "market palette index is out of range",
            ))),
        }
    }
}

/// One occupied journal coordinate, as proven by `begin_market_pending`'s
/// create-new failure itself: either the `intent.json` reservation hit an
/// io `AlreadyExists`, or the slot already holds a settled result. Any other
/// failure is a real error and propagates.
fn slot_occupied(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

/// Decoded `RefreshStatus`/`CachedStatus` classification: only the `Accepted`
/// arm is acceptance. `Blocked` and `ResolveFailed` complete normally on
/// chain, and a missing or unparseable status fails closed — a transaction
/// success is never acceptance evidence by itself.
fn status_accepted(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Object(map)) => map.len() == 1 && map.contains_key("Accepted"),
        Some(Value::String(s)) => s == "Accepted",
        _ => false,
    }
}

/// `get_cached(asset)` readback: a `Some` entry alone can carry a
/// `ResolveFailed` status, so the cached status must classify `Accepted`.
fn cached_accepted(
    st: &Stellar<'_, RealExecutor>,
    source: &str,
    runtime: &ContractId,
    asset_json: &str,
) -> bool {
    match st.view(
        runtime.as_str(),
        source,
        CONTRACT_GET_CACHED,
        &[("asset", asset_json.to_string())],
    ) {
        Ok(v) => !v.is_null() && status_accepted(v.get("status")),
        Err(_) => false,
    }
}

/// Adapter `lastprice(asset)` readback: `Some` is the only read encoding
/// accepted-AND-fresh (it reads `None` on staleness and on a tripped
/// breaker), so presence is the freshness authority.
fn lastprice_present(
    st: &Stellar<'_, RealExecutor>,
    source: &str,
    adapter: &ContractId,
    asset_json: &str,
) -> bool {
    matches!(
        st.view(
            adapter.as_str(),
            source,
            CONTRACT_LASTPRICE,
            &[("asset", asset_json.to_string())],
        ),
        Ok(v) if !v.is_null()
    )
}
/// Frozen unresolved-submission classification: a result that is both
/// "ok" and error-bearing, or whose outcome cannot be established, is
/// unresolved custody — a custody slot is never closed (settled or
/// cleared) against one.
fn unresolved_submission(inv: &crate::stellar::Invocation) -> bool {
    inv.ok == inv.error.is_some() || !definite_outcome(inv)
}

/// Breaker health read: publication and value-movement preconditions
/// require `get_breaker_set_view` to be readable and to report neither a
/// manual trip nor blocking. An automatic trip blocks while
/// `is_manually_tripped` remains false, so both flags are checked; an
/// absent or null view fails the precondition closed.
fn breaker_clear(
    st: &Stellar<'_, RealExecutor>,
    source: &str,
    runtime: &ContractId,
    asset_json: &str,
) -> bool {
    match st.view(
        runtime.as_str(),
        source,
        CONTRACT_GET_BREAKER_VIEW,
        &[("asset", asset_json.to_string())],
    ) {
        Ok(v) if !v.is_null() => {
            v.get("is_manually_tripped").and_then(Value::as_bool) == Some(false)
                && v.get("is_blocking").and_then(Value::as_bool) == Some(false)
        }
        _ => false,
    }
}

/// Renders one settled outcome under market policy: every stored or surfaced
/// string is a policy-rendered `Safe`; violations carry closed-enum
/// contract-error text (e.g. stale-price `ReserveStale`/`#1210`) — never raw
/// CLI bytes.
enum InvocationOutcome {
    Ok,
    Failed(ErrorKind),
    Notice(Safe),
    NothingSent,
}

impl InvocationOutcome {
    fn of(inv: &crate::stellar::Invocation) -> Self {
        if inv.ok {
            Self::Ok
        } else {
            inv.error
                .as_ref()
                .map(|kind| Self::Failed(kind.clone()))
                .unwrap_or(Self::NothingSent)
        }
    }

    fn render(&self) -> (Safe, Vec<Safe>) {
        match self {
            Self::Ok => (Safe::literal("accepted"), Vec::new()),
            Self::Failed(kind) => (
                Safe::error_summary(Policy::Market, kind),
                vec![Safe::error_explained(Policy::Market, kind)],
            ),
            Self::Notice(text) => (text.clone(), Vec::new()),
            Self::NothingSent => (Safe::literal("not sent"), Vec::new()),
        }
    }
}

/// Market-mode `SideHooks`: market presentation only. Nothing here touches
/// pair state: no captured snapshot exists (a market capture facility is
/// deferred, and mutating steps never reach the shared primitive), and
/// mint/auction recording is unreachable for the same reason.
struct MarketHooks<'a> {
    halted: &'a AtomicBool,
}

impl SideHooks for MarketHooks<'_> {
    fn captured(&self) -> Option<&crate::observe::Snapshot> {
        None
    }

    fn mint_recorded(&mut self, _side: char, _token: Tok, _amount: i64) -> Result<()> {
        Err(anyhow!(
            "market capture facility absent: a mint state effect reached the hooks with no journaled custody",
        ))
    }

    fn auction_recorded(&mut self, _side: char, _user: Who, _block: u32) -> Result<()> {
        Err(anyhow!(
            "market capture facility absent: an auction state effect reached the hooks with no journaled custody",
        ))
    }

    fn wait(&mut self, _secs: u64, _label: &str) -> Result<bool> {
        Ok(!self.halted.load(Ordering::SeqCst))
    }
}

/// Wall-clock seconds for the per-asset publication timestamp argument (the
/// mock publisher requires it); the proxy's own freshness filters are the
/// enforcement truth, so no local clock arithmetic gates anything.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// SHA-256 round constants (FIPS 180-4).
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 compression function over one 64-byte block (FIPS 180-4 §6.2).
fn sha256_compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(big_s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    let sums = [a, b, c, d, e, f, g, h];
    for (word, sum) in state.iter_mut().zip(sums) {
        *word = word.wrapping_add(sum);
    }
}

/// Lowercase-hex SHA-256 (FIPS 180-4) of `data`. Pure Rust, dependency
/// free: this crate has no `sha2`, and a local implementation is the
/// sanctioned path for the custody payload digest. Gated by NIST
/// known-answer tests below — a wrong implementation cannot pass them.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut chunks = data.chunks_exact(64);
    for chunk in chunks.by_ref() {
        sha256_compress(&mut state, chunk.try_into().expect("64-byte chunk"));
    }
    // Padding (FIPS 180-4 §5.1): append 0x80, zeros, then the 64-bit
    // big-endian bit length; a second block when the length overflows the
    // first. `chunks` is exhausted, so its `remainder()` is the true message
    // tail — never a fresh iterator's prefix. Message length is bounded by
    // `isize::MAX`, so the bit length always fits in a `u64`.
    let tail = chunks.remainder();
    let bit_len = (data.len() as u64) * 8;
    let mut block = [0u8; 64];
    block[..tail.len()].copy_from_slice(tail);
    block[tail.len()] = 0x80;
    if tail.len() > 55 {
        sha256_compress(&mut state, &block);
        block = [0u8; 64];
    }
    block[56..].copy_from_slice(&bit_len.to_be_bytes());
    sha256_compress(&mut state, &block);
    let mut out = String::with_capacity(64);
    for word in state {
        for byte in word.to_be_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// The tool's canonicalization seam: the SHA-256 payload digest stored in a
/// `MarketCustodyRecord` is taken over the deterministic serialization of
/// the record's real fields (phase, mutation kind, action label, network,
/// signer, target, artifact/predicted digests, rendered pre-state anchors)
/// with the digest slot blanked. Any serialization failure returns `Err`
/// and the mutation aborts before its intent is written — a constant or
/// unverifiable digest can never reach the journal. Where the tool grows a
/// general canonicalization facility, this is the seam it belongs in;
/// deploy-phase custody must call this same function.
pub(crate) fn custody_payload_digest(record: &MarketCustodyRecord) -> Result<Sha256Hex> {
    let mut value = serde_json::to_value(record)
        .map_err(|_| anyhow!("custody payload canonicalization failed"))?;
    let Some(map) = value.as_object_mut() else {
        return Err(anyhow!("custody payload canonicalization shape mismatch"));
    };
    map.insert(
        "payload_digest".to_string(),
        serde_json::Value::String(String::new()),
    );
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| anyhow!("custody payload canonicalization failed"))?;
    sha256_hex(&bytes).try_into()
}

/// The canonical pre-state anchor for one market call: the contract
/// method and every argument — name and value, in send order — bound as
/// exact ordered bytes. `begin_mutation` derives every play intent's
/// pre-state and through it its payload digest from this helper, so two
/// same-action mutations that differ in asset, price, timestamp, proposal
/// id, operation, or TTL can never share a custody digest, and an
/// ambiguous journal entry can never be attributed to a payload it did
/// not reserve. Identical payloads bind identically (replay detection).
pub(crate) fn play_call_pre_state(method: &str, args: &[(&str, String)]) -> Result<Vec<Safe>> {
    let bound_call = json!({
        "method": method,
        "args": args,
    });
    Ok(vec![Safe::render_market_json(&bound_call, 65_536)?])
}

#[cfg(test)]
mod custody_tests {
    use super::*;

    #[test]
    fn sha256_nist_known_answers() {
        // FIPS 180-4 / NIST examples — the gate for the local helper.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // 56-byte FIPS 180-2 vector: the tail is 56 bytes, so the 0x80 byte
        // and the 64-bit length cannot share the final block — the only
        // vector here that reaches the two-block padding branch.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Multi-block vector: one million copies of 'a'.
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex(&million),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_nist_896bit_known_answer() {
        // NIST SHA-256 sample, 896 bits / 112 bytes — not a multiple of the
        // 64-byte block: a wrong message tail (e.g. a remainder taken from
        // a fresh iterator instead of the exhausted one) fails this vector
        // or panics, while the short and 1e6-'a' vectors pass. The expected
        // digest is the value produced by coreutils `sha256sum` over this
        // exact message, not a remembered constant.
        assert_eq!(
            sha256_hex(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn status_accepted_only_for_the_accepted_arm() {
        // Decoded enum forms: tagged object (data variant) and bare string
        // (unit-shaped render). Blocked/ResolveFailed/null/missing fail.
        assert!(status_accepted(Some(&json!({"Accepted": {"price": 1}}))));
        assert!(status_accepted(Some(&json!("Accepted"))));
        assert!(!status_accepted(Some(&json!({"Blocked": 2}))));
        assert!(!status_accepted(Some(&json!({"ResolveFailed": 1}))));
        assert!(!status_accepted(Some(&Value::Null)));
        assert!(!status_accepted(None));
        // A tagged object with extra arms is not a clean single-arm decode.
        assert!(!status_accepted(Some(
            &json!({"Accepted": 1, "Blocked": 0})
        )));
    }

    #[test]
    fn slot_occupied_recognizes_only_reservation_failures() {
        use std::io::{Error as IoError, ErrorKind as IoErrorKind};
        let io = IoError::from(IoErrorKind::AlreadyExists);
        let reserved = anyhow::Error::new(io).context("reserving market intent");
        assert!(slot_occupied(&reserved));
        let real = anyhow::Error::msg("syncing market intent failed");
        assert!(!slot_occupied(&real));
    }
}

#[cfg(test)]
mod play_integrity_tests {
    use super::*;
    use crate::manifest::{begin_market_pending, finish_market_pending, market_pending_dir};
    use std::str::FromStr;

    // Checksum-valid StrKey fixtures reused from the in-tree test law: the
    // zero-key admin and distinct actor addresses, plus the harness factory
    // contract used as a second distinct asset target.
    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const G_FRODO: &str = "GAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHV4";
    const G_SAM: &str = "GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX";
    const G_USER: &str = "GABQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABEQO";
    const C_ZERO: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
    const C_OTHER: &str = "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53";

    use std::path::{Path, PathBuf};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abtest-play-integrity-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn account(s: &str) -> AccountId {
        AccountId::from_str(s).expect("valid fixture account")
    }

    fn contract(s: &str) -> ContractId {
        ContractId::from_str(s).expect("valid fixture contract")
    }

    /// A complete market manifest fixture. `with_user` controls whether the
    /// frozen run-local user identity is present — the SEC005 discriminator.
    fn manifest_fixture(with_user: bool) -> MarketManifest {
        let identities = if with_user {
            json!({"frodo": G_FRODO, "sam": G_SAM, "user": G_USER})
        } else {
            json!({})
        };
        serde_json::from_value(json!({
            "network": {"label": "testnet", "passphrase": MARKET_TESTNET_PASSPHRASE},
            "admin": G_ADMIN,
            "rpc_host": "rpc.testnet.stellar.example",
            "oracle": {
                "runtime": C_ZERO, "governance": C_ZERO, "adapter": C_ZERO,
                "sources": [C_ZERO, C_ZERO, C_ZERO],
                "min_sources": 3, "max_age_secs": 3600, "max_clock_drift_secs": 60,
                "decimals": 7, "resolution": 300, "base": "USD"
            },
            "tokens": {},
            "pool": C_ZERO, "backstop": C_ZERO, "factory": C_ZERO,
            "comet": C_ZERO, "emitter": C_ZERO,
            "reserves": {},
            "identities": identities,
            "deploy_ledger": 1
        }))
        .expect("fixture market manifest")
    }

    /// A market execution context pointing at a nonexistent CLI binary: any
    /// attempted spawn fails locally (ENOENT), so a test can never send.
    fn ctx_fixture(out: &Path) -> MarketExecutionContext {
        ctx_with_bin(
            out,
            format!("/nonexistent-abtest-cli-{}", std::process::id()).into(),
        )
    }

    fn ctx_with_bin(out: &Path, bin: PathBuf) -> MarketExecutionContext {
        MarketExecutionContext::new(
            "http://127.0.0.1:9/rpc".to_string(),
            Vec::new(),
            "abtest-operator-admin".to_string(),
            out.join("admin-cfg"),
            out.join("actor-cfg"),
            bin,
            out.join("xdg"),
            "/".into(),
        )
        .expect("fixture market context")
    }

    fn play_record(
        kind: MarketMutationKind,
        action: &'static str,
        signer: AccountId,
        target: &str,
        method: &str,
        args: &[(&str, String)],
    ) -> MarketCustodyRecord {
        let mut record = MarketCustodyRecord {
            phase: PLAY_PHASE,
            kind,
            action: Safe::literal(action),
            network: MarketNetwork::Testnet,
            signer,
            payload_digest: Sha256Hex::default(),
            target: Some(contract(target)),
            artifact_digest: None,
            predicted: None,
            pre_state: play_call_pre_state(method, args).expect("canonical binding"),
            tool_error: None,
        };
        record.payload_digest = custody_payload_digest(&record).expect("fixture custody digest");
        record
    }

    fn settle_step(record: &MarketCustodyRecord) -> MarketStepRecord {
        MarketStepRecord {
            phase: record.phase,
            action: record.action.clone(),
            command: record.action.clone(),
            outcome: Safe::literal("accepted"),
            target: record.target.clone(),
            violations: Vec::new(),
        }
    }

    fn publish_args(asset: &str, price: &str, timestamp: &str) -> Vec<(&'static str, String)> {
        vec![
            ("asset", asset.to_string()),
            ("price", price.to_string()),
            ("timestamp", timestamp.to_string()),
        ]
    }

    #[test]
    fn truncated_settled_result_with_real_intent_blocks_play_without_any_send() {
        // SEC002: a settled result that is truncated (or otherwise fails to
        // parse and match its intent) must never read as settled. Play
        // admission refuses before any executor or chain call exists, and
        // nothing is written to the journal in the meantime.
        let out = scratch("truncated-result");
        settle_play_coordinate(&out, 1);
        let result_path = market_pending_dir(&out, PLAY_PHASE, 1).join("result.json");
        let full = std::fs::read(&result_path).expect("settled fixture readable");
        let truncated = &full[..full.len() / 3];
        assert!(
            serde_json::from_slice::<MarketPendingResult>(truncated).is_err(),
            "the truncated prefix must not parse as a settled result"
        );
        std::fs::write(&result_path, truncated).expect("result truncated");

        assert!(
            !journal_clear_for_action(&out),
            "a truncated settled result must never clear the strict gate"
        );
        let err = MarketRunner::new(
            manifest_fixture(true),
            ctx_fixture(&out),
            MarketJournal::open(&out),
        )
        .err()
        .expect("play admission must refuse a malformed settled result");
        assert!(
            format!("{err:#}").contains("not strictly clear"),
            "play must refuse at the integrity gate: {err:#}"
        );
        // No forged settlement: the corrupt bytes are untouched and no new
        // journal coordinate appeared anywhere.
        assert_eq!(std::fs::read(&result_path).expect("unreadable"), truncated);
        assert_eq!(
            journal_coords(&out),
            vec!["001-01".to_string()],
            "journal layout changed under refusal"
        );
    }

    #[test]
    fn zero_length_result_with_real_intent_never_forges_settlement() {
        // SEC002: a zero-length result beside a real intent is not a
        // settlement. The gate stays closed, play refuses, and the same
        // coordinate cannot be re-reserved — reconciliation, not overwrite.
        let out = scratch("zero-result");
        let record = play_record(
            MarketMutationKind::Invoke,
            "refresh asset",
            account(G_USER),
            C_ZERO,
            CONTRACT_REFRESH,
            &[("asset", json!({"Stellar": C_ZERO}).to_string())],
        );
        begin_market_pending(&out, PLAY_PHASE, 2, &record).expect("intent reserved");
        let dir = market_pending_dir(&out, PLAY_PHASE, 2);
        std::fs::write(dir.join("result.json"), b"").expect("zero-length result planted");

        assert!(
            !journal_clear_for_action(&out),
            "a zero-length settled result must never clear the strict gate"
        );
        let err = MarketRunner::new(
            manifest_fixture(true),
            ctx_fixture(&out),
            MarketJournal::open(&out),
        )
        .err()
        .expect("play admission must refuse the forged settlement");
        assert!(
            format!("{err:#}").contains("not strictly clear"),
            "play must refuse at the integrity gate: {err:#}"
        );
        assert_eq!(std::fs::metadata(dir.join("result.json")).unwrap().len(), 0);
        assert!(
            begin_market_pending(&out, PLAY_PHASE, 2, &record).is_err(),
            "a planted result must block, not be silently overwritten"
        );
    }

    #[test]
    fn missing_result_after_crash_cannot_be_settled_by_a_replay() {
        // SEC002: an open intent (crash after reservation) keeps the strict
        // gate closed, play admission refuses, and a re-attempt can never
        // forge a settlement at the occupied coordinate.
        let out = scratch("open-intent");
        let record = play_record(
            MarketMutationKind::Invoke,
            "price publish",
            account(G_ADMIN),
            C_ZERO,
            CONTRACT_SET_PRICE,
            &publish_args(
                &json!({"Stellar": C_ZERO}).to_string(),
                "2000",
                "1700000000",
            ),
        );
        begin_market_pending(&out, PLAY_PHASE, 3, &record).expect("intent reserved");
        assert!(
            !journal_clear_for_action(&out),
            "an open intent must never clear the strict gate"
        );
        let err = MarketRunner::new(
            manifest_fixture(true),
            ctx_fixture(&out),
            MarketJournal::open(&out),
        )
        .err()
        .expect("play admission must refuse an open intent");
        assert!(
            format!("{err:#}").contains("not strictly clear"),
            "play must refuse at the integrity gate: {err:#}"
        );
        assert!(
            begin_market_pending(&out, PLAY_PHASE, 3, &record).is_err(),
            "the occupied coordinate refuses replay, so nothing can be forged settled"
        );
        assert!(!market_pending_dir(&out, PLAY_PHASE, 3)
            .join("result.json")
            .exists());
    }

    #[test]
    fn same_action_different_payloads_cannot_share_an_intent_digest() {
        // SEC004: the journal must identify the exact write payload. Every
        // discriminating field — asset, price, timestamp, proposal id, TTL,
        // operation, and even argument order — flows into the canonical
        // call binding and through it into the custody payload digest.
        let asset_a = json!({"Stellar": C_ZERO}).to_string();
        let asset_b = json!({"Stellar": C_OTHER}).to_string();
        let sets: Vec<Vec<(&str, String)>> = vec![
            publish_args(&asset_a, "2000", "1700000000"),
            publish_args(&asset_b, "2000", "1700000000"),
            publish_args(&asset_a, "2001", "1700000000"),
            publish_args(&asset_a, "2000", "1700000001"),
            vec![
                ("price", "2000".to_string()),
                ("asset", asset_a.clone()),
                ("timestamp", "1700000000".to_string()),
            ],
        ];
        let mut digests = Vec::new();
        for args in &sets {
            let record = play_record(
                MarketMutationKind::Invoke,
                "price publish",
                account(G_ADMIN),
                C_ZERO,
                CONTRACT_SET_PRICE,
                args,
            );
            digests.push(record.payload_digest.clone());
        }
        for (i, a) in digests.iter().enumerate() {
            for (j, b) in digests.iter().enumerate() {
                if i != j {
                    assert_ne!(
                        a, b,
                        "payload set {i} and {j} collided in one custody digest"
                    );
                }
            }
        }
        // The same payload binds identically (replay detection), and the
        // reserved journal records carry exactly those bindings.
        let out = scratch("payload-binding");
        let first = play_record(
            MarketMutationKind::Invoke,
            "price publish",
            account(G_ADMIN),
            C_ZERO,
            CONTRACT_SET_PRICE,
            &sets[0],
        );
        let repeat = play_record(
            MarketMutationKind::Invoke,
            "price publish",
            account(G_ADMIN),
            C_ZERO,
            CONTRACT_SET_PRICE,
            &sets[0],
        );
        assert_eq!(first.payload_digest, repeat.payload_digest);
        begin_market_pending(&out, PLAY_PHASE, 4, &first).expect("intent reserved");
        assert_eq!(
            read_market_intent(&out, PLAY_PHASE, 4)
                .expect("stored intent")
                .payload_digest,
            first.payload_digest,
            "the journal must identify the exact reserved write payload"
        );
        // A different proposal id is a different intent even with a
        // byte-identical operation and TTL.
        let op = json!({"SetManualTrip": [json!({"Stellar": C_ZERO}), true, null]}).to_string();
        let id7 = play_record(
            MarketMutationKind::Governance,
            "manual trip proposal",
            account(G_ADMIN),
            C_ZERO,
            "create_proposal",
            &[
                ("caller", G_ADMIN.to_string()),
                ("id", "7".to_string()),
                ("operation", op.clone()),
                ("requested_ttl", "0".to_string()),
            ],
        );
        let id8 = play_record(
            MarketMutationKind::Governance,
            "manual trip proposal",
            account(G_ADMIN),
            C_ZERO,
            "create_proposal",
            &[
                ("caller", G_ADMIN.to_string()),
                ("id", "8".to_string()),
                ("operation", op),
                ("requested_ttl", "0".to_string()),
            ],
        );
        assert_ne!(id7.payload_digest, id8.payload_digest);
    }

    #[test]
    fn refresh_intent_binds_the_frozen_user_and_never_the_operator() {
        // SEC005: a refresh intent bound to the frozen user G is a distinct
        // custody object from one bound to the operator, so neither can be
        // settled or attributed as the other.
        let asset = json!({"Stellar": C_ZERO}).to_string();
        let user_intent = play_record(
            MarketMutationKind::Invoke,
            "refresh asset",
            account(G_USER),
            C_ZERO,
            CONTRACT_REFRESH,
            &[("asset", asset.clone())],
        );
        let admin_intent = play_record(
            MarketMutationKind::Invoke,
            "refresh asset",
            account(G_ADMIN),
            C_ZERO,
            CONTRACT_REFRESH,
            &[("asset", asset)],
        );
        assert_ne!(G_USER, G_ADMIN);
        assert_ne!(user_intent.signer, admin_intent.signer);
        assert_ne!(user_intent.payload_digest, admin_intent.payload_digest);
    }

    #[test]
    fn play_admission_requires_the_frozen_user_identity() {
        // SEC005: without a frozen user identity the runner refuses at
        // construction — before any executor exists — so a refresh can
        // never silently execute or record itself as the operator.
        let out = scratch("user-attribution");
        let record = play_record(
            MarketMutationKind::Invoke,
            "price publish",
            account(G_ADMIN),
            C_ZERO,
            CONTRACT_SET_PRICE,
            &publish_args(
                &json!({"Stellar": C_ZERO}).to_string(),
                "2000",
                "1700000000",
            ),
        );
        // A legitimately settled, reconcilable fixture so the strict gate
        // itself is clear and the identity law is what decides.
        begin_market_pending(&out, PLAY_PHASE, 9, &record).expect("fixture intent");
        finish_market_pending(
            &out,
            PLAY_PHASE,
            9,
            &MarketPendingResult {
                custody: record.clone(),
                step: settle_step(&record),
            },
        )
        .expect("fixture settled");
        assert!(
            journal_clear_for_action(&out),
            "a settled, well-formed journal must clear the strict gate"
        );

        let err = MarketRunner::new(
            manifest_fixture(false),
            ctx_fixture(&out),
            MarketJournal::open(&out),
        )
        .err()
        .expect("construction must require the frozen user identity");
        assert!(
            format!("{err:#}").contains("no frozen user identity"),
            "missing frozen user must refuse before any executor: {err:#}"
        );

        // With the frozen user present, construction proceeds to the pinned
        // RPC probe and refuses there (the fixture CLI does not exist),
        // proving the earlier refusal was the identity law, not noise.
        let err = MarketRunner::new(
            manifest_fixture(true),
            ctx_fixture(&out),
            MarketJournal::open(&out),
        )
        .err()
        .expect("the absent CLI probe must refuse before play");
        assert!(
            !format!("{err:#}").contains("no frozen user identity"),
            "the frozen-user refusal must not repeat once the identity is present: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A logging `stellar` stand-in: it answers only the two read-only probes
    /// play admission needs (the CLI version gate and the RPC network
    /// identity), refuses anything else, and records every argv it was handed,
    /// so a test can prove a refusal sent nothing. It also creates the
    /// journal root a deployed run always has, so admission sees a clear journal.
    fn logging_ctx(out: &Path) -> (MarketExecutionContext, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(out.join("market-pending")).expect("journal root");
        let log = out.join("stellar-calls.log");
        let script = out.join("stellar");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n'--version') printf 'stellar 26.1.0 (1228cff)\\n'; exit 0 ;;\nesac\ncase \"$1 $2\" in\n'network info') printf '{{\"passphrase\":\"{}\"}}\\n'; exit 0 ;;\nesac\nprintf 'the read-only stand-in refuses this command\\n' >&2\nexit 1\n",
                log.display(),
                MARKET_TESTNET_PASSPHRASE
            ),
        )
        .expect("stand-in CLI");
        std::fs::set_permissions(&script, PermissionsExt::from_mode(0o755))
            .expect("stand-in CLI executable");
        (
            MarketExecutionContext::new(
                "http://127.0.0.1:9/rpc".to_string(),
                Vec::new(),
                "abtest-operator-admin".to_string(),
                out.join("admin-cfg"),
                out.join("actor-cfg"),
                script,
                out.join("xdg"),
                "/".into(),
            )
            .expect("fixture market context"),
            log,
        )
    }

    /// Every CLI invocation recorded so far, in order.
    fn cli_calls(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The journal's coordinate directory names: enough to prove a refusal
    /// reserved none of them.
    fn journal_coords(out: &Path) -> Vec<String> {
        std::fs::read_dir(out.join("market-pending"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A fully settled play coordinate: an intent plus its matching settled
    /// result, so the strict integrity gate still clears and capacity alone is
    /// what decides admission.
    fn settle_play_coordinate(out: &Path, slot: u32) {
        let record = play_record(
            MarketMutationKind::Invoke,
            "refresh asset",
            account(G_USER),
            C_ZERO,
            CONTRACT_REFRESH,
            &[("asset", json!({"Stellar": C_ZERO}).to_string())],
        );
        begin_market_pending(out, PLAY_PHASE, slot, &record).expect("seed intent");
        finish_market_pending(
            out,
            PLAY_PHASE,
            slot,
            &MarketPendingResult {
                custody: record.clone(),
                step: settle_step(&record),
            },
        )
        .expect("seed settlement");
    }

    /// The same manifest with every oracle asset registered — the shape the
    /// publication journey sizes its journal capacity against.
    fn asset_manifest_fixture() -> MarketManifest {
        let mut manifest = manifest_fixture(true);
        let ids = [C_ZERO, C_OTHER, C_ZERO, C_OTHER];
        manifest.tokens = Tok::ORACLE_ORDER
            .iter()
            .enumerate()
            .map(|(index, tok)| (*tok, contract(ids[index])))
            .collect();
        manifest
    }

    #[test]
    fn publication_journey_refuses_up_front_when_play_slots_cannot_hold_it() {
        // One journaled mutation per (source, registered asset) publication
        // plus one refresh per registered asset. Occupy one coordinate beyond
        // what the journey can afford and prove the whole journey is refused
        // before its first publish: no executor call, no reserved coordinate.
        let out = scratch("journey-capacity");
        let (ctx, log) = logging_ctx(&out);
        let manifest = asset_manifest_fixture();
        let needed =
            manifest.oracle.sources.len() * Tok::ORACLE_ORDER.len() + Tok::ORACLE_ORDER.len();
        let mut runner =
            MarketRunner::new(manifest, ctx, MarketJournal::open(&out)).expect("fixture runner");
        assert!(
            !cli_calls(&log).is_empty(),
            "admission must have probed the CLI"
        );
        let occupied = JOURNAL_SLOT_SCAN_CEILING as usize - needed + 1;
        for slot in 1..=occupied {
            settle_play_coordinate(&out, slot as u32);
        }
        assert_eq!(
            runner.free_play_slots(),
            needed - 1,
            "one slot short of the journey"
        );
        assert!(
            journal_clear_for_action(&out),
            "the planted settlements must keep the integrity gate clear, so capacity alone decides"
        );
        let calls = cli_calls(&log);
        let coords = journal_coords(&out);
        let event = runner
            .execute_market_step(
                Safe::literal("publish prices"),
                &Step::SetPrices(vec![2_000_i128; Tok::ORACLE_ORDER.len()]),
            )
            .expect("a refusal is reported as an event");
        match event {
            MarketEvent::Failed(reason) => assert!(
                reason.to_string().contains("too few free play coordinates"),
                "the refusal must name the capacity precondition: {reason}"
            ),
            other => panic!("a journey that cannot finish must be refused, got {other:?}"),
        }
        assert_eq!(
            cli_calls(&log),
            calls,
            "the capacity refusal must make zero executor calls"
        );
        assert_eq!(
            journal_coords(&out),
            coords,
            "the capacity refusal must reserve no journal coordinate"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn offset_price_publication_is_refused_before_any_mutation() {
        // The publication journey stamps its own timestamp, so an offset probe
        // would silently publish the present. It is refused as an event, with
        // no executor call and no journal coordinate.
        let out = scratch("offset-publish");
        let (ctx, log) = logging_ctx(&out);
        let mut runner =
            MarketRunner::new(asset_manifest_fixture(), ctx, MarketJournal::open(&out))
                .expect("fixture runner");
        let calls = cli_calls(&log);
        let event = runner
            .execute_market_step(
                Safe::literal("offset publish"),
                &Step::SetPricesAt {
                    prices: vec![2_000_i128; Tok::ORACLE_ORDER.len()],
                    offset_secs: 3_600,
                },
            )
            .expect("a refusal is reported as an event");
        match event {
            MarketEvent::Failed(reason) => assert!(
                reason
                    .to_string()
                    .contains("offset price publication is not supported"),
                "the refusal must name the offset refusal: {reason}"
            ),
            other => panic!("an offset publication must be refused, got {other:?}"),
        }
        assert_eq!(
            cli_calls(&log),
            calls,
            "the offset refusal must make zero executor calls"
        );
        assert!(
            journal_coords(&out).is_empty(),
            "the offset refusal must reserve no journal coordinate"
        );
        let _ = std::fs::remove_dir_all(&out);
    }
}
