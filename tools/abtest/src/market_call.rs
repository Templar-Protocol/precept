//! Guarded generic invocation core for manifest-bound market contracts
//! (plan §3/§4): one shared preparation/execution core for CLI and TUI.
//!
//! `prepare` performs only read-only native CLI calls (version gate, live
//! interface discovery, `keys public-key`, and a `--send no` simulation)
//! and returns a `PreparedCall` whose bounded `Safe` preview binds the
//! exact testnet, manifest content digest, live target and whole-interface
//! spec digest, method, every canonical argument value, the resolved
//! signer G-address, the fee envelope, and the simulated effect. A failed
//! native simulation yields no executable action: `prepare` returns `Err`
//! with a sanitized outcome. Nothing here parses or encodes XDR: the
//! native CLI is the only parser and the only transport.
//!
//! `execute` consumes the `PreparedCall` (one attempt per object by move
//! semantics) and requires the exact one-use confirmation token from the
//! same process and preparation. Immediately before sending it rechecks
//! the exact testnet, re-resolves the live interface and the signer,
//! re-resolves the manifest digest, and requires the pinned journal
//! integrity gate (`market_status::journal_clear_for_action`). It then
//! durably reserves a collision-free intent (generic-action phases
//! 900..=999, slots 00..=99, `create_new`, never an overwrite), performs
//! exactly one `Stellar::invoke_bound` (explicit `--send yes`, fixed inclusion
//! bid, pinned signer address and alias, single attempt under `Policy::Market`), and:
//! - settles a definite accepted/rejected outcome in place (the pending
//!   directory is completed, never cleared);
//! - leaves an ambiguous outcome unresolved: the intent stays journaled,
//!   no result is written, and no older identical receipt can settle it,
//!   because the journal gate blocks any further action until external
//!   reconciliation.
//!
//! No raw CLI bytes or seeds ever enter this module: every surfaced or
//! persisted string is a checked market `Safe`, and the seed never leaves
//! the CLI's own key storage (only `keys public-key` is run, against the
//! explicit config dir, and status is never used as signer proof).

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
#[cfg(test)]
use std::str::FromStr;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::deploy::MarketJournal;
#[cfg(test)]
use crate::manifest::begin_market_pending;
use crate::manifest::{
    read_market_intent, MarketCustodyRecord, MarketManifest, MarketMutationKind,
    MarketNetwork, MarketPendingResult, MarketStepRecord, RunLock, Sha256Hex, MARKET_NETWORK_LABEL,
    MARKET_TESTNET_PASSPHRASE,
};
use crate::market::{custody_payload_digest, sha256_hex};
use crate::market_explorer::resolve_function;
use crate::market_query::validate_args;
use crate::market_status::{journal_clear_for_action, PENDING_RECORD_CAP};
use crate::stellar::{
    definite_outcome, AccountId, ContractId, Executor, Invocation, Policy, Safe, Stellar, WasmHash,
    MARKET_CAPTURE_STDOUT_CAP, MARKET_TEXT_CAP,
};

/// Generic-action journal phases: a dedicated range that never collides
/// with deploy/play phases. Coordinates are reserved only through the
/// create-new journal API, so nothing is ever overwritten.
pub(crate) const ACTION_PHASES: std::ops::RangeInclusive<u32> = 900..=999;
/// Journal slots per phase (00..=99).
const ACTION_SLOTS: std::ops::RangeInclusive<u32> = 0..=99;

/// Fixed input-free refusal for every confirmation/gate mismatch. The
/// mismatch class is disclosed; no caller value is ever echoed.
const REFUSE: &str = "the confirmed market action failed its safety checks and was not sent";

/// Fee envelope disclosed in every preview (agreed with the CLI seam
/// owner): the fixed inclusion bid plus the variable resource fee.
const FEE_ENVELOPE: &str =
    "inclusion bid 10000 stroops; the resource fee is additional and is not capped by this bid";
/// Preview risk class for any post-deployment callable function.
const RISK_CLASS: &str =
    "generic mutation on the exact Stellar testnet: confirm only what you fully understand";

/// Length bounds for operator-supplied names (control-free as well).
const MAX_ROLE_LEN: usize = 64;
const MAX_METHOD_LEN: usize = 128;
const MAX_ALIAS_LEN: usize = 128;
/// Bounded render ceiling for the canonical preview and custody pre-state.
const INTENT_RENDER_CAP: usize = 65_536;

/// The one-use confirmation token (64 lowercase hex characters).
const CONFIRMATION_LEN: usize = 64;

/// A prepared, never-sent market action. Holds the whole canonical intent
/// binding; is consumed by `execute` (one attempt per value by move
/// semantics); never Clone; its `Debug` renders no argument values, no
/// token, and no alias.
pub struct PreparedCall {
    role: String,
    method: String,
    signer_alias: String,
    signer: AccountId,
    args: Vec<(String, String)>,
    target: ContractId,
    spec_hash: WasmHash,
    manifest_digest: String,
    // Non-Clone run-lock guard: holding it serializes this action for the
    // whole run through human confirmation and execution; dropping the
    // preview releases the lock.
    _guard: RunLock,
    intent_digest: String,
    challenge: String,
    preview: Safe,
}

impl std::fmt::Debug for PreparedCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedCall")
            .field("target", &self.target)
            .field("spec_hash", &self.spec_hash)
            .finish_non_exhaustive()
    }
}

impl PreparedCall {
    /// The bounded, sanitized structured preview for the CLI/TUI confirm
    /// surface. Never raw CLI bytes; never includes a seed.
    pub fn summary(&self) -> &Safe {
        &self.preview
    }

    /// The exact one-use confirmation challenge the human must provide in
    /// this process for this preparation. The CLI/TUI must obtain it
    /// through its own exact-token gate; it is never persisted here.
    pub fn confirmation(&self) -> &str {
        &self.challenge
    }
}

/// Honest execution result: settled acceptance or definite rejection,
/// plus the sanitized outcome text. Accepted call values are returned
/// exactly as the native CLI decoded them (JSON nulls preserved). No
/// transaction hash is ever claimed by this module.
#[derive(Debug)]
pub struct CallResult {
    pub accepted: bool,
    pub outcome: Safe,
    pub value: Option<Value>,
}

/// Pure-local control/name discipline before any executor use.
fn name_ok(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.chars().any(char::is_control)
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Signer alias discipline mirrored from the deploy lane: a plain CLI
/// alias only — never a path, secret, or StrKey.
fn alias_ok(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= MAX_ALIAS_LEN
        && !alias
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\')
        && alias.parse::<AccountId>().is_err()
        && !((alias.starts_with('S') || alias.starts_with('M')) && alias.len() == 56)
}

/// Pure-local exact-testnet discipline: manifest binding and execution
/// network must both be the exact testnet, before and again immediately
/// before any executor use.
fn ensure_exact_testnet<E: Executor>(
    st: &Stellar<'_, E>,
    manifest: &MarketManifest,
) -> anyhow::Result<()> {
    if manifest.network.passphrase.to_string() != MARKET_TESTNET_PASSPHRASE
        || manifest.network.label.to_string() != MARKET_NETWORK_LABEL
        || st.net.passphrase != MARKET_TESTNET_PASSPHRASE
    {
        return Err(anyhow::Error::msg(Safe::literal(
            "market action requires the exact testnet in both the manifest and the execution network",
        )));
    }
    Ok(())
}

/// Frozen unresolved-submission classification (same law as market play):
/// a result that is both "ok" and error-bearing, or whose outcome cannot
/// be established, is unresolved custody and never settles a slot.
fn unresolved_submission(inv: &Invocation) -> bool {
    inv.ok == inv.error.is_some() || !definite_outcome(inv)
}

fn occupied(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

/// One-time CSPRNG read for the fresh confirmation challenge.
fn random_challenge() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom").map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market action confirmation entropy unavailable")
        )
    })?;
    f.read_exact(&mut bytes).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market action confirmation entropy unavailable")
        )
    })?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0x0f)] as char);
    }
    Ok(out)
}

/// Content digest of the manifest as persisted: any manifest change after
/// preparation breaks the binding (fail closed).
fn manifest_digest(manifest: &MarketManifest) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(manifest).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market manifest canonicalization failed")
        )
    })?;
    anyhow::ensure!(
        bytes.len() <= MARKET_CAPTURE_STDOUT_CAP,
        "{}",
        Safe::literal("market manifest exceeds the canonicalization cap")
    );
    Ok(sha256_hex(&bytes))
}

/// The canonical intent the confirmation locks and the journal persists:
/// exact network, manifest binding digest and deploy ledger, role,
/// method, live target and whole-interface spec digest, every argument
/// (name and value), resolved signer, and the fee/risk disclosures. The
/// simulated effect or explicit simulation error is disclosed only in
/// the operator preview bound to the one-use confirmation, never in the
/// pre-send equality payload. `serde_json` maps sort keys, so the
/// serialization is canonical.
#[allow(clippy::too_many_arguments)]
fn canonical_intent(
    manifest: &MarketManifest,
    manifest_digest: &str,
    role: &str,
    method: &str,
    args: &[(String, String)],
    signer: &AccountId,
    target: &ContractId,
    spec_hash: &WasmHash,
) -> Value {
    let mut sorted: Vec<&(String, String)> = args.iter().collect();
    sorted.sort();
    json!({
        "schema": "precept-abtest/market-call-intent/v1",
        "network": {
            "label": manifest.network.label.to_string(),
            "passphrase": manifest.network.passphrase.to_string(),
        },
        "manifest_digest": manifest_digest,
        "deploy_ledger": manifest.deploy_ledger,
        "role": role,
        "method": method,
        "target": target.as_str(),
        "spec_hash": spec_hash.as_str(),
        "args": sorted,
        "signer": signer.to_string(),
        "fee_envelope": FEE_ENVELOPE,
        "risk": RISK_CLASS,
    })
}

/// Prepares one guarded market action through read-only network and ABI
/// checks, signer resolution and one `--send no` simulation. A failed
/// simulation refuses preparation; it never yields a send capability.
#[allow(clippy::too_many_arguments)]
pub fn prepare<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
    method: &str,
    args: &[(String, String)],
    signer_alias: &str,
) -> Result<PreparedCall> {
    // Local discipline first: nothing reaches an executor on malformed
    // input, and the journal gate blocks actions on a run whose custody
    // state is missing, tampered, truncated, or unresolved.
    if !name_ok(role, MAX_ROLE_LEN) {
        return Err(anyhow::Error::msg(Safe::literal(
            "market call role is malformed",
        )));
    }
    if !name_ok(method, MAX_METHOD_LEN) {
        return Err(anyhow::Error::msg(Safe::literal(
            "market call method is malformed",
        )));
    }
    if !alias_ok(signer_alias) {
        return Err(anyhow::Error::msg(Safe::literal(
            "market call signer alias must be a plain CLI name, not a path or a secret",
        )));
    }
    validate_args(args)?;
    ensure_exact_testnet(st, manifest)?;
    let mdigest = manifest_digest(manifest)?;
    // Serialize market actions for the whole run before the journal gate
    // or any executor use, and hold the guard inside the returned value
    // through human confirmation and execution. A run owned by another
    // holder refuses with a fixed message; never forced. Dropping the
    // preview releases the lock and cancels the action.
    // ponytail: global per-run serialization is a deliberate throughput
    // ceiling; upgrade path is per-action locking, not a forced takeover.
    let _run_guard = crate::manifest::RunLock::acquire(out)
        .map_err(|_| anyhow::Error::msg(Safe::literal("market run lock is unavailable")))?
        .ok_or_else(|| {
            anyhow::Error::msg(Safe::literal(
                "market action refused: this run is already executing another action",
            ))
        })?;
    if !journal_clear_for_action(out) {
        return Err(anyhow::Error::msg(Safe::literal(
            "market journal is not clear for a guarded action",
        )));
    }
    st.assert_rpc_testnet()?;

    // Live-spec authority: resolve the target and function from the live
    // interface (attachment digests rechecked there); no static whitelist.
    let live = resolve_function(st, out, manifest, role, method)?;
    let inputs: HashSet<&str> = live.inputs.iter().map(String::as_str).collect();
    let names: HashSet<&str> = args.iter().map(|(n, _)| n.as_str()).collect();
    if names != inputs {
        return Err(anyhow::Error::msg(Safe::literal(
            "market call arguments must exactly match the selected live contract function inputs",
        )));
    }

    // Signer is resolved from the explicit config dir as its public key;
    // the seed never leaves CLI storage and status is never signer proof.
    let signer = st.keys_public_key(signer_alias)?.ok_or_else(|| {
        anyhow::Error::msg(Safe::literal(
            "market call signer alias is not present in the configured key directory",
        ))
    })?;

    // Preview effect: one simulation through the native CLI
    // (`--send no`). A failed native simulation yields no executable
    // action: `prepare` returns `Err` with a sanitized outcome, so invalid
    // typed arguments and contract failures can never reach send through
    // user confirmation. Previewing itself sends nothing, and the native
    // CLI is the only parser and the only transport.
    let kv: Vec<(&str, String)> = args.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
    let effect: Value = st.view(live.target.as_str(), signer_alias, method, &kv)?;
    let simulated = json!({"simulated": true, "effect": effect});

    let intent = canonical_intent(
        manifest,
        &mdigest,
        role,
        method,
        args,
        &signer,
        &live.target,
        &live.spec_hash,
    );
    let intent_bytes = serde_json::to_vec(&intent).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market action intent canonicalization failed")
        )
    })?;
    let intent_digest = sha256_hex(&intent_bytes);

    let challenge = random_challenge()?;
    let mut preview = intent;
    preview["simulated"] = simulated;
    let preview = Safe::render_market_json(&preview, INTENT_RENDER_CAP)?;

    Ok(PreparedCall {
        role: role.to_string(),
        method: method.to_string(),
        signer_alias: signer_alias.to_string(),
        signer,
        args: args.to_vec(),
        target: live.target,
        spec_hash: live.spec_hash,
        manifest_digest: mdigest,
        intent_digest,
        _guard: _run_guard,
        challenge,
        preview,
    })
}

/// Executes a fully revalidated, human-confirmed action exactly once.
/// Consumes the `PreparedCall`; requires the exact one-use confirmation
/// from this same process; re-resolves network, manifest, live interface,
/// and signer immediately before sending; requires the journal integrity
/// gate; durably reserves a collision-free intent before the single
/// invocation; settles definite outcomes in place and leaves any ambiguous
/// outcome unresolved and blocking. The run lock serializes market actions
/// for the whole run: it is acquired before the journal integrity check
/// and held through the single submission and result finish. The send
/// originates from the signer G-address frozen in the prepared call
/// (`--source <frozen G>`) and names the alias for CLI signing only
/// (`--sign-with-key <alias>`); the alias is re-resolved immediately
/// before submission and a vanished or rebound alias refuses before any
/// send (SEC003). Never a seed.
pub fn execute<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    prepared: PreparedCall,
    confirmation: &str,
) -> Result<CallResult> {
    // One-use confirmation: exact length, then constant-time compare.
    // A stale or mismatched token refuses the call with zero executor
    // use beyond this point; the consumed value cannot be replayed.
    anyhow::ensure!(
        confirmation.len() == CONFIRMATION_LEN,
        "{}",
        refuse_no_echo()
    );
    let want = prepared.challenge.as_bytes();
    let got = confirmation.as_bytes();
    let same = want.len() == got.len()
        && want
            .iter()
            .zip(got.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    anyhow::ensure!(same, "{}", refuse_no_echo());
    // The run lock is owned by `prepared` (acquired in `prepare`) and is
    // held through reservation, the single submission, and result finish;
    // dropped when this function returns, on every path.
    // The presenter still holds its preview-time object. Reopen the
    // persisted manifest under the run lock; hashing only that object again
    // would miss an on-disk target swap during human confirmation.
    ensure_exact_testnet(st, manifest)?;
    let persisted = MarketManifest::load(out).map_err(|_| refuse_no_echo())?;
    anyhow::ensure!(
        manifest_digest(manifest)? == prepared.manifest_digest
            && manifest_digest(&persisted)? == prepared.manifest_digest,
        "{}",
        refuse_no_echo()
    );

    // Journal integrity: missing, tampered, truncated, or unresolved
    // custody blocks; an older identical attempt keeps blocking.
    anyhow::ensure!(
        journal_clear_for_action(out),
        "{}",
        Safe::literal("market journal is not clear for a guarded action")
    );
    let journal = MarketJournal::open(out);
    st.assert_rpc_testnet()?;

    // Immediate pre-send re-resolution of the live interface: the target,
    // whole-interface spec digest, and function must be identical.
    let live = resolve_function(st, out, manifest, &prepared.role, &prepared.method)?;
    anyhow::ensure!(
        live.target == prepared.target && live.spec_hash == prepared.spec_hash,
        "{}",
        refuse_no_echo()
    );

    // Immediate pre-send re-resolution of the signer: the same alias in
    // the same explicit config dir must yield the same public key.
    let signer = st
        .keys_public_key(&prepared.signer_alias)?
        .ok_or_else(refuse_no_echo)?;
    anyhow::ensure!(signer == prepared.signer, "{}", refuse_no_echo());

    // Canonical rebind of the exact intent from the re-resolved facts;
    // anything that changed after preparation refuses the call.
    let mdigest = manifest_digest(manifest)?;

    let intent = canonical_intent(
        manifest,
        &mdigest,
        &prepared.role,
        &prepared.method,
        &prepared.args,
        &prepared.signer,
        &prepared.target,
        &prepared.spec_hash,
    );
    let intent_bytes = serde_json::to_vec(&intent).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market action intent canonicalization failed")
        )
    })?;
    let pre_state = Safe::render_market_json(&intent, INTENT_RENDER_CAP)?;
    anyhow::ensure!(
        sha256_hex(&intent_bytes) == prepared.intent_digest,
        "{}",
        refuse_no_echo()
    );

    // Durably reserve the exact canonical intent before any invocation,
    // at a collision-free coordinate (create_new; occupied slots advance
    // the scan; nothing is ever overwritten).
    let record = MarketCustodyRecord {
        phase: 0,
        kind: MarketMutationKind::Invoke,
        action: Safe::render_market_json(
            &serde_json::json!({"role": &prepared.role, "method": &prepared.method}),
            512,
        )?,
        network: MarketNetwork::Testnet,
        signer: prepared.signer.clone(),
        payload_digest: Sha256Hex::default(),
        target: Some(prepared.target.clone()),
        artifact_digest: None,
        predicted: None,
        pre_state: vec![pre_state],
        tool_error: None,
    };
    // Settlement-capacity preflight before the first journal write or
    // send. The intent and every definite settlement are serialized to
    // their exact encoded shapes; an action whose journal custody the
    // pinned reader could never return refuses here, input-free, so a
    // successful mutation can never strand itself oversized.
    let mut preflight = record.clone();
    let settlement_phase = *ACTION_PHASES.start();
    preflight.phase = settlement_phase;
    preflight.payload_digest = custody_payload_digest(&preflight)?;
    anyhow::ensure!(
        serde_json::to_vec_pretty(&preflight)
            .map_err(|_| refuse_no_echo())?
            .len()
            <= PENDING_RECORD_CAP,
        "{}",
        refuse_no_echo()
    );
    anyhow::ensure!(
        settlement_shapes_fit_capacity(&preflight),
        "{}",
        refuse_no_echo()
    );

    // Taken before reservation, so it is never later than the intent's
    // mtime: `market settle` relies on the send starting by then.
    let send_by = std::time::SystemTime::now()
        + std::time::Duration::from_secs(crate::stellar::MARKET_SEND_START_DEADLINE_SECS);
    // Durably reserve the exact canonical intent before any invocation,
    // at a collision-free coordinate (create_new; occupied slots advance
    // the scan; nothing is ever overwritten).
    let mut coordinate = None;
    'scan: for phase in ACTION_PHASES {
        for slot in ACTION_SLOTS {
            let mut attempt = record.clone();
            attempt.phase = phase;
            attempt.payload_digest = custody_payload_digest(&attempt)?;
            match journal.begin(phase, slot, &attempt) {
                Ok(()) => {
                    coordinate = Some((phase, slot));
                    break 'scan;
                }
                Err(e) if occupied(&e) => continue,
                Err(e) => return Err(e),
            }
        }
    }
    let (phase, slot) = coordinate.ok_or_else(|| {
        anyhow!(
            "{}",
            Safe::literal("market action journal slot space exhausted")
        )
    })?;
    let kv: Vec<(&str, String)> = prepared
        .args
        .iter()
        .map(|(n, v)| (n.as_str(), v.clone()))
        .collect();
    let inv = match st.invoke_confirmed_bound(
        &prepared.target,
        &prepared.signer,
        &prepared.signer_alias,
        &prepared.method,
        &kv,
        send_by,
    ) {
        Ok(inv) => inv,
        // Unclassified/transport failure at (or possibly before)
        // submission: custody stays open and blocking at the journal
        // coordinate. No result is written, nothing is cleared, no hash
        // is invented, and the cause never enters a journal record.
        Err(_) => {
            return Err(anyhow!(
                "{}",
                Safe::literal(
                    "market action outcome unresolved: the journal holds the attempt until external reconciliation"
                )
            ));
        }
    };

    if unresolved_submission(&inv) {
        return Err(anyhow!(
            "{}",
            Safe::literal(
                "market action outcome unresolved: the journal holds the intent until external reconciliation"
            )
        ));
    }

    let custody = read_market_intent(out, phase, slot)?;
    let (accepted, outcome, violations, value) = if inv.ok {
        (true, Safe::literal("accepted"), Vec::new(), inv.value)
    } else {
        let kind = inv.error.as_ref().ok_or_else(|| {
            anyhow!(
                "{}",
                Safe::literal("definite market rejection carried no classification")
            )
        })?;
        (
            false,
            Safe::error_summary(Policy::Market, kind),
            vec![Safe::error_explained(Policy::Market, kind)],
            None,
        )
    };
    let step = MarketStepRecord {
        phase,
        action: custody.action.clone(),
        command: custody.action.clone(),
        outcome: outcome.clone(),
        target: custody.target.clone(),
        violations,
    };
    journal.finish(phase, slot, &MarketPendingResult { custody, step })?;

    Ok(CallResult {
        accepted,
        outcome,
        value,
    })
}

/// Fixed refusal sentence (input-free; distinct site label for tests).
fn refuse_no_echo() -> anyhow::Error {
    anyhow!("{}", Safe::literal(REFUSE))
}

/// Both definite settlement shapes must fit the shared journal reader
/// capacity before the first journal write or send. The accepted shape is
/// serialized exactly. Every market error arm funnels through the same
/// sanitizer, whose emitted length is bounded by `MARKET_TEXT_CAP` plus
/// one escaped character, and each emitted character costs at most six
/// bytes under worst-case UTF-8 and JSON escape expansion.
fn settlement_shapes_fit_capacity(custody: &MarketCustodyRecord) -> bool {
    let phase = custody.phase;
    if !definite_settlement_fits(custody, phase, &Safe::literal("accepted"), Vec::new()) {
        return false;
    }
    let sanitizer_ceiling = MARKET_TEXT_CAP + 1;
    // Each definite outcome account (one outcome plus both violation
    // slots) can store at most the sanitizer ceiling expanded six-fold.
    let per_account = sanitizer_ceiling * 6;
    let outcome = bound_safe(per_account);
    let violations = vec![bound_safe(per_account), bound_safe(per_account)];
    definite_settlement_fits(custody, phase, &outcome, violations)
}

fn bound_safe(bytes: usize) -> Safe {
    serde_json::from_value(serde_json::Value::String("0".repeat(bytes))).expect("bounded Safe text")
}

fn definite_settlement_fits(
    custody: &MarketCustodyRecord,
    phase: u32,
    outcome: &Safe,
    violations: Vec<Safe>,
) -> bool {
    let mut custody = custody.clone();
    custody.phase = phase;
    let digest = match custody_payload_digest(&custody) {
        Ok(digest) => digest,
        Err(_) => return false,
    };
    custody.payload_digest = digest;
    let step = MarketStepRecord {
        phase,
        action: custody.action.clone(),
        command: custody.action.clone(),
        outcome: outcome.clone(),
        target: custody.target.clone(),
        violations,
    };
    serde_json::to_vec_pretty(&MarketPendingResult { custody, step })
        .is_ok_and(|bytes| bytes.len() <= PENDING_RECORD_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{finish_market_pending, market_pending_dir, RunLock};
    use crate::stellar::fake::FakeExecutor;
    use crate::stellar::{CmdOutput, ErrorKind, Net};
    use std::path::PathBuf;

    /// Scripts both fresh RPC identity probes: the action's own proof,
    /// then checked_spec's proof before its live interface read. Each
    /// pair is `--version` followed by testnet `network info --output json`.
    fn network_probes(tail: Vec<CmdOutput>) -> Vec<CmdOutput> {
        let info = serde_json::json!({ "passphrase": MARKET_TESTNET_PASSPHRASE }).to_string();
        let mut calls = vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&info, ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&info, ""),
        ];
        calls.extend(tail);
        calls
    }

    /// Scripts both network probes, live interface, signer lookup, and
    /// simulation before caller-supplied execution outputs.
    fn scripted_calls(tail: Vec<CmdOutput>) -> Vec<CmdOutput> {
        let mut calls = network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(RESULT, ""),
        ]);
        calls.extend(tail);
        calls
    }

    /// Prepares one valid fixture action. Preparation acquires the run
    /// lock and holds it inside the returned value. Dropping the value
    /// is the supported cancellation path and releases the lock.
    fn prepare_fixture_action(
        st: &Stellar<'_, FakeExecutor>,
        out: &Path,
        manifest: &MarketManifest,
    ) -> PreparedCall {
        prepare(
            st,
            out,
            manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect("valid action prepares")
    }

    #[test]
    fn signer_alias_must_be_a_plain_untrimmed_name() {
        assert!(alias_ok("ralplan-check"));
        for refused in [
            " admin ",
            "admin\t",
            "",
            "keys/admin",
            "GCNQ4ZLK45KOYNIQLIYI7GT6TYWGK2OZEVZEY7A3W5L7QSPBNCBWSJAQ",
        ] {
            assert!(
                !alias_ok(refused),
                "{refused:?} must not pass as a signer alias"
            );
        }
    }

    #[test]
    fn prepare_previews_without_sending_and_hidden_challenge_authorizes_once() {
        let out = scratch("prepare");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        assert!(
            journal_clear_for_action(&out),
            "fixture must clear the gate"
        );
        let fx = FakeExecutor::new(scripted_calls(Vec::new()));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        assert_eq!(send_yes_calls(&fx), 0, "preparation may never send");
        let summary = prepared.summary().to_string();
        for bound in [ID_B, G_SIGNER, "get_quote", FEE_ENVELOPE, "10000"] {
            assert!(summary.contains(bound), "preview must bind {bound}");
        }
        let challenge = prepared.confirmation();
        assert_eq!(challenge.len(), CONFIRMATION_LEN, "one-use challenge shape");
        assert!(challenge
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert!(
            !prepared.summary().to_string().contains(challenge),
            "the one-use token never enters the printable preview"
        );
        for call in fx.calls.borrow().iter() {
            if call.iter().any(|a| a == "invoke") {
                let send = call.iter().position(|a| a == "--send");
                assert!(
                    send.is_some_and(|i| call.get(i + 1) == Some(&"no".to_string())),
                    "preparation must simulate with --send no only"
                );
                let source = call.iter().position(|a| a == "--source");
                assert_eq!(
                    source.and_then(|i| call.get(i + 1)).map(String::as_str),
                    Some("admin"),
                    "the native CLI source must be the signer alias"
                );
                assert!(
                    !call.iter().any(|a| a == G_SIGNER),
                    "the signer G-address must never be a CLI source"
                );
            }
        }
        assert!(
            !market_pending_dir(&out, 900, 1).exists(),
            "preparation may not even reserve a coordinate"
        );
        assert!(
            journal_clear_for_action(&out),
            "a preview-only run must leave the journal clear"
        );
    }

    #[test]
    fn wrong_confirmation_refuses_before_executor_use() {
        let out = scratch("confirmation");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(scripted_calls(scripted_calls(Vec::new())));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let short = prepare_fixture_action(&st, &out, &manifest);
        let challenge = short.confirmation().to_string();
        let before = fx.calls.borrow().len();
        let err = execute(
            &st,
            &out,
            &manifest,
            short,
            &"0".repeat(CONFIRMATION_LEN - 1),
        )
        .expect_err("a wrong-shaped confirmation must refuse");
        assert!(
            !err.to_string().contains(&challenge),
            "a refusal may not leak the challenge"
        );
        assert_eq!(err.to_string(), REFUSE, "refusal is fixed and input-free");
        assert_eq!(
            fx.calls.borrow().len(),
            before,
            "a wrong-shaped confirmation may not reach the executor"
        );

        let exact = prepare_fixture_action(&st, &out, &manifest);
        let challenge = exact.confirmation().to_string();
        let before = fx.calls.borrow().len();
        let err = execute(&st, &out, &manifest, exact, &"0".repeat(CONFIRMATION_LEN))
            .expect_err("a mismatched exact-length confirmation must refuse");
        assert!(
            !err.to_string().contains(&challenge),
            "a refusal may not leak the challenge"
        );
        assert_eq!(err.to_string(), REFUSE, "refusal is fixed and input-free");
        assert_eq!(
            fx.calls.borrow().len(),
            before,
            "a refused confirmation may not reach the executor at all"
        );
        assert_eq!(
            send_yes_calls(&fx),
            0,
            "a refused confirmation may never send"
        );
        assert!(
            journal_clear_for_action(&out),
            "a refusal must not disturb custody"
        );
        assert!(
            !market_pending_dir(&out, 900, 1).exists(),
            "a refused confirmation may not even reserve an intent"
        );
    }

    #[test]
    fn manifest_swap_during_confirmation_refuses_before_reservation_or_send() {
        let out = scratch("manifest-swap");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(scripted_calls(Vec::new()));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        let token = prepared.confirmation().to_string();
        let before = fx.calls.borrow().len();
        let mut swapped = manifest.clone();
        swapped.pool = ID_A.parse().expect("different valid pool target");
        std::fs::write(
            out.join("market-manifest.toml"),
            toml::to_string_pretty(&swapped).expect("serialize changed manifest"),
        )
        .expect("swap persisted manifest during confirmation");
        let error = execute(&st, &out, &manifest, prepared, &token)
            .expect_err("stale preview may not authorize a changed run");
        assert_eq!(
            error.to_string(),
            REFUSE,
            "refusal must not expose run bytes"
        );
        assert_eq!(
            fx.calls.borrow().len(),
            before,
            "the swapped manifest must refuse before any executor call"
        );
        assert!(!market_pending_dir(&out, 900, 1).exists());
        assert_eq!(send_yes_calls(&fx), 0);
    }

    #[test]
    fn execute_sends_once_and_settles_the_definite_outcome_in_place() {
        let out = scratch("settled");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(scripted_calls(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            // The bound send re-resolves the alias for comparison-only
            // immediately before submission (SEC003); the pin is unchanged.
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(RESULT, ""),
        ])));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        let token = prepared.confirmation().to_string();
        let call =
            execute(&st, &out, &manifest, prepared, &token).expect("exact confirmation executes");
        assert!(call.accepted, "a settled acceptance reports honestly");
        assert_eq!(call.outcome.to_string(), "accepted");
        let expected: Value = serde_json::from_str(RESULT).expect("fixture result");
        assert_eq!(call.value, Some(expected), "decoded values pass through");
        assert_eq!(send_yes_calls(&fx), 1, "exactly one submission");
        assert_send_binds_frozen_g(&fx);
        let intent = read_market_intent(&out, 900, 1).expect("reserved intent");
        assert!(intent.target.as_ref().is_some_and(|t| t.as_str() == ID_A));
        assert_eq!(intent.signer.to_string(), G_SIGNER);
        assert!(intent
            .pre_state
            .iter()
            .any(|s| s.to_string().contains("10000000") && s.to_string().contains(ID_B)));
        assert_eq!(
            intent.payload_digest,
            custody_payload_digest(&intent).unwrap()
        );
        assert!(
            journal_clear_for_action(&out),
            "a settled outcome clears the gate"
        );
        assert!(
            begin_market_pending(&out, 900, 1, &intent).is_err(),
            "settled custody cannot be re-reserved"
        );
        assert!(
            !std::fs::read(market_pending_dir(&out, 900, 1).join("result.json"))
                .expect("durable settled result")
                .is_empty()
        );
    }

    #[test]
    fn held_run_lock_refuses_prepare_before_executor_use_and_cancellation_releases_it() {
        let out = scratch("lockbusy");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let held = RunLock::acquire(&out)
            .expect("lock acquire")
            .expect("first acquire wins");
        let fx = FakeExecutor::new(Vec::new());
        let st = Stellar::new(&net, &fx, Policy::Market);
        let err = prepare(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("a busy run must refuse preparation");
        assert!(
            err.to_string().contains("already executing another action"),
            "the busy-run refusal must be explicit"
        );
        assert_eq!(
            fx.calls.borrow().len(),
            0,
            "a busy run must reach no executor"
        );
        assert_eq!(send_yes_calls(&fx), 0, "a busy run never sends");
        assert!(
            !market_pending_dir(&out, 900, 1).exists(),
            "a busy-run refusal may not reserve an intent"
        );
        assert!(
            journal_clear_for_action(&out),
            "the refusal disturbed nothing"
        );

        // Once the external holder releases the lock, one preview is
        // allowed, but its cancellation must release the lock for the
        // next legitimate preview. A concurrent second preview proves
        // one run cannot hold two live `PreparedCall` values.
        drop(held);
        let fx = FakeExecutor::new(scripted_calls(scripted_calls(Vec::new())));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let first = prepare_fixture_action(&st, &out, &manifest);
        let second = prepare(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        );
        assert!(
            second.is_err(),
            "one run cannot hold two live prepared actions"
        );
        drop(first);
        let third = prepare_fixture_action(&st, &out, &manifest);
        assert_eq!(
            send_yes_calls(&fx),
            0,
            "preview-only cancellation sent nothing"
        );
        drop(third);
        assert!(
            !market_pending_dir(&out, 900, 1).exists(),
            "preview-only cancellation may not reserve an intent"
        );
        assert!(
            journal_clear_for_action(&out),
            "preview-only cancellation must leave the journal clear for the next legitimate action"
        );
    }

    const V26: &str = "stellar 26.1.0 (1228cff)";
    const SPEC: &str = r#"[{"function_v0":{"doc":"","name":"get_quote","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"amount","type_":"i128"}],"outputs":[{"result":{"ok_type":{"vec":{"element_type":"u32"}},"error_type":{"udt":{"name":"ContractError"}}}}]}},{"function_v0":{"doc":"","name":"noargs","inputs":[],"outputs":[{"vec":{"element_type":{"udt":{"name":"Asset"}}}}]}}]"#;
    const SPEC_MOVED: &str = r#"[{"function_v0":{"doc":"moved","name":"get_quote","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"amount","type_":"i128"}],"outputs":[{"result":{"ok_type":{"vec":{"element_type":"u32"}},"error_type":{"udt":{"name":"ContractError"}}}}]}},{"function_v0":{"doc":"","name":"noargs","inputs":[],"outputs":[{"vec":{"element_type":{"udt":{"name":"Asset"}}}}]}}]"#;
    const RESULT: &str = r#"{"Ok":{"map":[["price","12500000000"],["ttl","300"],["note",null]]}}"#;
    const ID_A: &str = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT";
    const ID_B: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
    const ID_C: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
    const ID_D: &str = "CBBTYOXBO7SO3JJ5EM5OBV4WTV5TML4G2K327TYSX422WL47B6RVMJRI";
    const ID_E: &str = "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY";
    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const G_SIGNER: &str = "GDKOBI6KTEE7GYZCWJDZTAZPKW6Q2FW7MBY2XT6HWWZHHJCBZFXNPGAW";
    const CANARY: &str = "CANARY-CLI-WORDS-MUST-NEVER-BE-EVIDENCE";

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abtest-market-call-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(
            dir.join("market-manifest.toml"),
            toml::to_string_pretty(&fixture_manifest()).expect("serialize fixture manifest"),
        )
        .expect("persist fixture manifest");
        dir
    }

    fn fixture_manifest() -> MarketManifest {
        serde_json::from_value(serde_json::json!({
            "network": {"label": "testnet", "passphrase": MARKET_TESTNET_PASSPHRASE},
            "admin": G_ADMIN,
            "rpc_host": "rpc.testnet.stellar.example",
            "oracle": {
                "runtime": ID_B, "governance": ID_C, "adapter": ID_A,
                "sources": [ID_A, ID_B, ID_C],
                "min_sources": 3, "max_age_secs": 3600, "max_clock_drift_secs": 60,
                "decimals": 7, "resolution": 300, "base": "USD"
            },
            "tokens": {"blnd": ID_B},
            "pool": ID_D, "backstop": ID_E, "factory": ID_A,
            "comet": ID_B, "emitter": ID_C,
            "reserves": {}, "identities": {}, "deploy_ledger": 1
        }))
        .expect("fixture market manifest")
    }

    fn market_net(out: &Path) -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: MARKET_TESTNET_PASSPHRASE.into(),
            config_dir: out.join("cfg"),
            headers: vec![],
        }
    }

    fn call_args() -> Vec<(String, String)> {
        vec![
            ("amount".into(), "10000000".into()),
            ("asset".into(), ID_B.into()),
        ]
    }

    fn send_yes_calls(fx: &FakeExecutor) -> usize {
        fx.calls
            .borrow()
            .iter()
            .filter(|c| {
                c.iter().any(|a| a == "invoke")
                    && c.windows(2).any(|w| w[0] == "--send" && w[1] == "yes")
            })
            .count()
    }

    fn fixture_record(out: &Path, phase: u32, slot: u32) -> MarketCustodyRecord {
        let mut record = MarketCustodyRecord {
            phase,
            kind: MarketMutationKind::Invoke,
            action: Safe::literal("market-action"),
            network: MarketNetwork::Testnet,
            signer: FromStr::from_str(G_ADMIN).expect("fixture signer"),
            payload_digest: Sha256Hex::default(),
            target: Some(FromStr::from_str(ID_A).expect("fixture target")),
            artifact_digest: None,
            predicted: None,
            pre_state: vec![Safe::literal("pre-state")],
            tool_error: None,
        };
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        begin_market_pending(out, phase, slot, &record).expect("fixture intent");
        record
    }

    fn settle_fixture(out: &Path, phase: u32, slot: u32) {
        let custody = fixture_record(out, phase, slot);
        finish_market_pending(
            out,
            phase,
            slot,
            &MarketPendingResult {
                custody: custody.clone(),
                step: MarketStepRecord {
                    phase,
                    action: custody.action.clone(),
                    command: custody.action.clone(),
                    outcome: Safe::literal("accepted"),
                    target: custody.target.clone(),
                    violations: vec![],
                },
            },
        )
        .expect("fixture settle");
    }
    #[test]
    fn definite_contract_rejection_is_settled_and_never_echoes_cli_bytes() {
        let out = scratch("refused");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let rejection = format!(
            "error: transaction simulation failed: HostError: Error(Contract, #1200)\n{CANARY}"
        );
        let fx = FakeExecutor::new(scripted_calls(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            // The bound send re-resolves the alias for comparison-only
            // immediately before submission (SEC003); the pin is unchanged.
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::fail(&rejection),
        ])));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        let token = prepared.confirmation().to_string();
        let call = execute(&st, &out, &manifest, prepared, &token)
            .expect("a definite rejection is an outcome, not a tool fault");
        assert!(
            !call.accepted,
            "a failed contract call is never marked successful"
        );
        assert_eq!(send_yes_calls(&fx), 1, "exactly one submission");
        let intent = read_market_intent(&out, 900, 1).expect("reserved intent");
        let settled: MarketPendingResult = serde_json::from_str(
            &std::fs::read_to_string(market_pending_dir(&out, 900, 1).join("result.json"))
                .expect("settled result recorded"),
        )
        .expect("settled result parses");
        assert_eq!(settled.custody.payload_digest, intent.payload_digest);
        assert_eq!(settled.step.target, intent.target);
        assert!(
            intent.action.to_string().contains("get_quote"),
            "the operator-facing intent must identify the actual method"
        );
        assert_eq!(&settled.step.action, &intent.action);
        assert_eq!(&settled.step.command, &intent.action);
        let bytes = std::fs::read(market_pending_dir(&out, 900, 1).join("result.json")).unwrap();
        assert!(
            !bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()),
            "violations hold only closed-enum text"
        );
        assert!(
            journal_clear_for_action(&out),
            "the settled slot clears the gate"
        );
    }

    #[test]
    fn simulation_failure_produces_no_executable_and_never_sends() {
        let out = scratch("simfail");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let rejection = format!(
            "error: transaction simulation failed: HostError: Error(Contract, #1200)\n{CANARY}"
        );
        let fx = FakeExecutor::new(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::fail(&rejection),
        ]));
        let st = Stellar::new(&net, &fx, Policy::Market);
        assert!(
            journal_clear_for_action(&out),
            "fixture must clear the gate"
        );
        let err = prepare(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("a failed simulation must not yield an executable action");
        let diagnostic = Safe::market_error(&err).to_string();
        assert!(
            diagnostic.contains("BadRequest (1200)"),
            "sanitized outcome"
        );
        assert!(diagnostic.contains(crate::stellar::error_help(1200).unwrap()));
        assert_eq!(diagnostic, err.to_string());
        assert!(
            !err.to_string().contains(CANARY),
            "no raw CLI bytes surface"
        );
        assert_eq!(send_yes_calls(&fx), 0, "a failed simulation never sends");
        assert!(
            !market_pending_dir(&out, 900, 1).exists(),
            "a failed simulation may not reserve an intent"
        );
        assert!(
            journal_clear_for_action(&out),
            "the refusal disturbed nothing"
        );
        assert!(
            Safe::error_explained(Policy::Market, &ErrorKind::Contract(1200))
                .to_string()
                .contains("BadRequest (1200)"),
            "definite contract failures remain closed-enum classified"
        );
    }

    #[test]
    fn ambiguous_send_stays_unresolved_and_blocks_every_later_attempt() {
        let out = scratch("ambiguous");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(scripted_calls(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            // The bound send re-resolves the alias for comparison-only
            // immediately before submission (SEC003); the pin is unchanged.
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::fail("error: rpc transport timed out"),
        ])));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        let token = prepared.confirmation().to_string();
        let err = execute(&st, &out, &manifest, prepared, &token)
            .expect_err("an ambiguous outcome is not success");
        assert!(
            err.to_string().contains("unresolved"),
            "ambiguity is reported honestly"
        );
        assert_eq!(
            send_yes_calls(&fx),
            1,
            "the ambiguous send happened exactly once"
        );
        assert!(market_pending_dir(&out, 900, 1)
            .join("intent.json")
            .exists());
        assert!(
            !market_pending_dir(&out, 900, 1)
                .join("result.json")
                .exists(),
            "ambiguity never writes a result"
        );
        assert!(
            !journal_clear_for_action(&out),
            "the open intent blocks the gate"
        );
        let err = prepare(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("no later action may proceed against an unresolved attempt");
        assert!(err.to_string().contains("journal"));
        assert_eq!(
            send_yes_calls(&fx),
            1,
            "the unresolved attempt was not retried or replaced"
        );
    }

    #[test]
    fn changed_abi_or_signer_after_confirmation_refuses_before_any_send() {
        let out = scratch("revalidate");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let mut queue = scripted_calls(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC_MOVED, ""),
        ]));
        queue.extend(scripted_calls(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_ADMIN, ""),
        ])));
        let fx = FakeExecutor::new(queue);
        let st = Stellar::new(&net, &fx, Policy::Market);
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        let token = prepared.confirmation().to_string();
        let err = execute(&st, &out, &manifest, prepared, &token)
            .expect_err("a changed live interface must refuse");
        assert_eq!(err.to_string(), REFUSE, "refusal is fixed and input-free");
        assert!(
            !err.to_string().contains(&token),
            "pre-send revalidation refusal may not leak the confirmation"
        );
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        let token = prepared.confirmation().to_string();
        let err = execute(&st, &out, &manifest, prepared, &token)
            .expect_err("a changed signer must refuse");
        assert_eq!(err.to_string(), REFUSE);
        assert_eq!(send_yes_calls(&fx), 0, "a refused action may never send");
        assert!(
            !market_pending_dir(&out, 900, 1).exists(),
            "pure refusals never reserve or leave journal entries"
        );
        assert!(
            journal_clear_for_action(&out),
            "a pure refusal must leave the journal clear for a legitimate action"
        );
    }

    #[test]
    fn unsupported_network_and_uncleared_journal_refuse_before_executor_use() {
        // A manifest or execution network that is not the exact testnet
        // refuses before any executor or journal call.
        let out = scratch("network");
        settle_fixture(&out, 900, 0);
        let mut bad = fixture_manifest();
        bad.network.label = Safe::literal("public");
        bad.network.passphrase = Safe::literal("Public Global Stellar Network ; September 2015");
        assert!(
            journal_clear_for_action(&out),
            "fixture must clear the gate"
        );
        // Build one genuine action so execute receives a real one-use
        // object; the network refusal must fire before its confirmation,
        // journal reads, or any executor use.
        let scripted = FakeExecutor::new(scripted_calls(scripted_calls(Vec::new())));
        let testnet = market_net(&out);
        let scripted_st = Stellar::new(&testnet, &scripted, Policy::Market);
        let genuine = prepare(
            &scripted_st,
            &out,
            &fixture_manifest(),
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect("valid preparation for network-refusal control");
        let genuine_token = genuine.confirmation().to_string();
        let empty = FakeExecutor::new(Vec::new());
        let empty_st = Stellar::new(&testnet, &empty, Policy::Market);
        let err = prepare(
            &empty_st,
            &out,
            &bad,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("a non-testnet manifest must refuse before executor use");
        assert!(err.to_string().contains("testnet"));
        assert_eq!(empty.calls.borrow().len(), 0, "refusal reached no executor");
        let err = execute(&empty_st, &out, &bad, genuine, &genuine_token)
            .expect_err("execute must refuse the same network");
        assert!(err.to_string().contains("testnet"));
        assert_eq!(empty.calls.borrow().len(), 0, "refusal reached no executor");
        let public = Net {
            rpc_url: "http://rpc".into(),
            passphrase: "Public Global Stellar Network ; September 2015".into(),
            config_dir: out.join("cfg"),
            headers: vec![],
        };
        let genuine = prepare(
            &scripted_st,
            &out,
            &fixture_manifest(),
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect("valid preparation for non-testnet network control");
        let genuine_token = genuine.confirmation().to_string();
        let public_empty = FakeExecutor::new(Vec::new());
        let public_st = Stellar::new(&public, &public_empty, Policy::Market);
        let err = prepare(
            &public_st,
            &out,
            &fixture_manifest(),
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("a non-testnet execution network must refuse");
        assert!(err.to_string().contains("testnet"));
        assert_eq!(public_empty.calls.borrow().len(), 0);
        let err = execute(
            &public_st,
            &out,
            &fixture_manifest(),
            genuine,
            &genuine_token,
        )
        .expect_err("execute must refuse a non-testnet execution network");
        assert!(err.to_string().contains("testnet"));
        assert_eq!(public_empty.calls.borrow().len(), 0);

        // A new manifest with no deployment journal fails closed.
        let out2 = scratch("nojournal");
        let manifest = fixture_manifest();
        let net = market_net(&out2);
        let fx = FakeExecutor::new(Vec::new());
        let st = Stellar::new(&net, &fx, Policy::Market);
        let err = prepare(
            &st,
            &out2,
            &manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("a manifest without a deployment journal must fail closed");
        assert!(err.to_string().contains("journal"));
        assert_eq!(fx.calls.borrow().len(), 0);

        // An unresolved earlier attempt blocks the journal gate.
        let out3 = scratch("unresolved");
        std::fs::create_dir_all(out3.join("market-pending")).expect("journal root");
        fixture_record(&out3, 900, 5);
        let net = market_net(&out3);
        let fx = FakeExecutor::new(Vec::new());
        let st = Stellar::new(&net, &fx, Policy::Market);
        let err = prepare(
            &st,
            &out3,
            &manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("an unresolved attempt must block a new action");
        assert!(err.to_string().contains("journal"));
        assert_eq!(fx.calls.borrow().len(), 0);

        // A forged settled result (digest mismatch) is equally blocking.
        let out4 = scratch("forged");
        std::fs::create_dir_all(out4.join("market-pending")).expect("journal root");
        let forged = fixture_record(&out4, 900, 6);
        std::fs::write(
            market_pending_dir(&out4, 900, 6).join("result.json"),
            b"{\"custody\":{\"phase\":900,\"kind\":\"Invoke\",\"action\":\"x\"},\"step\":null}",
        )
        .expect("forge result");
        assert!(custody_payload_digest(&forged).is_ok());
        let net = market_net(&out4);
        let fx = FakeExecutor::new(Vec::new());
        let st = Stellar::new(&net, &fx, Policy::Market);
        let err = prepare(
            &st,
            &out4,
            &manifest,
            "adapter",
            "get_quote",
            &call_args(),
            "admin",
        )
        .expect_err("a forged settlement must block a guarded action");
        assert!(err.to_string().contains("journal"));
        assert_eq!(fx.calls.borrow().len(), 0);
    }

    /// Any confirmed send that occurs must originate from the prepared
    /// call's frozen signer G-address and must sign by naming the alias
    /// alone (SEC003): never the alias as the transaction source, never a
    /// seed or muxed account value.
    fn assert_send_binds_frozen_g(fx: &FakeExecutor) {
        let calls = fx.calls.borrow();
        let mut sends = 0;
        for call in calls.iter() {
            if !call.windows(2).any(|w| w[0] == "--send" && w[1] == "yes") {
                continue;
            }
            sends += 1;
            assert!(
                call.windows(2)
                    .any(|w| w[0] == "--source" && w[1] == G_SIGNER),
                "a confirmed send must use the frozen G-address as the transaction source: {call:?}"
            );
            assert!(
                call.windows(2)
                    .any(|w| w[0] == "--sign-with-key" && w[1] == "admin"),
                "a confirmed send must sign by naming only the alias: {call:?}"
            );
            assert!(
                !call.iter().any(|a| a.starts_with('S') && a.len() == 56)
                    && !call.iter().any(|a| a.starts_with('M') && a.len() == 56),
                "a confirmed send must carry no raw signer seed or muxed account: {call:?}"
            );
        }
        assert!(
            sends >= 1,
            "the fixture must have produced a confirmed send to inspect"
        );
    }

    /// SEC003 alias-rebinding negative control: a rebound alias that only
    /// reveals itself at the final pre-submission recheck must refuse the
    /// send before any submission reaches the CLI, while the reserved
    /// journal intent stands as blocking custody.
    #[test]
    fn alias_rebinding_at_the_final_recheck_refuses_before_any_submission() {
        let out = scratch("rebind-final");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(scripted_calls(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            // The alias is rebound between confirmation and submission.
            FakeExecutor::ok(G_ADMIN, ""),
        ])));
        let st = Stellar::new(&net, &fx, Policy::Market);
        let prepared = prepare_fixture_action(&st, &out, &manifest);
        let token = prepared.confirmation().to_string();
        let err = execute(&st, &out, &manifest, prepared, &token)
            .expect_err("a rebound alias must refuse the bound send");
        assert!(
            err.to_string().contains("unresolved"),
            "a refused submission leaves open custody: {err}"
        );
        assert_eq!(
            send_yes_calls(&fx),
            0,
            "a rebound alias never reached a submission"
        );
        assert!(
            market_pending_dir(&out, 900, 1)
                .join("intent.json")
                .exists(),
            "the reserved intent stands as custody"
        );
        assert!(
            !market_pending_dir(&out, 900, 1)
                .join("result.json")
                .exists(),
            "a refused submission never settles its reserved coordinate"
        );
        assert!(
            !journal_clear_for_action(&out),
            "the run stays blocked until external reconciliation"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A bounded escaped argument flood is fully bounded by the input
    /// law and cannot strand the journal: either it never prepares or it
    /// settles through the ordinary one-send path without disturbing the
    /// journal law. The settlement-capacity preflight itself refuses any
    /// frozen settlement that could overrun the reader cap, and an
    /// oversized intent preimage is refused before create_new.
    #[test]
    fn escaped_argument_flood_cannot_strand_custody_and_preflight_refuses_oversized() {
        let out = scratch("oversized-send");
        settle_fixture(&out, 900, 0);
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(scripted_calls(network_probes(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(G_SIGNER, ""),
            FakeExecutor::ok(RESULT, ""),
        ])));
        let st = Stellar::new(&net, &fx, Policy::Market);
        if let Ok(prepared) = prepare(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &flood_args(),
            "admin",
        ) {
            let token = prepared.confirmation().to_string();
            let result = execute(&st, &out, &manifest, prepared, &token)
                .expect("a bounded escaped action settles through the ordinary law");
            assert!(result.accepted, "the scripted outcome settles honestly");
        }
        assert!(
            send_yes_calls(&fx) <= 1,
            "at most the one governed submission may happen"
        );
        assert!(journal_clear_for_action(&out), "the journal law held");

        let mut stranding = MarketCustodyRecord {
            phase: 900,
            kind: MarketMutationKind::Invoke,
            action: Safe::render_market_json(
                &json!({"role": "adapter", "method": "get_quote"}),
                512,
            )
            .expect("bounded label"),
            network: MarketNetwork::Testnet,
            signer: FromStr::from_str(G_SIGNER).expect("fixture signer"),
            payload_digest: Sha256Hex::default(),
            target: Some(FromStr::from_str(ID_A).expect("fixture target")),
            artifact_digest: None,
            predicted: None,
            pre_state: vec![bound_safe(60_000)],
            tool_error: None,
        };
        stranding.payload_digest = custody_payload_digest(&stranding).expect("stranding digest");
        assert!(
            !settlement_shapes_fit_capacity(&stranding),
            "a settlement that could overrun the reader cap must fail preflight closed"
        );

        let mut intent = stranding.clone();
        intent.pre_state.push(bound_safe(6_000));
        intent.payload_digest = custody_payload_digest(&intent).expect("intent digest");
        assert_eq!(
            crate::manifest::begin_market_pending(&out, 900, 5, &intent)
                .unwrap_err()
                .to_string(),
            crate::manifest::MARKET_JOURNAL_RECORD_OVERSIZE,
            "an oversized intent preimage is refused before any journal write"
        );
        assert!(
            !market_pending_dir(&out, 900, 5)
                .join("intent.json")
                .exists(),
            "the refusal may not create a journal coordinate"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    fn flood_args() -> Vec<(String, String)> {
        vec![
            ("amount".into(), "10000000".into()),
            ("asset".into(), "\u{7}".repeat(4_096)),
        ]
    }

    /// Every definite settlement (accepted or rejected at any definite
    /// classification, including worst-case escaped sanitizer output)
    /// fits the shared journal reader cap for a legitimate bounded action.
    #[test]
    fn every_definite_settlement_shape_fits_the_journal_cap() {
        let mut custody = MarketCustodyRecord {
            phase: 900,
            kind: MarketMutationKind::Invoke,
            action: Safe::render_market_json(
                &json!({"role": "adapter", "method": "get_quote"}),
                512,
            )
            .expect("bounded label"),
            network: MarketNetwork::Testnet,
            signer: FromStr::from_str(G_SIGNER).expect("fixture signer"),
            payload_digest: Sha256Hex::default(),
            target: Some(FromStr::from_str(ID_A).expect("fixture target")),
            artifact_digest: None,
            predicted: None,
            pre_state: vec![Safe::literal("pre-state")],
            tool_error: None,
        };
        custody.payload_digest = custody_payload_digest(&custody).expect("fixture digest");
        assert!(
            settlement_shapes_fit_capacity(&custody),
            "definite settlement shapes must fit the shared journal reader cap"
        );
    }
}
