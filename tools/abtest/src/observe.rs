//! Per-side observation after each step: typed views, raw storage rows, invariants.
//! Port of `test-suites/src/adr8_properties.rs::invariants`.
//!
//! Malformed or missing observations are never treated as observed emptiness:
//! every invariant certifies only what a read actually returned, and an
//! observation that cannot be parsed fails closed instead of coercing to 0.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::manifest::{SideManifest, Tok, Who};
use crate::stellar::{Executor, Invocation, Policy, RawEntry, RawKey, Safe, Stellar};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// who ("admin".."user", "pool", "backstop") -> Positions
    pub positions: BTreeMap<String, Value>,
    pub reserves: BTreeMap<Tok, Value>,
    pub config: Value,
    pub backstop_pool: Value,
    pub backstop_users: BTreeMap<Who, Value>,
    pub reward_zone: Value,
    /// token -> holder -> balance (decimal string)
    pub balances: BTreeMap<Tok, BTreeMap<String, String>>,
    /// who -> AuctionData (null only when the raw temporary auction row was
    /// observed absent; never for a failed or malformed read)
    pub auctions: BTreeMap<Who, Value>,
    /// raw persistent rows: per-reserve `ResData`, every user `Positions`
    /// row, `PoolEmis`, and the backstop's `RZ`/`PoolBalance`/`BEmisData`
    /// rows. Deliberately trimmed: rows with a typed twin (`ResConfig`,
    /// pool/backstop positions, `LastDist`, emission accruals) ride the typed
    /// views instead, because every `Key` row costs two CLI subprocesses and
    /// snapshots already dominate a run's non-waiting time. Immutability
    /// claims rest on THIS captured set plus those views — not all storage.
    /// `ResData` is required-present for a configured reserve; `Positions`
    /// and `PoolEmis` are default-on-missing, so `None` there is a real
    /// observation (`get_persistent_default`).
    pub raw: BTreeMap<String, Option<RawEntry>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepRecord {
    pub label: String,
    pub side: String,
    pub invocation: Option<Invocation>,
    /// The fresh snapshot taken for this side immediately BEFORE the
    /// invocation, so before/after claims bracket the call itself and replay
    /// can recompute deltas without trusting the neighbour step. Absent for
    /// evidence written before the field existed — and only then.
    #[serde(default)]
    pub before: Option<Snapshot>,
    pub snapshot: Snapshot,
    pub violations: Vec<Safe>,
}

/// Compose a finding under a policy: the pair arm stores `msg` verbatim
/// (byte-identical to the retired `Vec<String>` push), the market arm
/// sanitizes.
pub fn policy_finding(policy: Policy, msg: impl Into<String>) -> Safe {
    Safe::error_chain(policy, &anyhow::anyhow!("{}", msg.into()))
}

pub fn num(v: &Value) -> i128 {
    match v {
        Value::Number(n) => n.as_i64().map(i128::from).unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// The honest twin of `num`: `None` for a value that cannot be read as i128, so an
/// invariant can fail loudly instead of comparing a missing field as 0.
pub fn try_num(v: &Value) -> Option<i128> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Soroban maps serialize either as objects or as arrays of pairs.
pub fn map_entries(v: &Value) -> Vec<(String, Value)> {
    match v {
        Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|p| {
                p.as_array()
                    .filter(|p| p.len() == 2)
                    .map(|p| (p[0].to_string().trim_matches('"').to_string(), p[1].clone()))
            })
            .collect(),
        _ => vec![],
    }
}

/// The strict sibling of [`map_entries`]: `Some` only for a genuine Soroban map
/// encoding (object, or array of exact `[key, value]` pairs). `None` means the
/// value was never a map at all — a verdict may read that as "not observed /
/// malformed", never as an empty map.
pub fn strict_map(v: &Value) -> Option<Vec<(String, Value)>> {
    match v {
        Value::Object(_) => Some(map_entries(v)),
        Value::Array(a)
            if a.iter()
                .all(|p| matches!(p, Value::Array(kv) if kv.len() == 2)) =>
        {
            Some(map_entries(v))
        }
        _ => None,
    }
}

/// Central holder, reserve-key and quantity validation for every
/// `positions[who][field]` map, judged once here so replay
/// (`report::recompute` -> `invariants`) enforces it too without scattered
/// guards: only the identity roles the snapshot keys by (`Who::ALL` plus
/// `pool`/`backstop`) may hold positions — an unexpected holder fails, it is
/// never silently dropped from the conservation sums; keys of a positions
/// `Map<u32, i128>` are reserve ids, so a non-u32, duplicate or unknown id is
/// malformed observation data and can never certify a zero; amounts must
/// parse as nonnegative i128s, which the contract never writes otherwise.
fn check_reserve_keys(policy: Policy, m: &SideManifest, s: &Snapshot, out: &mut Vec<Safe>) {
    let valid: Vec<u32> = m.reserves.values().map(|r| r.index).collect();
    let holders: Vec<&str> = Who::ALL
        .iter()
        .map(|w| w.as_str())
        .chain(["pool", "backstop"])
        .collect();
    for (who, p) in &s.positions {
        if !holders.contains(&who.as_str()) {
            out.push(policy_finding(
                policy,
                format!("positions[{who}] unexpected holder"),
            ));
            continue;
        }
        let Some(p) = p.as_object() else {
            out.push(policy_finding(
                policy,
                format!("positions[{who}] missing/malformed"),
            ));
            continue;
        };
        for field in ["collateral", "liabilities", "supply"] {
            let Some(entries) = p.get(field).and_then(strict_map) else {
                out.push(policy_finding(
                    policy,
                    format!("positions[{who}].{field} missing/malformed"),
                ));
                continue;
            };
            let mut seen: Vec<u32> = Vec::new();
            for (k, amount) in entries {
                match try_num(&amount) {
                    Some(v) if v >= 0 => {}
                    Some(_) => out.push(policy_finding(
                        policy,
                        format!("positions[{who}].{field}[{k}] negative"),
                    )),
                    None => out.push(policy_finding(
                        policy,
                        format!("positions[{who}].{field}[{k}] not an i128"),
                    )),
                }
                match k.parse::<u32>() {
                    Ok(idx) => {
                        if seen.contains(&idx) {
                            out.push(policy_finding(
                                policy,
                                format!("positions[{who}].{field} has duplicate reserve key {k}"),
                            ));
                        } else if !valid.contains(&idx) {
                            out.push(policy_finding(
                                policy,
                                format!("positions[{who}].{field} has unknown reserve key {k}"),
                            ));
                        }
                        seen.push(idx);
                    }
                    Err(_) => out.push(policy_finding(
                        policy,
                        format!("positions[{who}].{field} has non-reserve key {k}"),
                    )),
                }
            }
        }
    }
}

/// One `positions[who][field]` reserve map entry, strictly parsed.
/// `Err` covers both "never observed" and "observed but malformed": either way
/// no invariant may certify against it. An absent key inside a genuine map is
/// `Ok(0)` — that is what a missing key means in a `Map<u32, i128>`.
fn position_field(pos: &Value, who: &str, field: &str, key: u32) -> Result<i128, String> {
    let obj = pos
        .as_object()
        .ok_or_else(|| format!("positions[{who}] missing/malformed"))?;
    let f = obj
        .get(field)
        .ok_or_else(|| format!("positions[{who}].{field} missing/malformed"))?;
    let entries = strict_map(f).ok_or_else(|| format!("positions[{who}].{field} not a map"))?;
    match entries.into_iter().find(|(k, _)| *k == key.to_string()) {
        Some((_, v)) => {
            try_num(&v).ok_or_else(|| format!("positions[{who}].{field}[{key}] not an i128"))
        }
        None => Ok(0),
    }
}

fn kv(k: &str, v: impl ToString) -> (&str, String) {
    (k, v.to_string())
}

fn enum_key(variant: &str, addr: &str) -> Value {
    json!({"vec": [{"symbol": variant}, {"address": addr}]})
}

/// PoolDataKey::Auction(AuctionKey) uses a contracttype struct (sorted symbol
/// map), not a tuple: pool/src/storage.rs::AuctionKey and get_auction.
fn auction_key(addr: &str) -> Value {
    json!({"vec": [
        {"symbol": "Auction"},
        {"map": [
            {"key": {"symbol": "auct_type"}, "val": {"u32": 0}},
            {"key": {"symbol": "user"}, "val": {"address": addr}}
        ]}
    ]})
}

fn observe_auction<E: Executor>(
    st: &Stellar<E>,
    pool: &str,
    src: &str,
    addr: &str,
) -> anyhow::Result<Value> {
    let key = st.xdr_encode_scval(&auction_key(addr))?;
    if st
        .read_temp(pool, RawKey::Xdr(key.as_str().to_string()))?
        .is_none()
    {
        return Ok(Value::Null);
    }
    let inv = st.try_view(
        pool,
        src,
        "get_auction",
        &[kv("auction_type", 0), kv("user", addr)],
    )?;
    anyhow::ensure!(
        inv.ok,
        "present auction view for {addr} failed: {:?}",
        inv.error
    );
    let value = inv.value.context("present auction returned no value")?;
    validate_auction(&value).with_context(|| format!("present auction for {addr} is malformed"))?;
    Ok(value)
}

/// Validate the CLI's rendering of `AuctionData` without interpreting its values.
fn validate_auction(value: &Value) -> anyhow::Result<()> {
    let data = value
        .as_object()
        .context("auction data must be an object")?;
    let block = data
        .get("block")
        .context("auction data is missing block")?
        .as_u64()
        .context("auction block must be a u32 number")?;
    anyhow::ensure!(
        u32::try_from(block).is_ok(),
        "auction block {block} is not a u32"
    );
    for field in ["bid", "lot"] {
        let map = data
            .get(field)
            .with_context(|| format!("auction data is missing {field}"))?;
        let entries = strict_map(map).with_context(|| format!("auction {field} must be a map"))?;
        let mut seen = std::collections::BTreeSet::new();
        for (key, amount) in entries {
            anyhow::ensure!(
                seen.insert(key.clone()),
                "auction {field} has duplicate asset {key}"
            );
            let amount = try_num(&amount)
                .with_context(|| format!("auction {field}[{key}] is not an i128"))?;
            anyhow::ensure!(amount >= 0, "auction {field}[{key}] negative");
        }
    }
    Ok(())
}

/// Post-step check called by the run loop's step_once: after a successful
/// auction-creating invocation (same trigger as the auction_blocks write),
/// that side's post-snapshot must observe an auction for the auctioned
/// `who`. `None` = satisfied; `Some(finding)` is a silent-absence defect —
/// the raw temporary row read absent (or the map entry vanished) after a
/// call that must have written it. Coverage boundary: `Snapshot::auctions`
/// is keyed by `Who::ALL`, so an auction for any other address is
/// unobservable through this helper and must never be claimed covered.
///
/// A present value must also parse as `AuctionData`; malformed data is a
/// finding, not silent coverage.
pub fn auction_observed(policy: Policy, s: &Snapshot, who: Who) -> Option<Safe> {
    let name = who.as_str();
    match s.auctions.get(&who) {
        None => Some(policy_finding(
            policy,
            format!("auction for {name} not observed"),
        )),
        Some(Value::Null) => Some(policy_finding(
            policy,
            format!("auction for {name} observed absent after a successful creating call"),
        )),
        Some(value) => validate_auction(value).err().map(|e| {
            policy_finding(
                policy,
                format!("auction for {name} observed malformed: {e:#}"),
            )
        }),
    }
}

/// Run independent read-only jobs in parallel (simulations consume no sequence numbers).
pub(crate) fn parallel<T: Send, F: Fn() -> anyhow::Result<T> + Send + Sync>(
    jobs: &[F],
) -> anyhow::Result<Vec<T>> {
    const WIDTH: usize = 8;
    let mut out = Vec::with_capacity(jobs.len());
    for chunk in jobs.chunks(WIDTH) {
        let results: Vec<anyhow::Result<T>> = std::thread::scope(|sc| {
            chunk
                .iter()
                .map(|job| sc.spawn(job))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().expect("view thread"))
                .collect()
        });
        for r in results {
            out.push(r?);
        }
    }
    Ok(out)
}

type ViewJob<'a> = Box<dyn Fn() -> anyhow::Result<Value> + Send + Sync + 'a>;

fn view_jobs<'a, E: Executor + Sync>(
    st: &'a Stellar<'a, E>,
    m: &'a SideManifest,
    ids: &'a BTreeMap<Who, String>,
) -> Vec<(String, ViewJob<'a>)> {
    static SOURCE_KEY: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| Who::User.key_name());
    let src: &'a str = &SOURCE_KEY;
    let c = &m.contracts;
    let mut jobs: Vec<(String, ViewJob<'a>)> = Vec::new();
    let mut add = |key: String, id: &'a str, f: &'static str, args: Vec<(&'static str, String)>| {
        jobs.push((
            key,
            Box::new(move || {
                st.view(
                    id,
                    src,
                    f,
                    &args
                        .iter()
                        .map(|(k, v)| (*k, v.clone()))
                        .collect::<Vec<_>>(),
                )
            }),
        ));
    };
    for who in Who::ALL {
        add(
            format!("positions.{}", who.as_str()),
            &c.pool,
            "get_positions",
            vec![("address", ids[&who].clone())],
        );
    }
    add(
        "positions.pool".into(),
        &c.pool,
        "get_positions",
        vec![("address", c.pool.clone())],
    );
    add(
        "positions.backstop".into(),
        &c.pool,
        "get_positions",
        vec![("address", c.backstop.clone())],
    );
    for tok in Tok::RESERVES {
        add(
            format!("reserves.{}", tok.as_str()),
            &c.pool,
            "get_reserve",
            vec![("asset", c.token(tok).to_string())],
        );
    }
    add("config".into(), &c.pool, "get_config", vec![]);
    add(
        "backstop_pool".into(),
        &c.backstop,
        "pool_data",
        vec![("pool", c.pool.clone())],
    );
    for who in Who::ALL {
        add(
            format!("backstop_users.{}", who.as_str()),
            &c.backstop,
            "user_balance",
            vec![("pool", c.pool.clone()), ("user", ids[&who].clone())],
        );
    }
    add("reward_zone".into(), &c.backstop, "reward_zone", vec![]);
    for tok in Tok::ALL {
        let mut names: Vec<(String, String)> = Who::ALL
            .iter()
            .map(|w| (w.as_str().to_string(), ids[w].clone()))
            .collect();
        names.push(("pool".into(), c.pool.clone()));
        names.push(("backstop".into(), c.backstop.clone()));
        names.push(("lp".into(), c.lp.clone()));
        for (name, addr) in names {
            add(
                format!("balances.{}.{}", tok.as_str(), name),
                c.token(tok),
                "balance",
                vec![("id", addr)],
            );
        }
    }
    jobs
}

pub fn snapshot<E: Executor + Sync>(
    st: &Stellar<E>,
    m: &SideManifest,
    ids: &BTreeMap<Who, String>,
) -> anyhow::Result<Snapshot> {
    let src = Who::User.key_name();
    let c = &m.contracts;
    let jobs = view_jobs(st, m, ids);
    let fns: Vec<&ViewJob> = jobs.iter().map(|(_, f)| f).collect();
    let values = parallel(&fns)?;
    let mut views: BTreeMap<String, Value> = BTreeMap::new();
    for ((key, _), v) in jobs.iter().zip(values) {
        views.insert(key.clone(), v);
    }

    let mut positions = BTreeMap::new();
    for who in Who::ALL
        .iter()
        .map(|w| w.as_str())
        .chain(["pool", "backstop"])
    {
        positions.insert(
            who.to_string(),
            views
                .remove(&format!("positions.{who}"))
                .unwrap_or(Value::Null),
        );
    }
    let mut reserves = BTreeMap::new();
    for tok in Tok::RESERVES {
        reserves.insert(
            tok,
            views
                .remove(&format!("reserves.{}", tok.as_str()))
                .unwrap_or(Value::Null),
        );
    }
    let config = views.remove("config").unwrap_or(Value::Null);
    let backstop_pool = views.remove("backstop_pool").unwrap_or(Value::Null);
    let mut backstop_users = BTreeMap::new();
    for who in Who::ALL {
        backstop_users.insert(
            who,
            views
                .remove(&format!("backstop_users.{}", who.as_str()))
                .unwrap_or(Value::Null),
        );
    }
    let reward_zone = views.remove("reward_zone").unwrap_or(Value::Null);
    let mut balances = BTreeMap::new();
    for tok in Tok::ALL {
        let mut holders = BTreeMap::new();
        for name in Who::ALL
            .iter()
            .map(|w| w.as_str())
            .chain(["pool", "backstop", "lp"])
        {
            let v = views
                .remove(&format!("balances.{}.{}", tok.as_str(), name))
                .unwrap_or(Value::Null);
            // Parse at the boundary: a balance that is not an i128 is an
            // observation error, not a zero.
            let n = try_num(&v)
                .with_context(|| format!("balance view {}.{} returned {v}", tok.as_str(), name))?;
            holders.insert(name.to_string(), n.to_string());
        }
        balances.insert(tok, holders);
    }

    // Auctions are temporary storage (`get_auction` reads
    // `temporary().get().unwrap_optimized()`, pool/src/storage.rs): the raw
    // row decides presence, and a present row must read back typed.
    let auction_jobs: Vec<Box<dyn Fn() -> anyhow::Result<Value> + Send + Sync>> = Who::ALL
        .iter()
        .map(|who| {
            let addr = ids[who].clone();
            let src = src.clone();
            let pool = c.pool.clone();
            Box::new(move || observe_auction(st, &pool, &src, &addr))
                as Box<dyn Fn() -> anyhow::Result<Value> + Send + Sync>
        })
        .collect();
    let auction_vals = parallel(&auction_jobs)?;
    let auctions: BTreeMap<Who, Value> = Who::ALL.iter().copied().zip(auction_vals).collect();

    // Raw persistent rows backing the immutability claims. The backstop rows
    // cover what the ADR 0011 traps can touch on that contract; the pool rows
    // cover reserve config/data, every position row and the emissions map.
    // Storage the tool cannot key is explicitly outside the claim.
    enum RawSpec {
        Symbol(&'static str),
        Key(Value),
    }
    let mut raw_specs: Vec<(String, String, RawSpec)> = Vec::new();
    for t in Tok::RESERVES {
        raw_specs.push((
            format!("res_data_{}", t.as_str()),
            c.pool.clone(),
            RawSpec::Key(enum_key("ResData", c.token(t))),
        ));
    }
    for w in Who::ALL {
        raw_specs.push((
            format!("positions_{}", w.as_str()),
            c.pool.clone(),
            RawSpec::Key(enum_key("Positions", &ids[&w])),
        ));
    }
    raw_specs.push((
        "pool_emis".into(),
        c.pool.clone(),
        RawSpec::Symbol("PoolEmis"),
    ));
    raw_specs.push((
        "backstop_rz".into(),
        c.backstop.clone(),
        RawSpec::Symbol("RZ"),
    ));
    raw_specs.push((
        "backstop_pool_balance".into(),
        c.backstop.clone(),
        RawSpec::Key(enum_key("PoolBalance", &c.pool)),
    ));
    raw_specs.push((
        "backstop_bemis".into(),
        c.backstop.clone(),
        RawSpec::Key(enum_key("BEmisData", &c.pool)),
    ));
    let raw_jobs: Vec<_> = raw_specs
        .iter()
        .map(|(_, contract, spec)| {
            move || match spec {
                RawSpec::Symbol(sym) => st.read_raw(contract, RawKey::Symbol((*sym).into())),
                RawSpec::Key(key) => {
                    let b64 = st.xdr_encode_scval(key)?;
                    st.read_raw(contract, RawKey::Xdr(b64.as_str().to_string()))
                }
            }
        })
        .collect();
    let raw_vals = parallel(&raw_jobs)?;
    let raw: BTreeMap<String, Option<RawEntry>> = raw_specs
        .into_iter()
        .map(|(k, _, _)| k)
        .zip(raw_vals)
        .collect();

    Ok(Snapshot {
        positions,
        reserves,
        config,
        backstop_pool,
        backstop_users,
        reward_zone,
        balances,
        auctions,
        raw,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvariantPolicy {
    Stock,
    Fork,
}

/// Per-side invariants. Returns human-readable failures.
///
/// A missing or malformed observation is itself a failure: invariants certify
/// equations over observed values only, and an observed empty map/zone stays
/// distinct from "never read". Integer sums are exact and checked.
/// `inv_policy` selects which zero/emptiness assumptions apply (Fork: no take,
/// credit, emissions, rewards, custody or backstop positions); accounting,
/// shape, manifest identities and rate bounds are universal for both sides.
pub fn invariants(
    policy: Policy,
    m: &SideManifest,
    s: &Snapshot,
    extra_minted: &BTreeMap<Tok, i64>,
    inv_policy: InvariantPolicy,
) -> Vec<Safe> {
    let mut out = Vec::new();
    check_reserve_keys(policy, m, s, &mut out);

    if !s.config.is_object() {
        out.push(Safe::literal("config missing/malformed (object expected)"));
    }
    if m.contracts.oracle.is_empty()
        || s.config["oracle"].as_str() != Some(m.contracts.oracle.as_str())
    {
        out.push(Safe::literal(
            "config.oracle missing/malformed or differs from manifest",
        ));
    }
    match try_num(&s.config["bstop_rate"]) {
        Some(v) if !(0..10_000_000).contains(&v) => {
            out.push(policy_finding(
                policy,
                format!("bstop_rate outside [0, 1): {v}"),
            ));
        }
        Some(v) if inv_policy == InvariantPolicy::Fork && v != 0 => {
            out.push(policy_finding(policy, format!("bstop_rate != 0: {v}")));
        }
        Some(_) => {}
        None => out.push(Safe::literal("config.bstop_rate missing/unparseable")),
    }
    match s.raw.get("pool_emis") {
        None => out.push(Safe::literal("raw pool_emis row not observed")),
        Some(Some(_)) if inv_policy == InvariantPolicy::Fork => {
            out.push(Safe::literal("PoolEmis map exists"));
        }
        Some(_) => {}
    }

    for tok in Tok::MOCK {
        let minted = match m
            .minted
            .get(&tok)
            .copied()
            .unwrap_or(0)
            .checked_add(extra_minted.get(&tok).copied().unwrap_or(0))
        {
            Some(x) => i128::from(x),
            None => {
                out.push(policy_finding(
                    policy,
                    format!("{}: minted total overflows i64", tok.as_str()),
                ));
                continue;
            }
        };
        let holders = match s.balances.get(&tok) {
            Some(h) => h,
            None => {
                out.push(policy_finding(
                    policy,
                    format!("{}: balances not observed", tok.as_str()),
                ));
                continue;
            }
        };
        let mut total: i128 = 0;
        let mut ok = true;
        for (holder, balance) in holders {
            match balance.parse::<i128>() {
                Ok(v) if v < 0 => {
                    out.push(policy_finding(
                        policy,
                        format!("{}: balance {holder} negative ({v})", tok.as_str()),
                    ));
                    ok = false;
                }
                Ok(v) => match total.checked_add(v) {
                    Some(x) => total = x,
                    None => {
                        out.push(policy_finding(
                            policy,
                            format!("{}: balance sum overflows i128", tok.as_str()),
                        ));
                        ok = false;
                    }
                },
                Err(_) => {
                    out.push(policy_finding(
                        policy,
                        format!("{}: balance {holder} unparseable ({balance})", tok.as_str()),
                    ));
                    ok = false;
                }
            }
        }
        if ok && total != minted {
            out.push(policy_finding(
                policy,
                format!("{}: balances {total} != minted {minted}", tok.as_str()),
            ));
        }
    }

    // Custody and backstop positions: their emptiness is only certifiable when
    // the rows were actually observed and parse as maps.
    match s.positions.get("pool") {
        None => out.push(Safe::literal("positions[pool] missing/malformed")),
        Some(p) => {
            let mut holds = false;
            for kind in ["collateral", "liabilities"] {
                match p.get(kind).and_then(strict_map) {
                    Some(entries) => holds |= !entries.is_empty(),
                    None => out.push(policy_finding(
                        policy,
                        format!("positions[pool].{kind} missing/malformed"),
                    )),
                }
            }
            if inv_policy == InvariantPolicy::Fork && holds {
                out.push(Safe::literal(
                    "pool custody holds collateral or liabilities",
                ));
            }
        }
    }
    match s.positions.get("backstop") {
        None => out.push(Safe::literal("positions[backstop] missing/malformed")),
        Some(p) => {
            for kind in ["collateral", "liabilities", "supply"] {
                match p.get(kind).and_then(strict_map) {
                    Some(entries) if inv_policy == InvariantPolicy::Fork && !entries.is_empty() => {
                        out.push(policy_finding(
                            policy,
                            format!("backstop address holds {kind}"),
                        ))
                    }
                    Some(_) => {}
                    None => out.push(policy_finding(
                        policy,
                        format!("positions[backstop].{kind} missing/malformed"),
                    )),
                }
            }
        }
    }

    for tok in m.reserves.keys() {
        if !matches!(s.reserves.get(tok), Some(v) if !v.is_null()) {
            out.push(policy_finding(
                policy,
                format!("{}: configured reserve not observed", tok.as_str()),
            ));
        }
    }
    for (who, auction) in &s.auctions {
        if auction.is_null() {
            continue;
        }
        if let Err(e) = validate_auction(auction) {
            out.push(policy_finding(
                policy,
                format!("auction for {} malformed: {e:#}", who.as_str()),
            ));
            continue;
        }
        for field in ["bid", "lot"] {
            for (key, _) in strict_map(&auction[field]).expect("validated auction map") {
                if key.is_empty() || !m.reserves.keys().any(|tok| m.contracts.token(*tok) == key) {
                    out.push(policy_finding(
                        policy,
                        format!(
                            "auction for {} {field} has unknown reserve asset {key}",
                            who.as_str()
                        ),
                    ));
                }
            }
        }
    }

    for (tok, r) in &s.reserves {
        let name = tok.as_str();
        let idx = match m.reserves.get(tok) {
            Some(r) => r.index,
            None => {
                out.push(policy_finding(
                    policy,
                    format!("{name}: reserve not in manifest"),
                ));
                continue;
            }
        };
        let asset = m.contracts.token(*tok);
        if asset.is_empty() || r["asset"].as_str() != Some(asset) {
            out.push(policy_finding(
                policy,
                format!("{name}: reserve asset missing/malformed or differs from manifest"),
            ));
        }
        if try_num(&r["config"]["index"]) != Some(i128::from(idx)) {
            out.push(policy_finding(
                policy,
                format!("{name}: reserve config.index missing/malformed or differs from manifest"),
            ));
        }
        // Required-present row: `set_reserve` persists `ResData`, so an
        // unobserved or absent row for a configured reserve is an
        // observation/tool anomaly — never a silent pass. (Positions and
        // emission rows are default-on-missing; `None` is genuine there.)
        match s.raw.get(&format!("res_data_{name}")) {
            Some(Some(_)) => {}
            Some(None) => out.push(policy_finding(
                policy,
                format!("{name}: raw ResData row absent for a configured reserve"),
            )),
            None => out.push(policy_finding(
                policy,
                format!("{name}: raw ResData row not observed"),
            )),
        }
        let data = match r.get("data").filter(|d| d.is_object()) {
            Some(d) => d,
            None => {
                out.push(policy_finding(
                    policy,
                    format!("{name}: reserve data missing/malformed"),
                ));
                continue;
            }
        };
        // Stock assigns defaulted debt to the backstop address, and that debt
        // is part of d_supply there; the conservation law holds only when the
        // sum includes it on both sides (it is always zero on the fork).
        let mut liabilities: i128 = 0;
        let mut supply: i128 = 0;
        let mut cons_ok = true;
        let mut add_pos = |acc: &mut i128, who: &str, field: &str, overflow: &str| {
            let v = match s.positions.get(who) {
                Some(p) => position_field(p, who, field, idx),
                None => Err(format!("positions[{who}] missing/malformed")),
            };
            match v.and_then(|v| {
                acc.checked_add(v)
                    .ok_or_else(|| format!("{name}: {overflow} overflows i128"))
            }) {
                Ok(x) => *acc = x,
                Err(msg) => {
                    out.push(policy_finding(policy, msg));
                    cons_ok = false;
                }
            }
        };
        for who in Who::ALL
            .iter()
            .map(|w| w.as_str())
            .chain(["pool", "backstop"])
        {
            add_pos(&mut liabilities, who, "liabilities", "liabilities sum");
            for field in ["supply", "collateral"] {
                add_pos(&mut supply, who, field, "supply sum");
            }
        }
        let d_supply = try_num(&data["d_supply"]);
        let b_supply = try_num(&data["b_supply"]);
        let b_rate = try_num(&data["b_rate"]);
        let d_rate = try_num(&data["d_rate"]);
        let credit = try_num(&data["backstop_credit"]);
        for (field, v) in [
            ("d_supply", d_supply),
            ("b_supply", b_supply),
            ("b_rate", b_rate),
            ("d_rate", d_rate),
            ("backstop_credit", credit),
        ] {
            if v.is_none() {
                out.push(policy_finding(
                    policy,
                    format!("{name}: reserve field {field} missing"),
                ));
            }
            if matches!(v, Some(v) if v < 0) {
                out.push(policy_finding(
                    policy,
                    format!("{name}: reserve field {field} negative"),
                ));
            }
        }
        if cons_ok {
            if d_supply != Some(liabilities) {
                out.push(policy_finding(
                    policy,
                    format!(
                        "{name}: d_supply {} != sum liabilities {liabilities}",
                        data["d_supply"]
                    ),
                ));
            }
            if b_supply != Some(supply) {
                out.push(policy_finding(
                    policy,
                    format!(
                        "{name}: b_supply {} != sum supply+collateral+custody {supply}",
                        data["b_supply"]
                    ),
                ));
            }
        }
        if matches!(b_rate, Some(v) if v < 0) {
            out.push(policy_finding(policy, format!("{name}: b_rate negative")));
        }
        if matches!(d_rate, Some(v) if v <= 0) {
            out.push(policy_finding(
                policy,
                format!("{name}: d_rate not positive"),
            ));
        }
        if inv_policy == InvariantPolicy::Fork && matches!(credit, Some(v) if v != 0) {
            out.push(policy_finding(
                policy,
                format!("{name}: backstop_credit != 0"),
            ));
        }
    }

    if !s.backstop_pool.is_object() {
        out.push(Safe::literal("backstop pool data missing/malformed"));
    }
    for field in ["shares", "tokens"] {
        match try_num(&s.backstop_pool[field]) {
            Some(v) if v >= 0 => {}
            Some(_) => out.push(policy_finding(
                policy,
                format!("backstop pool {field} negative"),
            )),
            None => out.push(policy_finding(
                policy,
                format!("backstop pool {field} missing/unparseable"),
            )),
        }
    }
    let mut user_shares: i128 = 0;
    let mut users_ok = true;
    for w in Who::ALL {
        let who = w.as_str();
        let u = match s.backstop_users.get(&w) {
            Some(u) if u.is_object() => u,
            _ => {
                out.push(policy_finding(
                    policy,
                    format!("backstop user {who} not observed"),
                ));
                users_ok = false;
                continue;
            }
        };
        let shares = match u.get("shares").and_then(try_num) {
            Some(v) if v < 0 => {
                out.push(policy_finding(
                    policy,
                    format!("backstop user {who} shares negative"),
                ));
                users_ok = false;
                continue;
            }
            Some(v) => v,
            None => {
                out.push(policy_finding(
                    policy,
                    format!("backstop user {who} shares missing/unparseable"),
                ));
                users_ok = false;
                continue;
            }
        };
        let q4w = match u.get("q4w") {
            Some(Value::Array(q)) => q,
            _ => {
                out.push(policy_finding(
                    policy,
                    format!("backstop user {who} q4w not observed (array expected)"),
                ));
                users_ok = false;
                continue;
            }
        };
        let mut qsum: i128 = 0;
        let mut qok = true;
        for entry in q4w {
            match entry.get("amount").and_then(try_num) {
                Some(v) if v < 0 => {
                    out.push(policy_finding(
                        policy,
                        format!("backstop user {who} q4w amount negative"),
                    ));
                    qok = false;
                }
                Some(v) => match qsum.checked_add(v) {
                    Some(x) => qsum = x,
                    None => {
                        out.push(policy_finding(
                            policy,
                            format!("backstop user {who} q4w sum overflows i128"),
                        ));
                        qok = false;
                    }
                },
                None => {
                    out.push(policy_finding(
                        policy,
                        format!("backstop user {who} q4w entry amount not an i128"),
                    ));
                    qok = false;
                }
            }
        }
        if qok {
            match user_shares
                .checked_add(shares)
                .and_then(|x| x.checked_add(qsum))
            {
                Some(x) => user_shares = x,
                None => {
                    out.push(Safe::literal("backstop user shares sum overflows i128"));
                    users_ok = false;
                }
            }
        } else {
            users_ok = false;
        }
    }
    if users_ok && try_num(&s.backstop_pool["shares"]) != Some(user_shares) {
        out.push(policy_finding(
            policy,
            format!(
                "backstop shares {} != sum user shares+q4w {user_shares}",
                s.backstop_pool["shares"]
            ),
        ));
    }
    let lp_at_backstop = match s.balances.get(&Tok::Lp).and_then(|h| h.get("backstop")) {
        Some(b) => match b.parse::<i128>() {
            Ok(v) => Some(v),
            Err(_) => {
                out.push(policy_finding(
                    policy,
                    format!("lp balance at backstop unparseable ({b})"),
                ));
                None
            }
        },
        None => {
            out.push(Safe::literal("lp balance at backstop not observed"));
            None
        }
    };
    if let Some(lp) = lp_at_backstop {
        if try_num(&s.backstop_pool["tokens"]) != Some(lp) {
            out.push(policy_finding(
                policy,
                format!(
                    "backstop tokens {} != lp balance {lp}",
                    s.backstop_pool["tokens"]
                ),
            ));
        }
    }
    match s.reward_zone.as_array() {
        None => out.push(Safe::literal(
            "reward_zone missing/malformed (array expected)",
        )),
        Some(a) if inv_policy == InvariantPolicy::Fork && !a.is_empty() => {
            out.push(Safe::literal("reward_zone not empty"));
        }
        Some(_) => {}
    }
    out
}

/// Observable state a rejected call must not have changed.
///
/// Deliberately a narrow claim, and it must read as one: it compares the
/// persistent raw rows `snapshot` captured (value bytes only — `last_modified`
/// and `live_until` are TTL bookkeeping that plain reads bump, not contract
/// effects) plus the persisted typed views: pool config, reserve config,
/// all positions, backstop pool/user data, the reward zone, token balances
/// and auctions. The typed reserve `data` view is excluded: `get_reserve`
/// recomputes rates and `last_time` at observation time, so it moves across
/// a bracketing pair even when a rejected call wrote nothing; persisted
/// reserve amounts/rates are judged from the raw `ResData` row instead.
/// Storage the tool cannot key is outside the claim; for a rejected call
/// nothing in the captured set may move. Sound only for a snapshot pair
/// bracketing the single rejected invocation: emission rows and accrued
/// views move across ledger time, so comparing against a previous step's
/// snapshot would report ordinary waits as violations.
pub fn no_state_change(policy: Policy, before: &Snapshot, after: &Snapshot) -> Vec<Safe> {
    let mut out = Vec::new();
    let mut keys: Vec<&String> = before.raw.keys().chain(after.raw.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        match (before.raw.get(k), after.raw.get(k)) {
            (Some(b), Some(a)) => match (b.as_ref(), a.as_ref()) {
                (Some(b), Some(a)) if b.key != a.key || b.val != a.val => {
                    out.push(policy_finding(policy, format!("raw entry changed: {k}")))
                }
                // Observed-absent on one side and a row on the other is a
                // presence flip: written (or expired) inside the bracket.
                (Some(_), None) | (None, Some(_)) => out.push(policy_finding(
                    policy,
                    format!("raw entry presence changed: {k}"),
                )),
                (Some(_), Some(_)) | (None, None) => {}
            },
            (None, Some(_)) => out.push(policy_finding(
                policy,
                format!("raw entry {k} not observed before"),
            )),
            (Some(_), None) => out.push(policy_finding(
                policy,
                format!("raw entry {k} not observed after"),
            )),
            (None, None) => {}
        }
    }
    for (group, changed) in [
        ("pool config", before.config != after.config),
        ("reserve config", reserve_config_changed(before, after)),
        ("positions", before.positions != after.positions),
        (
            "backstop pool data",
            before.backstop_pool != after.backstop_pool,
        ),
        (
            "backstop users",
            before.backstop_users != after.backstop_users,
        ),
        ("reward zone", before.reward_zone != after.reward_zone),
        ("token balances", before.balances != after.balances),
        ("auctions", before.auctions != after.auctions),
    ] {
        if changed {
            out.push(policy_finding(
                policy,
                format!("observable state changed: {group}"),
            ));
        }
    }
    out
}

fn reserve_config_changed(before: &Snapshot, after: &Snapshot) -> bool {
    before
        .reserves
        .keys()
        .chain(after.reserves.keys())
        .any(|tok| {
            before.reserves.get(tok).and_then(|r| r.get("config"))
                != after.reserves.get(tok).and_then(|r| r.get("config"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Contracts, ReserveRef};
    use crate::stellar::{fake::FakeExecutor, Net};

    /// Expected finding bytes via the derived-Deserialize route.
    fn finding(text: &str) -> Safe {
        serde_json::from_value(json!(text)).unwrap()
    }

    fn side() -> SideManifest {
        SideManifest {
            label: "x".into(),
            deploy_ledger: 1,
            wasm: BTreeMap::new(),
            contracts: Contracts {
                oracle: "CORACLE".into(),
                stable: "CSTABLE".into(),
                usdc: "CUSDC".into(),
                ..Default::default()
            },
            reserves: [
                (
                    Tok::Stable,
                    ReserveRef {
                        index: 0,
                        decimals: 6,
                    },
                ),
                (
                    Tok::Usdc,
                    ReserveRef {
                        index: 1,
                        decimals: 7,
                    },
                ),
            ]
            .into(),
            minted: [(Tok::Stable, 100)].into(),
        }
    }

    fn snap(d_supply: i128, liab: i128) -> Snapshot {
        let empty = json!({"collateral": {}, "liabilities": {}, "supply": {}});
        let mut positions = BTreeMap::new();
        for w in Who::ALL {
            positions.insert(w.as_str().to_string(), empty.clone());
        }
        positions.insert(
            "sam".into(),
            json!({"collateral": {}, "liabilities": {"0": liab.to_string()}, "supply": {}}),
        );
        positions.insert("pool".into(), empty.clone());
        positions.insert("backstop".into(), empty);
        let mut balances = BTreeMap::new();
        for tok in Tok::ALL {
            balances.insert(
                tok,
                [
                    ("frodo".to_string(), "0".to_string()),
                    ("pool".to_string(), "0".to_string()),
                    ("backstop".to_string(), "0".to_string()),
                ]
                .into(),
            );
        }
        balances.insert(
            Tok::Stable,
            [
                ("frodo".to_string(), "60".to_string()),
                ("pool".to_string(), "40".to_string()),
            ]
            .into(),
        );
        let backstop_users = Who::ALL
            .iter()
            .map(|w| (*w, json!({"shares": "0", "q4w": []})))
            .collect();
        Snapshot {
            positions,
            reserves: [
                (
                    Tok::Stable,
                    json!({"asset": "CSTABLE", "config": {"index": 0}, "data": {"d_supply": d_supply.to_string(), "b_supply": "0", "b_rate": "1", "d_rate": "1", "backstop_credit": "0"}}),
                ),
                (
                    Tok::Usdc,
                    json!({"asset": "CUSDC", "config": {"index": 1}, "data": {"d_supply": "0", "b_supply": "0", "b_rate": "1", "d_rate": "1", "backstop_credit": "0"}}),
                ),
            ]
            .into(),
            config: json!({"bstop_rate": 0, "oracle": "CORACLE"}),
            backstop_pool: json!({"shares": "0", "tokens": "0"}),
            backstop_users,
            reward_zone: json!([]),
            balances,
            auctions: BTreeMap::new(),
            raw: [
                ("pool_emis".to_string(), None),
                (
                    "res_data_stable".to_string(),
                    Some(RawEntry {
                        key: "rd".into(),
                        val: "AAA".into(),
                        last_modified: 1,
                        live_until: None,
                    }),
                ),
                (
                    "res_data_usdc".to_string(),
                    Some(RawEntry {
                        key: "rd".into(),
                        val: "AAA".into(),
                        last_modified: 1,
                        live_until: None,
                    }),
                ),
            ]
            .into(),
        }
    }

    #[test]
    fn invariants_hold_on_consistent_snapshot() {
        assert!(invariants(
            Policy::Pair,
            &side(),
            &snap(5, 5),
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .is_empty());
    }

    #[test]
    fn invariants_flag_debt_mismatch_and_conservation() {
        let v = invariants(
            Policy::Pair,
            &side(),
            &snap(5, 7),
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(v.iter().any(|x| x.to_string().contains("d_supply")));
        let mut s = snap(5, 5);
        s.balances
            .get_mut(&Tok::Stable)
            .unwrap()
            .insert("pool".into(), "41".into());
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .iter()
        .any(|x| x.to_string().contains("minted")));
    }

    #[test]
    fn stock_accepts_conserved_backstop_debt_and_fork_rejects_the_position() {
        // Stock's bad_debt path parks defaulted debt on the backstop address;
        // per-reserve conservation must count it and stock may hold it.
        let mut s = snap(8, 5);
        s.positions.insert(
            "backstop".into(),
            json!({"collateral": {}, "liabilities": {"0": "3"}, "supply": {}}),
        );
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .is_empty());
        // The fork keeps the conservation law but forbids the position.
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork,
        );
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("backstop address holds liabilities")),
            "{v:?}"
        );
        assert!(
            !v.iter().any(|x| x.to_string().contains("d_supply")),
            "{v:?}"
        );
    }

    #[test]
    fn stock_accepts_conserved_pool_custody_and_rejects_uncompensated_claims() {
        let mut s = snap(5, 5);
        s.positions.insert(
            "pool".into(),
            json!({"collateral": {"0": "3"}, "liabilities": {"0": "2"}, "supply": {}}),
        );
        s.reserves.get_mut(&Tok::Stable).unwrap()["data"]["d_supply"] = json!("7");
        s.reserves.get_mut(&Tok::Stable).unwrap()["data"]["b_supply"] = json!("3");
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .is_empty());

        let mut s = snap(7, 7);
        s.positions.insert(
            "pool".into(),
            json!({"collateral": {"0": "1"}, "liabilities": {"0": "2"}, "supply": {}}),
        );
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(
            v.iter().any(|x| x.to_string().contains("stable: d_supply")),
            "{v:?}"
        );
        assert!(
            v.iter().any(|x| x.to_string().contains("stable: b_supply")),
            "{v:?}"
        );
    }

    #[test]
    fn wrong_reserve_debt_cannot_be_covered_by_another_asset() {
        // Same-sized debt as the conserved stock control, but booked under
        // usdc (index 1) while stable (index 0) claims it: BOTH per-reserve
        // conservation laws must fail; nothing nets across assets.
        let mut s = snap(5, 5);
        s.positions.insert(
            "sam".into(),
            json!({"collateral": {}, "liabilities": {"1": "5"}, "supply": {}}),
        );
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(
            v.iter().any(|x| x.to_string().contains("stable: d_supply")),
            "{v:?}"
        );
        assert!(
            v.iter().any(|x| x.to_string().contains("usdc: d_supply")),
            "{v:?}"
        );
    }

    #[test]
    fn unexpected_position_holders_are_rejected_not_dropped() {
        let mut s = snap(5, 5);
        s.positions.insert(
            "mallory".into(),
            json!({"collateral": {"0": "1"}, "liabilities": {"0": "1"}, "supply": {"0": "1"}}),
        );
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(
            v.iter()
                .any(|x| x == &finding("positions[mallory] unexpected holder")),
            "{v:?}"
        );

        let mut s = snap(5, 5);
        s.positions.insert(
            "mallory".into(),
            json!({"collateral": {}, "liabilities": {}, "supply": {}}),
        );
        assert!(
            invariants(
                Policy::Pair,
                &side(),
                &s,
                &BTreeMap::new(),
                InvariantPolicy::Stock
            )
            .iter()
            .any(|x| x == &finding("positions[mallory] unexpected holder")),
            "unexpected holder must fail even when empty"
        );
    }

    #[test]
    fn malformed_or_missing_observations_fail_instead_of_certifying_absence() {
        // A null reward zone once read as "empty": the drained predicate's
        // favourite false PASS.
        let mut s = snap(5, 5);
        s.reward_zone = json!(null);
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("reward_zone")));

        let mut s = snap(5, 5);
        s.positions.remove("pool");
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("positions[pool]")));

        let mut s = snap(5, 5);
        s.balances
            .get_mut(&Tok::Stable)
            .unwrap()
            .insert("pool".into(), "1e9".into());
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("unparseable")));

        let mut s = snap(5, 5);
        s.balances.remove(&Tok::Xlm);
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("balances not observed")));

        let mut s = snap(5, 5);
        s.raw.remove("pool_emis");
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("pool_emis row not observed")));

        // A configured reserve's `ResData` is required-present: never read
        // and read-absent are both violations, not quiet passes.
        let mut s = snap(5, 5);
        s.raw.remove("res_data_stable");
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("raw ResData row not observed")));

        let mut s = snap(5, 5);
        s.raw.insert("res_data_stable".into(), None);
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("absent for a configured reserve")));

        let mut s = snap(5, 5);
        s.backstop_users.remove(&Who::Sam);
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("backstop user sam not observed")));

        let mut s = snap(5, 5);
        s.reserves.insert(Tok::Stable, json!(null));
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .iter()
        .any(|x| x.to_string().contains("reserve data missing/malformed")));
    }

    #[test]
    fn observed_empty_maps_stay_passing() {
        // The distinction goes both ways: a genuinely empty, observed zone/map
        // must not be flagged, or the strictness would only manufacture noise.
        let s = snap(5, 5);
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork
        )
        .is_empty());
    }

    #[test]
    fn fork_zero_restrictions_fire_only_under_fork() {
        let mut s = snap(5, 5);
        s.config["bstop_rate"] = json!(5_000_000);
        s.raw.insert(
            "pool_emis".into(),
            Some(RawEntry {
                key: "pe".into(),
                val: "AAE".into(),
                last_modified: 1,
                live_until: None,
            }),
        );
        s.reserves.get_mut(&Tok::Stable).unwrap()["data"]["backstop_credit"] = json!("1");
        s.reward_zone = json!([{"who": "frodo"}]);
        // Stock may legally take, credit, emit and pay rewards; the fork's
        // zero assumptions are applied here, never via message filtering.
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .is_empty());
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork,
        );
        for want in [
            "bstop_rate != 0",
            "PoolEmis map exists",
            "backstop_credit != 0",
            "reward_zone not empty",
        ] {
            assert!(
                v.iter().any(|x| x.to_string().contains(want)),
                "{want} missing: {v:?}"
            );
        }
        // The contract's own reserve range is universal: >= 1.0 is malformed
        // data on either side, not a policy choice.
        let mut s = snap(5, 5);
        s.config["bstop_rate"] = json!(10_000_000);
        for policy in [InvariantPolicy::Stock, InvariantPolicy::Fork] {
            assert!(
                invariants(Policy::Pair, &side(), &s, &BTreeMap::new(), policy)
                    .iter()
                    .any(|x| x.to_string().contains("bstop_rate outside")),
                "{policy:?}"
            );
        }
    }

    #[test]
    fn manifest_identity_tampering_never_certifies() {
        for policy in [InvariantPolicy::Stock, InvariantPolicy::Fork] {
            let mut s = snap(5, 5);
            s.config["oracle"] = json!("CAMPER");
            let v = invariants(Policy::Pair, &side(), &s, &BTreeMap::new(), policy);
            assert!(
                v.iter().any(|x| x.to_string().contains("config.oracle")),
                "{policy:?} {v:?}"
            );

            let mut s = snap(5, 5);
            s.reserves.get_mut(&Tok::Usdc).unwrap()["asset"] = json!("CSTABLE");
            let v = invariants(Policy::Pair, &side(), &s, &BTreeMap::new(), policy);
            assert!(
                v.iter()
                    .any(|x| x.to_string().contains("usdc: reserve asset")),
                "{policy:?} {v:?}"
            );

            let mut s = snap(5, 5);
            s.reserves.get_mut(&Tok::Usdc).unwrap()["config"]["index"] = json!(0);
            let v = invariants(Policy::Pair, &side(), &s, &BTreeMap::new(), policy);
            assert!(
                v.iter()
                    .any(|x| x.to_string().contains("usdc: reserve config.index")),
                "{policy:?} {v:?}"
            );
        }
    }

    #[test]
    fn negative_actual_quantities_fail_under_either_policy() {
        let mut s = snap(5, 5);
        s.balances
            .get_mut(&Tok::Stable)
            .unwrap()
            .insert("frodo".into(), "-1".into());
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("balance frodo negative")),
            "{v:?}"
        );

        let mut s = snap(5, 5);
        s.positions.insert(
            "frodo".into(),
            json!({"collateral": {"0": "-1"}, "liabilities": {}, "supply": {}}),
        );
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("collateral[0] negative")),
            "{v:?}"
        );

        let mut s = snap(5, 5);
        s.backstop_users
            .insert(Who::Sam, json!({"shares": "-1", "q4w": []}));
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork,
        );
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("sam shares negative")),
            "{v:?}"
        );

        let mut s = snap(5, 5);
        s.backstop_users
            .insert(Who::Sam, json!({"shares": "0", "q4w": [{"amount": "-1"}]}));
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Fork,
        );
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("q4w amount negative")),
            "{v:?}"
        );
    }

    #[test]
    fn no_state_change_ignores_ttl_bookkeeping_but_flags_state_and_views() {
        let raw = |val: &str, lm: u32, live: u32| {
            Some(RawEntry {
                key: "k".into(),
                val: val.into(),
                last_modified: lm,
                live_until: Some(live),
            })
        };
        let mut b = snap(5, 5);
        let mut a = snap(5, 5);
        b.raw.insert("res_data_stable".into(), raw("AAA", 10, 100));
        a.raw.insert("res_data_stable".into(), raw("AAA", 11, 140));
        // Plain reads bump the TTL columns; that bookkeeping is not a state change.
        assert!(no_state_change(Policy::Pair, &b, &a).is_empty());

        a.raw.insert("res_data_stable".into(), raw("BBB", 11, 140));
        let v = no_state_change(Policy::Pair, &b, &a);
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("raw entry changed: res_data_stable")),
            "{v:?}"
        );

        let mut a2 = snap(5, 5);
        a2.config = json!({"bstop_rate": 1});
        assert!(no_state_change(Policy::Pair, &snap(5, 5), &a2)
            .iter()
            .any(|x| x
                .to_string()
                .contains("observable state changed: pool config")));

        let mut a3 = snap(5, 5);
        a3.raw.remove("pool_emis");
        assert!(no_state_change(Policy::Pair, &snap(5, 5), &a3)
            .iter()
            .any(|x| x.to_string().contains("not observed after")));
    }

    #[test]
    fn map_entries_accepts_both_encodings() {
        assert_eq!(
            map_entries(&json!({"0": "1"})),
            vec![("0".to_string(), json!("1"))]
        );
        assert_eq!(
            map_entries(&json!([[0, "1"]])),
            vec![("0".to_string(), json!("1"))]
        );
        assert!(strict_map(&json!(null)).is_none());
        assert!(strict_map(&json!("")).is_none());
        assert!(strict_map(&json!([[0]])).is_none());
        assert_eq!(strict_map(&json!([[0, "1"]])).unwrap().len(), 1);
    }

    fn auction_net() -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: "pp".into(),
            config_dir: "/cfg".into(),
            headers: Vec::new(),
        }
    }

    fn auction_st<'a>(net: &'a Net, fx: &'a FakeExecutor) -> Stellar<'a, FakeExecutor> {
        let mut st = Stellar::new(net, fx, Policy::Pair);
        st.sleep = |_| {};
        st
    }

    #[test]
    fn auction_key_encodes_the_contracttype_struct() {
        // pool/src/storage.rs::AuctionKey is a struct key (sorted symbol
        // map), not a tuple variant: the raw probe must encode it exactly.
        // Literal-vs-literal, so this pins intent and guards against edits;
        // the real encoding evidence is the run loop's read-back-after-
        // creation control (auction_observed), not this comparison.
        assert_eq!(
            auction_key("GAU"),
            json!({"vec": [
                {"symbol": "Auction"},
                {"map": [
                    {"key": {"symbol": "auct_type"}, "val": {"u32": 0}},
                    {"key": {"symbol": "user"}, "val": {"address": "GAU"}}
                ]}
            ]})
        );
    }

    #[test]
    fn raw_absent_auction_row_is_absence_not_an_error() {
        let net = auction_net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", ""),
            FakeExecutor::fail(
                "error: no matching contract data entries were found for the specified contract id",
            ),
        ]);
        let st = auction_st(&net, &fx);
        assert_eq!(
            observe_auction(&st, "CPOOL", "user", "GAU").unwrap(),
            Value::Null
        );
        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            2,
            "absence must not spend a typed get_auction call"
        );
        assert!(
            calls[1].iter().any(|a| a == "temporary"),
            "auctions are temporary storage: {calls:?}"
        );
        assert!(
            calls[1]
                .iter()
                .any(|a| a == "AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="),
            "the exact encoded key must reach the temporary read: {calls:?}"
        );
    }

    #[test]
    fn present_raw_auction_row_must_read_back_typed() {
        let net = auction_net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", ""),
            FakeExecutor::ok(
                "AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==,AAAAAQ==,AAAABw==,AAAAZA==",
                "",
            ),
            FakeExecutor::ok("{\"bid\":{},\"lot\":{},\"block\":7}", ""),
        ]);
        let st = auction_st(&net, &fx);
        let v = observe_auction(&st, "CPOOL", "user", "GAU").unwrap();
        assert_eq!(v["block"], 7);
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 3, "a present row must be read back typed");
        assert!(calls[2].iter().any(|a| a == "get_auction"), "{calls:?}");
    }

    #[test]
    fn present_row_with_failing_typed_read_is_an_error_not_absence() {
        // get_auction traps (UnreachableCodeReached) on a row the raw probe
        // says exists: that is an observation error, never a silent Null.
        let net = auction_net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", ""),
            FakeExecutor::ok(
                "AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==,AAAAAQ==,AAAABw==,AAAAZA==",
                "",
            ),
            FakeExecutor::fail("HostError: Error(Contract, #1211)"),
        ]);
        let st = auction_st(&net, &fx);
        assert!(observe_auction(&st, "CPOOL", "user", "GAU").is_err());
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 3, "a present row must be read back typed");
        assert!(calls[2].iter().any(|a| a == "get_auction"), "{calls:?}");
    }

    #[test]
    fn present_row_with_malformed_typed_value_is_an_error() {
        let net = auction_net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", ""),
            FakeExecutor::ok(
                "AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==,AAAAAQ==,AAAABw==,AAAAZA==",
                "",
            ),
            FakeExecutor::ok("null", ""),
        ]);
        let st = auction_st(&net, &fx);
        assert!(observe_auction(&st, "CPOOL", "user", "GAU").is_err());
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 3, "a present row must be read back typed");
        assert!(calls[2].iter().any(|a| a == "get_auction"), "{calls:?}");
    }

    #[test]
    fn auction_read_hard_errors_propagate() {
        // Only the exact no-matching-entries diagnostic reads as absence;
        // any other failure must surface, not be guessed away.
        let net = auction_net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("AAAADwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", ""),
            FakeExecutor::fail("HostError: Error(Contract, #1211)"),
        ]);
        let st = auction_st(&net, &fx);
        assert!(observe_auction(&st, "CPOOL", "user", "GAU").is_err());
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 2, "the raw read must be attempted once");
        assert!(
            calls[1].iter().any(|a| a == "temporary"),
            "auctions are temporary storage: {calls:?}"
        );
    }

    #[test]
    fn a_created_auction_must_be_observed() {
        let mut s = snap(5, 5);
        assert!(
            auction_observed(Policy::Pair, &s, Who::Sam).is_some(),
            "missing entry is a finding"
        );
        s.auctions.insert(Who::Sam, Value::Null);
        assert!(auction_observed(Policy::Pair, &s, Who::Sam)
            .unwrap()
            .to_string()
            .contains("observed absent"));
        s.auctions
            .insert(Who::Sam, json!({"block": 1, "bid": {}, "lot": {}}));
        assert!(auction_observed(Policy::Pair, &s, Who::Sam).is_none());
        s.auctions
            .insert(Who::Sam, json!({"block": 1, "bid": {}, "lot": null}));
        assert!(auction_observed(Policy::Pair, &s, Who::Sam)
            .unwrap()
            .to_string()
            .contains("observed malformed"));
    }

    #[test]
    fn unknown_duplicate_and_non_numeric_reserve_keys_are_rejected() {
        let mut s = snap(5, 5);
        s.positions.insert(
            "frodo".into(),
            json!({"collateral": {"7": "1"}, "liabilities": {}, "supply": {}}),
        );
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .iter()
        .any(|x| x == &finding("positions[frodo].collateral has unknown reserve key 7")));

        let mut s = snap(5, 5);
        s.positions.insert(
            "frodo".into(),
            json!({"collateral": {}, "liabilities": [["0", "1"], ["0", "2"]], "supply": {}}),
        );
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .iter()
        .any(|x| x.to_string().contains("duplicate reserve key 0")));

        let mut s = snap(5, 5);
        s.positions.insert(
            "frodo".into(),
            json!({"collateral": {"x": "1"}, "liabilities": {}, "supply": {}}),
        );
        assert!(invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock
        )
        .iter()
        .any(|x| x.to_string().contains("non-reserve key x")));
    }

    #[test]
    fn auction_asset_keys_must_be_configured_reserves() {
        let mut s = snap(5, 5);
        s.auctions.insert(
            Who::Sam,
            json!({"block": 1, "bid": {"CSTABLE": "5"}, "lot": {"CDUNK": "1"}}),
        );
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(
            v.iter().any(|x| x
                .to_string()
                .contains("lot has unknown reserve asset CDUNK")),
            "{v:?}"
        );
        assert!(
            !v.iter().any(|x| x.to_string().contains("bid has unknown")),
            "{v:?}"
        );
        s.auctions.insert(
            Who::Sam,
            json!({"block": 1, "bid": {"CSTABLE": "-5"}, "lot": {}}),
        );
        let v = invariants(
            Policy::Pair,
            &side(),
            &s,
            &BTreeMap::new(),
            InvariantPolicy::Stock,
        );
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("bid[CSTABLE] negative")),
            "{v:?}"
        );
    }

    #[test]
    fn minted_overflow_is_a_finding_not_a_panic() {
        let mut extra = BTreeMap::new();
        extra.insert(Tok::Stable, i64::MAX);
        assert!(invariants(
            Policy::Pair,
            &side(),
            &snap(5, 5),
            &extra,
            InvariantPolicy::Stock
        )
        .iter()
        .any(|x| x.to_string().contains("minted total overflows")));
    }

    #[test]
    fn no_state_change_tolerates_getter_rate_drift_but_flags_persisted_state() {
        let reserve = |rate: &str| {
            json!({"config": {"supply_cap": "100"},
                   "data": {"d_supply": "5", "b_supply": "0", "b_rate": rate,
                            "d_rate": rate, "backstop_credit": "0", "last_time": rate}})
        };
        // Positive control: only the accrued getter view moved; the raw
        // persisted row is untouched, so nothing may be reported.
        let mut b = snap(5, 5);
        let mut a = snap(5, 5);
        b.reserves.insert(Tok::Stable, reserve("1"));
        a.reserves.insert(Tok::Stable, reserve("2"));
        let v = no_state_change(Policy::Pair, &b, &a);
        assert!(
            v.is_empty(),
            "observation-time rate drift is not a state change: {v:?}"
        );

        // Persisted rate bytes moving is a real state change.
        let mut a = snap(5, 5);
        a.raw.insert(
            "res_data_stable".into(),
            Some(RawEntry {
                key: "rd".into(),
                val: "AAB".into(),
                last_modified: 1,
                live_until: None,
            }),
        );
        let v = no_state_change(Policy::Pair, &snap(5, 5), &a);
        assert!(
            v.iter()
                .any(|x| x.to_string().contains("raw entry changed: res_data_stable")),
            "{v:?}"
        );

        // A typed config write is a real state change.
        let mut a = snap(5, 5);
        a.reserves.insert(
            Tok::Stable,
            json!({"config": {"supply_cap": "101"}, "data": {}}),
        );
        let v = no_state_change(Policy::Pair, &snap(5, 5), &a);
        assert!(
            v.iter().any(|x| x
                .to_string()
                .contains("observable state changed: reserve config")),
            "{v:?}"
        );

        // An observed-absent row becoming present is a presence flip.
        let mut a = snap(5, 5);
        a.raw.insert(
            "pool_emis".into(),
            Some(RawEntry {
                key: "pe".into(),
                val: "AAE".into(),
                last_modified: 1,
                live_until: None,
            }),
        );
        let v = no_state_change(Policy::Pair, &snap(5, 5), &a);
        assert!(
            v.iter().any(|x| x
                .to_string()
                .contains("raw entry presence changed: pool_emis")),
            "{v:?}"
        );
    }
}
