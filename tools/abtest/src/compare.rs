//! Cross-side classification of one step: Match, Expected (per profile), Violation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{PairManifest, Profile, Tok, Who};
use crate::observe::{map_entries, no_state_change, try_num, StepRecord};
use crate::run::check_expectation;
use crate::scenario::{self, Expect, LabeledStep, Step, TrapCall};
use crate::stellar::{ErrorKind, Event, Policy, Safe};
use crate::timeline::{emits_named_loss, emits_pool_loss};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Class {
    Match,
    Expected(String),
    Violation(String),
}

pub type Diff = Vec<(String, Class)>;

const IGNORED_LEAVES: [&str; 6] = [
    "last_time",
    "block",
    "exp",
    "timestamp",
    "ir_mod",
    "last_modified",
];
const FORK_ONLY_EVENTS: [&str; 4] = [
    "debt_setoff",
    "collateral_orphaned",
    "orphan_settled",
    "defaulted_debt",
];
const STOCK_ONLY_EVENTS: [&str; 1] = ["bad_debt"];

fn outcome(r: &StepRecord) -> String {
    match &r.invocation {
        None => "none".into(),
        Some(i) if i.ok => "ok".into(),
        Some(i) => match &i.error {
            Some(ErrorKind::Contract(c)) => format!("err({c})"),
            Some(ErrorKind::Auth) => "auth".into(),
            Some(ErrorKind::Other(o)) => {
                Safe::error_chain(Policy::Pair, &anyhow::anyhow!("other({o})")).to_string()
            }
            None => "err".into(),
        },
    }
}

fn event_names(r: &StepRecord) -> Vec<String> {
    let mut v: Vec<String> = r
        .invocation
        .iter()
        .flat_map(|i| i.events.iter())
        .filter_map(|e| {
            e.topics
                .first()
                .and_then(|t| t["symbol"].as_str())
                .map(str::to_string)
        })
        .collect();
    v.sort();
    v
}

/// Resolve only compiled probes; a label prefix is not authority.
fn probe(label: &str) -> Option<&'static LabeledStep> {
    scenario::current_step(label).filter(|s| s.expect == Expect::PerSide)
}

fn backstop_liabilities_empty(r: &StepRecord) -> bool {
    r.snapshot
        .positions
        .get("backstop")
        .and_then(|p| p.get("liabilities"))
        .map_or(true, |v| map_entries(v).is_empty())
}

fn action_event_names(step: &Step) -> &'static [&'static str] {
    match step {
        Step::Trap(t) => match t {
            TrapCall::BackstopDistribute => &["distribute"],
            TrapCall::BackstopGulpEmissions => &["gulp_emissions"],
            TrapCall::BackstopAddReward => &["rw_zone_add"],
            TrapCall::BackstopRemoveReward => &["rw_zone_remove"],
            TrapCall::BackstopClaim => &["claim"],
            // backstop/src/contract.rs:305 publishes no backstop event; anything
            // observed there comes from the emitter, which is not grounded here.
            TrapCall::BackstopDrop => &[],
            TrapCall::PoolUpdatePool => &["update_pool"],
            TrapCall::PoolFlashLoan => &["flash_loan"],
            // pool/src/events.rs:117 - emission config changes publish this topic.
            TrapCall::PoolSetEmissionsConfig => &["reserve_emission_update"],
        },
        Step::QueueSetReserve { .. } => &["queue_set_reserve"],
        Step::SetStatus(0) => &["set_status"],
        Step::Gulp { .. } => &["gulp"],
        _ => &[],
    }
}

fn future_borrow_step(step: &Step) -> bool {
    matches!(step, Step::Submit { from: Who::Frodo, reqs }
        if reqs.len() == 1
            && reqs[0].kind == scenario::req::BORROW
            && reqs[0].asset == Tok::Xlm)
}

fn path_under(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}."))
}

/// Exact state footprint supported by the probe's action and captured evidence.
/// Queue, gulp, and traps have no demonstrated persistent typed-state footprint:
/// only their event outcome is sanctioned. Backstop accounting remains the
/// typed invariant policy's responsibility, not this comparator's. The freeze
/// probe's `config.status` spread is sanctioned only by the exact 0-vs-4 leaf
/// clause in `compare_step`, never here — a probe label waives no other spread.
fn probe_state_allowed(step: &Step, path: &str, structural: bool) -> bool {
    future_borrow_step(step)
        && (path_under(path, "positions.frodo.liabilities")
            || matches!(
                path,
                "balances.xlm.frodo" | "balances.xlm.pool" | "reserves.xlm.data.d_supply"
            )
            || (structural
                && matches!(path, "balances.xlm" | "reserves.xlm" | "reserves.xlm.data")))
}

fn designed_stock_probe(step: &LabeledStep, a: &StepRecord, b: &StepRecord) -> bool {
    b.invocation.as_ref().is_some_and(|i| !i.ok)
        && [('a', a), ('b', b)].into_iter().all(|(side, record)| {
            let Some(inv) = &record.invocation else {
                return false;
            };
            check_expectation(
                Policy::Pair,
                Profile::StockVsFork,
                side,
                step,
                Some(inv),
                record.before.as_ref(),
            )
            .is_empty()
                && (inv.ok
                    || (inv.events.is_empty()
                        && record.before.as_ref().is_some_and(|before| {
                            no_state_change(Policy::Pair, before, &record.snapshot).is_empty()
                        })))
        })
}

/// The complete event footprint a compiled single-borrow request authorizes:
/// exactly one `borrow` on the side's own pool for the exact asset/borrower
/// carrying the requested quantity, and exactly one SEP-41 `transfer` of that
/// asset's own token contract from that pool to that borrower for that exact
/// amount. Duplicates, foreign contracts, wrong parties/amounts, malformed
/// payloads, and any additional event are not admitted — one matching transfer
/// inside a name set never blesses the step.
fn future_borrow_footprint_ok(step: &Step, a: &StepRecord, m: &PairManifest) -> bool {
    let Step::Submit { from, reqs } = step else {
        return false;
    };
    let [req] = reqs.as_slice() else {
        return false;
    };
    let Ok(amount) = req.amount.parse::<i128>() else {
        return false;
    };
    let Some(inv) = &a.invocation else {
        return false;
    };
    if !inv.ok || inv.events.len() != 2 {
        return false;
    }
    let pool = m.a.contracts.pool.as_str();
    let token = m.a.contracts.token(req.asset);
    let borrower = m.addr(*from);
    let (mut borrow, mut transfer) = (false, false);
    for e in &inv.events {
        match e.topics.first().and_then(|t| t["symbol"].as_str()) {
            Some("borrow") if !borrow => borrow = borrow_event_ok(e, pool, token, borrower, amount),
            Some("transfer") if !transfer => {
                transfer = transfer_event_ok(e, token, pool, borrower, amount)
            }
            _ => return false,
        }
    }
    borrow && transfer
}

fn single_eq(v: &Value, key: &str, want: &str) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.get(key).and_then(Value::as_str) == Some(want))
}

fn sole_i128(v: &Value) -> Option<i128> {
    v.as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("i128"))
        .and_then(try_num)
}

// The SCVal grammar is read strictly: a topic or value carrying any tagged key
// beyond its sole discriminator/quantity is malformed evidence, not evidence
// with an ignorable extra.
fn borrow_event_ok(e: &Event, pool: &str, asset: &str, borrower: &str, amount: i128) -> bool {
    let Some(values) = e
        .value
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("vec"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if values.len() != 2 {
        return false;
    }
    e.contract == pool
        && e.topics.len() == 3
        && single_eq(&e.topics[0], "symbol", "borrow")
        && single_eq(&e.topics[1], "address", asset)
        && single_eq(&e.topics[2], "address", borrower)
        && sole_i128(&values[0]) == Some(amount)
        && sole_i128(&values[1]).is_some_and(|d| d >= 0)
}

fn transfer_event_ok(
    e: &Event,
    token: &str,
    from_pool: &str,
    borrower: &str,
    amount: i128,
) -> bool {
    e.contract == token
        && e.topics.len() == 3
        && single_eq(&e.topics[0], "symbol", "transfer")
        && single_eq(&e.topics[1], "address", from_pool)
        && single_eq(&e.topics[2], "address", borrower)
        && sole_i128(&e.value) == Some(amount)
}

/// Compare two sides' records for one step.
///
/// `delta_dependent` exempts only auction bid/lot amounts. Structural state is
/// always compared. Expected cross-side state is sanctioned only by a
/// successful stock future borrow carrying its complete authenticated footprint;
/// an inert rejected side sanctions no spread. Backstop liability amounts are
/// deliberately outside this comparator's key-set coverage; accounting divergences
/// belong to the typed invariant policy.
pub fn compare_step(
    m: &PairManifest,
    label: &str,
    a: &StepRecord,
    b: &StepRecord,
    delta_dependent: bool,
    pool_loss_seen: bool,
    freeze_split_seen: bool,
) -> Diff {
    let mut out = Diff::new();
    let p = m.profile;
    let tolerance = tolerance(p);
    let compiled = if p == Profile::StockVsFork {
        probe(label)
    } else {
        None
    };
    let designed = compiled.is_some_and(|s| designed_stock_probe(s, a, b));
    let future_borrow_valid = compiled
        .is_some_and(|s| future_borrow_step(&s.step) && future_borrow_footprint_ok(&s.step, a, m));

    // 1. outcome
    let (oa, ob) = (outcome(a), outcome(b));
    if oa != ob {
        // `designed_stock_probe` certifies both sides against the compiled
        // contract: stock recorded ok or a definite eventless/unchanged failure,
        // fork the exact declared rejection and unchanged. Any spread between
        // those two certified outcomes is the designed divergence.
        let expected = designed;
        let msg = format!("a={oa} b={ob}");
        out.push((
            "outcome".into(),
            if expected {
                Class::Expected(format!("designed probe: {msg}"))
            } else {
                Class::Violation(msg)
            },
        ));
    }

    // 2. events
    let (ea, eb) = (event_names(a), event_names(b));
    if ea != eb {
        let only_a: Vec<_> = ea.iter().filter(|e| !eb.contains(e)).cloned().collect();
        let only_b: Vec<_> = eb.iter().filter(|e| !ea.contains(e)).cloned().collect();
        let probe_events = compiled.is_some_and(|s| {
            only_b.is_empty()
                && if future_borrow_step(&s.step) {
                    future_borrow_valid
                } else {
                    only_a
                        .iter()
                        .all(|e| action_event_names(&s.step).contains(&e.as_str()))
                }
        });
        let loss_direction = p == Profile::StockVsFork
            && !only_a.is_empty()
            && !only_b.is_empty()
            && only_a
                .iter()
                .all(|e| STOCK_ONLY_EVENTS.contains(&e.as_str()) && emits_named_loss(a, e, &m.a))
            && only_b
                .iter()
                .all(|e| FORK_ONLY_EVENTS.contains(&e.as_str()) && emits_named_loss(b, e, &m.b));
        let allowed = designed && probe_events || loss_direction;
        let msg = format!("only_a={only_a:?} only_b={only_b:?}");
        out.push((
            "events".into(),
            if allowed {
                Class::Expected(msg)
            } else {
                Class::Violation(msg)
            },
        ));
    } else if compiled.is_some_and(|s| {
        future_borrow_step(&s.step)
            && a.invocation.as_ref().is_some_and(|i| i.ok)
            && !future_borrow_valid
    }) {
        // An executed future borrow must show its complete authorized footprint
        // even where event names agree across sides: an empty-equal or
        // name-equal pair does not prove completeness.
        out.push((
            "events".into(),
            Class::Violation(format!(
                "future borrow executed without its complete authorized footprint: a={ea:?}"
            )),
        ));
    }

    // State spreads are sanctioned only by a future borrow stock actually
    // executed with its full authenticated footprint: an inert failed-stock
    // probe certifies no footprint, so an inherited spread stays a Violation.
    let state_sanctioned = designed && future_borrow_valid;

    // 3. holders, key sets, and custody. A holder or position kind missing from
    // either side is a structural corruption, not a panic or a silent empty map.
    let custody_expected = p == Profile::StockVsFork
        && (pool_loss_seen || emits_pool_loss(a, &m.a) || emits_pool_loss(b, &m.b));
    let stock_routes_loss = p == Profile::StockVsFork
        && (pool_loss_seen || emits_named_loss(a, "bad_debt", &m.a))
        && backstop_liabilities_empty(b);
    let holders: std::collections::BTreeSet<&String> = a
        .snapshot
        .positions
        .keys()
        .chain(b.snapshot.positions.keys())
        .collect();
    for who in holders {
        let pa = a.snapshot.positions.get(who);
        let pb = b.snapshot.positions.get(who);
        if pa.is_none() || pb.is_none() {
            let path = format!("positions.{who}.keys");
            let msg = format!("a={:?} b={:?}", pa.is_some(), pb.is_some());
            let expected = (who == "pool" && custody_expected)
                || (who == "backstop" && stock_routes_loss)
                || (state_sanctioned
                    && compiled.is_some_and(|s| probe_state_allowed(&s.step, &path, true)));
            out.push((
                path,
                if expected {
                    Class::Expected(msg)
                } else {
                    Class::Violation(msg)
                },
            ));
            continue;
        }
        for kind in ["collateral", "liabilities", "supply"] {
            let va = pa.unwrap().get(kind);
            let vb = pb.unwrap().get(kind);
            let expected_kind = |path: &str, kind_expected: bool| {
                (who == "pool" && custody_expected)
                    || (who == "backstop" && stock_routes_loss && kind_expected)
                    || (state_sanctioned
                        && compiled.is_some_and(|s| probe_state_allowed(&s.step, path, true)))
            };
            match (va, vb) {
                (None, None) => {}
                (None, Some(_)) | (Some(_), None) => {
                    let path = format!("positions.{who}.{kind}");
                    let msg = format!("a={:?} b={:?}", va.is_some(), vb.is_some());
                    let expected = expected_kind(&path, kind == "liabilities");
                    out.push((
                        path,
                        if expected {
                            Class::Expected(msg)
                        } else {
                            Class::Violation(msg)
                        },
                    ));
                }
                (Some(x), Some(y)) => {
                    let ka = map_entries(x)
                        .into_iter()
                        .map(|(k, _)| k)
                        .collect::<Vec<_>>();
                    let kb = map_entries(y)
                        .into_iter()
                        .map(|(k, _)| k)
                        .collect::<Vec<_>>();
                    if ka != kb {
                        let path = format!("positions.{who}.{kind}.keys");
                        let msg = format!("a={ka:?} b={kb:?}");
                        let expected = expected_kind(&path, kind == "liabilities");
                        out.push((
                            path,
                            if expected {
                                Class::Expected(msg)
                            } else {
                                Class::Violation(msg)
                            },
                        ));
                    }
                }
            }
        }
    }
    let pool_a = a.snapshot.positions.get("pool");
    let pool_b = b.snapshot.positions.get("pool");
    if pool_a != pool_b {
        let expected = custody_expected
            || stock_routes_loss
            || (state_sanctioned
                && compiled.is_some_and(|s| probe_state_allowed(&s.step, "positions.pool", true)));
        out.push((
            "positions.pool".into(),
            if expected {
                Class::Expected(format!("sanctioned custody divergence (`{label}`)"))
            } else {
                Class::Violation("custody differs".into())
            },
        ));
    }

    // 4/5. leaf diffs. Structural disagreements are never delta-waived.
    let mut sa = serde_json::to_value(&a.snapshot).unwrap();
    let mut sb = serde_json::to_value(&b.snapshot).unwrap();
    for (s, oracle) in [
        (&mut sa, m.a.contracts.oracle.as_str()),
        (&mut sb, m.b.contracts.oracle.as_str()),
    ] {
        if let Value::Object(object) = s {
            object.remove("raw");
            if let Some(Value::Object(pos)) = object.get_mut("positions") {
                pos.remove("pool");
                pos.remove("backstop");
            }
        }
        canonicalize_oracle(s, oracle);
        canonicalize_auction_keys(s);
    }
    let mut leaf_diffs = Vec::new();
    diff_values("", &sa, &sb, tolerance, &mut leaf_diffs);
    for d in leaf_diffs {
        let probe_state = state_sanctioned
            && compiled.is_some_and(|s| probe_state_allowed(&s.step, &d.path, d.structural));
        let stock_credit = p == Profile::StockVsFork
            && d.path.ends_with(".data.backstop_credit")
            && try_num(&d.a).is_some_and(|x| x != 0)
            && try_num(&d.b) == Some(0);
        let freeze_split = p == Profile::StockVsFork
            && d.path == "config.status"
            && try_num(&d.a) == Some(0)
            && try_num(&d.b) == Some(4)
            && (freeze_split_seen || label == scenario::FREEZE_DOWNGRADE_PROBE);
        let class = if probe_state || stock_credit || freeze_split {
            Class::Expected(d.msg)
        } else if d.structural {
            Class::Violation(format!("structural: {}", d.msg))
        } else if delta_dependent && auction_amount_leaf(&d.path) {
            Class::Expected(format!("delta-dependent auction amount: {}", d.msg))
        } else {
            Class::Violation(d.msg)
        };
        out.push((d.path, class));
    }
    out
}

fn is_address(v: &Value) -> bool {
    matches!(v, Value::String(s) if s.len() == 56 && (s.starts_with('C') || s.starts_with('G')))
}

/// The pool's configured oracle is itself a per-side deployed contract
/// (`deploy.rs` salts it), and `observe::invariants` already pins each side's
/// `config.oracle` to its own manifest address. Canonicalize only that exact
/// manifest match; any other value stays raw, so a substituted or forged oracle
/// stays a difference instead of a waived address pair.
fn canonicalize_oracle(s: &mut Value, oracle: &str) {
    if oracle.is_empty() {
        return;
    }
    if let Some(observed) = s.get_mut("config").and_then(|c| c.get_mut("oracle")) {
        if observed.as_str() == Some(oracle) {
            *observed = Value::String("<oracle>".to_string());
        }
    }
}

/// Remap only auction address keys whose per-side asset is known from that
/// snapshot. Unknown foreign keys remain raw, so two foreign assets can never
/// collide into a generic label and hide a cross-auction substitution.
fn canonicalize_auction_keys(s: &mut Value) {
    use std::collections::BTreeMap;
    let mut addr_to_tok: BTreeMap<String, String> = BTreeMap::new();
    if let Some(res) = s.get("reserves").and_then(|r| r.as_object()) {
        for (tok, data) in res {
            if let Some(a) = data.get("asset").and_then(Value::as_str) {
                addr_to_tok.insert(a.to_string(), tok.clone());
            }
        }
    }
    fn remap(v: &mut Value, m: &BTreeMap<String, String>) {
        match v {
            Value::Object(map) => {
                let renames: Vec<(String, String)> = map
                    .keys()
                    .filter_map(|k| m.get(k).map(|new| (k.clone(), new.clone())))
                    .filter(|(old, new)| old != new && !map.contains_key(new))
                    .collect();
                for (old, new) in renames {
                    if let Some(val) = map.remove(&old) {
                        map.insert(new, val);
                    }
                }
                for val in map.values_mut() {
                    remap(val, m);
                }
            }
            Value::Array(xs) => {
                for x in xs {
                    remap(x, m);
                }
            }
            _ => {}
        }
    }
    if let Some(a) = s.get_mut("auctions") {
        remap(a, &addr_to_tok);
    }
}

#[derive(Clone)]
struct LeafDiff {
    path: String,
    msg: String,
    structural: bool,
    a: Value,
    b: Value,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Object,
    Array,
    Null,
    Boolean,
    Number,
    Text,
}

fn shape(v: &Value) -> Shape {
    match v {
        Value::Object(_) => Shape::Object,
        Value::Array(_) => Shape::Array,
        Value::Null => Shape::Null,
        Value::Bool(_) => Shape::Boolean,
        Value::Number(_) => Shape::Number,
        Value::String(s) if s.parse::<i128>().is_ok() => Shape::Number,
        Value::String(_) => Shape::Text,
    }
}

fn auction_amount_leaf(path: &str) -> bool {
    let mut parts = path.split('.');
    parts.next() == Some("auctions") && parts.any(|p| p == "bid" || p == "lot")
}

/// Only the reserve's own deployed token identity is per-side by construction.
fn reserve_identity_path(path: &str) -> bool {
    let mut parts = path.split('.');
    parts.next() == Some("reserves")
        && parts.next().is_some()
        && parts.next() == Some("asset")
        && parts.next().is_none()
}

#[allow(clippy::too_many_arguments)]
fn leaf_diff(
    path: &str,
    msg: String,
    structural: bool,
    a: &Value,
    b: &Value,
    out: &mut Vec<LeafDiff>,
) {
    out.push(LeafDiff {
        path: path.to_string(),
        msg,
        structural,
        a: a.clone(),
        b: b.clone(),
    });
}

fn diff_values(path: &str, a: &Value, b: &Value, tol: f64, out: &mut Vec<LeafDiff>) {
    let leaf = path.rsplit('.').next().unwrap_or("");
    if IGNORED_LEAVES.contains(&leaf) {
        return;
    }
    let (sa, sb) = (shape(a), shape(b));
    if sa == Shape::Object && sb == Shape::Object {
        let ma = a.as_object().unwrap();
        let mb = b.as_object().unwrap();
        let keys: std::collections::BTreeSet<&String> = ma.keys().chain(mb.keys()).collect();
        for k in keys {
            let p = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            match (ma.get(k), mb.get(k)) {
                (Some(x), Some(y)) => diff_values(&p, x, y, tol, out),
                _ => leaf_diff(
                    &p,
                    "key present on one side only".into(),
                    true,
                    &Value::Null,
                    &Value::Null,
                    out,
                ),
            }
        }
        return;
    }
    if sa == Shape::Array && sb == Shape::Array {
        let xa = a.as_array().unwrap();
        let xb = b.as_array().unwrap();
        if xa.len() != xb.len() {
            leaf_diff(
                path,
                format!("array length a={} b={}", xa.len(), xb.len()),
                true,
                a,
                b,
                out,
            );
            return;
        }
        for (i, (x, y)) in xa.iter().zip(xb).enumerate() {
            diff_values(&format!("{path}.{i}"), x, y, tol, out);
        }
        return;
    }
    if sa != sb {
        leaf_diff(path, format!("type a={sa:?} b={sb:?}"), true, a, b, out);
        return;
    }
    if sa == Shape::Number {
        // A value that cannot be read as i128 must make noise, not compare as 0.
        match (try_num(a), try_num(b)) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => {
                let denom = x.abs().max(y.abs()).max(1) as f64;
                let rel = (x - y).abs() as f64 / denom;
                // Tolerance is never a delta-mode question: ordinary amounts keep
                // their profile tolerance, and delta mode only re-classifies the
                // auction bid/lot leaves (see the caller).
                if rel > tol {
                    leaf_diff(path, format!("a={x} b={y} rel={rel:.2e}"), false, a, b, out);
                }
            }
            _ => leaf_diff(
                path,
                format!("unparseable number a={a} b={b}"),
                true,
                a,
                b,
                out,
            ),
        }
        return;
    }
    if sa == Shape::Text && reserve_identity_path(path) && is_address(a) && is_address(b) {
        return;
    }
    if a != b {
        leaf_diff(path, format!("a={a} b={b}"), true, a, b, out);
    }
}

// Historical string-shaped invariant waivers were removed deliberately. A
// typed invariant policy owns profile-specific accounting divergences; a
// comparator exemption must never infer them from an invariant message.

pub fn tolerance(p: Profile) -> f64 {
    match p {
        Profile::StockVsFork => 1e-5,
        Profile::ForkVsFork => 1e-2,
    }
}

pub fn summarize(diffs: &BTreeMap<String, Diff>) -> (usize, usize) {
    let mut expected = 0;
    let mut violations = 0;
    for d in diffs.values() {
        for (_, c) in d {
            match c {
                Class::Expected(_) => expected += 1,
                Class::Violation(_) => violations += 1,
                Class::Match => {}
            }
        }
    }
    (expected, violations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::Snapshot;
    use crate::stellar::{Event, RawEntry};
    use serde_json::json;

    fn test_address(c: char) -> String {
        format!("{c}{}", "A".repeat(55))
    }

    /// A per-side deployed contract address: `deploy.rs` salts every contract, so
    /// the two deployments never share one.
    fn test_deployed(tag: char) -> String {
        format!("C{tag}{}", "A".repeat(54))
    }

    fn test_manifest(profile: Profile) -> PairManifest {
        let side = crate::manifest::SideManifest {
            label: "test".into(),
            deploy_ledger: 1,
            wasm: BTreeMap::new(),
            contracts: crate::manifest::Contracts {
                pool: test_deployed('A'),
                oracle: test_deployed('O'),
                stable: test_address('C'),
                xlm: test_address('C'),
                ..Default::default()
            },
            reserves: [
                (
                    Tok::Stable,
                    crate::manifest::ReserveRef {
                        index: 0,
                        decimals: 6,
                    },
                ),
                (
                    Tok::Xlm,
                    crate::manifest::ReserveRef {
                        index: 1,
                        decimals: 7,
                    },
                ),
            ]
            .into(),
            minted: BTreeMap::new(),
        };
        let mut b = side.clone();
        b.contracts.pool = test_deployed('B');
        b.contracts.oracle = test_deployed('P');
        PairManifest {
            profile,
            description: None,
            net: crate::stellar::Net {
                rpc_url: "http://localhost".into(),
                passphrase: "test".into(),
                config_dir: "/tmp".into(),
                headers: Vec::new(),
            },
            identities: [
                (Who::Admin, test_address('D')),
                (Who::Frodo, test_address('F')),
                (Who::Sam, test_address('S')),
                (Who::User, test_address('U')),
            ]
            .into(),
            a: side,
            b,
        }
    }

    fn snap(d_rate: &str) -> Snapshot {
        let empty = json!({"collateral": {}, "liabilities": {}, "supply": {}});
        let mut positions = BTreeMap::new();
        for w in ["admin", "frodo", "sam", "user", "pool", "backstop"] {
            positions.insert(w.to_string(), empty.clone());
        }
        Snapshot {
            positions,
            reserves: [(
                crate::manifest::Tok::Stable,
                json!({"asset": test_address('C'), "data": {"d_rate": d_rate, "last_time": 1}}),
            )]
            .into(),
            config: json!({}),
            backstop_pool: json!({}),
            backstop_users: BTreeMap::new(),
            reward_zone: json!([]),
            balances: BTreeMap::new(),
            auctions: BTreeMap::new(),
            raw: BTreeMap::new(),
        }
    }

    /// Each deployment owns a salted pool contract, like `deploy.rs`. Loss
    /// admission must only trust the event's own side: an event must name that
    /// side's pool contract, and the asset topic is passed explicitly — never
    /// derived from the snapshot's first reserve entry.
    fn side_pool(side: &str) -> String {
        test_deployed(if side == "a" { 'A' } else { 'B' })
    }

    fn test_event(name: &str, pool: &str, asset: &str) -> Event {
        let loss = ["defaulted_debt", "bad_debt", "collateral_orphaned"];
        if loss.contains(&name) {
            // Every loss event places the defaulted user before the asset.
            let topics = vec![
                json!({"symbol": name}),
                json!({"address": test_address('U')}),
                json!({"address": asset}),
            ];
            Event {
                contract: pool.into(),
                topics,
                value: json!({"i128": "1"}),
            }
        } else {
            Event {
                contract: "C".into(),
                topics: vec![json!({"symbol": name})],
                value: Value::Null,
            }
        }
    }

    fn rec_inv(
        side: &str,
        ok: bool,
        error: Option<ErrorKind>,
        events: Vec<Event>,
        snap: Snapshot,
        before: Option<Snapshot>,
    ) -> StepRecord {
        let stderr = match &error {
            Some(ErrorKind::Contract(c)) => format!("error: Error(Contract, #{c})"),
            Some(ErrorKind::Auth) => {
                "error: Missing signing key for account GTESTSIGNINGONLYACCOUNTXXXXXXXXXXXXXXX"
                    .into()
            }
            Some(ErrorKind::Other(o)) => format!("error: {o}"),
            None => String::new(),
        };
        StepRecord {
            label: "x".into(),
            side: side.into(),
            invocation: Some(
                serde_json::from_value(json!({
                    "ok": ok,
                    "value": null,
                    "error": serde_json::to_value(&error).unwrap(),
                    "events": serde_json::to_value(&events).unwrap(),
                    "stderr": stderr,
                }))
                .unwrap(),
            ),
            before,
            snapshot: snap,
            violations: vec![],
        }
    }

    fn rec_code(
        side: &str,
        ok: bool,
        error: u32,
        events: &[&str],
        snap: Snapshot,
        before: Option<Snapshot>,
    ) -> StepRecord {
        let pool = side_pool(side);
        let asset = test_address('C');
        let invocation_events = events
            .iter()
            .map(|e| test_event(e, &pool, &asset))
            .collect();
        rec_inv(
            side,
            ok,
            if ok {
                None
            } else {
                Some(ErrorKind::Contract(error))
            },
            invocation_events,
            snap,
            before,
        )
    }

    fn rec(side: &str, ok: bool, events: &[&str], snap: Snapshot) -> StepRecord {
        rec_code(side, ok, 1200, events, snap, None)
    }

    fn designed_probe(
        stock_events: &[&str],
        fork_error: u32,
        sa: Snapshot,
        sb: Snapshot,
    ) -> (StepRecord, StepRecord) {
        let before = sb.clone();
        let a = rec_code("a", true, 0, stock_events, sa, None);
        let b = rec_code("b", false, fork_error, &[], sb, Some(before));
        (a, b)
    }

    /// The complete realistic footprint of the compiled future borrow
    /// (Frodo borrows `100000000` XLM at `oracle_borrow_future_price`), built
    /// only from manifest identities and the compiled request quantity.
    fn borrow_fixture(m: &PairManifest) -> (Event, Event) {
        let pool = m.a.contracts.pool.clone();
        let token = m.a.contracts.token(Tok::Xlm).to_string();
        let frodo = m.addr(Who::Frodo);
        (
            Event {
                contract: pool.clone(),
                topics: vec![
                    json!({"symbol": "borrow"}),
                    json!({"address": token.clone()}),
                    json!({"address": frodo}),
                ],
                value: json!({"vec": [{"i128": "100000000"}, {"i128": "99999637"}]}),
            },
            Event {
                contract: token,
                topics: vec![
                    json!({"symbol": "transfer"}),
                    json!({"address": pool}),
                    json!({"address": frodo}),
                ],
                value: json!({"i128": "100000000"}),
            },
        )
    }

    #[test]
    fn identical_records_match() {
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "borrow",
            &rec("a", true, &["borrow"], snap("1000000000000")),
            &rec("b", true, &["borrow"], snap("1000000000000")),
            false,
            false,
            false,
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn small_drift_matches_large_drift_violates() {
        let ok = compare_step(
            &test_manifest(Profile::StockVsFork),
            "x",
            &rec("a", true, &[], snap("1000000000000")),
            &rec("b", true, &[], snap("1000000001000")),
            false,
            false,
            false,
        );
        assert!(ok.is_empty());
        let bad = compare_step(
            &test_manifest(Profile::StockVsFork),
            "x",
            &rec("a", true, &[], snap("1000000000000")),
            &rec("b", true, &[], snap("1100000000000")),
            false,
            false,
            false,
        );
        assert!(matches!(bad[0].1, Class::Violation(_)));
        // Delta mode exempts auction bid/lot only; it never overrides tolerance.
        let ok_delta = compare_step(
            &test_manifest(Profile::StockVsFork),
            "x",
            &rec("a", true, &[], snap("1000000000000")),
            &rec("b", true, &[], snap("1000000000001")),
            true,
            false,
            false,
        );
        assert!(
            ok_delta.is_empty(),
            "in-tolerance drift in delta mode: {ok_delta:?}"
        );
        let bad_delta = compare_step(
            &test_manifest(Profile::StockVsFork),
            "x",
            &rec("a", true, &[], snap("1000000000000")),
            &rec("b", true, &[], snap("1100000000000")),
            true,
            false,
            false,
        );
        assert!(
            matches!(bad_delta[0].1, Class::Violation(_)),
            "delta mode must not tolerate an out-of-tolerance reserve amount: {bad_delta:?}"
        );
    }

    #[test]
    fn fork_only_event_direction_matters() {
        let e = compare_step(
            &test_manifest(Profile::StockVsFork),
            "direct_default_bad_debt",
            &rec("a", true, &["bad_debt"], snap("1")),
            &rec("b", true, &["defaulted_debt"], snap("1")),
            false,
            false,
            false,
        );
        assert!(
            e.iter().all(|(_, c)| matches!(c, Class::Expected(_))),
            "{e:?}"
        );
        let v = compare_step(
            &test_manifest(Profile::StockVsFork),
            "direct_default_bad_debt",
            &rec("a", true, &["defaulted_debt"], snap("1")),
            &rec("b", true, &["bad_debt"], snap("1")),
            false,
            false,
            false,
        );
        assert!(v.iter().any(|(_, c)| matches!(c, Class::Violation(_))));
        let f = compare_step(
            &test_manifest(Profile::ForkVsFork),
            "direct_default_bad_debt",
            &rec("a", true, &["bad_debt"], snap("1")),
            &rec("b", true, &["defaulted_debt"], snap("1")),
            false,
            false,
            false,
        );
        assert!(f.iter().any(|(_, c)| matches!(c, Class::Violation(_))));
    }

    #[test]
    fn failed_or_foreign_loss_events_do_not_arm_custody() {
        let mut sb = snap("1000000000000");
        sb.positions.insert(
            "pool".into(),
            json!({"collateral": {}, "liabilities": {}, "supply": {"0": "5"}}),
        );
        let failed = rec_code("b", false, 1200, &["defaulted_debt"], sb.clone(), None);
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_final",
            &rec("a", true, &[], snap("1000000000000")),
            &failed,
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "positions.pool" && matches!(c, Class::Violation(_))),
            "a failed invocation must not arm custody: {d:?}"
        );
        // Rewrite the asset topic explicitly to an address that side does
        // not configure; success, a real pool contract, and a positive
        // amount all stay valid.
        let mut foreign = rec("b", true, &["defaulted_debt"], sb.clone());
        for e in foreign.invocation.as_mut().unwrap().events.iter_mut() {
            if let Some(t) = e.topics.last_mut().and_then(|t| t.as_object_mut()) {
                t.insert("address".into(), json!(test_address('F')));
            }
        }
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_final",
            &rec("a", true, &[], snap("1000000000000")),
            &foreign,
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "positions.pool" && matches!(c, Class::Violation(_))),
            "a loss event naming an asset absent from that side must not arm custody: {d:?}"
        );
        // Forged origin: a well-formed loss for a configured asset, positive
        // amount, successful call — but the event names the peer's pool
        // contract instead of the emitting side's own. It must not arm
        // custody on this side.
        let mut forged = rec("b", true, &["defaulted_debt"], sb);
        for e in forged.invocation.as_mut().unwrap().events.iter_mut() {
            e.contract = side_pool("a");
        }
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_final",
            &rec("a", true, &[], snap("1000000000000")),
            &forged,
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "positions.pool" && matches!(c, Class::Violation(_))),
            "a loss event forged under the peer pool contract must not arm custody: {d:?}"
        );
    }

    /// Positions used to be dropped from the numeric diff wholesale, which masked
    /// wrong-user credits: a 10x holder difference must reach the verdict.
    #[test]
    fn a_positions_amount_difference_is_a_violation() {
        let mut sa = snap("1000000000000");
        let mut sb = snap("1000000000000");
        sa.positions.insert(
            "frodo".into(),
            json!({"collateral": {}, "liabilities": {}, "supply": {"0": "100"}}),
        );
        sb.positions.insert(
            "frodo".into(),
            json!({"collateral": {}, "liabilities": {}, "supply": {"0": "1000"}}),
        );
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "borrow",
            &rec("a", true, &["borrow"], sa),
            &rec("b", true, &["borrow"], sb),
            false,
            false,
            false,
        );
        assert!(
            d.iter().any(|(_, c)| matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    /// A value that cannot be read as i128 must make noise, not compare as 0 == 0.
    #[test]
    fn an_unparseable_number_is_reported_not_compared_as_zero() {
        let mut sa = snap("1000000000000");
        let mut sb = snap("1000000000000");
        sa.reserves.insert(
            crate::manifest::Tok::Stable,
            json!({"asset": "A", "config": {}, "data": {"b_supply": "100x", "d_supply": "0", "b_rate": "1", "d_rate": "1000000000000"}}),
        );
        sb.reserves.insert(
            crate::manifest::Tok::Stable,
            json!({"asset": "A", "config": {}, "data": {"b_supply": "100y", "d_supply": "0", "b_rate": "1", "d_rate": "1000000000000"}}),
        );
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "borrow",
            &rec("a", true, &["borrow"], sa),
            &rec("b", true, &["borrow"], sb),
            false,
            false,
            false,
        );
        assert!(
            d.iter().any(|(_, c)| matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    #[test]
    fn trap_outcomes_expected_only_for_stock_vs_fork() {
        let (a, b) = designed_probe(&[], 1200, snap("1"), snap("1"));
        assert!(matches!(
            compare_step(
                &test_manifest(Profile::StockVsFork),
                "trap_pool_update_pool",
                &a,
                &b,
                false,
                false,
                false,
            )[0]
            .1,
            Class::Expected(_)
        ));
        assert!(matches!(
            compare_step(
                &test_manifest(Profile::ForkVsFork),
                "trap_pool_update_pool",
                &a,
                &b,
                false,
                false,
                false,
            )[0]
            .1,
            Class::Violation(_)
        ));
    }

    /// Custody parked by a sanctioned loss event is designed divergence, and it
    /// persists: the same judgment must cover the key-set loop and every step
    /// after the loss, under either profile.
    #[test]
    fn custody_divergence_is_expected_only_once_a_loss_sanctions_it() {
        let a = rec("a", true, &[], snap("1000000000000"));
        let mut sb = snap("1000000000000");
        sb.positions.insert(
            "pool".into(),
            json!({"collateral": {}, "liabilities": {}, "supply": {"0": "5"}}),
        );
        let plain = rec("b", true, &[], sb.clone());
        let loss = rec("b", true, &["defaulted_debt"], sb.clone());

        // No loss anywhere: the divergence is a real finding, key-set included.
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "borrow",
            &a,
            &plain,
            true,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "positions.pool" && matches!(c, Class::Violation(_))),
            "{d:?}"
        );
        assert!(
            d.iter().any(|(p, c)| p == "positions.pool.supply.keys"
                && matches!(c, Class::Violation(_))),
            "the key-set loop must agree with the custody block: {d:?}"
        );

        // On the loss step the custody difference becomes Expected. Pool custody
        // is judged structurally here; delta mode has no bearing on that judgment.
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_final",
            &a,
            &loss,
            true,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "positions.pool" && matches!(c, Class::Expected(_))),
            "{d:?}"
        );
        assert!(
            d.iter()
                .all(|(p, c)| !p.starts_with("positions") || matches!(c, Class::Expected(_))),
            "key set must not contradict custody: {d:?}"
        );

        // After the loss the flag alone carries the judgment...
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "freeze_supply_rejected",
            &a,
            &plain,
            false,
            true,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "positions.pool" && matches!(c, Class::Expected(_))),
            "{d:?}"
        );
        // ...and fork-vs-fork still judges custody divergence hard.
        let d = compare_step(
            &test_manifest(Profile::ForkVsFork),
            "freeze_supply_rejected",
            &a,
            &plain,
            false,
            true,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "positions.pool" && matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    /// `freeze_set_status_0_rejected` is a designed probe: stock takes the
    /// 4 -> 0 downgrade and emits the event; the fork refuses without changing
    /// its captured state. Only that exact footprint is Expected.
    #[test]
    fn freeze_downgrade_probe_is_expected_divergence() {
        let mut sa = snap("1000000000000");
        sa.config = json!({"status": 0});
        let mut sb = snap("1000000000000");
        sb.config = json!({"status": 4});
        let (a, b) = designed_probe(&["set_status"], 1204, sa, sb);
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "freeze_set_status_0_rejected",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(!d.is_empty(), "the probe must report its divergence");
        for (path, c) in &d {
            assert!(matches!(c, Class::Expected(_)), "{path}: {c:?}");
        }
    }

    /// A probe label alone does not waive arbitrary state. The exact status
    /// spread is sanctioned; a different status, or the designed outcome on a
    /// nonprobe label, must remain a Violation.
    #[test]
    fn other_spreads_and_plain_labels_still_violate() {
        let mut sa = snap("1000000000000");
        sa.config = json!({"status": 0});
        let mut sb = snap("1000000000000");
        sb.config = json!({"status": 3});
        let (a, b) = designed_probe(&[], 1204, sa, sb);
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "freeze_set_status_0_rejected",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "config.status" && matches!(c, Class::Violation(_))),
            "{d:?}"
        );
        let mut sa = snap("1000000000000");
        sa.config = json!({"status": 0});
        let mut sb = snap("1000000000000");
        sb.config = json!({"status": 4});
        let (a, b) = designed_probe(&["set_status"], 1204, sa, sb);
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "borrow",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "outcome" && matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    /// Live witness 015: the stock side records a definite signing/auth failure
    /// while the fork returns its exact declared rejection. `PerSide` records
    /// stock's failure without judging it, so with the rejecting sides inert
    /// this spread is the designed divergence, not a Violation.
    #[test]
    fn stock_auth_failure_with_exact_fork_rejection_is_designed_divergence() {
        let sa = snap("1000000000000");
        let sb = snap("1000000000000");
        let before_a = sa.clone();
        let a = rec_inv(
            "a",
            false,
            Some(ErrorKind::Auth),
            vec![],
            sa,
            Some(before_a),
        );
        let before_b = sb.clone();
        let b = rec_code("b", false, 1200, &[], sb, Some(before_b));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "trap_pool_update_pool",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "outcome").map(|(_, c)| c),
                Some(Class::Expected(_))
            ),
            "stock's recorded auth failure is not judged: {d:?}"
        );
        assert!(
            !d.iter().any(|(_, c)| matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    /// Live witness 016: stock rejects the flash-loan trap with its own definite
    /// `Error(Contract, #1205)`, events empty and state unchanged; the fork's
    /// declared 1200 still matches exactly.
    #[test]
    fn stock_contract_rejection_with_exact_fork_rejection_is_designed_divergence() {
        let sa = snap("1000000000000");
        let sb = snap("1000000000000");
        let before_a = sa.clone();
        let a = rec_code("a", false, 1205, &[], sa, Some(before_a));
        let before_b = sb.clone();
        let b = rec_code("b", false, 1200, &[], sb, Some(before_b));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "trap_pool_flash_loan",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "outcome").map(|(_, c)| c),
                Some(Class::Expected(_))
            ),
            "a recorded definite stock rejection is not judged: {d:?}"
        );
        assert!(
            !d.iter().any(|(_, c)| matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    /// The designed exemption belongs to a rejection that proved inert: a
    /// stock-side failure whose own before/after moved state stays a Violation.
    #[test]
    fn stock_rejection_that_mutated_state_is_not_designed_divergence() {
        let mut sa = snap("1000000000000");
        let before_a = sa.clone();
        sa.raw.insert(
            "reserve.d_rate".into(),
            Some(RawEntry {
                key: "reserve.d_rate".into(),
                val: "2000000000000".into(),
                last_modified: 2,
                live_until: None,
            }),
        );
        let a = rec_code("a", false, 1205, &[], sa, Some(before_a));
        let sb = snap("1000000000000");
        let before_b = sb.clone();
        let b = rec_code("b", false, 1200, &[], sb, Some(before_b));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "trap_pool_flash_loan",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "outcome").map(|(_, c)| c),
                Some(Class::Violation(_))
            ),
            "a failure that moved state is not a recorded rejection: {d:?}"
        );
    }

    /// A stock rejection accompanied by events was not inert; the compiled
    /// contract certifies no such footprint, so the spread stays a Violation.
    #[test]
    fn stock_rejection_with_events_is_not_designed_divergence() {
        let sa = snap("1000000000000");
        let before_a = sa.clone();
        let a = rec_code("a", false, 1205, &["interest"], sa, Some(before_a));
        let sb = snap("1000000000000");
        let before_b = sb.clone();
        let b = rec_code("b", false, 1200, &[], sb, Some(before_b));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "trap_pool_update_pool",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "outcome").map(|(_, c)| c),
                Some(Class::Violation(_))
            ),
            "a rejecting stock side must be eventless to be designed: {d:?}"
        );
    }

    /// Symmetric guard: a fork that claims to refuse while its captured
    /// before/after moved state is not the certified exact rejection.
    #[test]
    fn fork_rejection_that_mutated_state_is_not_designed_divergence() {
        let sa = snap("1000000000000");
        let a = rec_code("a", true, 0, &[], sa, None);
        let before_b = snap("1000000000000");
        let mut sb = snap("1000000000000");
        sb.raw.insert(
            "reserve.d_rate".into(),
            Some(RawEntry {
                key: "reserve.d_rate".into(),
                val: "2000000000000".into(),
                last_modified: 2,
                live_until: None,
            }),
        );
        let b = rec_code("b", false, 1200, &[], sb, Some(before_b));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "trap_pool_update_pool",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "outcome").map(|(_, c)| c),
                Some(Class::Violation(_))
            ),
            "a refusing fork must be unchanged to be designed: {d:?}"
        );
    }

    // --- wave 4: classified-divergence guards ------------------------------

    /// The future-borrow probe exempts only the complete footprint the compiled
    /// borrow authorizes: one own-pool borrow and one own-token transfer of the
    /// exact asset/borrower/amount. Missing, duplicate, foreign, or wrong-amount
    /// events are not silently blessed by name-set membership.
    #[test]
    fn probe_event_exemption_requires_the_exact_action_on_the_executing_side() {
        let m = test_manifest(Profile::StockVsFork);
        let (borrow, transfer) = borrow_fixture(&m);
        let row = |a_events: Vec<Event>, b_events: Vec<Event>| {
            let sa = snap("1000000000000");
            let sb = snap("1000000000000");
            let before = sb.clone();
            let a = rec_inv("a", true, None, a_events, sa, None);
            let b = rec_inv(
                "b",
                false,
                Some(ErrorKind::Contract(1210)),
                b_events,
                sb,
                Some(before),
            );
            compare_step(
                &m,
                "oracle_borrow_future_price",
                &a,
                &b,
                false,
                false,
                false,
            )
            .into_iter()
            .find(|(p, _)| p == "events")
            .map(|(_, c)| c)
        };
        let pair = || vec![borrow.clone(), transfer.clone()];
        assert!(
            matches!(row(pair(), vec![]), Some(Class::Expected(_))),
            "the compiled borrow's own pool borrow plus exact own-token transfer is the sanctioned footprint"
        );
        assert!(
            matches!(row(vec![borrow.clone()], vec![]), Some(Class::Violation(_))),
            "a borrow with no matching transfer is not a complete footprint"
        );
        assert!(
            matches!(
                row(vec![transfer.clone()], vec![]),
                Some(Class::Violation(_))
            ),
            "a bare transfer is not the compiled borrow"
        );
        let wrong_amount = Event {
            value: json!({"i128": "99999999"}),
            ..transfer.clone()
        };
        assert!(
            matches!(
                row(vec![borrow.clone(), wrong_amount], vec![]),
                Some(Class::Violation(_))
            ),
            "a wrong-amount transfer is forged evidence, not a name match"
        );
        let foreign_token = Event {
            contract: test_address('X'),
            ..transfer.clone()
        };
        assert!(
            matches!(
                row(vec![borrow.clone(), foreign_token], vec![]),
                Some(Class::Violation(_))
            ),
            "a transfer published by a foreign token contract proves nothing"
        );
        let wrong_borrower = Event {
            topics: vec![
                json!({"symbol": "transfer"}),
                json!({"address": m.a.contracts.pool.clone()}),
                json!({"address": test_address('S')}),
            ],
            ..transfer.clone()
        };
        assert!(
            matches!(
                row(vec![borrow.clone(), wrong_borrower], vec![]),
                Some(Class::Violation(_))
            ),
            "a transfer to someone other than the compiled borrower is not sanctioned"
        );
        assert!(
            matches!(
                row(
                    vec![borrow.clone(), transfer.clone(), transfer.clone()],
                    vec![]
                ),
                Some(Class::Violation(_))
            ),
            "a duplicate transfer exceeds the single authorized move"
        );
        let foreign_pool = Event {
            contract: test_deployed('Z'),
            ..borrow.clone()
        };
        assert!(
            matches!(
                row(vec![foreign_pool, transfer.clone()], vec![]),
                Some(Class::Violation(_))
            ),
            "a borrow published by a foreign pool proves nothing"
        );
        let unrelated = test_event("interest", &side_pool("a"), &test_address('C'));
        assert!(
            matches!(row(vec![unrelated], vec![]), Some(Class::Violation(_))),
            "an unrelated stock event is not the compiled action"
        );
        let loss = test_event("bad_debt", &side_pool("a"), &test_address('C'));
        assert!(
            matches!(row(vec![loss], vec![]), Some(Class::Violation(_))),
            "a loss event cannot hide behind a borrow probe"
        );
        let fork_event = test_event("interest", &side_pool("b"), &test_address('C'));
        assert!(
            matches!(row(pair(), vec![fork_event]), Some(Class::Violation(_))),
            "the refusing fork may not manufacture state-bearing events"
        );
        assert!(
            matches!(row(vec![], vec![]), Some(Class::Violation(_))),
            "an executed borrow with no events at all is not a complete footprint"
        );
        let bloated_value = Event {
            value: json!({"i128": "100000000", "vec": []}),
            ..transfer.clone()
        };
        assert!(
            matches!(
                row(vec![borrow.clone(), bloated_value], vec![]),
                Some(Class::Violation(_))
            ),
            "a value carrying tagged keys beyond its sole i128 is malformed evidence"
        );
        let bloated_topic = Event {
            topics: vec![
                json!({"symbol": "borrow", "address": test_address('X')}),
                json!({"address": m.a.contracts.token(Tok::Xlm)}),
                json!({"address": m.addr(Who::Frodo)}),
            ],
            ..borrow.clone()
        };
        assert!(
            matches!(
                row(vec![bloated_topic, transfer.clone()], vec![]),
                Some(Class::Violation(_))
            ),
            "a topic carrying more than its sole discriminator/address is malformed evidence"
        );
    }

    /// The probe's state footprint is sanctioned only when stock actually
    /// executed the borrow with the full authenticated pair: a future-borrow
    /// probe whose stock side failed inertly keeps its inherited liability
    /// spread a Violation even though the outcome row is Expected.
    #[test]
    fn failed_stock_future_borrow_does_not_authorize_its_state_footprint() {
        let mut sa = snap("1000000000000");
        sa.positions.insert(
            "frodo".into(),
            json!({"collateral": {}, "liabilities": {"1": "100000000"}, "supply": {}}),
        );
        let before_a = sa.clone();
        let a = rec_code("a", false, 1205, &[], sa, Some(before_a));
        let sb = snap("1000000000000");
        let before_b = sb.clone();
        let b = rec_code("b", false, 1210, &[], sb, Some(before_b));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "oracle_borrow_future_price",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "outcome").map(|(_, c)| c),
                Some(Class::Expected(_))
            ),
            "the inert stock failure is still a recorded outcome: {d:?}"
        );
        assert!(
            d.iter()
                .any(|(p, c)| p.starts_with("positions.frodo.liabilities")
                    && matches!(c, Class::Violation(_))),
            "a rejected probe cannot sanction its own liability spread: {d:?}"
        );
    }

    #[test]
    fn future_price_borrow_footprint_is_sanctioned_and_reserve_drift_is_not() {
        let base = snap("1000000000000");
        let mut sa = base.clone();
        sa.balances.insert(
            Tok::Xlm,
            [
                ("frodo".to_string(), "100000000".to_string()),
                ("pool".to_string(), "900000000".to_string()),
            ]
            .into(),
        );
        sa.positions.insert(
            "frodo".into(),
            json!({"collateral": {}, "liabilities": {"1": "100000000"}, "supply": {}}),
        );
        sa.reserves.insert(
            Tok::Xlm,
            json!({"asset": test_address('C'), "data": {"d_supply": "100000000"}}),
        );
        let m = test_manifest(Profile::StockVsFork);
        let (borrow, transfer) = borrow_fixture(&m);
        let before_b = base.clone();
        let a = rec_inv("a", true, None, vec![borrow, transfer], sa, None);
        let b = rec_inv(
            "b",
            false,
            Some(ErrorKind::Contract(1210)),
            vec![],
            base,
            Some(before_b),
        );
        let d = compare_step(
            &m,
            "oracle_borrow_future_price",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            !d.iter().any(|(_, c)| matches!(c, Class::Violation(_))),
            "the compiled future borrow's exact footprint is intentional: {d:?}"
        );

        let mut sa = snap("1000000000000");
        let mut sb = snap("1000000000000");
        sa.reserves
            .get_mut(&Tok::Stable)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .get_mut("data")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("d_supply".into(), json!("100"));
        sb.reserves
            .get_mut(&Tok::Stable)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .get_mut("data")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("d_supply".into(), json!("0"));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "borrow",
            &rec("a", true, &[], sa),
            &rec("b", true, &[], sb),
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "reserves.stable.data.d_supply"
                    && matches!(c, Class::Violation(_))),
            "the comparator must not suppress a wrong-reserve supply invariant: {d:?}"
        );
    }

    #[test]
    fn sealed_queue_probe_must_not_exempt_a_rejected_reserve_config_change() {
        let mut clean = snap("1000000000000");
        clean.reserves.insert(
            Tok::Xlm,
            json!({"asset":"A","config":{"c_factor":100000,"index":1},"data":{}}),
        );
        let sa = clean.clone();
        let mut sb = clean.clone();
        sb.reserves
            .get_mut(&Tok::Xlm)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .get_mut("config")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("c_factor".into(), json!(1));
        let a = rec_code("a", true, 0, &["queue_set_reserve"], sa, None);
        let b = rec_code("b", false, 1202, &[], sb, Some(clean));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "sealed_queue_modify",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "reserves.xlm.config.c_factor"
                    && matches!(c, Class::Violation(_))),
            "the rejected fork's sealed config moved and its state is not sanctioned: {d:?}"
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "outcome" && matches!(c, Class::Violation(_))),
            "a rejected side cannot change state and still receive the probe exemption: {d:?}"
        );
    }

    #[test]
    fn per_side_probes_diverge_as_expected_plain_labels_do_not() {
        let mut sa = snap("1000000000000");
        let mut sb = snap("1000000000000");
        sa.reserves.insert(
            Tok::Xlm,
            json!({"asset":"A","config":{"c_factor":100000,"index":1},"data":{}}),
        );
        sb.reserves.insert(
            Tok::Xlm,
            json!({"asset":"A","config":{"c_factor":100000,"index":1},"data":{}}),
        );
        let (a, b) = designed_probe(&["queue_set_reserve"], 1202, sa, sb);
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "sealed_queue_modify",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            !d.iter().any(|(_, c)| matches!(c, Class::Violation(_))),
            "{d:?}"
        );
        let (a, b) = designed_probe(&[], 1202, snap("1"), snap("1"));
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "borrow",
            &a,
            &b,
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "outcome" && matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    #[test]
    fn snapshot_gulp_credit_expected_when_only_stock_holds_it() {
        let credit = |a_credit: i64, b_credit: i64| {
            let mut sa = snap("1000000000000");
            let mut sb = snap("1000000000000");
            sa.reserves.get_mut(&crate::manifest::Tok::Stable).unwrap()["data"]
                ["backstop_credit"] = json!(a_credit);
            sb.reserves.get_mut(&crate::manifest::Tok::Stable).unwrap()["data"]
                ["backstop_credit"] = json!(b_credit);
            compare_step(
                &test_manifest(Profile::StockVsFork),
                "gulp_credit_spread",
                &rec("a", true, &[], sa),
                &rec("b", true, &[], sb),
                false,
                false,
                false,
            )
        };
        let d = credit(5, 0);
        assert!(
            d.iter()
                .any(|(p, c)| p.ends_with(".data.backstop_credit")
                    && matches!(c, Class::Expected(_))),
            "{d:?}"
        );
        let d = credit(10, 5);
        assert!(
            d.iter()
                .any(|(p, c)| p.ends_with(".data.backstop_credit")
                    && matches!(c, Class::Violation(_))),
            "a credit on the guarded side is not stock-only: {d:?}"
        );
    }

    #[test]
    fn config_status_split_requires_the_probe() {
        let status_row = |freeze_seen: bool, label: &str| {
            let mut sa = snap("1000000000000");
            let mut sb = snap("1000000000000");
            sa.config = json!({"status": 0});
            sb.config = json!({"status": 4});
            compare_step(
                &test_manifest(Profile::StockVsFork),
                label,
                &rec("a", true, &[], sa),
                &rec("b", true, &[], sb),
                false,
                false,
                freeze_seen,
            )
            .into_iter()
            .find(|(p, _)| p == "config.status")
            .map(|(_, c)| c)
        };
        assert!(
            matches!(status_row(false, "later_step"), Some(Class::Violation(_))),
            "0-vs-4 before the probe is a real divergence"
        );
        assert!(matches!(
            status_row(true, "later_step"),
            Some(Class::Expected(_))
        ));
        assert!(matches!(
            status_row(false, crate::scenario::FREEZE_DOWNGRADE_PROBE),
            Some(Class::Expected(_))
        ));
    }

    #[test]
    fn auction_keys_map_to_reserve_names_not_per_side_addresses() {
        let addr = |c: char| format!("{c}{}", "A".repeat(55));
        let (ca, cb) = (addr('C'), addr('G'));
        let side = |asset: &str| {
            let mut s = snap("1000000000000");
            s.reserves.insert(
                crate::manifest::Tok::Stable,
                json!({"asset": asset, "data": {"d_rate": "1000000000000", "last_time": 1}}),
            );
            s.auctions
                .insert(crate::manifest::Who::Sam, json!({"bid": {asset: "100"}}));
            s
        };
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_final",
            &rec("a", true, &[], side(&ca)),
            &rec("b", true, &[], side(&cb)),
            false,
            false,
            false,
        );
        assert!(
            d.is_empty(),
            "known per-side identity must canonicalize: {d:?}"
        );
    }

    /// Each deployment owns a salted oracle (`deploy.rs`), so `config.oracle` is
    /// compared as that side's manifest identity: never literal address equality,
    /// and never waived for a substituted address.
    #[test]
    fn config_oracle_canonicalizes_only_against_its_own_manifest() {
        let m = test_manifest(Profile::StockVsFork);
        let side = |oracle: &str| {
            let mut s = snap("1000000000000");
            s.config["oracle"] = json!(oracle);
            s
        };
        let d = compare_step(
            &m,
            "x",
            &rec("a", true, &[], side(&m.a.contracts.oracle)),
            &rec("b", true, &[], side(&m.b.contracts.oracle)),
            false,
            false,
            false,
        );
        assert!(
            d.iter().all(|(p, _)| p != "config.oracle"),
            "each side's own deployed oracle is per-side identity: {d:?}"
        );

        // The other side's genuine address is still a substitution here.
        let d = compare_step(
            &m,
            "x",
            &rec("a", true, &[], side(&m.b.contracts.oracle)),
            &rec("b", true, &[], side(&m.b.contracts.oracle)),
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "config.oracle").map(|(_, c)| c),
                Some(Class::Violation(_))
            ),
            "a swapped oracle must stay a violation: {d:?}"
        );

        let d = compare_step(
            &m,
            "x",
            &rec("a", true, &[], side(&m.a.contracts.oracle)),
            &rec("b", true, &[], side(&test_deployed('Z'))),
            false,
            false,
            false,
        );
        assert!(
            matches!(
                d.iter().find(|(p, _)| p == "config.oracle").map(|(_, c)| c),
                Some(Class::Violation(_))
            ),
            "a foreign oracle must stay a violation: {d:?}"
        );
    }

    #[test]
    fn foreign_auction_asset_keys_cannot_collide_into_a_generic_label() {
        let addr = |c: char| format!("{c}{}", "A".repeat(55));
        let side = |bid_key: &str| {
            let mut s = snap("1000000000000");
            s.reserves.insert(
                crate::manifest::Tok::Stable,
                json!({"asset": addr('C'), "data": {"d_rate": "1000000000000", "last_time": 1}}),
            );
            s.auctions
                .insert(crate::manifest::Who::Sam, json!({"bid": {bid_key: "100"}}));
            s
        };
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_final",
            &rec("a", true, &[], side(&addr('C'))),
            &rec("b", true, &[], side(&addr('G'))),
            false,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p.contains("auctions.sam.bid") && matches!(c, Class::Violation(_))),
            "an unmapped foreign asset must remain a distinct key: {d:?}"
        );
    }

    #[test]
    fn delta_mode_cannot_hide_a_deleted_or_typed_auction() {
        let mut sa = snap("1000000000000");
        let mut sb = snap("1000000000000");
        sa.auctions
            .insert(crate::manifest::Who::Sam, json!({"bid": {"x": "1"}}));
        sb.auctions.insert(crate::manifest::Who::Sam, Value::Null);
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_final",
            &rec("a", true, &[], sa),
            &rec("b", true, &[], sb),
            true,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p.starts_with("auctions.sam") && matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }

    #[test]
    fn delta_mode_cannot_hide_config_corruption() {
        let mut sa = snap("1000000000000");
        let mut sb = snap("1000000000000");
        sa.config = json!({"status": 4});
        sb.config = json!({"status": "corrupt"});
        let d = compare_step(
            &test_manifest(Profile::StockVsFork),
            "liq_fill_25",
            &rec("a", true, &[], sa),
            &rec("b", true, &[], sb),
            true,
            false,
            false,
        );
        assert!(
            d.iter()
                .any(|(p, c)| p == "config.status" && matches!(c, Class::Violation(_))),
            "{d:?}"
        );
    }
}
