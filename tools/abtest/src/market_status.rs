//! Read-only protected-market state, bounded journal status, and readiness.
//!
//! Every chain observation is a simulation (`--send no`) against identifiers
//! frozen in the completed market manifest. No CLI output is surfaced raw.
//! Unsupported, failed, missing, and null observations remain distinct and
//! readiness stays closed unless every required market leg is observed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsFd, AsRawFd};
use std::path::Path;

use libc::{O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use serde_json::{json, Value};

use crate::manifest::{
    MarketCustodyRecord, MarketManifest, MarketPendingResult, Tok, Who, MARKET_NETWORK_LABEL,
    MARKET_TESTNET_PASSPHRASE,
};
use crate::market::custody_payload_digest;
use crate::market_explorer::SPEC_ENTRY_TAGS;
use crate::observe::{parallel, strict_map, try_num};
use crate::stellar::{ContractId, Executor, Safe, Stellar};

const STATUS_SCHEMA_VERSION: u32 = 1;
const STATUS_RENDER_CAP: usize = 65_536;
const STATUS_OBSERVED_CAP: usize = 4_096;
// ponytail: the custody scan inspects at most 256 journal coordinates in one
// pass. A run holding exactly 256 settled entries reports journal-full and
// the action gate refuses the next reservation before it can create an entry
// no future status pass can inspect; beyond 256 it reports journal-truncated.
// Upgrade path for a long-lived market: a verified sealed archive plus
// indexed pagination, not a larger scan constant.
const PENDING_SCAN_LIMIT: usize = 256;
pub(crate) const PENDING_RECORD_CAP: usize = 65_536;

const MAX_REASON_COUNT: usize = 32;

const FN_LASTPRICE: &str = "lastprice";
const FN_GET_CACHED: &str = "get_cached";
const FN_GET_BREAKER_VIEW: &str = "get_breaker_set_view";
const FN_GET_CONFIG: &str = "get_config";
const FN_GET_ADMIN: &str = "get_admin";
const FN_GET_RESERVE: &str = "get_reserve";
const FN_GET_POSITIONS: &str = "get_positions";
const FN_POOL_DATA: &str = "pool_data";
const FN_USER_BALANCE: &str = "user_balance";
const FN_REWARD_ZONE: &str = "reward_zone";
const FN_BALANCE: &str = "balance";
const FN_NEXT_PROPOSAL_ID: &str = "next_proposal_id";
const FN_ACTIVE_IDS: &str = "active_ids";
const FN_GET_PROPOSAL: &str = "get_proposal";
// ponytail: shows only the 16 newest pending proposals, so the contract's 64
// maximal-metadata proposals cannot overflow the 64 KiB status render cap.
// Paginate governance reads if an operator needs the whole queue here.
const MAX_SHOWN_PROPOSALS: usize = 16;

const STATE_INTERFACE_AVAILABLE: &str = "interface-available";
const STATE_INTERFACE_UNAVAILABLE: &str = "interface-unavailable";
const STATE_INTERFACE_MALFORMED: &str = "interface-malformed";
const STATE_INTERFACE_MISMATCH: &str = "interface-mismatch";
const STATE_UNSUPPORTED: &str = "unsupported";
const STATE_READ_FAILED: &str = "read-failed";
const STATE_NULL: &str = "null";
const STATE_MISSING: &str = "missing";
const STATE_MALFORMED: &str = "malformed-response";
const STATE_OBSERVED: &str = "observed";
const STATE_PRICE_NULL: &str = "adapter-price-null";
const STATE_PRICE_AVAILABLE: &str = "adapter-price-observed";
const STATE_PRICE_MALFORMED: &str = "adapter-price-malformed";
const STATE_CACHE_ACCEPTED: &str = "oracle-cache-accepted";
const STATE_CACHE_BLOCKED: &str = "oracle-cache-blocked";
const STATE_CACHE_FAILED: &str = "oracle-cache-resolution-failed";
const STATE_BREAKER_CLEAR: &str = "breaker-clear";
const STATE_BREAKER_TRIPPED: &str = "breaker-tripped";
const STATE_BREAKER_BLOCKING: &str = "breaker-blocking";
const STATE_POOL_ACTIVE: &str = "pool-active";
const STATE_POOL_INACTIVE: &str = "pool-inactive";
const STATE_ORACLE_MISMATCH: &str = "oracle-mismatch";
const STATE_ADMIN_MATCH: &str = "admin-match";
const STATE_ADDRESS_MISMATCH: &str = "address-mismatch";
const STATE_ACCOUNT_BOUND: &str = "account-bound";
const STATE_IDENTITY_COLLAPSE: &str = "admin-actor-identity-collapse";
const STATE_GOVERNANCE_UNBOUND: &str = "governance-unbound";
const STATE_RESERVE_ENABLED: &str = "reserve-enabled";
const STATE_RESERVE_DISABLED: &str = "reserve-disabled";
const STATE_PENDING_CLEAR: &str = "journal-clear";
const STATE_PENDING_UNRESOLVED: &str = "journal-unresolved";
const STATE_PENDING_TRUNCATED: &str = "journal-truncated";
pub(crate) const STATE_PENDING_FULL: &str = "journal-full";
const STATE_PENDING_UNAVAILABLE: &str = "journal-unavailable";

const REASON_CLI: &str = "market CLI version gate unavailable";
const REASON_NETWORK: &str = "manifest or execution network is not exact testnet";
const REASON_INTERFACE: &str = "a manifest-bound contract interface is unavailable or malformed";
const REASON_PRICE: &str = "a required adapter price is null, stale, or unavailable";
const REASON_CACHE: &str = "a required oracle cache is unavailable, null, or not accepted";
const REASON_BREAKER: &str = "a circuit breaker is unavailable, tripped, or blocking";
const REASON_POOL: &str = "pool configuration is unavailable, malformed, or inactive";
const REASON_ADMIN: &str = "pool admin does not match the manifest signer";
const REASON_IDENTITY: &str = "an admin or actor identity is missing or collapsed";
const REASON_RESERVE: &str = "a required reserve is unavailable, disabled, or malformed";
const REASON_POSITION: &str = "required position state is unavailable, null, or malformed";
const REASON_BACKSTOP: &str = "backstop state is unavailable, null, or malformed";
const REASON_BALANCE: &str = "a required token balance is unavailable, null, or malformed";
const REASON_JOURNAL: &str = "journal state is unresolved or unavailable";
const REASON_JOURNAL_FULL: &str =
    "journal scan capacity reached; preserve evidence and start a fresh disposable run or publicly reconcile with holders";
const REASON_JOURNAL_TOO_LARGE: &str =
    "journal scan too large to fully inspect; preserve evidence and start a fresh disposable run or publicly reconcile with holders";
const REASON_SUPPRESSED: &str = "additional readiness findings suppressed";

struct TargetState {
    state: &'static str,
    functions: BTreeMap<String, Vec<String>>,
}

/// One independent status observation pending its bounded fan-out, or a
/// manifest gap that never reaches the chain. The vector order is the
/// report order: assembly re-walks it regardless of completion order.
enum StatusRead<'a> {
    Observe(Observe<'a>),
    Missing { panel: Panel, reason: &'static str },
}

struct Observe<'a> {
    panel: Panel,
    target: &'a ContractId,
    target_state: Option<&'a TargetState>,
    function: &'static str,
    args: Vec<(&'static str, String)>,
    kind: ReadKind<'a>,
}

enum ReadKind<'a> {
    AdapterPrice,
    ProxyCache,
    ProxyBreaker,
    PoolConfig {
        adapter: &'a str,
    },
    PoolAdmin {
        admin: &'a str,
    },
    Reserve {
        expected_asset: &'a str,
        index: i128,
        decimals: i128,
    },
    StrictMap {
        reason: &'static str,
    },
    RewardZone,
    Numeric,
    // Governance reads are display-only: they never gate readiness.
    NextProposalId,
    ProposalIds,
    Proposal,
}

enum Panel {
    Oracle {
        asset: &'static str,
        key: &'static str,
    },
    OracleMissing {
        asset: &'static str,
    },
    Pool(&'static str),
    Reserves(&'static str),
    Positions(&'static str),
    Backstop(&'static str),
    BackstopBalance(&'static str),
    Balance {
        token: &'static str,
        holder: &'static str,
    },
    Governance(&'static str),
    Proposal(u64),
}

enum ViewOutcome {
    Guarded(&'static str),
    Raw(Value),
}

fn read_request<'a>(
    panel: Panel,
    target: &'a ContractId,
    target_state: Option<&'a TargetState>,
    function: &'static str,
    args: Vec<(&'static str, String)>,
    kind: ReadKind<'a>,
) -> StatusRead<'a> {
    StatusRead::Observe(Observe {
        panel,
        target,
        target_state,
        function,
        args,
        kind,
    })
}

struct Panels {
    oracle: Value,
    pool: Value,
    reserves: Value,
    positions: Value,
    backstop: Value,
    backstop_user_balances: Value,
    balances: Value,
    governance: Value,
}

fn place(panels: &mut Panels, panel: &Panel, field: Value) {
    match panel {
        Panel::Oracle { asset, key } => panels.oracle[asset][key] = field,
        Panel::OracleMissing { asset } => panels.oracle[asset] = field,
        Panel::Pool(key) => panels.pool[key] = field,
        Panel::Reserves(asset) => panels.reserves[asset] = field,
        Panel::Positions(label) => panels.positions[label] = field,
        Panel::Backstop(key) => panels.backstop[key] = field,
        Panel::BackstopBalance(label) => panels.backstop_user_balances[label] = field,
        Panel::Balance { token, holder } => panels.balances[token][holder] = field,
        Panel::Governance(key) => panels.governance[key] = field,
        Panel::Proposal(id) => panels.governance["proposals"][id.to_string()] = field,
    }
}

/// Compose a bounded, read-only market dashboard.
///
/// The returned boolean is readiness, not a request status. A dashboard is
/// still returned when a leg is null, unsupported, or failed so operators can
/// see the partial state that prevented a playable claim.
pub fn status<E: Executor + Sync>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
) -> anyhow::Result<(Safe, bool)> {
    let mut reasons: Vec<&'static str> = Vec::new();
    let mut ready = true;

    if st.assert_market_cli_version().is_err() {
        add_reason(&mut reasons, REASON_CLI);
        return Ok((render_status(&reasons, false)?, false));
    }
    let exact_testnet = manifest.network.label.to_string() == MARKET_NETWORK_LABEL
        && manifest.network.passphrase.to_string() == MARKET_TESTNET_PASSPHRASE
        && st.net.passphrase == MARKET_TESTNET_PASSPHRASE;
    if !exact_testnet {
        add_reason(&mut reasons, REASON_NETWORK);
        return Ok((render_status(&reasons, false)?, false));
    }

    let source = manifest.admin.as_str();
    let actor_identities = [
        (Who::Frodo, &manifest.identities),
        (Who::Sam, &manifest.identities),
        (Who::User, &manifest.identities),
    ];
    let mut identity_panel = json!({});
    let mut identity_collapse = false;
    for (who, identities) in actor_identities {
        match identities.get(&who) {
            Some(account) if account == &manifest.admin => {
                identity_collapse = true;
                identity_panel[who.as_str()] = json!({
                    "state": STATE_IDENTITY_COLLAPSE,
                    "address": account.as_str(),
                });
            }
            Some(account) => {
                let address = account.as_str();
                identity_panel[who.as_str()] = json!({
                    "state": STATE_ACCOUNT_BOUND,
                    "address": address,
                });
            }
            None => {
                identity_panel[who.as_str()] = json!({ "state": STATE_MISSING });
                mark_unready(&mut reasons, &mut ready, REASON_IDENTITY);
            }
        }
    }
    for account in manifest.identities.values() {
        if account == &manifest.admin {
            identity_collapse = true;
        }
    }
    let mut bound_seen = BTreeSet::new();
    for account in manifest.identities.values() {
        if !bound_seen.insert(account.as_str()) {
            identity_collapse = true;
        }
    }
    if identity_collapse {
        mark_unready(&mut reasons, &mut ready, REASON_IDENTITY);
    }

    if st.assert_rpc_network(st.market_network()).is_err() {
        add_reason(&mut reasons, REASON_NETWORK);
        return Ok((render_status(&reasons, false)?, false));
    }

    let mut targets: Vec<&ContractId> = vec![
        &manifest.pool,
        &manifest.backstop,
        &manifest.oracle.runtime,
        &manifest.oracle.adapter,
    ];
    targets.extend(manifest.tokens.values());
    // Deduplicate in first-occurrence order, then fan the ABI reads out
    // through the same bounded width the observations use. Assembly below
    // walks this exact order, so interface reasons keep their sequence.
    // Governance is appended for display only and never gates readiness.
    let mut unique_targets: Vec<&ContractId> = Vec::new();
    for target in targets
        .iter()
        .copied()
        .chain(manifest.oracle.governance.iter())
    {
        if !unique_targets.contains(&target) {
            unique_targets.push(target);
        }
    }
    let interface_states = parallel(
        &unique_targets
            .iter()
            .map(|target| move || Ok(fetch_target_state(st, target)))
            .collect::<Vec<_>>(),
    )?;
    let mut target_states: HashMap<&ContractId, TargetState> = HashMap::new();
    for (target, state) in unique_targets.into_iter().zip(interface_states) {
        if state.state != STATE_INTERFACE_AVAILABLE && targets.contains(&target) {
            mark_unready(&mut reasons, &mut ready, REASON_INTERFACE);
        }
        target_states.insert(target, state);
    }

    // One bounded fan-out for every independent no-send observation. Each
    // read keeps the existing per-job retry law inside `Stellar::try_view`;
    // assembly below re-walks the requests in deterministic order, so the
    // panel layout and first-occurrence reason ordering are exactly what
    // the serial collector produced.
    let adapter_state = target_states.get(&manifest.oracle.adapter);
    let runtime_state = target_states.get(&manifest.oracle.runtime);
    let pool_state = target_states.get(&manifest.pool);
    let backstop_state = target_states.get(&manifest.backstop);
    let mut reads: Vec<StatusRead<'_>> = Vec::new();

    for asset in Tok::ORACLE_ORDER {
        match manifest.tokens.get(&asset) {
            Some(token) => {
                let asset_json = json!({"Stellar": token.as_str()}).to_string();
                reads.push(read_request(
                    Panel::Oracle {
                        asset: asset.as_str(),
                        key: "adapter_lastprice",
                    },
                    &manifest.oracle.adapter,
                    adapter_state,
                    FN_LASTPRICE,
                    vec![("asset", asset_json.clone())],
                    ReadKind::AdapterPrice,
                ));
                reads.push(read_request(
                    Panel::Oracle {
                        asset: asset.as_str(),
                        key: "proxy_cache",
                    },
                    &manifest.oracle.runtime,
                    runtime_state,
                    FN_GET_CACHED,
                    vec![("asset", asset_json.clone())],
                    ReadKind::ProxyCache,
                ));
                reads.push(read_request(
                    Panel::Oracle {
                        asset: asset.as_str(),
                        key: "proxy_breaker",
                    },
                    &manifest.oracle.runtime,
                    runtime_state,
                    FN_GET_BREAKER_VIEW,
                    vec![("asset", asset_json)],
                    ReadKind::ProxyBreaker,
                ));
            }
            None => reads.push(StatusRead::Missing {
                panel: Panel::OracleMissing {
                    asset: asset.as_str(),
                },
                reason: REASON_MISSING_ASSET,
            }),
        }
    }

    reads.push(read_request(
        Panel::Pool("config"),
        &manifest.pool,
        pool_state,
        FN_GET_CONFIG,
        vec![],
        ReadKind::PoolConfig {
            adapter: manifest.oracle.adapter.as_str(),
        },
    ));
    reads.push(read_request(
        Panel::Pool("admin"),
        &manifest.pool,
        pool_state,
        FN_GET_ADMIN,
        vec![],
        ReadKind::PoolAdmin {
            admin: manifest.admin.as_str(),
        },
    ));

    for asset in Tok::RESERVES {
        match (manifest.tokens.get(&asset), manifest.reserves.get(&asset)) {
            (Some(token), Some(reserve)) => reads.push(read_request(
                Panel::Reserves(asset.as_str()),
                &manifest.pool,
                pool_state,
                FN_GET_RESERVE,
                vec![("asset", token.as_str().to_owned())],
                ReadKind::Reserve {
                    expected_asset: token.as_str(),
                    index: i128::from(reserve.index),
                    decimals: i128::from(reserve.decimals),
                },
            )),
            _ => reads.push(StatusRead::Missing {
                panel: Panel::Reserves(asset.as_str()),
                reason: REASON_MISSING_ASSET,
            }),
        }
    }

    let mut position_accounts = vec![("admin", manifest.admin.as_str().to_owned())];
    position_accounts.extend(
        manifest
            .identities
            .iter()
            .map(|(who, account)| (who.as_str(), account.as_str().to_owned())),
    );
    position_accounts.push(("pool", manifest.pool.as_str().to_owned()));
    position_accounts.push(("backstop", manifest.backstop.as_str().to_owned()));
    for (label, address) in position_accounts {
        reads.push(read_request(
            Panel::Positions(label),
            &manifest.pool,
            pool_state,
            FN_GET_POSITIONS,
            vec![("address", address)],
            ReadKind::StrictMap {
                reason: REASON_POSITION,
            },
        ));
    }

    reads.push(read_request(
        Panel::Backstop("pool_data"),
        &manifest.backstop,
        backstop_state,
        FN_POOL_DATA,
        vec![("pool", manifest.pool.as_str().to_owned())],
        ReadKind::StrictMap {
            reason: REASON_BACKSTOP,
        },
    ));
    let mut backstop_balance_accounts = vec![("admin", manifest.admin.as_str().to_owned())];
    backstop_balance_accounts.extend(
        manifest
            .identities
            .iter()
            .map(|(who, account)| (who.as_str(), account.as_str().to_owned())),
    );
    for (label, user) in backstop_balance_accounts {
        reads.push(read_request(
            Panel::BackstopBalance(label),
            &manifest.backstop,
            backstop_state,
            FN_USER_BALANCE,
            vec![("pool", manifest.pool.as_str().to_owned()), ("user", user)],
            ReadKind::StrictMap {
                reason: REASON_BACKSTOP,
            },
        ));
    }
    reads.push(read_request(
        Panel::Backstop("reward_zone"),
        &manifest.backstop,
        backstop_state,
        FN_REWARD_ZONE,
        vec![],
        ReadKind::RewardZone,
    ));

    let balance_accounts = vec![
        ("pool", manifest.pool.as_str().to_owned()),
        ("backstop", manifest.backstop.as_str().to_owned()),
        ("admin", manifest.admin.as_str().to_owned()),
    ];
    for token in Tok::MOCK {
        match manifest.tokens.get(&token) {
            Some(contract) => {
                let token_state = target_states.get(contract);
                for (label, holder) in &balance_accounts {
                    reads.push(read_request(
                        Panel::Balance {
                            token: token.as_str(),
                            holder: label,
                        },
                        contract,
                        token_state,
                        FN_BALANCE,
                        vec![("id", holder.clone())],
                        ReadKind::Numeric,
                    ));
                }
            }
            None => {
                for (label, _) in &balance_accounts {
                    reads.push(StatusRead::Missing {
                        panel: Panel::Balance {
                            token: token.as_str(),
                            holder: label,
                        },
                        reason: REASON_BALANCE,
                    });
                }
            }
        }
    }

    if let Some(governance) = &manifest.oracle.governance {
        let governance_state = target_states.get(governance);
        reads.push(read_request(
            Panel::Governance("next_proposal_id"),
            governance,
            governance_state,
            FN_NEXT_PROPOSAL_ID,
            vec![],
            ReadKind::NextProposalId,
        ));
        reads.push(read_request(
            Panel::Governance("active_ids"),
            governance,
            governance_state,
            FN_ACTIVE_IDS,
            vec![],
            ReadKind::ProposalIds,
        ));
    }

    let pending = reads.iter().filter_map(|read| match read {
        StatusRead::Observe(observe) => Some(observe),
        StatusRead::Missing { .. } => None,
    });
    let mut outcomes = parallel(
        &pending
            .map(|observe| move || Ok(run_status_read(st, source, observe)))
            .collect::<Vec<_>>(),
    )?
    .into_iter();

    let mut panels = Panels {
        oracle: json!({}),
        pool: json!({}),
        reserves: json!({}),
        positions: json!({}),
        backstop: json!({}),
        backstop_user_balances: json!({}),
        balances: json!({}),
        governance: json!({}),
    };
    let mut active_ids = None;
    for read in &reads {
        let (panel, field, requirement) = match read {
            StatusRead::Missing { panel, reason } => {
                mark_unready(&mut reasons, &mut ready, reason);
                (panel, state_json(STATE_MISSING), None)
            }
            StatusRead::Observe(observe) => {
                let outcome = outcomes
                    .next()
                    .expect("parallel returns one outcome per pending read");
                if let (ReadKind::ProposalIds, ViewOutcome::Raw(value)) = (&observe.kind, &outcome)
                {
                    active_ids = proposal_ids(value);
                }
                let (field, requirement) = classify_read(observe, outcome);
                (&observe.panel, field, requirement)
            }
        };
        if let Some((expected, reason)) = requirement {
            if field.get("state").and_then(Value::as_str) != Some(expected) {
                mark_unready(&mut reasons, &mut ready, reason);
            }
        }
        place(&mut panels, panel, field);
    }
    debug_assert!(
        outcomes.next().is_none(),
        "every fan-out result was consumed"
    );

    if manifest.oracle.governance.is_none() {
        // Display-only: an unbound governance contract is reported unbound,
        // never a fabricated pass and never a readiness finding.
        panels.governance = json!({ "state": STATE_GOVERNANCE_UNBOUND });
    }

    // Proposal bodies depend on the live id list, so they are one more
    // bounded fan-out. An unreadable list leaves `proposals` absent rather
    // than claiming an empty queue.
    if let (Some(governance), Some(mut ids)) = (&manifest.oracle.governance, active_ids) {
        ids.sort_unstable();
        panels.governance["proposals"] = json!({});
        if ids.len() > MAX_SHOWN_PROPOSALS {
            panels.governance["proposals_shown"] = json!(MAX_SHOWN_PROPOSALS);
        }
        let proposal_reads: Vec<Observe<'_>> = ids[ids.len().saturating_sub(MAX_SHOWN_PROPOSALS)..]
            .iter()
            .map(|id| Observe {
                panel: Panel::Proposal(*id),
                target: governance,
                target_state: target_states.get(governance),
                function: FN_GET_PROPOSAL,
                args: vec![("id", id.to_string())],
                kind: ReadKind::Proposal,
            })
            .collect();
        let outcomes = parallel(
            &proposal_reads
                .iter()
                .map(|observe| move || Ok(run_status_read(st, source, observe)))
                .collect::<Vec<_>>(),
        )?;
        for (observe, outcome) in proposal_reads.iter().zip(outcomes) {
            let asset = match &outcome {
                ViewOutcome::Raw(value) => proposal_asset(value, manifest),
                ViewOutcome::Guarded(_) => None,
            };
            let (mut field, _) = classify_read(observe, outcome);
            if let Some(asset) = asset {
                field["asset"] = json!(asset);
            }
            place(&mut panels, &observe.panel, field);
        }
    }
    panels.pool["reserves"] = std::mem::take(&mut panels.reserves);
    panels.pool["positions"] = std::mem::take(&mut panels.positions);
    panels.backstop["user_balances"] = std::mem::take(&mut panels.backstop_user_balances);

    let journal = journal_status(out);
    let journal_state = journal.get("state").and_then(Value::as_str);
    if journal_state != Some(STATE_PENDING_CLEAR) {
        mark_unready(&mut reasons, &mut ready, REASON_JOURNAL);
    }
    if journal_state == Some(STATE_PENDING_FULL) {
        mark_unready(&mut reasons, &mut ready, REASON_JOURNAL_FULL);
    }
    if journal_state == Some(STATE_PENDING_TRUNCATED) {
        mark_unready(&mut reasons, &mut ready, REASON_JOURNAL_TOO_LARGE);
    }

    let dashboard = json!({
        "schema_version": STATUS_SCHEMA_VERSION,
        "readiness": if ready { "ready" } else { "not-ready" },
        "ready": ready,
        "reasons": reasons,
        "identity": {
            "state": if identity_collapse {
                STATE_IDENTITY_COLLAPSE
            } else {
                STATE_ACCOUNT_BOUND
            },
            "admin_actor_collapse": identity_collapse,
            "actors": identity_panel,
        },
        "oracle": panels.oracle,
        "pool": panels.pool,
        "backstop": panels.backstop,
        "token_balances": panels.balances,
        "governance": panels.governance,
        "journal": journal,
        "observed_data": "bounded safe render",
        "mutation": "none"
    });
    Ok((
        Safe::render_market_json(&dashboard, STATUS_RENDER_CAP)?,
        ready,
    ))
}

const REASON_MISSING_ASSET: &str = "a required market asset is missing from the manifest";

fn fetch_target_state<E: Executor>(st: &Stellar<'_, E>, target: &ContractId) -> TargetState {
    match st.contract_interface(target) {
        Err(_) => unavailable_target_state(),
        Ok(spec) => match parse_interface_functions(&spec) {
            Some(functions) => TargetState {
                state: STATE_INTERFACE_AVAILABLE,
                functions,
            },
            None => TargetState {
                state: STATE_INTERFACE_MALFORMED,
                functions: BTreeMap::new(),
            },
        },
    }
}

fn unavailable_target_state() -> TargetState {
    TargetState {
        state: STATE_INTERFACE_UNAVAILABLE,
        functions: BTreeMap::new(),
    }
}

fn parse_interface_functions(spec: &Value) -> Option<BTreeMap<String, Vec<String>>> {
    let entries = spec.as_array()?;
    let mut functions = BTreeMap::new();
    for entry in entries {
        let entry = entry.as_object()?;
        if entry.len() != 1 {
            return None;
        }
        let (tag, payload) = entry.iter().next()?;
        if !SPEC_ENTRY_TAGS.contains(&tag.as_str()) || !payload.is_object() {
            return None;
        }
        if tag != "function_v0" {
            continue;
        }
        let name = valid_name(payload.get("name"))?;
        let input_entries = payload.get("inputs")?.as_array()?;
        let mut seen = BTreeSet::new();
        let mut inputs = Vec::with_capacity(input_entries.len());
        for input in input_entries {
            let input = input.as_object()?;
            let input_name = valid_name(input.get("name"))?;
            if !seen.insert(input_name) {
                return None;
            }
            inputs.push(input_name.to_owned());
        }
        if functions.insert(name.to_owned(), inputs).is_some() {
            return None;
        }
    }
    Some(functions)
}

fn valid_name(value: Option<&Value>) -> Option<&str> {
    value?
        .as_str()
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
}

fn run_status_read<E: Executor>(
    st: &Stellar<'_, E>,
    source: &str,
    read: &Observe<'_>,
) -> ViewOutcome {
    let Some(target_state) = read.target_state else {
        return ViewOutcome::Guarded(STATE_INTERFACE_UNAVAILABLE);
    };
    if target_state.state != STATE_INTERFACE_AVAILABLE {
        return ViewOutcome::Guarded(target_state.state);
    }
    let Some(inputs) = target_state.functions.get(read.function) else {
        return ViewOutcome::Guarded(STATE_UNSUPPORTED);
    };
    if inputs.len() != read.args.len()
        || inputs
            .iter()
            .zip(&read.args)
            .any(|(actual, (name, _))| actual.as_str() != *name)
    {
        return ViewOutcome::Guarded(STATE_INTERFACE_MISMATCH);
    }
    match st.try_view(read.target.as_str(), source, read.function, &read.args) {
        Ok(invocation) if invocation.ok => {
            ViewOutcome::Raw(invocation.value.unwrap_or(Value::Null))
        }
        _ => ViewOutcome::Guarded(STATE_READ_FAILED),
    }
}

/// Classify one raw observation exactly as the serial reader did: failed,
/// null, malformed, and observed stay distinct, and a failed bounded render
/// never launders the value into data. The requirement is the state a
/// readiness-gating leg must reach and the reason it closes otherwise.
fn classify_read(
    read: &Observe<'_>,
    outcome: ViewOutcome,
) -> (Value, Option<(&'static str, &'static str)>) {
    let requirement = requirement(&read.kind);
    match outcome {
        ViewOutcome::Guarded(state) => (state_json(state), requirement),
        ViewOutcome::Raw(value) => {
            let state = match &read.kind {
                ReadKind::AdapterPrice => classify_adapter_price(&value),
                ReadKind::ProxyCache => classify_cache_status(&value),
                ReadKind::ProxyBreaker => classify_breaker_status(&value),
                ReadKind::PoolConfig { adapter } => classify_pool_config(&value, adapter),
                ReadKind::PoolAdmin { admin } => classify_address(&value, admin),
                ReadKind::Reserve {
                    expected_asset,
                    index,
                    decimals,
                } => classify_reserve(&value, expected_asset, *index, *decimals),
                ReadKind::StrictMap { .. } => classify_strict_map(&value),
                ReadKind::RewardZone => classify_reward_zone(&value),
                ReadKind::Numeric | ReadKind::NextProposalId => classify_numeric(&value),
                ReadKind::ProposalIds => classify_proposal_ids(&value),
                ReadKind::Proposal => classify_proposal(&value),
            };
            let (state, observed) = if state == STATE_NULL || state == STATE_PRICE_NULL {
                (state, Value::Null)
            } else {
                match safe_observed(&value) {
                    Ok(observed) => (state, json!(observed.to_string())),
                    Err(_) => (STATE_READ_FAILED, json!({ "state": STATE_READ_FAILED })),
                }
            };
            (json!({ "state": state, "observed": observed }), requirement)
        }
    }
}

fn requirement(kind: &ReadKind<'_>) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        ReadKind::AdapterPrice => (STATE_PRICE_AVAILABLE, REASON_PRICE),
        ReadKind::ProxyCache => (STATE_CACHE_ACCEPTED, REASON_CACHE),
        ReadKind::ProxyBreaker => (STATE_BREAKER_CLEAR, REASON_BREAKER),
        ReadKind::PoolConfig { .. } => (STATE_POOL_ACTIVE, REASON_POOL),
        ReadKind::PoolAdmin { .. } => (STATE_ADMIN_MATCH, REASON_ADMIN),
        ReadKind::Reserve { .. } => (STATE_RESERVE_ENABLED, REASON_RESERVE),
        ReadKind::StrictMap { reason } => (STATE_OBSERVED, *reason),
        ReadKind::RewardZone => (STATE_OBSERVED, REASON_BACKSTOP),
        ReadKind::Numeric => (STATE_OBSERVED, REASON_BALANCE),
        ReadKind::NextProposalId | ReadKind::ProposalIds | ReadKind::Proposal => return None,
    })
}

fn proposal_ids(value: &Value) -> Option<Vec<u64>> {
    value.as_array()?.iter().map(Value::as_u64).collect()
}

fn classify_proposal_ids(value: &Value) -> &'static str {
    if value.is_null() {
        STATE_NULL
    } else if value
        .as_array()
        .is_some_and(|ids| ids.iter().all(|id| id.as_u64().is_some()))
    {
        STATE_OBSERVED
    } else {
        STATE_MALFORMED
    }
}

/// A proposal executed or cancelled between the id list and this read
/// returns null, which stays visible instead of being dropped.
fn classify_proposal(value: &Value) -> &'static str {
    if value.is_null() {
        STATE_NULL
    } else if ["operation", "created_at_ns", "ttl_ns", "created_by"]
        .iter()
        .all(|key| value.get(key).is_some())
    {
        STATE_OBSERVED
    } else {
        STATE_MALFORMED
    }
}

/// Name the manifest token a proposal's operation targets, so a trip on the
/// wrong asset is visible without decoding contract ids by hand.
fn proposal_asset(proposal: &Value, manifest: &MarketManifest) -> Option<&'static str> {
    fn stellar(value: &Value) -> Option<&str> {
        match value {
            Value::Object(object) => object
                .get("Stellar")
                .and_then(Value::as_str)
                .or_else(|| object.values().find_map(stellar)),
            Value::Array(items) => items.iter().find_map(stellar),
            _ => None,
        }
    }
    let contract = stellar(proposal.get("operation")?)?;
    manifest
        .tokens
        .iter()
        .find(|(_, id)| id.as_str() == contract)
        .map(|(token, _)| token.as_str())
}

fn classify_adapter_price(value: &Value) -> &'static str {
    if value.is_null() {
        STATE_PRICE_NULL
    } else if has_number(value, "price") && has_number(value, "timestamp") {
        STATE_PRICE_AVAILABLE
    } else {
        STATE_PRICE_MALFORMED
    }
}

fn classify_cache_status(value: &Value) -> &'static str {
    let status = cache_status_tag(Some(value)).or_else(|| {
        value
            .get("status")
            .and_then(|inner| cache_status_tag(Some(inner)))
    });
    match status {
        Some("Accepted") => STATE_CACHE_ACCEPTED,
        Some("Blocked") => STATE_CACHE_BLOCKED,
        Some("ResolveFailed") => STATE_CACHE_FAILED,
        Some(_) => STATE_MALFORMED,
        None if value.is_null() => STATE_NULL,
        None => STATE_MALFORMED,
    }
}

fn cache_status_tag(value: Option<&Value>) -> Option<&'static str> {
    match value? {
        Value::String(status) => match status.as_str() {
            "Accepted" => Some("Accepted"),
            "Blocked" => Some("Blocked"),
            "ResolveFailed" => Some("ResolveFailed"),
            _ => None,
        },
        Value::Object(object) if object.len() == 1 => match object.keys().next()?.as_str() {
            "Accepted" => Some("Accepted"),
            "Blocked" => Some("Blocked"),
            "ResolveFailed" => Some("ResolveFailed"),
            _ => None,
        },
        _ => None,
    }
}

fn classify_breaker_status(value: &Value) -> &'static str {
    if value.is_null() {
        return STATE_NULL;
    }
    let manual_trip = value.get("is_manually_tripped").and_then(Value::as_bool);
    let blocking = value.get("is_blocking").and_then(Value::as_bool);
    if manual_trip == Some(true) {
        STATE_BREAKER_TRIPPED
    } else if blocking == Some(true) {
        STATE_BREAKER_BLOCKING
    } else if manual_trip == Some(false) && blocking == Some(false) {
        STATE_BREAKER_CLEAR
    } else {
        STATE_MALFORMED
    }
}

fn classify_pool_config(value: &Value, adapter: &str) -> &'static str {
    if value.is_null() {
        return STATE_NULL;
    }
    let status = value.get("status").and_then(try_num);
    let oracle_matches = value.get("oracle").and_then(Value::as_str) == Some(adapter);
    if matches!(status, Some(0) | Some(1)) {
        if oracle_matches {
            STATE_POOL_ACTIVE
        } else {
            STATE_ORACLE_MISMATCH
        }
    } else if matches!(status, Some(2) | Some(3) | Some(4) | Some(5) | Some(6)) {
        STATE_POOL_INACTIVE
    } else {
        STATE_MALFORMED
    }
}

fn classify_address(value: &Value, expected: &str) -> &'static str {
    if value.is_null() {
        STATE_NULL
    } else if value.as_str() == Some(expected) {
        STATE_ADMIN_MATCH
    } else {
        STATE_ADDRESS_MISMATCH
    }
}

fn classify_reserve(
    value: &Value,
    expected_asset: &str,
    expected_index: i128,
    expected_decimals: i128,
) -> &'static str {
    if value.is_null() {
        return STATE_NULL;
    }
    let asset_matches = value.get("asset").and_then(Value::as_str) == Some(expected_asset);
    let config = value.get("config");
    let config_matches = config.is_some_and(|config| {
        config.get("index").and_then(try_num) == Some(expected_index)
            && config.get("decimals").and_then(try_num) == Some(expected_decimals)
    });
    if !asset_matches || !config_matches {
        return STATE_MALFORMED;
    }
    let enabled = config.and_then(|config| config.get("enabled"));
    if enabled != Some(&Value::Bool(true)) {
        return STATE_RESERVE_DISABLED;
    }
    let fields = value.get("data");
    let fields_ok = fields.is_some_and(|fields| {
        [
            "d_rate",
            "b_rate",
            "ir_mod",
            "b_supply",
            "d_supply",
            "backstop_credit",
            "last_time",
        ]
        .iter()
        .all(|field| has_number(fields, field))
    });
    if fields_ok {
        STATE_RESERVE_ENABLED
    } else {
        STATE_MALFORMED
    }
}

fn classify_reward_zone(value: &Value) -> &'static str {
    if value.is_null() {
        STATE_NULL
    } else if value.is_array() {
        STATE_OBSERVED
    } else {
        STATE_MALFORMED
    }
}

fn classify_strict_map(value: &Value) -> &'static str {
    if value.is_null() {
        STATE_NULL
    } else if strict_map(value).is_some() {
        STATE_OBSERVED
    } else {
        STATE_MALFORMED
    }
}

fn classify_numeric(value: &Value) -> &'static str {
    if value.is_null() {
        STATE_NULL
    } else if try_num(value).is_some() {
        STATE_OBSERVED
    } else {
        STATE_MALFORMED
    }
}

fn safe_observed(value: &Value) -> anyhow::Result<Safe> {
    Safe::render_market_json(value, STATUS_OBSERVED_CAP)
}

fn state_json(state: &'static str) -> Value {
    json!({ "state": state })
}

fn has_number(value: &Value, field: &str) -> bool {
    value.get(field).and_then(try_num).is_some()
}

/// Reuse the status pane's pinned, bounded custody scan as the write gate.
/// Missing, tampered, truncated, unresolved, or capacity-full journals are
/// never clear: at the scan limit the next reservation could create a journal
/// entry no future status pass can inspect, so admission closes before it.
pub fn journal_clear_for_action(out: &Path) -> bool {
    journal_status(out).get("state").and_then(Value::as_str) == Some(STATE_PENDING_CLEAR)
}

pub(crate) fn journal_status(out: &Path) -> Value {
    let root = out.join("market-pending");
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return pending_unavailable(),
        Err(error) if error.kind() == ErrorKind::NotFound => return pending_unavailable(),
        Err(_) => return pending_unavailable(),
    }
    let Ok(root_dir) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(&root)
    else {
        return pending_unavailable();
    };
    let Ok(dir_type) = root_dir.metadata().map(|metadata| metadata.file_type()) else {
        return pending_unavailable();
    };
    if !dir_type.is_dir() || dir_type.is_symlink() {
        return pending_unavailable();
    }
    let Ok(mut entries) = std::fs::read_dir(pinned_raw_fd_path(root_dir.as_raw_fd())) else {
        return pending_unavailable();
    };

    let mut inspected = 0;
    let mut unresolved = 0;
    while inspected < PENDING_SCAN_LIMIT {
        match entries.next() {
            Some(Err(_)) => return pending_unavailable(),
            Some(Ok(entry)) => {
                inspected += 1;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Ok(entry_type) = entry.file_type() else {
                    return pending_unavailable();
                };
                if entry_type.is_symlink() || !entry_type.is_dir() {
                    unresolved += 1;
                    continue;
                }
                let Some(coordinate) = market_coordinate(&name) else {
                    unresolved += 1;
                    continue;
                };
                let Ok(entry_dir) = pinned_child(root_dir.as_fd(), &name) else {
                    return pending_unavailable();
                };
                if !entry_dir_is_settled(&entry_dir, coordinate) {
                    unresolved += 1;
                }
            }
            None => return pending_summary(unresolved, false, false),
        }
    }

    match entries.next() {
        Some(Err(_)) => pending_unavailable(),
        Some(Ok(_)) => pending_summary(unresolved, true, true),
        None => pending_summary(unresolved, false, true),
    }
}

pub(crate) fn market_coordinate(name: &str) -> Option<(u32, u32)> {
    let bytes = name.as_bytes();
    if bytes.len() != 6
        || !bytes[..3].iter().all(u8::is_ascii_digit)
        || bytes[3] != b'-'
        || !bytes[4..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let phase = name[..3].parse::<u32>().ok()?;
    let slot = name[4..].parse::<u32>().ok()?;
    if format!("{phase:03}-{slot:02}") != name {
        return None;
    }
    Some((phase, slot))
}

fn pinned_child(basis: std::os::fd::BorrowedFd<'_>, name: &str) -> anyhow::Result<std::fs::File> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        anyhow::bail!("pinned journal name is not a path segment");
    }
    let path = format!("/proc/self/fd/{}/{}", basis.as_raw_fd(), name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!("pinned journal coordinate is not a directory");
    }
    Ok(file)
}

/// Read one coordinate beneath pinned run and journal directories. An absent
/// root or coordinate is unreserved. An existing coordinate must have a valid
/// intent, even when its result has not yet been written.
pub(crate) fn read_journal_coordinate(
    out: &Path,
    phase: u32,
    slot: u32,
) -> anyhow::Result<Option<(MarketCustodyRecord, Option<MarketPendingResult>)>> {
    let read = || -> anyhow::Result<_> {
        let run = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_DIRECTORY)
            .open(out)
        {
            Ok(run) => run,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let root = match pinned_child(run.as_fd(), "market-pending") {
            Ok(root) => root,
            Err(error) if record_not_found(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let name = format!("{phase:03}-{slot:02}");
        anyhow::ensure!(
            market_coordinate(&name) == Some((phase, slot)),
            "invalid coordinate"
        );
        let dir = match pinned_child(root.as_fd(), &name) {
            Ok(dir) => dir,
            Err(error) if record_not_found(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(
            read_coordinate_records(&dir, (phase, slot))?
                .ok_or_else(|| anyhow::anyhow!("market journal coordinate is empty"))?,
        ))
    };
    read().map_err(|_| anyhow::anyhow!("market journal custody is unreadable or inconsistent"))
}

fn record_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == ErrorKind::NotFound)
}

fn read_coordinate_records(
    dir: &std::fs::File,
    coordinate: (u32, u32),
) -> anyhow::Result<Option<(MarketCustodyRecord, Option<MarketPendingResult>)>> {
    let intent_bytes = match read_pinned_record(dir, "intent.json") {
        Ok(bytes) => bytes,
        Err(error) if record_not_found(&error) => {
            // A settlement without its pre-send intent is never evidence of
            // completion; only a coordinate holding neither record is open.
            match read_pinned_record(dir, "result.json") {
                Ok(_) => anyhow::bail!("a settlement exists without its pre-send intent"),
                Err(error) if record_not_found(&error) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    let intent: MarketCustodyRecord = serde_json::from_slice(&intent_bytes)?;
    anyhow::ensure!(
        intent.phase == coordinate.0 && custody_payload_digest(&intent)? == intent.payload_digest,
        "invalid journal intent"
    );
    let bytes = match read_pinned_record(dir, "result.json") {
        Ok(bytes) => bytes,
        Err(error) if record_not_found(&error) => return Ok(Some((intent, None))),
        Err(error) => return Err(error),
    };
    let result: MarketPendingResult = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        verify_settled_custody(coordinate, &intent, &result),
        "inconsistent journal settlement"
    );
    Ok(Some((intent, Some(result))))
}

fn entry_dir_is_settled(dir: &std::fs::File, coordinate: (u32, u32)) -> bool {
    matches!(
        read_coordinate_records(dir, coordinate),
        Ok(Some((_, Some(_))))
    )
}

pub(crate) fn verify_settled_custody(
    coordinate: (u32, u32),
    intent: &MarketCustodyRecord,
    result: &MarketPendingResult,
) -> bool {
    if intent.phase != coordinate.0
        || result.custody.phase != coordinate.0
        || result.step.phase != coordinate.0
    {
        return false;
    }
    let custody_matches = match (
        serde_json::to_value(intent),
        serde_json::to_value(&result.custody),
    ) {
        (Ok(intent), Ok(custody)) => intent == custody,
        _ => false,
    };
    // Both frozen IDs are authoritative if present: a readback must not
    // satisfy `target` while contradicting a separately frozen prediction.
    // A targetless local operation may settle only if no target was frozen.
    let target_law = match (&intent.target, &result.step.target) {
        (Some(expected), Some(claimed)) => {
            expected == claimed
                && intent
                    .predicted
                    .as_ref()
                    .is_none_or(|predicted| predicted == claimed)
        }
        (Some(_), None) => false,
        (None, Some(claimed)) => intent
            .predicted
            .as_ref()
            .is_some_and(|predicted| predicted == claimed),
        (None, None) => true,
    };
    if !custody_matches
        || result.step.action.to_string() != result.custody.action.to_string()
        || !target_law
    {
        return false;
    }
    custody_payload_digest(&result.custody).ok().as_deref()
        == Some(result.custody.payload_digest.as_str())
}

fn read_pinned_record(dir: &std::fs::File, name: &str) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK | O_NOFOLLOW)
        .open(format!(
            "/proc/self/fd/{}/{}",
            dir.as_fd().as_raw_fd(),
            name
        ))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        anyhow::bail!("pinned journal record is not a regular file");
    }
    let expected = metadata.len();
    if expected > PENDING_RECORD_CAP as u64 {
        anyhow::bail!("pinned journal record exceeds its read cap");
    }
    let mut bytes = Vec::new();
    let mut reader = file.take((PENDING_RECORD_CAP + 1) as u64);
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() as u64 > expected || bytes.len() > PENDING_RECORD_CAP {
            anyhow::bail!("pinned journal record exceeds its read cap");
        }
    }
    if bytes.len() as u64 != expected {
        anyhow::bail!("pinned journal record changed while being read");
    }
    Ok(bytes)
}

fn pinned_raw_fd_path(fd: std::os::unix::io::RawFd) -> String {
    format!("/proc/self/fd/{fd}")
}

fn pending_summary(unresolved: usize, truncated: bool, capacity_full: bool) -> Value {
    let state = if unresolved > 0 {
        STATE_PENDING_UNRESOLVED
    } else if truncated {
        STATE_PENDING_TRUNCATED
    } else if capacity_full {
        STATE_PENDING_FULL
    } else {
        STATE_PENDING_CLEAR
    };
    json!({
        "state": state,
        "pending_count": unresolved,
        "truncated": truncated,
        "capacity_full": capacity_full,
        "scan_limit": PENDING_SCAN_LIMIT
    })
}

fn pending_unavailable() -> Value {
    json!({
        "state": STATE_PENDING_UNAVAILABLE,
        "pending_count": null,
        "truncated": null
    })
}

fn mark_unready(reasons: &mut Vec<&'static str>, ready: &mut bool, reason: &'static str) {
    *ready = false;
    add_reason(reasons, reason);
}

fn add_reason(reasons: &mut Vec<&'static str>, reason: &'static str) {
    if reasons.contains(&reason) {
        return;
    }
    if reasons.len() >= MAX_REASON_COUNT {
        if !reasons.contains(&REASON_SUPPRESSED) {
            reasons.push(REASON_SUPPRESSED);
        }
        return;
    }
    reasons.push(reason);
}

fn render_status(reasons: &[&'static str], ready: bool) -> anyhow::Result<Safe> {
    Safe::render_market_json(
        &json!({
            "schema_version": STATUS_SCHEMA_VERSION,
            "readiness": if ready { "ready" } else { "not-ready" },
            "ready": ready,
            "reasons": reasons,
            "mutation": "none"
        }),
        STATUS_RENDER_CAP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MARKET_TESTNET_PASSPHRASE;
    use crate::manifest::{
        begin_market_pending, finish_market_pending, MarketMutationKind, MarketNetwork,
        MarketStepRecord, Sha256Hex,
    };
    use crate::stellar::{fake::FakeExecutor, CmdOutput, Executor, Net, Policy};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    fn plant_fifo(path: &Path) {
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", "mkfifo \"$1\"", "mkfifo-test"])
            .arg(path)
            .status()
            .expect("mkfifo runner");
        assert!(status.success(), "planted fifo: {}", path.display());
    }
    const V26: &str = "stellar 26.1.0 (1228cff)";
    const C_ALL: &str = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT";
    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const G_FRODO: &str = "GAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHV4";
    const G_SAM: &str = "GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX";
    const G_USER: &str = "GABQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABEQO";
    const SPEC: &str = r#"[
      {"function_v0":{"name":"lastprice","inputs":[{"name":"asset"}]}},
      {"function_v0":{"name":"get_cached","inputs":[{"name":"asset"}]}},
      {"function_v0":{"name":"get_breaker_set_view","inputs":[{"name":"asset"}]}},
      {"function_v0":{"name":"get_config","inputs":[]}},
      {"function_v0":{"name":"get_admin","inputs":[]}},
      {"function_v0":{"name":"get_reserve","inputs":[{"name":"asset"}]}},
      {"function_v0":{"name":"get_positions","inputs":[{"name":"address"}]}},
      {"function_v0":{"name":"pool_data","inputs":[{"name":"pool"}]}},
      {"function_v0":{"name":"user_balance","inputs":[{"name":"pool"},{"name":"user"}]}},
      {"function_v0":{"name":"reward_zone","inputs":[]}},
      {"function_v0":{"name":"balance","inputs":[{"name":"id"}]}},
      {"function_v0":{"name":"next_proposal_id","inputs":[]}},
      {"function_v0":{"name":"active_ids","inputs":[]}},
      {"function_v0":{"name":"get_proposal","inputs":[{"name":"id"}]}}
    ]"#;

    fn scratch(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("abtest-market-status-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        path
    }

    fn net() -> Net {
        Net {
            rpc_url: "https://rpc.testnet.local".to_owned(),
            passphrase: MARKET_TESTNET_PASSPHRASE.to_owned(),
            config_dir: scratch("config").join("stellar"),
            headers: Vec::new(),
        }
    }

    fn manifest() -> MarketManifest {
        serde_json::from_value(json!({
            "network": {"label": "testnet", "passphrase": MARKET_TESTNET_PASSPHRASE},
            "admin": G_ADMIN,
            "rpc_host": "rpc.testnet.local",
            "oracle": {
                "runtime": C_ALL,
                "governance": C_ALL,
                "adapter": C_ALL,
                "sources": [C_ALL, C_ALL, C_ALL],
                "min_sources": 3,
                "max_age_secs": 3600,
                "max_clock_drift_secs": 60,
                "decimals": 7,
                "resolution": 300,
                "base": "USD"
            },
            "tokens": {
                "blnd": C_ALL,
                "usdc": C_ALL,
                "weth": C_ALL,
                "xlm": C_ALL,
                "stable": C_ALL
            },
            "pool": C_ALL,
            "backstop": C_ALL,
            "factory": C_ALL,
            "comet": C_ALL,
            "emitter": C_ALL,
            "reserves": {
                "stable": {"index": 0, "decimals": 6},
                "xlm": {"index": 1, "decimals": 7},
                "weth": {"index": 2, "decimals": 9}
            },
            "identities": {
                "frodo": G_FRODO,
                "sam": G_SAM,
                "user": G_USER
            },
            "deploy_ledger": 1
        }))
        .expect("fixture market manifest")
    }

    fn no_sleep(_: Duration) {}

    /// Thread-safe argv-dispatched seam for the parallel collector: the FIFO
    /// `RefCell` fixture cannot answer calls from several workers, while this
    /// one answers by call shape and still records argv for the no-send and
    /// manifest-source laws.
    struct StatusExecutor {
        network_passphrase: Option<&'static str>,
        views: BTreeMap<&'static str, &'static str>,
        delayed_view: Option<(&'static str, Duration)>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl StatusExecutor {
        fn ready(views: Vec<(&'static str, &'static str)>) -> Self {
            Self {
                network_passphrase: Some(MARKET_TESTNET_PASSPHRASE),
                views: views.into_iter().collect(),
                delayed_view: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl Executor for StatusExecutor {
        fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(args.to_vec());
            match (
                args.first().map(String::as_str),
                args.get(1).map(String::as_str),
            ) {
                (Some("--version"), _) => Ok(FakeExecutor::ok(V26, "")),
                (Some("network"), _) => Ok(match self.network_passphrase {
                    Some(passphrase) => {
                        FakeExecutor::ok(&json!({"passphrase": passphrase}).to_string(), "")
                    }
                    None => FakeExecutor::fail("connection refused"),
                }),
                (Some("contract"), Some("info")) => Ok(FakeExecutor::ok(SPEC, "")),
                (Some("contract"), Some("invoke")) => {
                    let function = args
                        .windows(2)
                        .find(|window| window[0] == "--")
                        .map(|window| window[1].clone())
                        .unwrap_or_default();
                    if let Some((delayed, delay)) = self.delayed_view {
                        if delayed == function {
                            std::thread::sleep(delay);
                        }
                    }
                    match self.views.get(function.as_str()) {
                        Some(stdout) => Ok(FakeExecutor::ok(stdout, "")),
                        None => Err(anyhow::anyhow!(
                            "status fixture: unscripted view {function}"
                        )),
                    }
                }
                _ => Err(anyhow::anyhow!("status fixture: unexpected argv {args:?}")),
            }
        }
    }

    #[test]
    fn out_of_order_completion_attributes_each_observation_to_its_own_field() {
        let out = scratch("out-of-order");
        let mut fixture = StatusExecutor::ready(vec![
            ("lastprice", r#"{"price":10000000,"timestamp":123}"#),
            ("get_cached", r#"{"status":"Accepted"}"#),
            (
                "get_breaker_set_view",
                r#"{"is_manually_tripped":false,"is_blocking":false}"#,
            ),
        ]);
        fixture.delayed_view = Some(("lastprice", Duration::from_millis(250)));
        let network = net();
        let mut st = Stellar::new(&network, &fixture, Policy::Market);
        st.sleep = no_sleep;
        let (report, _) = status(&st, &out, &manifest()).expect("status is returned");
        let doc: Value =
            serde_json::from_str(&report.to_string()).expect("rendered status is valid json");
        for asset in Tok::ORACLE_ORDER {
            let asset_panel = &doc["oracle"][asset.as_str()];
            assert_eq!(
                asset_panel["adapter_lastprice"]["state"].as_str(),
                Some(STATE_PRICE_AVAILABLE),
                "the late price must stay attributed to its own field: {report}"
            );
            assert_eq!(
                asset_panel["proxy_cache"]["state"].as_str(),
                Some(STATE_CACHE_ACCEPTED),
                "an early cache read must not absorb the delayed price: {report}"
            );
            assert_eq!(
                asset_panel["proxy_breaker"]["state"].as_str(),
                Some(STATE_BREAKER_CLEAR),
                "an early breaker read must keep its own classification: {report}"
            );
        }
        let observed: Value = serde_json::from_str(
            doc["oracle"]["weth"]["adapter_lastprice"]["observed"]
                .as_str()
                .expect("observed is a bounded string"),
        )
        .expect("observed price is valid json");
        assert_eq!(
            observed["price"].as_u64(),
            Some(10000000),
            "the delayed payload must reach the price field it belongs to"
        );
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn a_failed_observation_stays_unready_without_hiding_its_siblings() {
        let out = scratch("failed-observation");
        // `get_cached` is deliberately unscripted: its job fails closed while
        // sibling reads succeed, and neither outcome may leak into the other.
        let fixture = StatusExecutor::ready(vec![
            ("lastprice", r#"{"price":10000000,"timestamp":123}"#),
            (
                "get_breaker_set_view",
                r#"{"is_manually_tripped":false,"is_blocking":false}"#,
            ),
        ]);
        let network = net();
        let mut st = Stellar::new(&network, &fixture, Policy::Market);
        st.sleep = no_sleep;
        let (report, ready) = status(&st, &out, &manifest()).expect("partial status returned");
        let doc: Value =
            serde_json::from_str(&report.to_string()).expect("rendered status is valid json");
        assert!(!ready, "a failed cache read must close readiness");
        for asset in Tok::ORACLE_ORDER {
            let asset_panel = &doc["oracle"][asset.as_str()];
            assert_eq!(
                asset_panel["adapter_lastprice"]["state"].as_str(),
                Some(STATE_PRICE_AVAILABLE),
                "a failed sibling must not hide the price observation: {report}"
            );
            assert_eq!(
                asset_panel["proxy_breaker"]["state"].as_str(),
                Some(STATE_BREAKER_CLEAR),
                "a failed sibling must not hide the breaker observation: {report}"
            );
            assert_eq!(
                asset_panel["proxy_cache"]["state"].as_str(),
                Some(STATE_READ_FAILED),
                "the failed read must stay present as unready: {report}"
            );
        }
        let reasons: Vec<&str> = doc["reasons"]
            .as_array()
            .expect("reasons array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            reasons.contains(&REASON_CACHE),
            "the failed leg must carry its own reason: {reasons:?}"
        );
        assert!(
            !reasons.contains(&REASON_PRICE) && !reasons.contains(&REASON_BREAKER),
            "successful siblings must not raise findings: {reasons:?}"
        );
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn pending_proposals_are_listed_without_gating_readiness() {
        const PROPOSAL: &str = r#"{"created_at_ns":1,"created_by":"GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF","operation":{"SetManualTrip":[{"Stellar":"CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT"},true,null]},"ttl_ns":0}"#;
        let out = scratch("governance");
        let report = |active_ids: &'static str| {
            let fixture = StatusExecutor::ready(vec![
                ("next_proposal_id", "5"),
                ("active_ids", active_ids),
                ("get_proposal", PROPOSAL),
            ]);
            let network = net();
            let mut st = Stellar::new(&network, &fixture, Policy::Market);
            st.sleep = no_sleep;
            let (report, ready) = status(&st, &out, &manifest()).expect("status is returned");
            let doc: Value =
                serde_json::from_str(&report.to_string()).expect("rendered status is valid json");
            (doc, ready)
        };

        let (listed, listed_ready) = report("[4,2]");
        let governance = &listed["governance"];
        assert_eq!(governance["active_ids"]["state"], STATE_OBSERVED);
        let ids: Vec<&str> = governance["proposals"]
            .as_object()
            .expect("a readable id list yields a proposal map")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(ids, ["2", "4"], "every pending id is read: {governance}");
        for id in ids {
            assert_eq!(governance["proposals"][id]["state"], STATE_OBSERVED);
            assert!(
                governance["proposals"][id]["asset"].is_string(),
                "the targeted manifest token is named: {governance}"
            );
        }

        let (unreadable, unreadable_ready) = report(r#"{"not":"ids"}"#);
        assert_eq!(
            unreadable["governance"]["active_ids"]["state"],
            STATE_MALFORMED
        );
        assert!(
            unreadable["governance"].get("proposals").is_none(),
            "an unreadable id list must not read as an empty queue"
        );
        assert_eq!(
            (listed_ready, &listed["reasons"]),
            (unreadable_ready, &unreadable["reasons"]),
            "governance is display-only and never changes readiness"
        );
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn an_unbound_governance_contract_is_reported_unbound_not_ready() {
        let out = scratch("governance-unbound");
        let network = net();
        let fx = StatusExecutor::ready(vec![]);
        let mut st = Stellar::new(&network, &fx, Policy::Market);
        st.sleep = no_sleep;
        let mut market = manifest();
        market.oracle.governance = None;
        let (report, _) = status(&st, &out, &market).expect("status is returned");
        let doc: Value =
            serde_json::from_str(&report.to_string()).expect("rendered status is valid json");
        assert_eq!(
            doc["governance"]["state"].as_str(),
            Some(STATE_GOVERNANCE_UNBOUND),
            "the dashboard must name the absence, not stay silent"
        );
        assert!(
            doc["governance"].get("active_ids").is_none(),
            "no governance read may be invented against no contract"
        );
        let calls = fx
            .calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            calls
                .iter()
                .flatten()
                .all(|arg| arg != "next_proposal_id" && arg != "active_ids"),
            "an unbound governance contract must not be probed: {calls:?}"
        );
        drop(calls);
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn stale_or_null_price_is_visible_without_mutation_or_readiness() {
        let out = scratch("stale-null-price");
        let net = net();
        let fx = StatusExecutor::ready(vec![("lastprice", "null")]);
        let mut st = Stellar::new(&net, &fx, Policy::Market);
        st.sleep = no_sleep;

        let (report, ready) = status(&st, &out, &manifest()).expect("partial status is returned");
        let rendered = report.to_string();
        assert!(!ready, "null adapter price must not certify readiness");
        assert!(
            rendered.contains(STATE_PRICE_NULL),
            "null adapter price must remain explicit: {rendered}"
        );
        assert!(
            rendered.contains(REASON_PRICE),
            "not-ready reason must remain explicit: {rendered}"
        );

        let calls = fx.calls();
        assert!(
            calls.iter().any(|call| {
                call.first().map(String::as_str) == Some("contract")
                    && call.get(1).map(String::as_str) == Some("invoke")
                    && call
                        .windows(2)
                        .any(|window| window[0] == "--id" && window[1] == C_ALL)
                    && call
                        .windows(2)
                        .any(|window| window[0] == "--" && window[1] == FN_LASTPRICE)
            }),
            "the scripted null response must reach the adapter price getter: {calls:?}"
        );
        for call in calls.iter() {
            if call.first().map(String::as_str) != Some("contract")
                || call.get(1).map(String::as_str) != Some("invoke")
            {
                continue;
            }
            assert!(
                call.windows(2)
                    .any(|window| window[0] == "--send" && window[1] == "no"),
                "every market status read must be a simulation: {call:?}"
            );
            assert!(
                !call
                    .windows(2)
                    .any(|window| window[0] == "--send" && window[1] == "yes"),
                "market status must never send: {call:?}"
            );
            let source = call
                .windows(2)
                .find(|window| window[0] == "--source")
                .map(|window| window[1].as_str());
            assert_eq!(source, Some(G_ADMIN), "source must remain manifest-bound");
        }
        drop(calls);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn nested_cache_status_is_classified_without_losing_null_or_malformed_state() {
        assert_eq!(
            classify_cache_status(&json!({"status": "Blocked"})),
            STATE_CACHE_BLOCKED
        );
        assert_eq!(
            classify_cache_status(&json!({"status": {"Accepted": null}})),
            STATE_CACHE_ACCEPTED
        );
        assert_eq!(
            classify_cache_status(&json!({"status": {"ResolveFailed": null}})),
            STATE_CACHE_FAILED
        );
        assert_eq!(classify_cache_status(&json!(null)), STATE_NULL);
        assert_eq!(
            classify_cache_status(&json!({"status": {"Unknown": null}})),
            STATE_MALFORMED
        );
    }

    fn settle_legitimate_coordinate(out: &Path) -> MarketCustodyRecord {
        let target: ContractId = C_ALL.parse().expect("fixture target");
        let signer: crate::stellar::AccountId = G_ADMIN.parse().expect("fixture signer");
        let mut custody = MarketCustodyRecord {
            phase: 1,
            kind: MarketMutationKind::Deploy,
            action: Safe::literal("deploy-pool"),
            network: MarketNetwork::Testnet,
            signer,
            payload_digest: Sha256Hex::default(),
            target: Some(target),
            artifact_digest: None,
            predicted: None,
            pre_state: Vec::new(),
            tool_error: None,
        };
        custody.payload_digest = custody_payload_digest(&custody).expect("custody digest");
        begin_market_pending(out, custody.phase, 0, &custody).expect("reserve intent");
        finish_market_pending(
            out,
            custody.phase,
            0,
            &MarketPendingResult {
                custody: custody.clone(),
                step: MarketStepRecord {
                    phase: custody.phase,
                    action: custody.action.clone(),
                    command: Safe::literal("test settlement"),
                    outcome: Safe::literal("settled"),
                    target: custody.target.clone(),
                    violations: Vec::new(),
                },
            },
        )
        .expect("write settled result");
        custody
    }

    #[test]
    fn legitimate_settlement_clears_but_a_corrupt_result_does_not() {
        let out = scratch("corrupt-result");
        settle_legitimate_coordinate(&out);
        let settled = journal_status(&out);
        assert_eq!(
            settled.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "legitimate settled custody must report clear: {settled}"
        );

        let root = out.join("market-pending");
        let coordinate = root.join("001-00");
        let result = coordinate.join("result.json");
        std::fs::write(&result, b"{").expect("corrupt settled result");
        let corrupted = journal_status(&out);
        assert_ne!(
            corrupted.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "truncated custody must not clear: {corrupted}"
        );
        assert!(
            corrupted
                .get("pending_count")
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0),
            "a corrupted coordinate must remain a pending question: {corrupted}"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn settled_deployment_target_respects_pre_send_authority() {
        let out = scratch("deployment-target");
        let mut intent = settle_legitimate_coordinate(&out);
        let result_path = out.join("market-pending/001-00/result.json");
        let mut result: MarketPendingResult =
            serde_json::from_slice(&std::fs::read(&result_path).expect("read settled fixture"))
                .expect("parse settled fixture");
        let deployed = intent.target.take().expect("fixture target");
        intent.predicted = Some(deployed.clone());
        intent.payload_digest = custody_payload_digest(&intent).expect("rebind prediction");
        result.custody = intent.clone();
        assert!(
            verify_settled_custody((1, 0), &intent, &result),
            "a deployment records its predicted ID before send and its target after readback"
        );
        let other: ContractId = "CD5BOMYXALWOHYZJZLLHLI2N25U7MH72O2BHCDDCGJKSQEQTV24TD6JT"
            .parse()
            .expect("other valid test target");
        intent.target = Some(deployed.clone());
        intent.predicted = Some(other.clone());
        intent.payload_digest = custody_payload_digest(&intent).expect("rebind conflicting IDs");
        result.custody = intent.clone();
        assert!(
            !verify_settled_custody((1, 0), &intent, &result),
            "a frozen target cannot mask a conflicting frozen deployment prediction"
        );
        intent.target = None;
        intent.predicted = Some(deployed.clone());
        intent.payload_digest = custody_payload_digest(&intent).expect("restore prediction");
        result.custody = intent.clone();
        result.step.target = Some(other);
        assert!(
            !verify_settled_custody((1, 0), &intent, &result),
            "a different post-send deployment target must not settle the predicted intent"
        );
        result.step.target = None;
        assert!(
            verify_settled_custody((1, 0), &intent, &result),
            "a reconciliation with no claimed target still settles its frozen predicted id"
        );
        intent.predicted = None;
        intent.kind = MarketMutationKind::Config;
        intent.action = Safe::literal("publish market manifest");
        intent.payload_digest = custody_payload_digest(&intent).expect("rebind config");
        result.custody = intent.clone();
        result.step.action = intent.action.clone();
        result.step.target = Some(deployed);
        assert!(
            !verify_settled_custody((1, 0), &intent, &result),
            "an unbound publication cannot claim a contract after send"
        );
        result.step.target = None;
        assert!(
            verify_settled_custody((1, 0), &intent, &result),
            "a local publication with no contract target can settle its frozen intent"
        );
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn settled_coordinate_does_not_follow_symlinked_result() {
        let out = scratch("symlink-result");
        let outside = scratch("outside-result");
        let external = outside.join("settled.json");
        std::fs::write(&external, b"external").expect("external target");
        settle_legitimate_coordinate(&out);
        let result = out
            .join("market-pending")
            .join("001-00")
            .join("result.json");
        std::fs::remove_file(&result).expect("remove settled result");
        std::os::unix::fs::symlink(&external, &result).expect("replace result with symlink");
        let status = journal_status(&out);
        assert_ne!(
            status.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "settlement must never follow a result symlink: {status}"
        );
        let _ = std::fs::remove_dir_all(out);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn forged_custody_cannot_masquerade_as_legitimate_settlement() {
        let out = scratch("custody-mismatch");
        let mut legitimate = settle_legitimate_coordinate(&out);
        let slot = std::process::id() % 100;
        legitimate.phase = 999;
        legitimate.payload_digest =
            custody_payload_digest(&legitimate).expect("legitimate intent digest");
        begin_market_pending(&out, legitimate.phase, slot, &legitimate)
            .expect("reserve real intent");
        let mut forged = legitimate.clone();
        forged.action = Safe::literal("forged-pool");
        forged.payload_digest = custody_payload_digest(&forged).expect("forged digest");
        let forged_result = MarketPendingResult {
            custody: forged,
            step: MarketStepRecord {
                phase: 999,
                action: Safe::literal("forged-pool"),
                command: Safe::literal("forged"),
                outcome: Safe::literal("settled"),
                target: legitimate.target.clone(),
                violations: Vec::new(),
            },
        };
        let result_path = out
            .join("market-pending")
            .join(format!("999-{slot:02}"))
            .join("result.json");
        assert!(
            finish_market_pending(&out, 999, slot, &forged_result).is_err()
                && !result_path.exists(),
            "the production writer must not settle an intent with forged custody"
        );
        // Bypass the writer as a local attacker could. The pinned reader must
        // still leave the genuine intent unresolved instead of accepting it.
        std::fs::write(
            &result_path,
            serde_json::to_vec_pretty(&forged_result).expect("forged fixture"),
        )
        .expect("simulate an outside writer's forged result");
        let status = journal_status(&out);
        assert_ne!(
            status.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "forged custody must not close a real journal: {status}"
        );
        assert!(
            status
                .get("pending_count")
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0),
            "a forged coordinate must remain pending: {status}"
        );
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn tampered_journal_root_cannot_signal_clear() {
        let out = scratch("symlink-root");
        let settled = scratch("settled-root");
        std::fs::create_dir_all(&settled).expect("create settled root");
        std::fs::create_dir_all(&out).expect("create journal parent");
        settle_legitimate_coordinate(&settled);
        std::fs::remove_dir_all(&settled).expect("remove settled root");
        std::os::unix::fs::symlink(&settled, out.join("market-pending"))
            .expect("replace root with symlink");
        let status = journal_status(&out);
        assert_ne!(
            status.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "symlinked journal root must be unavailable: {status}"
        );
        let _ = std::fs::remove_dir_all(out);
        let _ = std::fs::remove_dir_all(settled);
    }

    #[test]
    fn absent_journal_cannot_signal_clear_after_valid_manifest() {
        let out = scratch("absent-root");
        std::fs::create_dir_all(&out).expect("create report directory");
        let fixture = StatusExecutor::ready(vec![("lastprice", "null")]);
        let network = net();
        let mut st = Stellar::new(&network, &fixture, Policy::Market);
        st.sleep = no_sleep;
        let (report, ready) = status(&st, &out, &manifest()).expect("partial status returned");
        let rendered = report.to_string();
        assert!(
            rendered.contains(STATE_PENDING_UNAVAILABLE),
            "absent journal must be unavailable: {rendered}"
        );
        assert!(
            rendered.contains(REASON_JOURNAL),
            "absent journal must close readiness: {rendered}"
        );
        assert!(
            !rendered.contains(STATE_PENDING_CLEAR),
            "no false journal clear"
        );
        assert!(!ready, "absent journal cannot be ready");
        let pending = journal_status(&out);
        assert_eq!(
            pending.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_UNAVAILABLE)
        );
        assert!(
            pending
                .get("pending_count")
                .and_then(Value::as_null)
                .is_some(),
            "pending_count must remain null rather than false zero: {pending}"
        );
        assert!(
            pending.get("truncated").and_then(Value::as_null).is_some(),
            "truncated must remain null rather than false zero: {pending}"
        );
        let _ = std::fs::remove_dir_all(out);
    }

    fn synth_settled_coordinate(out: &Path, phase: u32, slot: u32) {
        let target: ContractId = C_ALL.parse().expect("fixture target");
        let signer: crate::stellar::AccountId = G_ADMIN.parse().expect("fixture signer");
        let mut custody = MarketCustodyRecord {
            phase,
            kind: MarketMutationKind::Deploy,
            action: Safe::literal("deploy-pool"),
            network: MarketNetwork::Testnet,
            signer,
            payload_digest: Sha256Hex::default(),
            target: Some(target),
            artifact_digest: None,
            predicted: None,
            pre_state: Vec::new(),
            tool_error: None,
        };
        custody.payload_digest =
            custody_payload_digest(&custody).expect("synthetic custody digest");
        let name = format!("{phase:03}-{slot:02}");
        assert!(
            market_coordinate(&name).is_some(),
            "synthetic coordinate must stay inside the journal grammar"
        );
        let dir = out.join("market-pending").join(name);
        std::fs::create_dir_all(&dir).expect("synthetic coordinate directory");
        std::fs::write(
            dir.join("intent.json"),
            serde_json::to_vec_pretty(&custody).expect("synthetic intent bytes"),
        )
        .expect("write synthetic intent");
        let result = MarketPendingResult {
            custody: custody.clone(),
            step: MarketStepRecord {
                phase,
                action: custody.action.clone(),
                command: Safe::literal("synthetic settlement"),
                outcome: Safe::literal("settled"),
                target: custody.target.clone(),
                violations: Vec::new(),
            },
        };
        std::fs::write(
            dir.join("result.json"),
            serde_json::to_vec_pretty(&result).expect("synthetic result bytes"),
        )
        .expect("write synthetic result");
    }

    fn synth_settled_range(out: &Path, from: usize, to: usize) {
        for index in from..to {
            let phase = 900 + (index / 100) as u32;
            let slot = (index % 100) as u32;
            synth_settled_coordinate(out, phase, slot);
        }
    }

    fn snapshot_journal_bytes(out: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        for phase in 900..=903u32 {
            for slot in 0..100u32 {
                let dir = out
                    .join("market-pending")
                    .join(format!("{phase:03}-{slot:02}"));
                for name in ["intent.json", "result.json"] {
                    let path = dir.join(name);
                    if let Ok(bytes) = std::fs::read(&path) {
                        files.insert(path, bytes);
                    }
                }
            }
        }
        files
    }

    #[test]
    fn journal_capacity_refuses_new_sends_and_a_fresh_run_clears_untouched_old_evidence() {
        let out = scratch("capacity-gate");
        synth_settled_range(&out, 0, 255);
        let below = journal_status(&out);
        assert_eq!(
            below.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "255 valid settled coordinates must remain clear: {below}"
        );
        assert_eq!(
            below.get("capacity_full").and_then(Value::as_bool),
            Some(false),
            "below the scan limit is not capacity: {below}"
        );
        assert!(
            journal_clear_for_action(&out),
            "255 settled coordinates clear the gate"
        );

        synth_settled_range(&out, 255, 256);
        let full = journal_status(&out);
        assert_eq!(
            full.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_FULL),
            "exactly 256 settled coordinates are capacity-full, never clear: {full}"
        );
        assert_eq!(
            full.get("capacity_full").and_then(Value::as_bool),
            Some(true),
            "the size signal must be explicit at capacity: {full}"
        );
        assert_eq!(full.get("truncated").and_then(Value::as_bool), Some(false));
        assert_eq!(full.get("pending_count").and_then(Value::as_u64), Some(0));
        assert!(
            !journal_clear_for_action(&out),
            "the gate must refuse before a 257th reservation becomes uninspectable"
        );
        let network = net();
        let fx = StatusExecutor::ready(vec![("lastprice", "null")]);
        let mut st = Stellar::new(&network, &fx, Policy::Market);
        st.sleep = no_sleep;
        let (report, ready) = status(&st, &out, &manifest()).expect("capacity status is returned");
        let rendered = report.to_string();
        assert!(!ready, "a capacity-full journal cannot certify readiness");
        assert!(
            rendered.contains(STATE_PENDING_FULL) && rendered.contains(REASON_JOURNAL_FULL),
            "capacity-full state and its recovery reason must stay visible: {rendered}"
        );
        assert_ne!(
            REASON_JOURNAL_FULL, REASON_JOURNAL,
            "capacity must be distinguishable from unresolved or unavailable"
        );

        synth_settled_range(&out, 256, 257);
        let oversized = journal_status(&out);
        assert_eq!(
            oversized.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_TRUNCATED),
            "more than 256 entries are truncated, never clear: {oversized}"
        );
        assert_eq!(
            oversized.get("truncated").and_then(Value::as_bool),
            Some(true),
            "the oversized journal must report its truncation: {oversized}"
        );
        assert_eq!(
            oversized.get("capacity_full").and_then(Value::as_bool),
            Some(true),
            "the size signal must be explicit above capacity: {oversized}"
        );
        assert!(
            !journal_clear_for_action(&out),
            "a truncated journal never clears"
        );
        assert_ne!(
            REASON_JOURNAL_TOO_LARGE, REASON_JOURNAL,
            "too-large must be distinguishable from unresolved or unavailable"
        );
        let preserved = snapshot_journal_bytes(&out);
        assert_eq!(
            preserved.len(),
            514,
            "257 coordinates carry two record files each"
        );
        let fx2 = StatusExecutor::ready(vec![("lastprice", "null")]);
        let mut st2 = Stellar::new(&network, &fx2, Policy::Market);
        st2.sleep = no_sleep;
        let (report, ready) =
            status(&st2, &out, &manifest()).expect("truncated status is returned");
        let rendered = report.to_string();
        assert!(
            !ready && rendered.contains(REASON_JOURNAL_TOO_LARGE),
            "the too-large reason must stay distinct from malformed or unavailable: {rendered}"
        );

        let fresh = scratch("fresh-recovery");
        synth_settled_coordinate(&fresh, 903, 0);
        let recovery = journal_status(&fresh);
        assert_eq!(
            recovery.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "a new disposable run with its own settled record reports clear: {recovery}"
        );
        assert!(
            journal_clear_for_action(&fresh),
            "the fresh run's own journal clears its gate"
        );
        for (path, bytes) in &preserved {
            let after = std::fs::read(path)
                .unwrap_or_else(|error| panic!("old evidence became unreadable: {error}"));
            assert_eq!(
                &after,
                bytes,
                "old evidence bytes changed: {}",
                path.display()
            );
        }
        let untouched = journal_status(&out);
        assert_eq!(
            untouched.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_TRUNCATED),
            "the old run must stay blocked without deleting evidence: {untouched}"
        );
        assert!(
            !journal_clear_for_action(&out),
            "the old run stays closed after recovery"
        );
        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    #[test]
    fn repeated_reasons_are_recorded_once() {
        let mut reasons = Vec::new();
        let mut ready = true;
        mark_unready(&mut reasons, &mut ready, REASON_JOURNAL);
        mark_unready(&mut reasons, &mut ready, REASON_JOURNAL);
        mark_unready(&mut reasons, &mut ready, REASON_JOURNAL);
        assert_eq!(reasons.as_slice(), [REASON_JOURNAL]);
    }

    #[test]
    fn oversized_market_render_cannot_produce_report() {
        let oversized = json!({"payload": "x".repeat(STATUS_RENDER_CAP + 1)});
        let err = Safe::render_market_json(&oversized, STATUS_RENDER_CAP)
            .expect_err("checked renderer must reject an over-cap document");
        assert_eq!(err.to_string(), crate::stellar::MARKET_RENDER_OVERSIZE);
    }

    #[test]
    fn network_probe_closes_readiness_before_any_contract_read() {
        for network_passphrase in [Some("Public Global Stellar Network ; September 2015"), None] {
            let out = scratch("probe-closed");
            let fixture = StatusExecutor {
                network_passphrase,
                ..StatusExecutor::ready(Vec::new())
            };
            let network = net();
            let mut st = Stellar::new(&network, &fixture, Policy::Market);
            st.sleep = no_sleep;
            let (report, ready) =
                status(&st, &out, &manifest()).expect("bounded status is returned");
            let rendered = report.to_string();
            assert!(
                !ready && rendered.contains(REASON_NETWORK),
                "a non-testnet or failed network probe must close readiness: {rendered}"
            );
            assert!(
                !fixture
                    .calls()
                    .iter()
                    .any(|call| call.first().map(String::as_str) == Some("contract")),
                "a failed network probe must stop before any contract interface or invoke"
            );
            let _ = std::fs::remove_dir_all(out);
        }
    }

    #[test]
    fn settled_result_without_its_intent_never_clears_the_journal() {
        let out = scratch("orphan-result");
        settle_legitimate_coordinate(&out);
        let coordinate = out.join("market-pending").join("001-00");
        std::fs::rename(
            coordinate.join("result.json"),
            out.join("orphaned-result.json"),
        )
        .expect("orphan the settlement");
        let cleared = journal_status(&out);
        assert_ne!(
            cleared.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "an open intent stripped of its settlement is a pending question: {cleared}"
        );
        std::fs::remove_dir_all(&coordinate).expect("empty the coordinate");
        std::fs::create_dir_all(out.join("market-pending").join("001-02")).expect("empty entry");
        assert!(
            !journal_clear_for_action(&out),
            "a bare coordinate directory must never clear the action gate"
        );
        let _ = std::fs::remove_dir_all(out);
    }

    /// A planted FIFO at a journal coordinate must fail the pinned read
    /// immediately (O_NONBLOCK) instead of hanging the scan before the
    /// regular-file law can reject it, and must keep the gate closed.
    #[test]
    fn planted_fifo_cannot_hang_the_pinned_reader_or_clear_the_gate() {
        let out = scratch("fifo-reader");
        settle_legitimate_coordinate(&out);
        std::fs::create_dir_all(out.join("market-pending").join("002-00"))
            .expect("fifo coordinate");
        plant_fifo(
            &out.join("market-pending")
                .join("002-00")
                .join("intent.json"),
        );

        let started = std::time::Instant::now();
        let status = journal_status(&out);
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the pinned reader must refuse a FIFO promptly, not block: {elapsed:?}"
        );
        assert_ne!(
            status.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "a malformed FIFO coordinate must never clear the journal: {status}"
        );
        assert!(
            !journal_clear_for_action(&out),
            "a malformed FIFO coordinate must block sends"
        );
        assert!(
            read_journal_coordinate(&out, 2, 0).is_err(),
            "a FIFO coordinate must be an error, never a default"
        );

        // A prompt refusal leaves no hidden reader attached: a writer
        // open of the FIFO must fail immediately with no reader present.
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(
                out.join("market-pending")
                    .join("002-00")
                    .join("intent.json"),
            );
        assert!(
            matches!(
                writer,
                Err(error) if error.raw_os_error() == Some(6) // ENXIO: no reader attached
            ),
            "no thread may remain blocked as a FIFO reader"
        );

        std::fs::remove_file(
            out.join("market-pending")
                .join("002-00")
                .join("intent.json"),
        )
        .expect("remove planted fifo");
        std::fs::remove_dir_all(out.join("market-pending").join("002-00"))
            .expect("empty the planted coordinate");
        assert!(
            journal_clear_for_action(&out),
            "O_NONBLOCK must not alter legitimate regular-file settlement reads"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The pinned reader refuses any nonregular coordinate promptly and
    /// without following links; O_NONBLOCK never changes a regular read.
    #[test]
    fn regular_settlements_stay_readable_and_nonregular_records_fail_closed() {
        let out = scratch("regular-and-nonregular");
        settle_legitimate_coordinate(&out);
        assert!(journal_clear_for_action(&out));
        std::fs::create_dir_all(out.join("market-pending").join("003-00"))
            .expect("nonregular coordinate");
        plant_fifo(
            &out.join("market-pending")
                .join("003-00")
                .join("intent.json"),
        );
        let started = std::time::Instant::now();
        let status = journal_status(&out);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "status must stay prompt: {status}"
        );
        assert_ne!(
            status.get("state").and_then(Value::as_str),
            Some(STATE_PENDING_CLEAR),
            "a nonregular record fails the read closed: {status}"
        );
        assert!(!journal_clear_for_action(&out));
        std::fs::remove_file(
            out.join("market-pending")
                .join("003-00")
                .join("intent.json"),
        )
        .expect("remove planted fifo");
        std::fs::remove_dir_all(out.join("market-pending").join("003-00"))
            .expect("empty the planted coordinate");
        assert!(journal_clear_for_action(&out));
        let _ = std::fs::remove_dir_all(&out);
    }
}
