//! Bounded, read-only event-history and transaction-receipt browsing for
//! market contracts, through the pinned native CLI seam only (plan §history).
//!
//! Every page is exactly ONE CLI26 `stellar events --id <target>` fetch
//! against the exact testnet, positioned by `--start-ledger <n>` or by
//! the validated `--cursor <handle>` a prior page handed back, plus
//! `--count <k> --output json` (single attempt, no retry loop, no
//! sleeps): the fetch returns immediately and zero events is a
//! successful empty page, never a hang.
//! Truthful retention limits:
//! - CLI26 has no end-ledger or upper-bound flag at all. Traversal is one
//!   bounded page forward from a caller-chosen start. The last validated
//!   event's cursor is exposed as `next_cursor` and is the exact position
//!   `events_after` (and the `--cursor` flag) resumes from, so a *requested*
//!   next page is explicit, never a silent loop, and a page boundary that
//!   falls inside one ledger neither repeats nor skips an event.
//! - The retention floor is server-set and never printed by `stellar
//!   events`; a failed window start is reported as history-unavailable
//!   with the floor named as server-set and unknown (an operator can
//!   query `stellar network health --output json` for `oldestLedger`),
//!   never as if the requested start were the boundary.
//! - Every event page fetch is preceded on every read by a fresh
//!   `stellar network info` identity proof: a long-lived process whose
//!   endpoint changed since startup refuses before any event is fetched.
//!   Receipts receive the same fresh proof before any transaction fetch.
//! - Receipts come from one `stellar tx fetch events --hash <64hex>
//!   --output json` fetch, preceded on every read by a fresh
//!   `stellar network info` identity proof: a long-lived process whose
//!   endpoint changed since startup refuses before fetching. An empty
//!   result is inconclusive: NOT_FOUND is indistinguishable from a
//!   transaction that genuinely carries no events, so the network proves
//!   nothing from an absent record.
//!
//! No chain state is mutated anywhere in this module; failure details are
//! sanitized and size-bounded through the market output boundary.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::str::FromStr;

use crate::manifest::{
    finish_market_pending, MarketMutationKind, MarketNetwork, MarketPendingResult,
    MarketStepRecord,
};
use crate::stellar::{
    ContractId, Executor, Safe, Stellar, TxHash, MARKET_CONFIRMED_SEND_BOUND_SECS,
    REAL_COORDINATE_SEND_BOUND_SECS,
};
use anyhow::anyhow;
use serde_json::json;

/// Ceiling on rendered history or receipt JSON.
const HISTORY_RENDER_CAP: usize = 65_536;

/// Every market page read must first re-prove the exact-testnet RPC
/// identity and must never send an out-of-range page request or a
/// continuation handle that the pinned CLI cannot consume.
fn page_read_guard(count: u32, cursor: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=100).contains(&count),
        "{}",
        Safe::literal("event page count must be 1..=100")
    );
    if let Some(cursor) = cursor {
        crate::stellar::validate_event_cursor(cursor)?;
    }
    Ok(())
}

/// The one dashboard both `events` and `events_after` render: the same
/// sanitized, size-bounded shape, with `start_ledger` present only for the
/// ledger-rooted page. The next-page cursor is the last event's own cursor.
fn page_dashboard(
    id: &ContractId,
    page: &[serde_json::Value],
    requested_count: u32,
    start_ledger: Option<u32>,
) -> anyhow::Result<Safe> {
    let mut body = json!({
        "contract_id": id.as_str(),
        "requested_count": requested_count,
        "returned": page.len(),
        "events": page,
        "next_cursor": page.last().and_then(|event| event.get("cursor")).cloned(),
        "limits": {
            "no_upper_ledger_bound": "CLI26 stellar events has no end-ledger flag; each call is one bounded page forward from the start or cursor",
            "retention_floor": "server-set and not reported by stellar events; a failed start may be permanently unavailable. Query stellar network health --output json for oldestLedger",
            "empty_page": "an empty page reports zero events for this page only, not a complete history",
        },
    });
    if let Some(start) = start_ledger {
        body["start_ledger"] = json!(start);
    }
    Safe::render_market_json(&body, HISTORY_RENDER_CAP)
}

/// Reads ONE bounded event page for the checksum-validated contract `id`
/// on the exact testnet: one CLI26 fetch, no retries, no streaming.
/// `count` is enforced at 1..=100 by the seam before anything runs. The
/// rendered dashboard is sanitized and size-bounded: returned event
/// cursors/ledgers, the next-page cursor only as an explicit continuation
/// handle, and the honest limits above (no CLI upper-ledger bound;
/// server-set retention). An empty page means zero events for this page
/// only, never a complete history.
/// Each fetch is preceded by a fresh `network info` identity proof:
/// a long-lived process whose endpoint changed since startup refuses
/// before any event is fetched.
pub fn events<E: Executor>(
    st: &Stellar<'_, E>,
    id: &ContractId,
    start_ledger: u32,
    count: u32,
) -> anyhow::Result<Safe> {
    page_read_guard(count, None)?;
    // A long-lived explorer's startup proof cannot authorize later
    // event pages: the RPC's own network identity is re-proved before
    // every bounded fetch, and a changed or unproven endpoint refuses
    // with one fixed sentence before any event is requested.
    st.assert_rpc_network(st.market_network())?;
    let page = st.read_events(id, start_ledger, count)?;
    page_dashboard(id, &page, count, Some(start_ledger))
}

/// Reads ONE bounded event page continuing strictly after the validated
/// opaque `cursor` a previous page handed back as `next_cursor`, for the
/// checksum-validated contract `id` on the exact testnet: one CLI26
/// fetch, no retries, no streaming. A cursor names a complete event
/// position (ledger and intra-ledger index), so continuation resumes at
/// the very next event even when two events share one ledger, and a page
/// that does not move strictly forward is rejected. `count` is enforced
/// at 1..=100 before anything runs. The rendered dashboard is sanitized
/// and size-bounded exactly as in `events`.
/// Each fetch is preceded by a fresh `network info` identity proof,
/// so an old continuation cursor cannot resume reads from an endpoint
/// whose identity changed since startup.
pub fn events_after<E: Executor>(
    st: &Stellar<'_, E>,
    id: &ContractId,
    cursor: &str,
    count: u32,
) -> anyhow::Result<Safe> {
    page_read_guard(count, Some(cursor))?;
    // Every page requires its own fresh proof of the network the run
    // froze: an endpoint whose identity changed after startup cannot use
    // an old continuation cursor to resume event reads.
    st.assert_rpc_network(st.market_network())?;
    let page = st.read_events_after(id, cursor, count)?;
    page_dashboard(id, &page, count, None)
}

/// Reads ONE transaction receipt for a strict 64-character lowercase hex
/// hash on the network the run froze through a single CLI26 fetch. The hash is
/// parsed into a typed value before any call and never echoed into output
/// or errors. Events, when present, are rendered sanitized and bounded;
/// an empty or absent record is rendered as explicit inconclusive status,
/// never as proof the transaction did not happen or failed.
/// Each receipt is preceded by a fresh `network info` identity proof,
/// and a changed or unproven endpoint refuses before any transaction
/// fetch.
pub fn receipt<E: Executor>(st: &Stellar<'_, E>, hash: &str) -> anyhow::Result<Safe> {
    let hash = TxHash::from_str(hash).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("receipt hash must be exactly 64 lowercase hex characters")
        )
    })?;
    // A long-lived explorer's startup probe cannot authorize later receipt
    // reads: the RPC's own network identity is re-proved on every read, and
    // a changed or unproven endpoint refuses before any transaction fetch.
    st.assert_rpc_network(st.market_network())?;
    let events = st.read_tx_events(&hash)?;
    Safe::render_market_json(
        &json!({
            "status": events.get("status"),
            "reason": events.get("reason"),
            "note": events.get("note"),
            "contract_events": events.get("contract_events"),
            "diagnostic_events": events.get("diagnostic_events"),
            "transaction_events": events.get("transaction_events"),
            "limits": {
                "single_fetch": "one bounded getTransaction read; no polling, no retries",
                "inconclusive": "absence of a record never proves a transaction did not happen or failed",
            },
        }),
        HISTORY_RENDER_CAP,
    )
}

/// A settle-able consumed transaction: its validated 64-hex hash, the
/// envelope facts read from chain, and the one signer sequence/time pair
/// its metadata recorded. Constructed only by proven reads; refusals
/// write nothing.
struct SettleTx {
    hash: TxHash,
    envelope: crate::stellar::EnvelopedTx,
    seq_time: u64,
}

/// The settlement slack (60 s) applied to both sides of the attempt
/// window: ledger close times and the journal mtime clock skew across
/// machines, while the sequence law below remains exact.
const SETTLE_SLACK: u64 = 60;

/// The intent.json mtime (unix seconds), read without following
/// symlinks and rejecting a non-regular file. The pinned journal reader
/// has already accepted this exact coordinate's intent bytes, so this
/// second look only classifies what kind of filesystem entry stands at
/// that path.
fn intent_mtime(out: &Path, phase: u32, slot: u32) -> anyhow::Result<u64> {
    let path = out
        .join("market-pending")
        .join(format!("{phase:03}-{slot:02}"))
        .join("intent.json");
    let meta = std::fs::symlink_metadata(&path).map_err(|_| settle_refused())?;
    anyhow::ensure!(meta.is_file(), "{}", settle_refused());
    u64::try_from(meta.mtime()).map_err(|_| settle_refused())
}

/// Where a real coordinate proven never applied is archived: outside the
/// numbered journal, so the next resume cannot read it as reserved, and
/// with its intent bytes and its proof, so the evidence survives the run.
const NEVER_APPLIED_DIR: &str = "market-unapplied";

/// The settlement refusal's one stable sentence.
fn settle_refused() -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        Safe::literal("settle refused: settlement evidence is unavailable or the attempt cannot be proven unapplied")
    )
}

/// Which law governs a coordinate whose submission is proven never to have
/// landed. The window proof itself — two hashes from the signer's
/// contiguous sequence history bracketing the attempt — is identical in
/// both; only the intent admission, the network identity, the send bound,
/// and what becomes of the coordinate differ.
#[derive(Clone, Copy)]
pub enum UnappliedLaw {
    /// A guarded explorer/CLI action (phases 900..=999) of a completed
    /// run on the exact testnet. The coordinate keeps its append-only
    /// settlement record, which is what `market status` reads.
    Lab,
    /// One deploy/config/invoke coordinate of a real market whose run is
    /// still partial: it has a frozen deployment plan and no completed
    /// manifest. The coordinate is archived as never applied, so the next
    /// `market deploy --resume` re-runs it.
    RealPartial,
}

impl UnappliedLaw {
    /// Whether this coordinate's intent may be settled as never applied,
    /// and the target its window must not touch.
    fn admits(
        self,
        intent: &crate::manifest::MarketCustodyRecord,
        phase: u32,
        expected: MarketNetwork,
    ) -> anyhow::Result<Option<ContractId>> {
        anyhow::ensure!(intent.network == expected, "{}", settle_refused());
        Ok(match self {
            Self::Lab => {
                anyhow::ensure!(
                    intent.kind == MarketMutationKind::Invoke
                        && crate::market_call::ACTION_PHASES.contains(&phase),
                    "{}",
                    settle_refused()
                );
                Some(intent.target.as_ref().ok_or_else(settle_refused)?.clone())
            }
            Self::RealPartial => {
                // Only a coordinate that submits a contract of its own is in
                // scope: an open upload is settled by the public fetch of
                // its declared hash, an open funded key by the run-local
                // keystore, and neither is a submission to a target.
                anyhow::ensure!(
                    matches!(
                        intent.kind,
                        MarketMutationKind::Deploy
                            | MarketMutationKind::Config
                            | MarketMutationKind::Invoke
                    ),
                    "{}",
                    settle_refused()
                );
                Some(
                    intent
                        .target
                        .as_ref()
                        .or(intent.predicted.as_ref())
                        .cloned()
                        .ok_or_else(settle_refused)?,
                )
            }
        })
    }

    /// The bound on when the attempt's send could still have read its
    /// account sequence, derived from the tool's own code paths.
    fn send_bound_secs(self) -> u64 {
        match self {
            Self::Lab => MARKET_CONFIRMED_SEND_BOUND_SECS,
            Self::RealPartial => REAL_COORDINATE_SEND_BOUND_SECS,
        }
    }

    /// Whether a window transaction could, by its operations, be this
    /// coordinate's own submission. A transaction that could not be it is an
    /// unrelated consumer of the sequence; one that could is a refusal.
    ///
    /// A lab coordinate can only ever have been an invocation of its one
    /// target, so a transaction that does not invoke that target is provably
    /// not the attempt, whatever else it carries. A real coordinate is asked
    /// by what it submitted:
    ///
    /// - a deployment is exactly one contract creation installing the
    ///   coordinate's declared Wasm, so a window transaction that creates a
    ///   contract from that Wasm — under any id preimage or salt, which only
    ///   makes the rule broader — or from an executable this reader cannot
    ///   classify might be it. A classic operation, an upload, an invocation
    ///   of another contract, a creation from other Wasm and a Stellar Asset
    ///   Contract creation cannot. A deployment that declares no digest is
    ///   the network's own asset contract: no Wasm of ours names it, so only
    ///   an unclassifiable creation might be it, and the predicted address
    ///   read in `settle_at` answers whether that contract exists;
    /// - a config or invoke coordinate is an invocation of its target, which
    ///   `collect_proof` already refuses, so nothing else in the transaction
    ///   can make it related.
    ///
    /// An operation of a shape the envelope reader does not know never gets
    /// here: the read itself is refused. Any other kind is out of scope and
    /// admitted by no one.
    fn window_tx_could_be_the_attempt(
        self,
        intent: &crate::manifest::MarketCustodyRecord,
        envelope: &crate::stellar::EnvelopedTx,
    ) -> bool {
        use crate::stellar::CreatedCode;
        match self {
            Self::Lab => false,
            Self::RealPartial => match intent.kind {
                MarketMutationKind::Deploy => {
                    envelope.creates.iter().any(|code| match code {
                        CreatedCode::Unclassified => true,
                        CreatedCode::Wasm(hash) => intent
                            .artifact_digest
                            .as_ref()
                            .is_some_and(|declared| declared.as_str().eq_ignore_ascii_case(hash)),
                        CreatedCode::StellarAsset => false,
                    })
                }
                MarketMutationKind::Config | MarketMutationKind::Invoke => false,
                // Uploads, funded keys and the rest are settled by their own
                // laws and refused before a window is ever read.
                _ => true,
            },
        }
    }
}

/// The attempt's sequence lies within `(first, last]`: its account sequence
/// was read after the intent was reserved (`T0`) and before its CLI
/// children ended, every one of them bounded. The slack absorbs ledger
/// close times and the clock skew between the journal's machine and the
/// network's.
fn attempt_window(t0: u64, send_bound_secs: u64) -> anyhow::Result<(u64, u64)> {
    Ok((
        t0.saturating_sub(SETTLE_SLACK),
        t0.checked_add(send_bound_secs)
            .and_then(|value| value.checked_add(SETTLE_SLACK))
            .ok_or_else(settle_refused)?,
    ))
}

/// Read and verify the bracketing transactions: every hash is a v1
/// envelope signed by the intent's own account, none of which invokes the
/// target; the sequences they carry are contiguous with no gap; and the
/// pair's ledger close times span the whole window the attempt could still
/// have occupied. Any gap, foreign source, fee-bump envelope,
/// target-touching transaction, or unprovable time bound is a refusal.
fn collect_proof<E: Executor>(
    st: &Stellar<'_, E>,
    intent: &crate::manifest::MarketCustodyRecord,
    target: Option<&ContractId>,
    hashes: &[TxHash],
    window: (u64, u64),
    law: UnappliedLaw,
) -> anyhow::Result<Vec<SettleTx>> {
    let mut txs: Vec<SettleTx> = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let envelope = st.read_tx_envelope(hash, target)?;
        anyhow::ensure!(
            envelope.source_account == intent.signer,
            "{}",
            settle_refused()
        );
        anyhow::ensure!(!envelope.invokes_target, "{}", settle_refused());
        // Whether the transaction could itself be the attempt depends on
        // what this coordinate submitted, not on whether it is a pure
        // invocation: the run's own classic operations and other contracts'
        // deployments are not.
        anyhow::ensure!(
            !law.window_tx_could_be_the_attempt(intent, &envelope),
            "{}",
            settle_refused()
        );
        let meta = st.read_tx_meta(hash)?;
        let mut times = meta
            .iter()
            .filter(|(account, seq, _)| *account == intent.signer && *seq == envelope.seq_num)
            .map(|(_, _, seq_time)| *seq_time);
        let seq_time = times.next().ok_or_else(settle_refused)?;
        anyhow::ensure!(times.next().is_none(), "{}", settle_refused());
        txs.push(SettleTx {
            hash: hash.clone(),
            envelope,
            seq_time,
        });
    }
    txs.sort_unstable_by_key(|tx| tx.envelope.seq_num);
    for pair in txs.windows(2) {
        let previous = pair[0].envelope.seq_num;
        anyhow::ensure!(
            pair[1].envelope.seq_num == previous + 1,
            "{}",
            settle_refused()
        );
    }
    // At least two hashes were required and each pushed one entry.
    let (first_tx, last_tx) = (&txs[0], &txs[txs.len() - 1]);
    anyhow::ensure!(
        first_tx.seq_time <= window.0 && last_tx.seq_time >= window.1,
        "{}",
        settle_refused()
    );
    Ok(txs)
}

/// The single settlement core. Every failure is a refusal that writes
/// nothing.
fn settle_at<E: Executor>(
    st: &Stellar<'_, E>,
    law: UnappliedLaw,
    expected: MarketNetwork,
    out: &Path,
    coordinate: &str,
    consumed_by: &[String],
) -> anyhow::Result<Safe> {
    let (phase, slot) =
        crate::market_status::market_coordinate(coordinate).ok_or_else(settle_refused)?;
    anyhow::ensure!(
        consumed_by.len() >= 2,
        "{}",
        Safe::literal("settle refused: at least two --consumed-by hashes are required")
    );
    anyhow::ensure!(
        consumed_by.len() <= 256,
        "{}",
        Safe::literal("settle refused: at most 256 --consumed-by hashes are accepted")
    );
    let mut hashes: Vec<TxHash> = Vec::with_capacity(consumed_by.len());
    for hash in consumed_by {
        let hash = TxHash::from_str(hash).map_err(|_| settle_refused())?;
        anyhow::ensure!(!hashes.contains(&hash), "{}", settle_refused());
        hashes.push(hash);
    }
    let Some((intent, None)) = crate::market_status::read_journal_coordinate(out, phase, slot)?
    else {
        return Err(settle_refused());
    };
    let target = law.admits(&intent, phase, expected)?;
    let t0 = intent_mtime(out, phase, slot)?;
    // Re-prove the network the law names once before any transaction read.
    st.assert_rpc_network(expected.passphrase())?;
    let window = attempt_window(t0, law.send_bound_secs())?;
    let txs = collect_proof(st, &intent, target.as_ref(), &hashes, window, law)?;
    // A deployment that landed leaves its contract behind, whatever the
    // window says. Settling archives the coordinate so a resume deploys it
    // again, so it must be proven that nothing is there: only the ledger's
    // own empty answer counts, and a read that failed refuses.
    if matches!(law, UnappliedLaw::RealPartial) && intent.kind == MarketMutationKind::Deploy {
        let predicted = target.as_ref().ok_or_else(settle_refused)?;
        let present = st
            .contract_instance_present(predicted)
            .map_err(|_| settle_refused())?;
        anyhow::ensure!(!present, "{}", settle_refused());
    }
    // One rendered proof is both the durable outcome and the stdout
    // summary. It is rendered before any write, so nothing can fail after
    // it, and it keeps every consumed hash so the settlement can be
    // re-verified later from the run directory alone.
    let evidence = Safe::render_market_json(
        &json!({
            "status": "not-applied",
            "coordinate": coordinate,
            "intent_written_at": t0,
            "required_window": [window.0, window.1],
            "consumed_sequences": [
                txs[0].envelope.seq_num,
                txs[txs.len() - 1].envelope.seq_num
            ],
            "consumed_hashes": txs.iter().map(|tx| tx.hash.as_str()).collect::<Vec<_>>(),
        }),
        32_768,
    )?;
    match law {
        UnappliedLaw::Lab => {
            let step = MarketStepRecord {
                phase,
                action: intent.action.clone(),
                command: intent.action.clone(),
                outcome: evidence.clone(),
                target: intent.target.clone(),
                violations: Vec::new(),
            };
            finish_market_pending(
                out,
                phase,
                slot,
                &MarketPendingResult {
                    custody: intent,
                    step,
                },
            )?;
        }
        UnappliedLaw::RealPartial => archive_never_applied(out, phase, slot, &evidence)?,
    }
    Ok(evidence)
}

/// Whether the run's graph has already moved past this coordinate, which
/// would mean a later coordinate holds an intent that this settlement
/// cannot speak for.
fn intent_beyond_here(out: &Path, phase: u32, slot: u32) -> anyhow::Result<bool> {
    let root = out.join("market-pending");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(false);
    };
    for entry in entries {
        let entry = entry.map_err(|_| settle_refused())?;
        let Some((later_phase, later_slot)) =
            crate::market_status::market_coordinate(&entry.file_name().to_string_lossy())
        else {
            continue;
        };
        if !entry
            .file_type()
            .map_err(|_| settle_refused())?
            .is_dir()
        {
            continue;
        }
        if (later_phase, later_slot) > (phase, slot) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Move an open coordinate out of the journal, with its intent bytes and
/// the proof that its submission never applied, so the next resume treats
/// it as never reserved and replays it under the ordinary gated path.
pub(crate) fn archive_never_applied(out: &Path, phase: u32, slot: u32, evidence: &Safe) -> anyhow::Result<()> {
    archive_open_intent(out, phase, slot, evidence, false)
}

/// Archive an open coordinate whose step ran to a definite, *refused* answer
/// (it reported violations) so that `--resume` runs it again. The same
/// coordinate can refuse many times before its condition holds, so each
/// archive takes the next free name (`003-07`, `003-07.2`, ...) and no
/// earlier evidence is overwritten.
pub(crate) fn archive_for_replay(out: &Path, phase: u32, slot: u32, evidence: &Safe) -> anyhow::Result<()> {
    archive_open_intent(out, phase, slot, evidence, true)
}

fn archive_open_intent(
    out: &Path,
    phase: u32,
    slot: u32,
    evidence: &Safe,
    successive: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !intent_beyond_here(out, phase, slot)?,
        "{}",
        settle_refused()
    );
    let name = format!("{phase:03}-{slot:02}");
    let pending = out
        .join("market-pending")
        .join(&name)
        .canonicalize()
        .map_err(|_| settle_refused())?;
    anyhow::ensure!(
        pending.parent() == out.join("market-pending").canonicalize().ok().as_deref(),
        "{}",
        settle_refused()
    );
    let archive_root = out.join(NEVER_APPLIED_DIR);
    std::fs::create_dir_all(&archive_root).map_err(|_| settle_refused())?;
    let mut archive = archive_root.join(&name);
    if successive {
        let mut attempt = 1u32;
        while std::fs::symlink_metadata(&archive).is_ok() {
            attempt += 1;
            archive = archive_root.join(format!("{name}.{attempt}"));
        }
    }
    anyhow::ensure!(
        std::fs::symlink_metadata(&archive).is_err(),
        "{}",
        settle_refused()
    );
    std::fs::write(
        pending.join("unapplied.json"),
        evidence.to_string().as_bytes(),
    )
    .map_err(|_| settle_refused())?;
    std::fs::rename(&pending, &archive).map_err(|_| settle_refused())?;
    Ok(())
}

/// Settle an open invoke intent as *not applied* once public chain data
/// proves its attempted sequence was consumed by verified unrelated
/// transactions. The coordinate keeps its append-only settlement record.
pub fn settle<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    coordinate: &str,
    consumed_by: &[String],
) -> anyhow::Result<Safe> {
    settle_at(
        st,
        UnappliedLaw::Lab,
        MarketNetwork::Testnet,
        out,
        coordinate,
        consumed_by,
    )
}

/// Settle one coordinate of a real market whose run is still partial — an
/// interrupted deploy/config/invoke whose CLI call failed locally — as
/// *never applied*, once public chain data proves its attempted sequence
/// was consumed by transactions that cannot be it.
///
/// The network is never a caller's choice: it is the one the run's frozen
/// deployment plan names, and the endpoint must report that passphrase.
/// The coordinate is archived rather than settled, so `market deploy
/// --resume` re-runs it: a submission proven absent is exactly the case
/// the resume law exists to replay, and the graph had not moved past it.
pub fn settle_unapplied<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    coordinate: &str,
    consumed_by: &[String],
) -> anyhow::Result<Safe> {
    let Some(plan) = crate::deploy::read_deployment_plan(out)? else {
        return Err(settle_refused());
    };
    anyhow::ensure!(
        !out.join("market-manifest.toml").exists(),
        "{}",
        settle_refused()
    );
    let declared = MarketNetwork::from_pair(&plan.passphrase, &plan.network_label)
        .map_err(|_| settle_refused())?;
    settle_at(
        st,
        UnappliedLaw::RealPartial,
        declared,
        out,
        coordinate,
        consumed_by,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MARKET_TESTNET_PASSPHRASE;
    use crate::stellar::{Net, Policy};
    use serde_json::Value;

    const V26: &str = "stellar 26.1.0 (1228cff)";
    const ID_A: &str = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT";
    const ID_B: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
    const TX: &str = "dd77fe6e7554cdf3d4ab32730fc2c9fb1039f0b16c5ed99769ae62b9809801f1";

    fn net() -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: MARKET_TESTNET_PASSPHRASE.into(),
            config_dir: "/cfg".into(),
            headers: vec![],
        }
    }

    fn id(s: &str) -> ContractId {
        ContractId::from_str(s).expect("fixture contract id")
    }

    fn decoded_line(contract: &str, ledger: u64, cursor: &str, marker: &str) -> String {
        serde_json::to_string(&json!({
            "id": cursor,
            "ledger": ledger,
            "ledger_closed_at": "2026-01-01T00:00:00Z",
            "type": "contract",
            "contract_id": contract,
            "event_name": "transfer",
            "prefix_topics": ["AAAABgAAAABTRUNSRVQ="],
            "params": {"note": marker, "amount": "1"},
        }))
        .expect("decoded event fixture")
    }

    fn raw_line(contract: &str, ledger: u64, cursor: &str, marker: &str) -> String {
        serde_json::to_string(&json!({
            "type": "contract",
            "ledger": ledger,
            "ledgerClosedAt": "2026-01-01T00:00:01Z",
            "contractId": contract,
            "id": cursor,
            "txHash": TX,
            "topic": [marker],
            "value": "AAAACgAAAAM=",
        }))
        .expect("raw event fixture")
    }

    fn empty_receipt_body() -> String {
        r#"{"contract_events":[],"diagnostic_events":[],"transaction_events":[]}"#.to_string()
    }

    /// The exact-testnet `network info` identity proof that every
    /// receipt and every event page read re-collects fresh before any
    /// transaction or event is touched.
    fn network_info_testnet() -> String {
        json!({"passphrase": MARKET_TESTNET_PASSPHRASE}).to_string()
    }

    /// A public-network response stands in for an endpoint whose own
    /// identity changed after a valid testnet startup proof.
    fn network_info_mainnet() -> String {
        json!({"passphrase": "Public Global Stellar Network ; September 2015"}).to_string()
    }

    #[test]
    fn events_refuse_out_of_range_pages_before_any_cli_call() {
        let n = net();
        for count in [0, 101] {
            let fx = crate::stellar::fake::FakeExecutor::new(vec![]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let err = events(&st, &id(ID_A), 100, count).expect_err("out-of-range page must fail");
            assert_eq!(err.to_string(), "event page count must be 1..=100");
            assert!(fx.calls.borrow().is_empty(), "no CLI call may run");
        }
    }

    #[test]
    fn receipt_rejects_non_hash_inputs_before_any_cli_call() {
        let n = net();
        let bad = [
            String::new(),
            "f".repeat(63),
            "F".repeat(64),
            "z".repeat(64),
            "0xdead".into(),
        ];
        for hash in bad.iter().map(String::as_str) {
            let fx = crate::stellar::fake::FakeExecutor::new(vec![]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let err = receipt(&st, hash).expect_err("malformed hash must fail closed");
            assert_eq!(
                err.to_string(),
                "receipt hash must be exactly 64 lowercase hex characters"
            );
            assert!(fx.calls.borrow().is_empty(), "no CLI call may run");
        }
    }

    #[test]
    fn tx_hash_deserialization_rejects_malformed_values() {
        let bad = [
            r#""deadbeef""#.to_string(),
            format!("\"{}\"", "F".repeat(64)),
            "\"\"".to_string(),
        ];
        for item in bad.iter().map(String::as_str) {
            let err = serde_json::from_str::<TxHash>(item)
                .expect_err("deserialize must parse, not trust, a tx hash");
            assert!(
                err.to_string().contains("invalid transaction hash"),
                "{err}"
            );
        }
        let ok: TxHash = serde_json::from_str(&format!("\"{TX}\"")).expect("valid hash");
        assert_eq!(ok.as_str(), TX);
    }

    #[test]
    fn empty_page_renders_bounded_empty_result_after_one_fetch() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok("", "No events"),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let safe = events(&st, &id(ID_A), 100, 10).expect("empty page must succeed");
        let text = safe.to_string();
        assert!(
            text.contains(r#""returned":0"#) && text.contains(r#""events":[]"#),
            "{text}"
        );
        assert!(
            text.contains("retention_floor") && text.contains("empty_page"),
            "{text}"
        );
        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            4,
            "two version gates, fresh identity proof, and one fetch"
        );
        assert_eq!(
            calls[3],
            vec![
                "events",
                "--id",
                ID_A,
                "--start-ledger",
                "100",
                "--count",
                "10",
                "--output",
                "json",
                "--network-passphrase",
                MARKET_TESTNET_PASSPHRASE,
                "--config-dir",
                "/cfg",
            ]
        );
    }

    #[test]
    fn mixed_event_shapes_paginate_and_expose_next_cursor() {
        let n = net();
        let stdout = format!(
            "{}\n{}",
            decoded_line(ID_A, 101, "4000000000000000001-0", "MIXED-A"),
            raw_line(ID_A, 102, "4000000000000000002-1", "MIXED-B")
        );
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&stdout, ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let safe = events(&st, &id(ID_A), 100, 10).expect("mixed shapes are both valid pages");
        let rendered = safe.to_string();
        let v: Value = serde_json::from_str(&rendered).expect("rendered dashboard is JSON");
        assert_eq!(v["returned"], 2);
        assert_eq!(v["next_cursor"], "4000000000000000002-1");
        assert_eq!(v["events"][0]["ledger"], 101);
        assert_eq!(v["events"][1]["contract_id"], ID_A);
        assert!(
            !rendered.contains("SECRET")
                && !rendered.contains("MIXED-A")
                && !rendered.contains("MIXED-B")
                && !rendered.contains("AAAABgAA"),
            "nested payloads must not escape the rebuilt page: {rendered}"
        );
    }

    #[test]
    fn wrong_contract_prestart_and_beyond_range_ledgers_are_rejected_without_leaks() {
        for stdout in [
            decoded_line(ID_B, 101, "4000000000000000001-0", "LEAK-A"),
            raw_line(ID_B, 101, "4000000000000000002-0", "LEAK-B"),
            decoded_line(ID_A, 99, "4000000000000000003-0", "LEAK-C"),
            raw_line(ID_A, 4294967296, "4000000000000000004-0", "LEAK-D"),
        ] {
            let n = net();
            let fx = crate::stellar::fake::FakeExecutor::new(vec![
                crate::stellar::fake::FakeExecutor::ok(V26, ""),
                crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
                crate::stellar::fake::FakeExecutor::ok(V26, ""),
                crate::stellar::fake::FakeExecutor::ok(&stdout, ""),
            ]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let err = events(&st, &id(ID_A), 100, 10)
                .expect_err("off-target or out-of-window events must fail closed");
            assert_eq!(err.to_string(), crate::stellar::HISTORY_MALFORMED);
            assert!(
                !err.to_string().contains("LEAK")
                    && !err.to_string().contains(ID_B)
                    && !err.to_string().contains("4294967296"),
                "rejection must not echo the untrusted event: {err}"
            );
        }
    }

    #[test]
    fn ledger_boundary_is_accepted_at_max_and_rejected_just_beyond() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(
                &decoded_line(
                    ID_A,
                    u64::from(u32::MAX),
                    "4000000000000000005-0",
                    "BOUNDARY",
                ),
                "",
            ),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let safe = events(&st, &id(ID_A), 100, 10).expect("the maximum ledger sequence is valid");
        let v: Value = serde_json::from_str(&safe.to_string()).expect("rendered dashboard is JSON");
        assert_eq!(v["events"][0]["ledger"], u64::from(u32::MAX));
    }

    #[test]
    fn malformed_event_id_grammar_is_rejected_without_leaks() {
        for cursor in [
            "4000000000000000001:0000000000:00000000",
            "abcdef-1",
            "4000000000000000001",
            "4000000000000000001--1",
            "-4000000000000000001-1",
            "+4000000000000000001-1",
            " 4000000000000000001-1",
            "4000000000000000001-1 ",
            "4000000000000000001-\t",
            "9999999999999999999999999999-1",
            "4000000000000000001-99999999999999",
        ] {
            let stdout = decoded_line(ID_A, 101, cursor, "CURSOR-LEAK");
            let n = net();
            let fx = crate::stellar::fake::FakeExecutor::new(vec![
                crate::stellar::fake::FakeExecutor::ok(V26, ""),
                crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
                crate::stellar::fake::FakeExecutor::ok(V26, ""),
                crate::stellar::fake::FakeExecutor::ok(&stdout, ""),
            ]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let err = events(&st, &id(ID_A), 100, 10)
                .expect_err("a handle the CLI cannot consume must be rejected");
            assert_eq!(err.to_string(), crate::stellar::HISTORY_MALFORMED);
            assert!(
                !err.to_string().contains("CURSOR-LEAK"),
                "rejection must not echo the cursor: {err}"
            );
        }
    }

    #[test]
    fn too_old_and_missing_server_are_explicit_history_unavailable() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::fail(
                "error: startLedger too old: requested ledger is older than the server retention window",
            ),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let err = events(&st, &id(ID_A), 100, 10).expect_err("expired retention is unavailable");
        assert_eq!(err.to_string(), crate::stellar::HISTORY_EXPIRED);

        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::fail(
                "error trying to connect: Connection refused (os error 111)",
            ),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let err = events(&st, &id(ID_A), 100, 10).expect_err("missing server is unavailable");
        assert_eq!(err.to_string(), crate::stellar::HISTORY_UNREACHABLE);
        assert_eq!(
            fx.calls.borrow().len(),
            4,
            "a dead server costs one fetch attempt, never a retry loop"
        );
    }

    #[test]
    fn transport_failures_never_report_missing_records_or_expired_retention() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::fail(
                "error trying to connect: Connection refused (os error 111): startLedger too old",
            ),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let err = events(&st, &id(ID_A), 100, 10)
            .expect_err("an unfinished fetch is unreachable, never expired");
        assert_eq!(err.to_string(), crate::stellar::HISTORY_UNREACHABLE);

        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::fail(
                "error trying to connect: Connection refused: transaction not found",
            ),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let err = receipt(&st, TX)
            .expect_err("an unfinished lookup is unreachable, never an absent record");
        assert_eq!(err.to_string(), crate::stellar::RECEIPT_UNREACHABLE);
    }

    #[test]
    fn oversize_and_malformed_pages_are_refused_without_raw_egress() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&"x".repeat(1_048_577), ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let err = events(&st, &id(ID_A), 100, 10).expect_err("oversize page must fail");
        assert_eq!(
            err.to_string(),
            crate::stellar::HISTORY_PAGE_OVERSIZE,
            "size exhaustion has its own fixed report"
        );

        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok("SECRET-LINE not json", ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let err = events(&st, &id(ID_A), 100, 10).expect_err("malformed line must fail");
        assert_eq!(
            err.to_string(),
            crate::stellar::HISTORY_LINE_UNPARSEABLE,
            "content failure has its own fixed report"
        );
        assert!(!err.to_string().contains("SECRET"), "no raw egress: {err}");
    }

    #[test]
    fn empty_receipt_is_inconclusive_and_never_proof() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&empty_receipt_body(), ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let safe = receipt(&st, TX).expect("inconclusive receipt still renders");
        let rendered = safe.to_string();
        assert!(rendered.contains("inconclusive"), "{rendered}");
        assert!(rendered.contains("never prove"), "{rendered}");
        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            4,
            "two version gates plus one fresh identity probe plus one getTransaction read"
        );
        assert_eq!(
            calls[1],
            vec![
                "network",
                "info",
                "--output",
                "json",
                "--network-passphrase",
                MARKET_TESTNET_PASSPHRASE,
                "--config-dir",
                "/cfg",
            ]
        );
        assert_eq!(
            calls[3],
            vec![
                "tx",
                "fetch",
                "events",
                "--hash",
                TX,
                "--output",
                "json",
                "--network-passphrase",
                MARKET_TESTNET_PASSPHRASE,
                "--config-dir",
                "/cfg",
            ]
        );
    }

    #[test]
    fn receipt_with_events_renders_observed_status() {
        let n = net();
        let body = r#"{"contract_events":[[{"type":"contract","contract_id":null,"body":{"v0":{"type":"contract"}}}]],"diagnostic_events":[],"transaction_events":[]}"#;
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(body, ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let safe = receipt(&st, TX).expect("receipt with events renders");
        assert!(safe.to_string().contains("observed"), "status must render");
    }

    #[test]
    fn receipt_not_found_exit_is_explicit_inconclusive() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::fail("error: transaction not found on network"),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let err = receipt(&st, TX).expect_err("a failed lookup is not a receipt");
        assert_eq!(err.to_string(), crate::stellar::RECEIPT_INCONCLUSIVE);
        assert!(
            !err.to_string().contains("not found"),
            "an inconclusive lookup must never claim absence: {err}"
        );
    }

    #[test]
    fn malformed_receipt_bodies_are_refused_without_raw_egress() {
        for body in [
            "SECRET-RECEIPT not json",
            r#"{"contract_events":{"oops":1}}"#,
            r#"{"contract_events":[1],"diagnostic_events":[],"transaction_events":[]}"#,
            r#"{"contract_events":[],"diagnostic_events":[1],"transaction_events":[]}"#,
            r#"{"contract_events":[],"diagnostic_events":[],"transaction_events":[1]}"#,
        ] {
            let n = net();
            let fx = crate::stellar::fake::FakeExecutor::new(vec![
                crate::stellar::fake::FakeExecutor::ok(V26, ""),
                crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
                crate::stellar::fake::FakeExecutor::ok(V26, ""),
                crate::stellar::fake::FakeExecutor::ok(body, ""),
            ]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let err = receipt(&st, TX).expect_err("malformed receipt body must fail closed");
            assert_eq!(err.to_string(), crate::stellar::RECEIPT_MALFORMED);
            assert!(!err.to_string().contains("SECRET"), "no raw egress: {err}");
        }
    }

    /// SEC006 negative control: a long-lived explorer whose startup probe
    /// proved the exact testnet must still refuse every later receipt read
    /// the instant the endpoint's own identity changes, and must not send
    /// the transaction hash anywhere. The queue is armed past the changed
    /// identity, so a path that skipped the fresh proof would still reach
    /// the fetch stage, and the error, call-count, and argv checks below
    /// would fail.
    #[test]
    fn receipt_refuses_when_rpc_identity_changes_after_startup() {
        let n = net();
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(
                r#"{"passphrase":"Public Global Stellar Network ; September 2015"}"#,
                "",
            ),
            crate::stellar::fake::FakeExecutor::ok(&empty_receipt_body(), ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        st.assert_rpc_testnet()
            .expect("startup proves the exact testnet identity");
        let err = receipt(&st, TX).expect_err("a changed network must refuse before fetching");
        assert_eq!(
            err.to_string(),
            "market RPC did not prove the exact testnet passphrase"
        );
        assert!(
            !err.to_string().contains("Public") && !err.to_string().contains(TX),
            "the refusal echoes neither the endpoint nor the hash: {err}"
        );
        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            4,
            "only the startup and receipt identity probes may run; the last queued record must stay unfetched"
        );
        assert!(
            calls
                .iter()
                .all(|argv| argv.first().map(String::as_str) != Some("tx")),
            "no transaction fetch may follow a failed identity check: {calls:?}"
        );
        assert!(
            calls.iter().all(|argv| !argv.iter().any(|a| a == TX)),
            "the transaction hash never reaches a changed endpoint"
        );
    }

    /// A long-lived explorer must refuse every later event page once the
    /// endpoint's identity changes. Each boundary re-proves the endpoint
    /// afresh: the startup proof consumes a version and a testnet answer,
    /// and every later read meets a version followed by a public-network
    /// answer and refuses before any page is fetched. The queue stays
    /// armed with valid pages, so a path that skipped a fresh proof would
    /// record a fetch and fail the checks below.
    #[test]
    fn event_reads_refuse_when_rpc_identity_changes_after_startup() {
        let n = net();
        let page_one = decoded_line(ID_A, 101, "4000000000000000001-0", "STALE-A");
        let page_two = decoded_line(ID_A, 101, "4000000000000000001-1", "STALE-B");
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_mainnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_mainnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(&page_one, ""),
            crate::stellar::fake::FakeExecutor::ok(&page_two, ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        st.assert_rpc_testnet()
            .expect("startup proves the exact testnet identity");

        let err =
            events(&st, &id(ID_A), 100, 1).expect_err("a changed network must refuse a fresh page");
        assert_eq!(
            err.to_string(),
            "market RPC did not prove the exact testnet passphrase"
        );
        let cursor = "4000000000000000001-0";
        let err = events_after(&st, &id(ID_A), cursor, 1)
            .expect_err("a changed network must refuse cursor continuation");
        assert_eq!(
            err.to_string(),
            "market RPC did not prove the exact testnet passphrase"
        );
        assert!(
            !err.to_string().contains("Public") && !err.to_string().contains(cursor),
            "the refusal echoes neither the endpoint nor the cursor: {err}"
        );

        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            6,
            "only version gates and identity probes may run; both queued pages must stay unfetched"
        );
        assert!(
            calls
                .iter()
                .all(|argv| argv.first().map(String::as_str) != Some("events")),
            "no event fetch may follow a failed identity check: {calls:?}"
        );
        assert!(
            calls.iter().all(|argv| !argv.iter().any(|a| a == cursor)),
            "the continuation cursor never reaches a changed endpoint"
        );
        for proof in calls.as_chunks::<2>().0 {
            assert_eq!(proof[0], ["--version"]);
            assert!(
                proof[1].len() >= 2 && proof[1][0] == "network" && proof[1][1] == "info",
                "every boundary must collect its own fresh identity proof: {calls:?}"
            );
        }
    }

    /// PRECEPT-P2-008: the `next_cursor` a page hands back must be a
    /// position the tool can actually resume from. Two events share one
    /// ledger here, so a ledger-numbered start can never reach the second
    /// one; continuing from the first page's cursor must return exactly
    /// that second event, once, and must not repeat the first.
    #[test]
    fn cursor_continuation_returns_the_next_same_ledger_event_exactly_once() {
        let n = net();
        let page_one = decoded_line(ID_A, 101, "4000000000000000001-0", "CONT-A");
        let page_two = decoded_line(ID_A, 101, "4000000000000000001-1", "CONT-B");
        let fx = crate::stellar::fake::FakeExecutor::new(vec![
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&page_one, ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&network_info_testnet(), ""),
            crate::stellar::fake::FakeExecutor::ok(V26, ""),
            crate::stellar::fake::FakeExecutor::ok(&page_two, ""),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Market);

        let safe = events(&st, &id(ID_A), 100, 1).expect("first page is a valid page");
        let first: Value = serde_json::from_str(&safe.to_string()).expect("page renders as JSON");
        assert_eq!(first["returned"], 1);
        assert_eq!(first["next_cursor"], "4000000000000000001-0");
        let cursor = first["next_cursor"]
            .as_str()
            .expect("continuation handle")
            .to_string();

        let safe = events_after(&st, &id(ID_A), &cursor, 1)
            .expect("the emitted cursor must resume the next event");
        let second: Value = serde_json::from_str(&safe.to_string()).expect("page renders as JSON");
        assert_eq!(second["returned"], 1);
        assert_eq!(second["events"][0]["cursor"], "4000000000000000001-1");
        assert_eq!(second["events"][0]["ledger"], 101);
        let rendered = safe.to_string();
        assert!(
            !rendered.contains("4000000000000000001-0"),
            "a continuation page must not repeat the event it continued past: {rendered}"
        );
        assert!(
            !rendered.contains("CONT-A") && !rendered.contains("CONT-B"),
            "nested payloads stay behind the page: {rendered}"
        );

        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            8,
            "two version gates and two fresh identity proofs plus two fetches"
        );
        assert_eq!(
            calls[3],
            vec![
                "events",
                "--id",
                ID_A,
                "--start-ledger",
                "100",
                "--count",
                "1",
                "--output",
                "json",
                "--network-passphrase",
                MARKET_TESTNET_PASSPHRASE,
                "--config-dir",
                "/cfg",
            ]
        );
        assert_eq!(
            calls[7],
            vec![
                "events",
                "--id",
                ID_A,
                "--cursor",
                "4000000000000000001-0",
                "--count",
                "1",
                "--output",
                "json",
                "--network-passphrase",
                MARKET_TESTNET_PASSPHRASE,
                "--config-dir",
                "/cfg",
            ]
        );
    }

    #[test]
    fn events_after_refuses_out_of_range_pages_before_any_cli_call() {
        let n = net();
        for count in [0, 101] {
            let fx = crate::stellar::fake::FakeExecutor::new(vec![]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let err = events_after(&st, &id(ID_A), "4000000000000000001-0", count)
                .expect_err("an out-of-range page cannot be requested");
            assert_eq!(err.to_string(), "event page count must be 1..=100");
            assert!(fx.calls.borrow().is_empty(), "no CLI call may run");
        }
    }

    #[test]
    fn events_after_refuses_a_malformed_cursor_before_any_cli_call() {
        for cursor in [
            "",
            "CURSOR-LEAK-1",
            "abcdef-1",
            "4000000000000000001",
            "4000000000000000001--1",
            "-4000000000000000001-1",
        ] {
            let n = net();
            let fx = crate::stellar::fake::FakeExecutor::new(vec![]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let err = events_after(&st, &id(ID_A), cursor, 10)
                .expect_err("a handle the pinned CLI cannot consume is refused");
            assert_eq!(err.to_string(), crate::stellar::MARKET_INVALID_CURSOR);
            assert!(
                !err.to_string().contains("CURSOR-LEAK"),
                "a refusal never echoes the cursor: {err}"
            );
            assert!(fx.calls.borrow().is_empty(), "no CLI call may run");
        }
    }

    mod settle_tests {
        use super::*;
        use crate::manifest::{
            begin_market_pending, market_pending_dir, MarketCustodyRecord, MarketMutationKind,
            MarketNetwork, MARKET_TESTNET_PASSPHRASE,
        };
        use crate::stellar::fake::FakeExecutor;

        const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
        const G_OTHER: &str = "GAYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAANVY";
        /// A valid contract address that is *not* the intent target.
        const C_UNRELATED: &str = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526";
        const C_POOL: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
        const H1: &str = "0000000000000000000000000000000000000000000000000000000000000001";
        const H2: &str = "0000000000000000000000000000000000000000000000000000000000000002";
        const SEQ_BASE: u64 = 2_089_035_585_552_490_000;
        const T0: u64 = 1_800_000_000;

        fn scratch(tag: &str) -> std::path::PathBuf {
            let dir =
                std::env::temp_dir().join(format!("abtest-settle-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn record() -> MarketCustodyRecord {
            MarketCustodyRecord {
                phase: 900,
                kind: MarketMutationKind::Invoke,
                action: Safe::literal("pool:set_data"),
                network: MarketNetwork::Testnet,
                signer: G_ADMIN.parse().unwrap(),
                payload_digest: Default::default(),
                target: Some(C_POOL.parse().unwrap()),
                artifact_digest: None,
                predicted: None,
                pre_state: Vec::new(),
                tool_error: None,
            }
        }

        /// Opens the coordinate at 900-45 with an Invoke intent whose
        /// mtime is fixed exactly at T0.
        fn open_intent(out: &std::path::Path) {
            let mut record = record();
            record.payload_digest = crate::market::custody_payload_digest(&record).unwrap();
            begin_market_pending(out, 900, 45, &record).unwrap();
            std::fs::File::options()
                .write(true)
                .open(market_pending_dir(out, 900, 45).join("intent.json"))
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(T0),
                )
                .unwrap();
        }

        fn envelope(seq: u64, source: &str, target: &str) -> String {
            envelope_with_body(
                seq,
                source,
                serde_json::json!({"invoke_host_function": {"host_function": {"invoke_contract": {
                    "contract_address": target,
                    "function_name": "mint",
                    "args": []
                }}, "auth": []}}),
            )
        }

        fn envelope_with_body(seq: u64, source: &str, body: serde_json::Value) -> String {
            serde_json::to_string(&serde_json::json!({
                "tx": {
                    "tx": {
                        "source_account": source,
                        "fee": 100,
                        "seq_num": seq.to_string(),
                        "cond": "none",
                        "operations": [{"source_account": null, "body": body}]
                    }
                }
            }))
            .unwrap()
        }

        /// Shaped like real CLI 26 meta: the source account's `state`
        /// (before) precedes its `updated` entry (after the sequence bump).
        fn meta(seq: u64, account: &str, seq_time: u64) -> String {
            let entry = |seq: u64, seq_time: u64| {
                serde_json::json!({"last_modified_ledger_seq": 5072869, "data": {"account": {
                    "account_id": account,
                    "seq_num": seq.to_string(),
                    "ext": {"v1": {"ext": {"v2": {"ext": {"v3": {
                        "seq_ledger": 5072869,
                        "seq_time": seq_time.to_string()
                    }}}}}}
                }}})
            };
            serde_json::to_string(&serde_json::json!({
                "v4": {"tx_changes_before": [
                    {"state": entry(seq - 1, seq_time - 10)},
                    {"updated": entry(seq, seq_time)}
                ]}
            }))
            .unwrap()
        }

        /// The valid window facts: two adjacent sequences signed by
        /// the admin, none invoking the intent target, the first closing
        /// at or before T0-60 and the last at or after the derived
        /// confirmed-send bound T0+3330+60.
        fn window() -> FakeExecutor {
            script(None, default_meta(), vec![])
        }

        fn default_meta() -> Vec<(String, u64, u64)> {
            vec![
                (G_ADMIN.to_string(), SEQ_BASE, T0 - 600),
                (G_ADMIN.to_string(), SEQ_BASE + 1, T0 + 3390),
            ]
        }

        /// Scripts one exact testnet proof, then per transaction pair:
        /// a pinned CLI version probe, the envelope (raw `String`
        /// override or the default admin/G_OTHER envelope), another
        /// version probe, and metadata (raw override or the recorded
        /// account/seq_time facts).
        /// Envelopes carry (source, target, seq, seq_time); metadata is
        /// recorded under `meta_accounts[i]` when given, else the row's
        /// own source. The wrong-source fixture uses the override so
        /// that ONLY the source guard can refuse.
        fn script_envelope_with_meta(
            tx: Vec<(String, String, u64, u64)>,
            meta_accounts: Vec<Option<String>>,
        ) -> FakeExecutor {
            let envelope_overrides: Vec<String> = tx
                .iter()
                .map(|(source, target, seq, _)| envelope(*seq, source, target))
                .collect();
            let meta = tx
                .iter()
                .enumerate()
                .map(|(i, (source, _, seq, seq_time))| {
                    (
                        meta_accounts[i].clone().unwrap_or_else(|| source.clone()),
                        *seq,
                        *seq_time,
                    )
                })
                .collect();
            script(Some(envelope_overrides), meta, vec![])
        }

        fn script_envelope(tx: Vec<(String, String, u64, u64)>) -> FakeExecutor {
            let meta_accounts = vec![None; tx.len()];
            script_envelope_with_meta(tx, meta_accounts)
        }

        fn script(
            envelope_overrides: Option<Vec<String>>,
            meta_facts: Vec<(String, u64, u64)>,
            meta_overrides: Vec<(usize, String)>,
        ) -> FakeExecutor {
            let info = serde_json::json!({"passphrase": MARKET_TESTNET_PASSPHRASE}).to_string();
            let mut calls = vec![FakeExecutor::ok(V26, ""), FakeExecutor::ok(&info, "")];
            for index in 0..meta_facts.len() {
                let seq = SEQ_BASE + index as u64;
                // Envelope read: its pinned version probe, then the
                // transaction envelope.
                calls.push(FakeExecutor::ok(V26, ""));
                let env_body = match &envelope_overrides {
                    Some(items) if index < items.len() => items[index].clone(),
                    _ => envelope(seq, G_ADMIN, C_UNRELATED),
                };
                calls.push(FakeExecutor::ok(&env_body, ""));
                // Meta read: its pinned version probe, then metadata.
                calls.push(FakeExecutor::ok(V26, ""));
                let meta_body = match meta_overrides.iter().find(|(i, _)| *i == index) {
                    Some((_, body)) => body.clone(),
                    None => {
                        let (account, seq, seq_time) = &meta_facts[index];
                        meta(*seq, account, *seq_time)
                    }
                };
                calls.push(FakeExecutor::ok(&meta_body, ""));
            }
            FakeExecutor::new(calls)
        }

        fn hashes(hashes: &[&str]) -> Vec<String> {
            hashes.iter().map(|h| h.to_string()).collect()
        }

        fn settled(out: &std::path::Path) -> bool {
            market_pending_dir(out, 900, 45)
                .join("result.json")
                .is_file()
        }

        fn run(
            out: &std::path::Path,
            fx: FakeExecutor,
            coordinate: &str,
            consumed: &[&str],
        ) -> (anyhow::Result<Safe>, Vec<Vec<String>>) {
            let n = net();
            let st = Stellar::new(&n, &fx, Policy::Market);
            let result = settle(&st, out, coordinate, &hashes(consumed));
            (result, fx.calls.borrow().clone())
        }

        #[test]
        fn settle_happy_path_writes_the_settlement() {
            let out = scratch("happy");
            open_intent(&out);
            let fx = window();
            let (result, calls) = run(&out, fx, "900-45", &[H1, H2]);
            let summary = match result {
                Ok(s) => s,
                Err(e) => panic!("valid settle: {e} calls={calls:?}"),
            };
            assert!(summary.to_string().contains("900-45"));
            // The durable record, read back through the journal reader,
            // keeps every consumed hash, not just the stdout summary.
            let (_, settlement) = crate::market_status::read_journal_coordinate(&out, 900, 45)
                .unwrap()
                .expect("coordinate present");
            let outcome = settlement.expect("settled").step.outcome.to_string();
            assert!(outcome.contains(H1) && outcome.contains(H2), "{outcome}");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_sequence_gap_without_writing() {
            let out = scratch("gap");
            open_intent(&out);
            let fx = script_envelope(vec![
                (
                    G_ADMIN.to_string(),
                    C_UNRELATED.to_string(),
                    SEQ_BASE,
                    T0 - 600,
                ),
                (
                    G_ADMIN.to_string(),
                    C_UNRELATED.to_string(),
                    SEQ_BASE + 2,
                    T0 + 3390,
                ),
            ]);
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_transaction_invoking_the_intent_target() {
            let out = scratch("invokes-target");
            open_intent(&out);
            let fx = script_envelope(vec![
                (
                    G_ADMIN.to_string(),
                    C_UNRELATED.to_string(),
                    SEQ_BASE,
                    T0 - 600,
                ),
                (
                    G_ADMIN.to_string(),
                    C_POOL.to_string(),
                    SEQ_BASE + 1,
                    T0 + 3390,
                ),
            ]);
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_first_sequence_at_or_after_the_intent() {
            let out = scratch("late-first");
            open_intent(&out);
            let fx = script_envelope(vec![
                (
                    G_ADMIN.to_string(),
                    C_UNRELATED.to_string(),
                    SEQ_BASE,
                    T0 - 59,
                ),
                (
                    G_ADMIN.to_string(),
                    C_UNRELATED.to_string(),
                    SEQ_BASE + 1,
                    T0 + 3390,
                ),
            ]);
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_an_attempt_window_ending_early() {
            let out = scratch("early-last");
            open_intent(&out);
            let fx = script_envelope(vec![
                (
                    G_ADMIN.to_string(),
                    C_UNRELATED.to_string(),
                    SEQ_BASE,
                    T0 - 600,
                ),
                (
                    G_ADMIN.to_string(),
                    C_UNRELATED.to_string(),
                    SEQ_BASE + 1,
                    T0 + 659,
                ),
            ]);
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_transaction_signed_by_another_account() {
            let out = scratch("wrong-source");
            open_intent(&out);
            let fx = script_envelope_with_meta(
                vec![
                    (
                        G_OTHER.to_string(),
                        C_UNRELATED.to_string(),
                        SEQ_BASE,
                        T0 - 600,
                    ),
                    (
                        G_ADMIN.to_string(),
                        C_UNRELATED.to_string(),
                        SEQ_BASE + 1,
                        T0 + 3390,
                    ),
                ],
                vec![Some(G_ADMIN.to_string()), Some(G_ADMIN.to_string())],
            );
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_fee_bump_envelope() {
            let out = scratch("fee-bump");
            open_intent(&out);
            let fx = script(
                Some(vec![serde_json::to_string(
                    &serde_json::json!({"tx_fee_bump": {}}),
                )
                .unwrap()]),
                default_meta(),
                vec![],
            );
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_an_unknown_envelope_shape() {
            let out = scratch("unknown-envelope");
            open_intent(&out);
            let fx = script(
                Some(vec![serde_json::to_string(
                    &serde_json::json!({"tx": {"tx": {}}}),
                )
                .unwrap()]),
                default_meta(),
                vec![],
            );
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_an_unmatched_metadata_sequence() {
            let out = scratch("meta-mismatch");
            open_intent(&out);
            let fx = script(
                None,
                default_meta(),
                vec![(
                    0usize,
                    serde_json::to_string(&serde_json::json!({
                        "v4": {"tx_changes_before": [{
                            "updated": {"data": {"account": {
                                "account_id": G_ADMIN,
                                "seq_num": (SEQ_BASE + 7).to_string(),
                                "ext": {"v1": {"ext": {"v2": {"ext": {"v3": {
                                    "seq_time": (T0 - 600).to_string()
                                }}}}}}
                            }}}
                        }]}
                    }))
                    .unwrap(),
                )],
            );
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_metadata_without_the_signer_entry() {
            let out = scratch("meta-missing");
            open_intent(&out);
            let fx = script(
                None,
                default_meta(),
                vec![(
                    0usize,
                    serde_json::to_string(&serde_json::json!({
                        "v4": {"tx_changes_before": []}
                    }))
                    .unwrap(),
                )],
            );
            assert!(run(&out, fx, "900-45", &[H1, H2]).0.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_duplicate_hash_before_any_cli_call() {
            let out = scratch("dup");
            open_intent(&out);
            let (result, calls) = run(&out, FakeExecutor::new(vec![]), "900-45", &[H1, H1]);
            assert!(result.is_err());
            assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_single_hash_before_any_cli_call() {
            let out = scratch("single");
            open_intent(&out);
            let (result, calls) = run(&out, FakeExecutor::new(vec![]), "900-45", &[H1]);
            assert!(result.is_err());
            assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_a_malformed_coordinate_before_any_cli_call() {
            for coordinate in ["90-45", "9000-45", "900-4", "900-455", "900-4a", "x00-45"] {
                let out = scratch("coordinate");
                open_intent(&out);
                let (result, calls) = run(&out, FakeExecutor::new(vec![]), coordinate, &[H1, H2]);
                assert!(result.is_err());
                assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
                assert!(!settled(&out), "a refusal writes no settlement");
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        #[test]
        fn settle_refuses_a_malformed_hash_before_any_cli_call() {
            for hash in [
                "f".repeat(63),
                "F".repeat(64),
                "z".repeat(64),
                "0xdead".into(),
            ] {
                let out = scratch("hash");
                open_intent(&out);
                let (result, calls) = run(&out, FakeExecutor::new(vec![]), "900-45", &[H1, &hash]);
                assert!(result.is_err());
                assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
                assert!(!settled(&out), "a refusal writes no settlement");
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        #[test]
        fn settle_refuses_a_coordinate_that_is_unreserved() {
            let out = scratch("absent");
            std::fs::create_dir_all(out.join("market-pending")).unwrap();
            let (result, calls) = run(&out, FakeExecutor::new(vec![]), "900-45", &[H1, H2]);
            assert!(result.is_err());
            assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_an_already_settled_coordinate() {
            let out = scratch("settled");
            open_intent(&out);
            let mut custody = record();
            custody.payload_digest = crate::market::custody_payload_digest(&custody).unwrap();
            crate::manifest::finish_market_pending(
                &out,
                900,
                45,
                &crate::manifest::MarketPendingResult {
                    custody: custody.clone(),
                    step: crate::manifest::MarketStepRecord {
                        phase: 900,
                        action: custody.action.clone(),
                        command: custody.action.clone(),
                        outcome: Safe::literal("accepted"),
                        target: custody.target.clone(),
                        violations: Vec::new(),
                    },
                },
            )
            .unwrap();
            let (result, calls) = run(&out, FakeExecutor::new(vec![]), "900-45", &[H1, H2]);
            assert!(result.is_err());
            assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_intents_outside_the_guarded_action_path() {
            // A non-invoke kind, a targetless invoke, and an invoke outside
            // the 900..=999 action phases are all refused before any CLI.
            for (kind, target, phase) in [
                (MarketMutationKind::Upload, Some(C_POOL), 900),
                (MarketMutationKind::Invoke, None, 900),
                (MarketMutationKind::Invoke, Some(C_POOL), 1),
            ] {
                let out = scratch("kind");
                let mut custody = record();
                custody.phase = phase;
                custody.kind = kind;
                custody.target = target.map(|t| t.parse().unwrap());
                custody.payload_digest = crate::market::custody_payload_digest(&custody).unwrap();
                begin_market_pending(&out, phase, 45, &custody).unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(market_pending_dir(&out, phase, 45).join("intent.json"))
                    .unwrap()
                    .set_modified(
                        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(T0),
                    )
                    .unwrap();
                let coordinate = format!("{phase:03}-45");
                let (result, calls) = run(&out, FakeExecutor::new(vec![]), &coordinate, &[H1, H2]);
                assert!(result.is_err());
                assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
                assert!(
                    !market_pending_dir(&out, phase, 45)
                        .join("result.json")
                        .exists(),
                    "a refusal writes no settlement"
                );
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        #[test]
        fn settle_refuses_an_operation_body_with_an_unknown_arm() {
            let out = scratch("mixed-arm");
            open_intent(&out);
            let mixed = envelope_with_body(
                SEQ_BASE,
                G_ADMIN,
                serde_json::json!({"payment": {}, "unknown_operation": {}}),
            );
            let (result, _) = run(
                &out,
                script(Some(vec![mixed]), default_meta(), vec![]),
                "900-45",
                &[H1, H2],
            );
            assert!(result.is_err());
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        /// Unrelated consumers in their native CLI 26 shapes: a wasm
        /// upload (bare hex payload), a TTL extension, and the unit
        /// sponsorship arm (a bare string, not an object).
        #[test]
        fn settle_counts_native_non_invoking_operations_as_unrelated_consumers() {
            for (tag, body) in [
                (
                    "upload",
                    serde_json::json!({"invoke_host_function": {
                        "host_function": {"upload_contract_wasm": "0061736d01000000"},
                        "auth": []
                    }}),
                ),
                (
                    "extend-ttl",
                    serde_json::json!({"extend_footprint_ttl": {"ext": "v0", "extend_to": 100}}),
                ),
                (
                    "end-sponsoring",
                    serde_json::json!("end_sponsoring_future_reserves"),
                ),
            ] {
                let out = scratch(tag);
                open_intent(&out);
                let consumer = envelope_with_body(SEQ_BASE, G_ADMIN, body);
                let (result, calls) = run(
                    &out,
                    script(Some(vec![consumer]), default_meta(), vec![]),
                    "900-45",
                    &[H1, H2],
                );
                assert!(
                    result.is_ok(),
                    "{tag} must count as unrelated: calls={calls:?}"
                );
                assert!(settled(&out), "{tag}");
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        #[test]
        fn settle_refuses_a_symlinked_intent_record() {
            let out = scratch("mtime");
            open_intent(&out);
            // The pinned journal reader refuses the symlinked record;
            // the mtime probe is the no-follow second line of defense.
            std::fs::remove_file(market_pending_dir(&out, 900, 45).join("intent.json")).unwrap();
            std::os::unix::fs::symlink(
                "/tmp",
                market_pending_dir(&out, 900, 45).join("intent.json"),
            )
            .unwrap();
            let (result, calls) = run(&out, FakeExecutor::new(vec![]), "900-45", &[H1, H2]);
            assert!(result.is_err());
            assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn settle_refuses_more_than_256_hashes_before_any_cli_call() {
            let out = scratch("too-many");
            open_intent(&out);
            let too_many = vec![H1; 257];
            let (result, calls) = run(&out, FakeExecutor::new(vec![]), "900-45", &too_many);
            assert!(result.is_err());
            assert!(calls.is_empty(), "pre-CLI refusals capture no calls");
            assert!(!settled(&out), "a refusal writes no settlement");
            let _ = std::fs::remove_dir_all(&out);
        }

        /// A real market's frozen plan: the network it names is the network
        /// a real settlement may prove, and no caller chooses it.
        fn freeze_plan(out: &std::path::Path, network: MarketNetwork) {
            let plan = crate::deploy::DeploymentPlan {
                schema: 1,
                config_digest: "0".repeat(64),
                nonce: "0".repeat(16),
                network_label: network.label().to_string(),
                passphrase: network.passphrase().to_string(),
                admin: G_ADMIN.to_string(),
                artifacts: std::collections::BTreeMap::new(),
                releases: std::collections::BTreeMap::new(),
                salt_roles: vec![],
                slots: vec![(3, 7)],
                uploads: vec![],
            };
            crate::deploy::write_deployment_plan(out, &plan).expect("frozen plan");
        }

        /// Opens a real deploy coordinate with the intent its CLI call left
        /// behind, its mtime fixed exactly at T0.
        fn open_real_intent(out: &std::path::Path, phase: u32, slot: u32, kind: MarketMutationKind) {
            open_real_intent_with(out, phase, slot, kind, None, None);
        }

        /// The same, naming the code a deployment declared (`digest`) or the
        /// contract an invoke or config coordinate acts on (`target`).
        fn open_real_intent_with(
            out: &std::path::Path,
            phase: u32,
            slot: u32,
            kind: MarketMutationKind,
            digest: Option<&str>,
            target: Option<&str>,
        ) {
            let mut record = MarketCustodyRecord {
                phase,
                kind,
                action: Safe::literal("deploy:pool"),
                network: MarketNetwork::Testnet,
                signer: G_ADMIN.parse().unwrap(),
                payload_digest: Default::default(),
                target: target.map(|id| id.parse().unwrap()),
                artifact_digest: digest
                    .map(|hash| crate::manifest::Sha256Hex::try_from(hash.to_string()).unwrap()),
                predicted: Some(C_POOL.parse().unwrap()),
                pre_state: Vec::new(),
                tool_error: None,
            };
            record.payload_digest = crate::market::custody_payload_digest(&record).unwrap();
            begin_market_pending(out, phase, slot, &record).unwrap();
            std::fs::File::options()
                .write(true)
                .open(market_pending_dir(out, phase, slot).join("intent.json"))
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(T0),
                )
                .unwrap();
        }

        /// The same window proof, widened to the bound a real coordinate's
        /// send can occupy: its last consumer closes after that bound.
        fn real_window() -> FakeExecutor {
            let bound = crate::stellar::REAL_COORDINATE_SEND_BOUND_SECS;
            then_read(
                script(
                    None,
                    vec![
                        (G_ADMIN.to_string(), SEQ_BASE, T0 - 600),
                        (G_ADMIN.to_string(), SEQ_BASE + 1, T0 + bound + 61),
                    ],
                    vec![],
                ),
                instance_absent(),
            )
        }

        /// The ledger's answer for a contract that is not there, as stellar
        /// CLI 26 prints it: exit 1, `contract not found: <id>` on stderr, and
        /// nothing on stdout (checked against the public testnet RPC).
        fn instance_absent() -> crate::stellar::CmdOutput {
            FakeExecutor::fail(&format!("\u{274c} error: contract not found: {C_POOL}"))
        }

        /// The ledger's answer for a contract instance that exists.
        fn instance_present() -> crate::stellar::CmdOutput {
            FakeExecutor::ok(
                r#"{"entries":[{"key":"AAAA","xdr":"AAAA","lastModifiedLedgerSeq":5,"liveUntilLedgerSeq":9}],"latestLedger":9}"#,
                "",
            )
        }

        /// Queue one more answer after everything already scripted: the
        /// predicted-contract read a real deploy settle makes once its window
        /// is proven.
        fn then_read(fx: FakeExecutor, answer: crate::stellar::CmdOutput) -> FakeExecutor {
            fx.outputs.borrow_mut().insert(0, answer);
            fx
        }

        /// A window of `bodies.len()` adjacent signer transactions, one per
        /// body, spanning the real send bound, followed by the instance read.
        /// Returns the script and the hashes to consume.
        fn real_window_of(
            bodies: Vec<serde_json::Value>,
            instance: crate::stellar::CmdOutput,
        ) -> (FakeExecutor, Vec<String>) {
            let bound = crate::stellar::REAL_COORDINATE_SEND_BOUND_SECS;
            let last = bodies.len() - 1;
            let facts = (0..bodies.len())
                .map(|i| {
                    let time = if i == 0 {
                        T0 - 600
                    } else if i == last {
                        T0 + bound + 61
                    } else {
                        T0 + i as u64
                    };
                    (G_ADMIN.to_string(), SEQ_BASE + i as u64, time)
                })
                .collect();
            let envelopes = bodies
                .into_iter()
                .enumerate()
                .map(|(i, body)| envelope_with_body(SEQ_BASE + i as u64, G_ADMIN, body))
                .collect();
            let hashes = (1..=last + 1).map(|i| format!("{i:064x}")).collect();
            (then_read(script(Some(envelopes), facts, vec![]), instance), hashes)
        }

        /// The Wasm digest a coordinate under test declared.
        fn own_wasm() -> String {
            "ab".repeat(32)
        }

        /// CLI 26's `create_contract_v2` / `create_contract` host function
        /// installing `wasm` under an address preimage, as `tx fetch` prints it.
        fn create_wasm(version: &str, wasm: &str) -> serde_json::Value {
            let mut create = serde_json::json!({
                "contract_id_preimage": {"address": {
                    "address": G_ADMIN,
                    "salt": "00".repeat(32)
                }},
                "executable": {"wasm": wasm}
            });
            if version == "create_contract_v2" {
                create["constructor_args"] = serde_json::json!([]);
            }
            serde_json::json!({"invoke_host_function": {
                "host_function": {version: create},
                "auth": []
            }})
        }

        /// The run's own Stellar Asset Contract deployment.
        fn create_sac() -> serde_json::Value {
            serde_json::json!({"invoke_host_function": {
                "host_function": {"create_contract": {
                    "contract_id_preimage": {"asset": "native"},
                    "executable": "stellar_asset"
                }},
                "auth": []
            }})
        }

        /// The classic operations a real run's own phase 2 sends, in CLI 26
        /// shapes.
        fn classic_ops() -> Vec<serde_json::Value> {
            vec![
                serde_json::json!({"create_account": {
                    "destination": G_OTHER, "starting_balance": 20_000_000
                }}),
                serde_json::json!({"change_trust": {
                    "line": {"credit_alphanum4": {"asset_code": "USDC", "issuer": G_OTHER}},
                    "limit": "9223372036854775807"
                }}),
                serde_json::json!({"payment": {
                    "destination": G_OTHER, "asset": "native", "amount": 1
                }}),
                serde_json::json!({"set_options": {
                    "inflation_dest": null, "clear_flags": null, "set_flags": null,
                    "master_weight": 0, "low_threshold": 1, "med_threshold": 1,
                    "high_threshold": 1, "home_domain": null, "signer": null
                }}),
            ]
        }

        fn run_real(
            out: &std::path::Path,
            fx: FakeExecutor,
            coordinate: &str,
            consumed: &[&str],
        ) -> (anyhow::Result<Safe>, Vec<Vec<String>>) {
            let n = net();
            let st = Stellar::new(&n, &fx, Policy::Market);
            let result = settle_unapplied(&st, out, coordinate, &hashes(consumed));
            (result, fx.calls.borrow().clone())
        }

        fn archived(out: &std::path::Path, name: &str) -> bool {
            out.join(NEVER_APPLIED_DIR)
                .join(name)
                .join("unapplied.json")
                .is_file()
        }

        /// The escape hatch a stranded real run needs: the coordinate leaves
        /// the journal with its evidence, and nothing reserves it again —
        /// so the next resume re-runs it under the ordinary gated path.
        #[test]
        fn a_real_coordinate_proven_never_applied_is_archived_for_replay() {
            let out = scratch("real-happy");
            freeze_plan(&out, MarketNetwork::Testnet);
            open_real_intent(&out, 3, 7, MarketMutationKind::Deploy);
            let (result, calls) = run_real(&out, real_window(), "003-07", &[H1, H2]);
            let summary = match result {
                Ok(s) => s,
                Err(e) => panic!("a real coordinate with a proven window settles: {e} calls={calls:?}"),
            };
            assert!(summary.to_string().contains("003-07"), "{summary}");
            assert!(archived(&out, "003-07"), "the evidence survives the run");
            let archive = out.join(NEVER_APPLIED_DIR).join("003-07");
            assert!(
                archive.join("intent.json").is_file() && archive.join("unapplied.json").is_file(),
                "the archived coordinate keeps the intent bytes and the proof together"
            );
            assert!(
                !market_pending_dir(&out, 3, 7).exists(),
                "the coordinate left the journal"
            );
            // The observable acceptance: the same coordinate can be reserved
            // again, which is exactly what a resume does to re-run it.
            let mut record = MarketCustodyRecord {
                phase: 3,
                kind: MarketMutationKind::Deploy,
                action: Safe::literal("deploy:pool"),
                network: MarketNetwork::Testnet,
                signer: G_ADMIN.parse().unwrap(),
                payload_digest: Default::default(),
                target: None,
                artifact_digest: None,
                predicted: Some(C_POOL.parse().unwrap()),
                pre_state: Vec::new(),
                tool_error: None,
            };
            record.payload_digest = crate::market::custody_payload_digest(&record).unwrap();
            begin_market_pending(&out, 3, 7, &record).expect("a resume can reserve the coordinate again");
            let _ = std::fs::remove_dir_all(&out);
        }

        /// The window a real run actually leaves: its own classic operations,
        /// its Stellar Asset Contract deployment and a deployment of some
        /// other contract's code. None of those can be a deployment of this
        /// coordinate's Wasm, so none may stop it from being proven unapplied.
        #[test]
        fn a_real_deploy_settles_across_the_runs_own_classic_and_other_code_transactions() {
            let out = scratch("real-mix");
            freeze_plan(&out, MarketNetwork::Testnet);
            open_real_intent_with(&out, 3, 7, MarketMutationKind::Deploy, Some(&own_wasm()), None);
            let mut bodies = classic_ops();
            bodies.push(create_sac());
            bodies.push(create_wasm("create_contract_v2", &"cd".repeat(32)));
            bodies.push(serde_json::json!({"invoke_host_function": {
                "host_function": {"upload_contract_wasm": "0061736d01000000"},
                "auth": []
            }}));
            let (fx, window) = real_window_of(bodies, instance_absent());
            let consumed: Vec<&str> = window.iter().map(String::as_str).collect();
            let (result, calls) = run_real(&out, fx, "003-07", &consumed);
            assert!(
                result.is_ok(),
                "a real run's own transactions cannot be this deployment: {:?} calls={calls:?}",
                result.err().map(|e| e.to_string())
            );
            assert!(archived(&out, "003-07"), "the proven coordinate is archived for replay");
            assert!(!market_pending_dir(&out, 3, 7).exists());
            let _ = std::fs::remove_dir_all(&out);
        }

        /// A window transaction that installs this coordinate's own Wasm could
        /// be the attempt that landed: refused under either creation form, and
        /// nothing is archived.
        #[test]
        fn a_real_deploy_refuses_a_window_transaction_that_installs_its_own_code() {
            for version in ["create_contract_v2", "create_contract"] {
                let out = scratch(&format!("real-own-code-{version}"));
                freeze_plan(&out, MarketNetwork::Testnet);
                open_real_intent_with(&out, 3, 7, MarketMutationKind::Deploy, Some(&own_wasm()), None);
                let mut bodies = classic_ops();
                bodies.push(create_wasm(version, &own_wasm()));
                bodies.push(serde_json::json!({"payment": {
                    "destination": G_OTHER, "asset": "native", "amount": 1
                }}));
                let (fx, window) = real_window_of(bodies, instance_absent());
                let consumed: Vec<&str> = window.iter().map(String::as_str).collect();
                let (result, _) = run_real(&out, fx, "003-07", &consumed);
                assert!(result.is_err(), "{version}: a creation of the intent's Wasm is not unrelated");
                assert!(!archived(&out, "003-07"), "{version}: a refusal archives nothing");
                assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        /// A creation whose executable this reader cannot classify might be
        /// the attempt, so it refuses; an operation of a shape the reader does
        /// not know refuses the read itself.
        #[test]
        fn a_real_deploy_refuses_what_it_cannot_classify() {
            let unclassified = |executable: serde_json::Value| {
                serde_json::json!({"invoke_host_function": {
                    "host_function": {"create_contract_v2": {
                        "contract_id_preimage": {"address": {
                            "address": G_ADMIN, "salt": "00".repeat(32)
                        }},
                        "executable": executable,
                        "constructor_args": []
                    }},
                    "auth": []
                }})
            };
            for (tag, body) in [
                ("odd-executable", unclassified(serde_json::json!({"wasm_hash": own_wasm()}))),
                ("short-hash", unclassified(serde_json::json!({"wasm": "abab"}))),
                ("no-executable", serde_json::json!({"invoke_host_function": {
                    "host_function": {"create_contract_v2": {"constructor_args": []}},
                    "auth": []
                }})),
                ("unknown-operation", serde_json::json!({"unknown_operation": {}})),
            ] {
                let out = scratch(&format!("real-unclassified-{tag}"));
                freeze_plan(&out, MarketNetwork::Testnet);
                open_real_intent_with(&out, 3, 7, MarketMutationKind::Deploy, Some(&own_wasm()), None);
                let (fx, window) = real_window_of(vec![body, classic_ops().remove(2)], instance_absent());
                let consumed: Vec<&str> = window.iter().map(String::as_str).collect();
                let (result, _) = run_real(&out, fx, "003-07", &consumed);
                assert!(result.is_err(), "{tag}: an unclassifiable transaction cannot stand as unrelated");
                assert!(!archived(&out, "003-07"), "{tag}");
                assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        /// An invoke or config coordinate is its target's invocation. The run's
        /// classic operations and creations are unrelated to it, but a window
        /// transaction that invokes the target is the attempt, whatever else the
        /// window holds.
        #[test]
        fn a_real_invoke_or_config_is_asked_only_whether_its_target_was_invoked() {
            for kind in [MarketMutationKind::Invoke, MarketMutationKind::Config] {
                let out = scratch(&format!("real-invoke-ok-{kind:?}"));
                freeze_plan(&out, MarketNetwork::Testnet);
                open_real_intent_with(&out, 3, 7, kind, None, Some(C_POOL));
                let mut bodies = classic_ops();
                bodies.push(create_sac());
                bodies.push(create_wasm("create_contract_v2", &own_wasm()));
                let (fx, window) = real_window_of(bodies, instance_absent());
                let consumed: Vec<&str> = window.iter().map(String::as_str).collect();
                let (result, calls) = run_real(&out, fx, "003-07", &consumed);
                assert!(
                    result.is_ok(),
                    "{kind:?}: classic operations and creations are unrelated: {:?} calls={calls:?}",
                    result.err().map(|e| e.to_string())
                );
                assert!(archived(&out, "003-07"), "{kind:?}");
                let _ = std::fs::remove_dir_all(&out);

                let out = scratch(&format!("real-invoke-target-{kind:?}"));
                freeze_plan(&out, MarketNetwork::Testnet);
                open_real_intent_with(&out, 3, 7, kind, None, Some(C_POOL));
                let mut bodies = classic_ops();
                bodies.push(serde_json::json!({"invoke_host_function": {
                    "host_function": {"invoke_contract": {
                        "contract_address": C_POOL, "function_name": "set_status", "args": []
                    }},
                    "auth": []
                }}));
                bodies.push(classic_ops().remove(2));
                let (fx, window) = real_window_of(bodies, instance_absent());
                let consumed: Vec<&str> = window.iter().map(String::as_str).collect();
                let (result, _) = run_real(&out, fx, "003-07", &consumed);
                assert!(result.is_err(), "{kind:?}: a window transaction invoking the target is the attempt");
                assert!(!archived(&out, "003-07"), "{kind:?}");
                assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        /// A deployment that landed leaves its contract behind, so a proven
        /// window is not enough: the predicted address must read back empty.
        /// A read that failed proves nothing and refuses just the same.
        #[test]
        fn a_real_deploy_refuses_unless_the_predicted_contract_is_provably_absent() {
            let failed_reads = [
                ("present", instance_present()),
                ("rpc-failure", FakeExecutor::fail("error: rpc unreachable")),
                (
                    "not-found-for-another-contract",
                    FakeExecutor::fail(&format!("error: contract not found: {C_UNRELATED}")),
                ),
                ("not-json", FakeExecutor::ok("not json", "")),
                (
                    "two-entries",
                    FakeExecutor::ok(r#"{"entries":[{"a":1},{"a":2}],"latestLedger":9}"#, ""),
                ),
            ];
            for (tag, answer) in failed_reads {
                let out = scratch(&format!("real-exists-{tag}"));
                freeze_plan(&out, MarketNetwork::Testnet);
                open_real_intent_with(&out, 3, 7, MarketMutationKind::Deploy, Some(&own_wasm()), None);
                let (fx, window) = real_window_of(classic_ops(), answer);
                let consumed: Vec<&str> = window.iter().map(String::as_str).collect();
                let (result, calls) = run_real(&out, fx, "003-07", &consumed);
                assert!(result.is_err(), "{tag}: the contract is not provably absent");
                assert!(!archived(&out, "003-07"), "{tag}: a refusal archives nothing");
                assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
                let read = calls.last().expect("a read was made");
                assert_eq!(
                    read[..4],
                    ["ledger", "entry", "fetch", "contract-data"].map(String::from)[..],
                    "{tag}: the presence read is the ledger's own: {read:?}"
                );
                assert!(read.iter().any(|word| word == "--instance"), "{tag}: {read:?}");
                let _ = std::fs::remove_dir_all(&out);
            }
        }

        /// Every refusal leaves the journal exactly as it found it: a
        /// settlement is never half-applied, because a coordinate that is
        /// neither settled nor replayable is worse than an open intent.
        #[test]
        fn a_real_settlement_refuses_without_touching_the_journal() {
            // A run with no frozen plan is not a real run.
            let out = scratch("real-no-plan");
            open_real_intent(&out, 3, 7, MarketMutationKind::Deploy);
            let (result, _) = run_real(&out, real_window(), "003-07", &[H1, H2]);
            assert!(result.is_err(), "a partial real run needs its frozen plan");
            assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
            let _ = std::fs::remove_dir_all(&out);

            // A published run is settled: its coordinates are history.
            let out = scratch("real-published");
            freeze_plan(&out, MarketNetwork::Testnet);
            std::fs::write(out.join("market-manifest.toml"), "schema = 1\n").unwrap();
            open_real_intent(&out, 3, 7, MarketMutationKind::Deploy);
            let (result, _) = run_real(&out, real_window(), "003-07", &[H1, H2]);
            assert!(result.is_err(), "a completed run never replays a coordinate");
            assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
            let _ = std::fs::remove_dir_all(&out);

            // An open upload or funded key is not a submission to a target.
            for (tag, kind) in [
                ("upload", MarketMutationKind::Upload),
                ("fundkey", MarketMutationKind::FundKey),
            ] {
                let out = scratch(&format!("real-kind-{tag}"));
                freeze_plan(&out, MarketNetwork::Testnet);
                open_real_intent(&out, 3, 7, kind);
                let (result, _) = run_real(&out, FakeExecutor::new(vec![]), "003-07", &[H1, H2]);
                assert!(result.is_err(), "an open {kind:?} is settled by its own law");
                assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
                let _ = std::fs::remove_dir_all(&out);
            }

            // The network is the plan's, not the caller's.
            let out = scratch("real-wrong-network");
            freeze_plan(&out, MarketNetwork::Mainnet);
            open_real_intent(&out, 3, 7, MarketMutationKind::Deploy);
            let (result, calls) = run_real(&out, real_window(), "003-07", &[H1, H2]);
            assert!(result.is_err(), "a mainnet plan is not proven on a testnet endpoint");
            assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
            let _ = std::fs::remove_dir_all(&out);
            let _ = calls;

            // The graph moved on: a later coordinate holds an intent, so the
            // sequence window cannot be attributed to this one alone.
            let out = scratch("real-later-intent");
            freeze_plan(&out, MarketNetwork::Testnet);
            open_real_intent(&out, 3, 7, MarketMutationKind::Deploy);
            open_real_intent(&out, 3, 8, MarketMutationKind::Config);
            let (result, _) = run_real(&out, real_window(), "003-07", &[H1, H2]);
            assert!(result.is_err(), "a run whose graph advanced is not replayable");
            assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
            let _ = std::fs::remove_dir_all(&out);

            // A window operation of a shape the envelope reader does not know
            // (here a `create_contract` placed where an operation body belongs)
            // refuses the read itself, so it cannot prove anything absent.
            let out = scratch("real-upload-window");
            freeze_plan(&out, MarketNetwork::Testnet);
            open_real_intent(&out, 3, 7, MarketMutationKind::Deploy);
            let upload = envelope_with_body(
                SEQ_BASE + 1,
                G_ADMIN,
                serde_json::json!({"create_contract": {"wasm_hash": "ab".repeat(32)}}),
            );
            let bound = crate::stellar::REAL_COORDINATE_SEND_BOUND_SECS;
            let fx = script(
                Some(vec![
                    envelope(SEQ_BASE, G_ADMIN, C_UNRELATED),
                    upload,
                ]),
                vec![
                    (G_ADMIN.to_string(), SEQ_BASE, T0 - 600),
                    (G_ADMIN.to_string(), SEQ_BASE + 1, T0 + bound + 61),
                ],
                vec![],
            );
            let (result, _) = run_real(&out, fx, "003-07", &[H1, H2]);
            assert!(result.is_err(), "an unreadable operation cannot stand as unrelated");
            assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
            let _ = std::fs::remove_dir_all(&out);

            // And one touching the contract this coordinate was to create.
            let out = scratch("real-target-window");
            freeze_plan(&out, MarketNetwork::Testnet);
            open_real_intent(&out, 3, 7, MarketMutationKind::Deploy);
            let bound = crate::stellar::REAL_COORDINATE_SEND_BOUND_SECS;
            let fx = script(
                Some(vec![
                    envelope(SEQ_BASE, G_ADMIN, C_UNRELATED),
                    envelope(SEQ_BASE + 1, G_ADMIN, C_POOL),
                ]),
                vec![
                    (G_ADMIN.to_string(), SEQ_BASE, T0 - 600),
                    (G_ADMIN.to_string(), SEQ_BASE + 1, T0 + bound + 61),
                ],
                vec![],
            );
            let (result, _) = run_real(&out, fx, "003-07", &[H1, H2]);
            assert!(result.is_err(), "a transaction touching the target is not unrelated");
            assert!(market_pending_dir(&out, 3, 7).join("intent.json").is_file());
            let _ = std::fs::remove_dir_all(&out);
        }
    }
}
