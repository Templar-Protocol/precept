//! Lockstep execution of scenarios against both sides, with evidence per step.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};

use crate::compare::compare_step;
use crate::deploy::{request_json, reserve_config, set_prices};
use crate::manifest::{
    begin_pending, clear_pending, finish_pending, pending_step_dir, reap_safe_pending,
    PairManifest, PendingIntent, Profile, RunState, SideManifest,
    SideOutcome as ManifestSideOutcome, Tok, Who,
};
use crate::observe::{
    invariants, no_state_change, num, snapshot, strict_map, try_num, Snapshot, StepRecord,
};
use crate::report::{write_step, StepEvidence};
use crate::scenario::{
    family_of, fork_expect, loss_prerequisites_ok, req, select_scenarios, traps, Expect,
    LabeledStep, Scenario, Step, TrapCall, SELECTION_GUIDANCE,
};

/// Evidence scenario name for operator-initiated steps; excluded from the verdict.
pub const OPERATOR_SCENARIO: &str = "operator";
use crate::stellar::{definite_outcome, ErrorKind, Executor, Invocation, Policy, Safe, Stellar};

fn kv(k: &str, v: impl ToString) -> (&str, String) {
    (k, v.to_string())
}

/// The raw-output policy for one pair side. The pair runner is pair-mode by
/// construction: its `Stellar` context is built with `Policy::Pair`, and every
/// pair evidence byte is pinned by AC9. The signature mirrors
/// `invariant_policy` so every policy-selected call site names the policy it
/// acts under instead of relying on a default. `MarketRunner` fixes
/// `Policy::Market` at its own execution-context construction (plan §3 item 2).
pub fn run_policy(_profile: Profile, _side: char) -> Policy {
    Policy::Pair
}

/// The single policy-armed store path for composed evidence text in this file.
/// The pair arm of `Safe::error_chain` stores the message verbatim —
/// byte-identical to the retired direct formatting (AC9 byte gate) — and the
/// market arm sanitizes before storage. Every `Safe` written by this file goes
/// through it or a more specific policy-arm constructor, so no lane can
/// persist unsanitized bytes outside `stellar.rs`.
fn policy_store(policy: Policy, msg: String) -> Safe {
    Safe::error_chain(policy, &anyhow::anyhow!("{msg}"))
}

// --------------------------------------------------------------------------
// Event protocol
//
// The runner reports progress by calling an injected hook, which returns the
// action to take. Headless callers pass no hook: `gate` then prints the legacy
// stderr line and returns `Continue`, so CI output is unchanged.
//
// An `Act` is only honoured at the variants marked "gate" below; returning one
// from any other variant is ignored by design.
// --------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Ev {
    /// Not a gate. `plan` is (scenario, step labels) including appended traps.
    RunStarted {
        plan: Vec<(String, Vec<String>)>,
        resumed: usize,
    },
    /// Gate. Acting here avoids executing the step at all.
    StepStarted {
        scenario: String,
        index: usize,
        label: String,
    },
    /// Not a gate. Per-side progress within a step.
    SideDone {
        tag: char,
        ok: bool,
        err: Option<Safe>,
    },
    /// Gate (Abort/Skip only). Emitted at least every 5s while waiting.
    Waiting {
        label: String,
        now: u32,
        target: u32,
        secs_left: u64,
    },
    /// Gate. Completed evidence is immutable; Continue/Skip advances.
    StepDone {
        evidence: Box<StepEvidence>,
        violations: usize,
        desynced: bool,
    },
    /// Not a gate. The step was not run at all.
    StepSkipped {
        scenario: String,
        label: String,
        why: String,
    },
    /// Gate. A hard error, or a semantic halt awaiting an operator decision.
    Halt {
        scenario: String,
        label: String,
        reason: Safe,
    },
    /// Not a gate. One `stellar` CLI invocation, for the stream panel.
    Cli {
        argv: String,
        ok: bool,
        ms: u64,
        stderr: String,
        transient: bool,
    },
    RunFinished {
        error: Option<Safe>,
    },
}

#[derive(Clone, Debug)]
pub enum Act {
    Continue,
    Skip,
    Abort,
    /// Run an ad-hoc operator step now, then re-enter the same gate.
    Exec {
        step: Box<LabeledStep>,
        sides: Sides,
    },
    /// Snapshot both sides into evidence, then re-enter the same gate.
    Snapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sides {
    A,
    B,
    Both,
}

impl Sides {
    fn covers(self, tag: char) -> bool {
        matches!(
            (self, tag),
            (Sides::Both, _) | (Sides::A, 'a') | (Sides::B, 'b')
        )
    }
}
/// The typed invariant policy for one side: fork-only zero-credit / drained
/// assumptions are asserted directly for the fork side, and for both sides
/// under fork-vs-fork. Nothing is ever waived afterwards by message strings.
pub fn invariant_policy(profile: Profile, side: char) -> crate::observe::InvariantPolicy {
    if profile == Profile::ForkVsFork || side == 'b' {
        crate::observe::InvariantPolicy::Fork
    } else {
        crate::observe::InvariantPolicy::Stock
    }
}

pub struct StepOutcome {
    pub evidence: Option<StepEvidence>,
    pub violations: usize,
    pub desynced: bool,
}

/// The three historical stderr lines, as data, so headless output stays byte-identical.
pub fn legacy_line(policy: Policy, ev: &Ev) -> Option<String> {
    match ev {
        Ev::StepSkipped {
            scenario,
            label,
            why,
        } if why == "desynced" => Some(format!("[{scenario}] {label} skipped: scenario desynced")),
        Ev::StepDone {
            evidence,
            violations,
            ..
        } => Some(format!(
            "[{}] {} a={} b={} diffs={} violations={}",
            evidence.scenario,
            evidence.label,
            outcome(policy, &evidence.a),
            outcome(policy, &evidence.b),
            evidence.diff.len(),
            violations
        )),
        _ => None,
    }
}

/// Compact one-line JSON for `run --json`. A derived `Serialize` would dump whole
/// snapshots per line, so the interesting fields are picked by hand. Error-class
/// fields carry policy-rendered `Safe` values, which serialize byte-identically
/// under the pair policy and sanitized under the market policy.
pub fn json_line(policy: Policy, ev: &Ev) -> String {
    let v = match ev {
        Ev::RunStarted { plan, resumed } => {
            json!({"ev":"run_started","scenarios":plan.len(),"resumed":resumed})
        }
        Ev::StepStarted {
            scenario,
            label,
            index,
            ..
        } => json!({"ev":"step_started","scenario":scenario,"label":label,"index":index}),
        Ev::SideDone { tag, ok, err } => {
            json!({"ev":"side_done","side":tag.to_string(),"ok":ok,"err":err})
        }
        Ev::Waiting {
            label,
            now,
            target,
            secs_left,
        } => json!({"ev":"waiting","label":label,"now":now,"target":target,"secs_left":secs_left}),
        Ev::StepDone {
            evidence,
            violations,
            desynced,
        } => json!({
            "ev":"step_done","scenario":evidence.scenario,"label":evidence.label,
            "a":outcome(policy, &evidence.a),"b":outcome(policy, &evidence.b),
            "diffs":evidence.diff.len(),"violations":violations,"desynced":desynced}),
        Ev::StepSkipped {
            scenario,
            label,
            why,
        } => json!({"ev":"step_skipped","scenario":scenario,"label":label,"why":why}),
        Ev::Halt {
            scenario,
            label,
            reason,
        } => json!({"ev":"halt","scenario":scenario,"label":label,"reason":reason}),
        Ev::Cli { argv, ok, ms, .. } => json!({"ev":"cli","argv":argv,"ok":ok,"ms":ms}),
        Ev::RunFinished { error } => json!({"ev":"run_finished","error":error}),
    };
    v.to_string()
}

pub struct Runner<'a, E: Executor + Sync> {
    pub st: &'a Stellar<'a, E>,
    pub m: &'a PairManifest,
    pub out: &'a Path,
    pub state: RunState,
    pub sleep: fn(Duration),
    /// Progress hook; see the event protocol above. `None` means headless.
    pub hook: Option<&'a dyn Fn(Ev) -> Act>,
    /// Counter for operator step labels.
    op_seq: usize,
    /// A mutating invocation ran in the current scenario in this process; an
    /// AssertRatesIncreased without one only compares read-only snapshots.
    mutated: bool,
    /// Operator aborted inside a long-poll step; leave the step unrecorded.
    step_abort: bool,
    /// A sanctioned pool-loss event has fired at-or-before this point in the run
    /// (recovered from existing evidence on resume), so pool custody may differ.
    loss_seen: bool,
    /// The freeze-downgrade probe has executed (recovered from evidence on
    /// resume), so stock's status 0 vs the fork's guarded 4 is sanctioned.
    freeze_split_seen: bool,
    /// Scenarios whose retained evidence records a failed expectation. This is a
    /// durable dependency barrier, re-derived from evidence on every resume, so a
    /// failed setup can never be skipped as merely "completed" and its dependents
    /// driven against state it never established.
    blocked: BTreeSet<String>,
    /// Set when a hook-mode step failed as a tool error (not a semantic halt).
    /// A tool failure must exit 1, never with the verdict code.
    pub fatal: Option<Safe>,
}

struct Side<'a> {
    tag: char,
    m: &'a SideManifest,
}

impl<'a, E: Executor + Sync> Runner<'a, E> {
    pub fn new(st: &'a Stellar<'a, E>, m: &'a PairManifest, out: &'a Path) -> anyhow::Result<Self> {
        // Reconstruct barriers and comparison context from readable canonical
        // evidence. Missing or corrupt records cannot grant permission to skip
        // a mutation that might already have been applied.
        // Operator records are disclosure-only: they neither arm these latches
        // nor satisfy completed canonical work.
        let unresolved = reap_safe_pending(out).context("auditing unresolved side mutations")?;
        anyhow::ensure!(
            unresolved.is_empty(),
            "run dir has unresolved side mutation(s); refuse to replay: {}",
            unresolved
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let state = RunState::load(out)?;
        let steps =
            crate::report::load_steps(out, m).context("loading retained execution evidence")?;
        let canonical = || steps.iter().filter(|s| s.scenario != OPERATOR_SCENARIO);
        let loss_seen = canonical().any(|s| {
            crate::timeline::emits_pool_loss(&s.a, &m.a)
                || crate::timeline::emits_pool_loss(&s.b, &m.b)
        });
        let freeze_split_seen =
            canonical().any(|s| s.label == crate::scenario::FREEZE_DOWNGRADE_PROBE);
        let mut blocked = canonical()
            .filter(|s| evidence_blocks_scenario(Policy::Pair, m, s))
            .map(|s| s.scenario.clone())
            .collect::<BTreeSet<String>>();
        let retained = canonical()
            .map(|s| (s.scenario.as_str(), s.label.as_str()))
            .collect::<BTreeSet<_>>();
        // A completed label may only be skipped as "already completed" while the
        // evidence that earned the completion is still retained. Deleted evidence
        // is unknown history: the step may already have been applied, so the
        // scenario is blocked rather than silently replayed or trusted.
        for (scenario, done) in &state.completed {
            if scenario == OPERATOR_SCENARIO {
                continue;
            }
            if done.iter().any(|label| {
                !crate::scenario::current_step(label).is_some_and(|s| is_wait(&s.step))
                    && !retained.contains(&(scenario.as_str(), label.as_str()))
            }) {
                blocked.insert(scenario.clone());
            }
        }
        // Operator evidence dirs survive sessions: seed the counter so a second
        // session cannot overwrite the first session's evidence.
        let op_seq = std::fs::read_dir(out.join("evidence").join(OPERATOR_SCENARIO))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|d| {
                        d.file_name()
                            .to_string_lossy()
                            .split_once('-')?
                            .0
                            .parse::<usize>()
                            .ok()
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        Ok(Self {
            st,
            m,
            out,
            state,
            sleep: std::thread::sleep,
            hook: None,
            op_seq,
            mutated: false,
            step_abort: false,
            loss_seen,
            freeze_split_seen,
            blocked,
            fatal: None,
        })
    }

    fn sides(&self) -> [Side<'a>; 2] {
        [
            Side {
                tag: 'a',
                m: &self.m.a,
            },
            Side {
                tag: 'b',
                m: &self.m.b,
            },
        ]
    }

    /// The raw-output policy this runner's evidence surfaces act under: pair
    /// by construction (see `run_policy`); pair bytes are pinned by AC9.
    pub fn policy(&self) -> Policy {
        run_policy(self.m.profile, 'a')
    }

    /// Both pools must still answer; a testnet reset invalidates the manifest.
    pub fn preflight(&self) -> anyhow::Result<()> {
        self.st.assert_version()?;
        for s in self.sides() {
            self.st
                .view(
                    &s.m.contracts.pool,
                    &Who::User.key_name(),
                    "get_config",
                    &[],
                )
                .with_context(|| format!("side {} pool unreachable (testnet reset?)", s.tag))?;
        }
        Ok(())
    }

    /// Emit an event and get the operator's decision. Headless (`hook == None`)
    /// prints the legacy line and continues.
    pub(crate) fn gate(&self, ev: Ev) -> Act {
        match self.hook {
            Some(h) => h(ev),
            None => {
                if let Some(line) = legacy_line(self.policy(), &ev) {
                    eprintln!("{line}");
                }
                Act::Continue
            }
        }
    }

    /// Sleep `total` in slices of at most 5s, emitting `mk(secs_left)` per slice so a
    /// long ledger wait stays legible and interruptible. `Ok(false)` means abort/skip.
    fn nap(&self, total: Duration, mk: &dyn Fn(u64) -> Ev) -> anyhow::Result<bool> {
        let mut left = total.as_secs();
        loop {
            match self.gate(mk(left)) {
                Act::Abort | Act::Skip => return Ok(false),
                _ => {}
            }
            if left == 0 {
                return Ok(true);
            }
            let slice = left.min(5);
            (self.sleep)(Duration::from_secs(slice));
            left -= slice;
        }
    }

    /// One step against the selected side(s): execute, snapshot both, check
    /// expectations and invariants, compare, persist evidence, mark completed.
    pub(crate) fn step_once(
        &mut self,
        scenario: &str,
        index: usize,
        step: &LabeledStep,
        last: &mut BTreeMap<char, Snapshot>,
        delta_dependent: bool,
        sides: Sides,
    ) -> anyhow::Result<StepOutcome> {
        self.step_abort = false;
        let step_dir = pending_step_dir(self.out, scenario, index, step.label);
        anyhow::ensure!(
            !step_dir.exists(),
            "step {scenario}/{} has an unresolved side outcome {}",
            step.label,
            step_dir.display()
        );
        let evidence_dir = evidence_step_dir(self.out, scenario, index, step.label);
        anyhow::ensure!(
            !evidence_dir.exists(),
            "refusing to overwrite completed evidence {}",
            evidence_dir.display()
        );
        if scenario != OPERATOR_SCENARIO {
            anyhow::ensure!(
                !self
                    .state
                    .completed
                    .get(scenario)
                    .is_some_and(|done| done.iter().any(|done| done == step.label)),
                "refusing to replay completed step {scenario}/{}",
                step.label
            );
        }
        let mut records = Vec::new();
        // Per-side "the expectation this step was driven under did not hold",
        // kept apart from invariant/state-change findings because it is what the
        // dependency barrier keys on, in this process and on every resume.
        let mut expectation_failed = Vec::new();
        // A recorded half-step (abort/Skip after one side already mutated) must
        // halt the scenario: driving later steps against a half-applied pair
        // while judging them synchronized hides the desync.
        let mut half_applied = false;
        let mut witness_missing = false;
        for s in self.sides() {
            // Report recompute judges read-only/assert steps against the
            // previous recorded after-snapshot: a fresh read on the same
            // lease accrues identically and proves nothing. Mutating steps
            // keep the freshly-read before as their state-change baseline.
            let before = match last.get(&s.tag) {
                Some(prev) if !should_journal(&step.step) => prev.clone(),
                _ => snapshot(self.st, s.m, &self.m.identities)?,
            };
            let journal = sides.covers(s.tag) && should_journal(&step.step);
            if journal {
                begin_pending(
                    self.out,
                    &PendingIntent {
                        scenario: scenario.into(),
                        index,
                        label: step.label.into(),
                        side: s.tag,
                    },
                )?;
            }
            let inv = if sides.covers(s.tag) {
                let st = self.st;
                let m: &'a PairManifest = self.m;
                let r = {
                    let view = SideView {
                        tag: s.tag,
                        m: s.m,
                        identities: &m.identities,
                    };
                    let mut hooks = PairSideHooks {
                        runner: &mut *self,
                        captured: &before,
                    };
                    invoke_side(st, &view, &step.step, &mut hooks)
                };
                if matches!(r, Ok(SideOutcome::Declined)) {
                    // The operator declined the firing gate; the pending
                    // mutation is never sent and the step stays unmarked.
                    self.step_abort = true;
                }
                let r = r.map(|o| match o {
                    SideOutcome::Invocation(inv) => Some(inv),
                    SideOutcome::Declined | SideOutcome::Unmutating => None,
                });
                match &r {
                    Ok(inv) => self.gate(Ev::SideDone {
                        tag: s.tag,
                        ok: inv.as_ref().map(|i| i.ok).unwrap_or(true),
                        err: inv
                            .as_ref()
                            .and_then(|i| i.error.as_ref())
                            .map(|e| Safe::error_debug(self.policy(), e)),
                    }),
                    Err(e) => self.gate(Ev::SideDone {
                        tag: s.tag,
                        ok: false,
                        err: Some(Safe::error_chain(self.policy(), e)),
                    }),
                };
                if journal {
                    let outcome = match &r {
                        Ok(Some(inv)) => ManifestSideOutcome::Invocation(inv.clone()),
                        Ok(None) if self.step_abort => ManifestSideOutcome::Declined,
                        Ok(None) if matches!(step.step, Step::RepayAll { .. }) => {
                            ManifestSideOutcome::Unmutating
                        }
                        Ok(None) => {
                            ManifestSideOutcome::ToolError("runner produced no invocation".into())
                        }
                        Err(e) => ManifestSideOutcome::ToolError(format!("{e:#}")),
                    };
                    finish_pending(self.out, scenario, index, step.label, s.tag, outcome)
                        .with_context(|| {
                            format!(
                                "persisting side {} outcome for {scenario}/{}",
                                s.tag, step.label
                            )
                        })?;
                }
                let inv = match r {
                    Ok(Some(inv)) => Some(inv),
                    Ok(None) if self.step_abort && records.is_empty() => {
                        clear_pending(self.out, scenario, index, step.label)?;
                        return Ok(StepOutcome {
                            evidence: None,
                            violations: 0,
                            desynced: false,
                        });
                    }
                    Ok(None) if self.step_abort => {
                        half_applied = true;
                        self.step_abort = false;
                        None
                    }
                    Ok(None) if !journal => None,
                    Ok(None) if matches!(step.step, Step::RepayAll { .. }) => None,
                    Ok(None) => anyhow::bail!(
                        "side {} produced no invocation for {scenario}/{}",
                        s.tag,
                        step.label
                    ),
                    Err(e) if journal => {
                        return Err(e).with_context(|| {
                            format!(
                                "side {} outcome for {scenario}/{} is unresolved",
                                s.tag, step.label
                            )
                        })
                    }
                    Err(e) => return Err(e),
                };
                if inv.as_ref().is_some_and(|inv| inv.ok) {
                    self.mutated = true;
                }
                inv
            } else {
                None
            };
            let snap = snapshot(self.st, s.m, &self.m.identities)?;
            let expectation = if sides.covers(s.tag) {
                check_expectation(
                    self.policy(),
                    self.m.profile,
                    s.tag,
                    step,
                    inv.as_ref(),
                    Some(&before),
                )
            } else {
                Vec::new()
            };
            expectation_failed.push(!expectation.is_empty());
            let mut violations = expectation;
            let extra = self
                .state
                .minted
                .get(&s.tag.to_string())
                .cloned()
                .unwrap_or_default();
            for v in invariants(
                self.policy(),
                s.m,
                &snap,
                &extra,
                invariant_policy(self.m.profile, s.tag),
            ) {
                violations.push(policy_store(self.policy(), format!("invariant: {v}")));
            }
            if matches!(step.step, Step::Trap(_)) || inv.as_ref().is_some_and(|i| !i.ok) {
                violations.extend(no_state_change(self.policy(), &before, &snap));
            }
            // A successful `new_auction` must be visible in the post-step
            // snapshot's raw temporary row: absence here is a silent
            // custody-loss defect, not a benign read gap.
            if inv.as_ref().is_some_and(|i| i.ok) {
                if let Step::NewAuction { user, .. } = &step.step {
                    if let Some(finding) =
                        crate::observe::auction_observed(self.policy(), &snap, *user)
                    {
                        violations.push(finding);
                        witness_missing = true;
                    }
                }
            }
            if let Step::AssertRatesIncreased { asset } = &step.step {
                if self.mutated {
                    let rates = &last[&s.tag].reserves[asset]["data"];
                    let after = &snap.reserves[asset]["data"];
                    for f in ["b_rate", "d_rate"] {
                        if num(&after[f]) <= num(&rates[f]) {
                            violations.push(policy_store(
                                self.policy(),
                                format!("{f} did not increase: {} -> {}", rates[f], after[f]),
                            ));
                        }
                    }
                } else {
                    violations.push(Safe::literal(
                        "rates not established by a mutation in this session",
                    ));
                }
            }
            last.insert(s.tag, snap.clone());
            records.push(StepRecord {
                label: step.label.into(),
                side: s.m.label.clone(),
                invocation: inv,
                before: Some(before),
                snapshot: snap,
                violations,
            });
        }
        let b = records.pop().unwrap();
        let a = records.pop().unwrap();
        let desynced = half_applied
            || witness_missing
            || (step.expect != Expect::Any && expectation_failed.iter().any(|f| *f));
        let diff = compare_step(
            self.m,
            step.label,
            &a,
            &b,
            delta_dependent,
            self.loss_seen,
            self.freeze_split_seen,
        );
        let violations = diff
            .iter()
            .filter(|(_, c)| matches!(c, crate::compare::Class::Violation(_)))
            .count()
            + a.violations.len()
            + b.violations.len();
        let evidence = StepEvidence {
            scenario: scenario.into(),
            index,
            label: step.label.into(),
            a,
            b,
            diff,
            findings: Vec::new(),
        };
        write_step(self.out, &evidence)?;
        // Operator steps are one-sided by design and excluded from every
        // verdict surface; they must not arm the cross-step custody exemption
        // that report replay re-derives without their chronology.
        if scenario != OPERATOR_SCENARIO {
            self.loss_seen |= crate::timeline::emits_pool_loss(&evidence.a, &self.m.a)
                || crate::timeline::emits_pool_loss(&evidence.b, &self.m.b);
        }
        if step.label == crate::scenario::FREEZE_DOWNGRADE_PROBE {
            self.freeze_split_seen = true;
        }
        self.mark(scenario, step.label)?;
        clear_pending(self.out, scenario, index, step.label)?;
        Ok(StepOutcome {
            evidence: Some(evidence),
            violations,
            desynced,
        })
    }

    /// Snapshot both sides and record it as operator evidence, moving no value.
    pub(crate) fn snapshot_only(
        &mut self,
        last: &mut BTreeMap<char, Snapshot>,
    ) -> anyhow::Result<()> {
        self.op_seq += 1;
        let index = self.op_seq;
        let mut records = Vec::new();
        for s in self.sides() {
            let snap = snapshot(self.st, s.m, &self.m.identities)?;
            last.insert(s.tag, snap.clone());
            records.push(StepRecord {
                label: format!("operator_{index:03}_snapshot"),
                side: s.m.label.clone(),
                invocation: None,
                before: Some(snap.clone()),
                snapshot: snap,
                violations: Vec::new(),
            });
        }
        let b = records.pop().unwrap();
        let a = records.pop().unwrap();
        write_step(
            self.out,
            &StepEvidence {
                scenario: OPERATOR_SCENARIO.into(),
                index,
                label: format!("operator_{index:03}_snapshot"),
                a,
                b,
                diff: Vec::new(),
                findings: Vec::new(),
            },
        )
    }

    /// Run an ad-hoc operator step through the ordinary step path, so it lands in
    /// evidence and is checked like any other step. A one-sided action genuinely
    /// desyncs the pair; that is the operator's choice and is recorded as such.
    pub(crate) fn operator_step(
        &mut self,
        step: &LabeledStep,
        sides: Sides,
        last: &mut BTreeMap<char, Snapshot>,
    ) -> anyhow::Result<()> {
        self.op_seq += 1;
        let index = self.op_seq;
        let out = self.step_once(OPERATOR_SCENARIO, index, step, last, false, sides)?;
        if let Some(ev) = out.evidence {
            self.gate(Ev::StepDone {
                evidence: Box::new(ev),
                violations: out.violations,
                desynced: out.desynced,
            });
        }
        Ok(())
    }

    pub fn run(&mut self, only: Option<&str>, from_step: Option<&str>) -> anyhow::Result<()> {
        let r = self.run_inner(only, from_step);
        self.gate(Ev::RunFinished {
            error: r
                .as_ref()
                .err()
                .map(|e| Safe::error_chain(self.policy(), e)),
        });
        r
    }

    fn run_inner(&mut self, only: Option<&str>, from_step: Option<&str>) -> anyhow::Result<()> {
        // Reject unsafe selections before any scope is recorded or any
        // mutation is attempted: absent driving scope, "all", duplicates,
        // reordered names, family mixing, missing loss prerequisites.
        let selected = select_scenarios(only)?;
        self.persist_requested(&selected)?;
        self.preflight()?;
        let mut last: BTreeMap<char, Snapshot> = BTreeMap::new();
        for s in self.sides() {
            last.insert(s.tag, snapshot(self.st, s.m, &self.m.identities)?);
        }
        let resumed = self.state.completed.values().map(|v| v.len()).sum();
        self.gate(Ev::RunStarted {
            plan: selected
                .iter()
                .map(|sc| {
                    let mut labels: Vec<String> =
                        sc.steps.iter().map(|s| s.label.to_string()).collect();
                    labels.extend(traps().iter().map(|s| s.label.to_string()));
                    (sc.name.to_string(), labels)
                })
                .collect(),
            resumed,
        });

        let mut skipping = from_step.is_some();
        for scenario in selected {
            let mut steps = scenario.steps.clone();
            steps.extend(traps());
            let mut desynced = self.blocked.contains(scenario.name);
            self.mutated = false;
            'steps: for (index, step) in steps.iter().enumerate() {
                let skip_why = if desynced && !matches!(step.step, Step::Trap(_)) {
                    Some("desynced")
                } else if skipping {
                    if Some(step.label) == from_step {
                        skipping = false;
                        None
                    } else {
                        Some("before --from-step")
                    }
                } else if self
                    .state
                    .completed
                    .get(scenario.name)
                    .map(|v| v.iter().any(|l| l == step.label))
                    .unwrap_or(false)
                {
                    Some("already completed")
                } else {
                    None
                };
                if let Some(why) = skip_why {
                    self.gate(Ev::StepSkipped {
                        scenario: scenario.name.into(),
                        label: step.label.into(),
                        why: why.into(),
                    });
                    continue;
                }

                loop {
                    match self.gate(Ev::StepStarted {
                        scenario: scenario.name.into(),
                        index,
                        label: step.label.into(),
                    }) {
                        Act::Skip => continue 'steps,
                        Act::Abort => return Ok(()),
                        Act::Exec { step, sides } => {
                            self.operator_step(&step, sides, &mut last)?;
                            continue;
                        }
                        Act::Snapshot => {
                            self.snapshot_only(&mut last)?;
                            continue;
                        }
                        _ => {}
                    }

                    if is_wait(&step.step) {
                        if !self.wait(step.label, &step.step)? {
                            continue 'steps;
                        }
                        self.mark(scenario.name, step.label)?;
                        if self.hook.is_none() {
                            eprintln!("[{}] {} done", scenario.name, step.label);
                        }
                        continue 'steps;
                    }

                    let ev = match self.step_once(
                        scenario.name,
                        index,
                        step,
                        &mut last,
                        step.delta_dependent,
                        Sides::Both,
                    ) {
                        // Headless keeps propagating errors exactly as before.
                        Err(e) if self.hook.is_none() => return Err(e),
                        Err(e) => {
                            self.fatal = Some(Safe::error_chain(self.policy(), &e));
                            Ev::Halt {
                                scenario: scenario.name.into(),
                                label: step.label.into(),
                                reason: Safe::error_chain(self.policy(), &e),
                            }
                        }
                        Ok(o) => {
                            desynced |= o.desynced;
                            if o.desynced {
                                self.blocked.insert(scenario.name.to_string());
                            }
                            match o.evidence {
                                // Operator aborted a long-poll step: leave it
                                // unmarked so a later run can still drive it.
                                None => continue 'steps,
                                Some(ev) => Ev::StepDone {
                                    evidence: Box::new(ev),
                                    violations: o.violations,
                                    desynced: o.desynced,
                                },
                            }
                        }
                    };
                    // Operator actions return to this decision, never to execution.
                    loop {
                        match self.gate(ev.clone()) {
                            Act::Abort => return Ok(()),
                            Act::Continue | Act::Skip => continue 'steps,
                            Act::Exec { step, sides } => {
                                self.operator_step(&step, sides, &mut last)?;
                            }
                            Act::Snapshot => self.snapshot_only(&mut last)?,
                        }
                    }
                }
            }
        }
        anyhow::ensure!(
            !skipping,
            "--from-step label {:?} never matched a step in the selected scenarios",
            from_step.unwrap_or_default()
        );
        Ok(())
    }

    fn mark(&mut self, scenario: &str, label: &str) -> anyhow::Result<()> {
        if scenario == OPERATOR_SCENARIO {
            return Ok(());
        }
        let done = self.state.completed.entry(scenario.into()).or_default();
        if !done.iter().any(|done| done == label) {
            done.push(label.into());
        }
        if !self.state.scenario_order.iter().any(|s| s == scenario) {
            self.state.scenario_order.push(scenario.to_string());
        }
        self.state.save(self.out)
    }

    fn persist_requested(&mut self, selected: &[Scenario]) -> anyhow::Result<()> {
        for scenario in self.state.completed.keys() {
            anyhow::ensure!(
                scenario != OPERATOR_SCENARIO,
                "run state contains forbidden operator completion scope"
            );
            anyhow::ensure!(
                self.state.requested.contains_key(scenario),
                "completed scenario {scenario} has no persisted requested scope"
            );
        }
        // The campaign is the union of what this run dir already requested and what
        // this invocation adds. Admitting a second family here only surfaces in the
        // report — after that family has already mutated the deployment the first
        // family was supposed to keep isolated and fresh — so the prospective union
        // is judged by the same authority, before a single byte of scope is saved.
        let mut union: Vec<&str> = self.state.requested.keys().map(String::as_str).collect();
        union.extend(selected.iter().map(|s| s.name));
        union.sort_unstable();
        union.dedup();
        let unioned = union.join(",");
        let family = family_of(&union).ok_or_else(|| {
            anyhow::anyhow!(
                "prospective requested scope [{unioned}] is not one isolated family; \
                 {SELECTION_GUIDANCE}"
            )
        })?;
        anyhow::ensure!(
            loss_prerequisites_ok(&union),
            "prospective requested scope [{unioned}] leaves loss-family prerequisites \
             incomplete (stale_auction requires liquidation; direct_default requires \
             liquidation,stale_auction); {SELECTION_GUIDANCE}"
        );
        // Canonical order is a property of the campaign, not just of one
        // invocation: a scenario canonically earlier than one already executed
        // would be driven out of order against a deployment that has moved on.
        let mut driven: Vec<&str> = self
            .state
            .scenario_order
            .iter()
            .map(String::as_str)
            .collect();
        for scenario in selected {
            if !driven.contains(&scenario.name) {
                driven.push(scenario.name);
            }
        }
        let mut previous: Option<usize> = None;
        for name in driven {
            let rank = family
                .iter()
                .position(|f| *f == name)
                .ok_or_else(|| anyhow::anyhow!("scenario {name} is not in family {family:?}"))?;
            anyhow::ensure!(
                !previous.is_some_and(|p| rank <= p),
                "prospective requested scope [{unioned}] is not in canonical scenario \
                 order (already driven: [{}]); {SELECTION_GUIDANCE}",
                self.state.scenario_order.join(",")
            );
            previous = Some(rank);
        }
        for scenario in selected {
            let labels = requested_labels(scenario);
            if let Some(existing) = self.state.requested.get(scenario.name) {
                anyhow::ensure!(
                    *existing == labels,
                    "scenario {} requested scope changed: {:?} != {:?}",
                    scenario.name,
                    existing,
                    labels
                );
            } else {
                self.state.requested.insert(scenario.name.into(), labels);
            }
        }
        self.state.save(self.out)
    }
}

/// One policy-rendered Debug of an optional error. The pair arm reproduces
/// today's `{:?}` of `Option<ErrorKind>` byte-for-byte; the market arm is the
/// sanitized one-line summary.
fn error_debug_opt(policy: Policy, error: Option<&ErrorKind>) -> Safe {
    match error {
        None => Safe::literal("None"),
        Some(kind) => policy_store(policy, format!("Some({})", Safe::error_debug(policy, kind))),
    }
}

/// Expectation findings for one side under a policy. The pair arm stores each
/// finding byte-identically to the retired `Vec<String>` content (AC9); the
/// market arm sanitizes before storage.
pub fn check_expectation(
    policy: Policy,
    profile: Profile,
    side: char,
    step: &LabeledStep,
    inv: Option<&Invocation>,
    before: Option<&Snapshot>,
) -> Vec<Safe> {
    // Presence first, for every side and every expectation: a mutation whose
    // result was never recorded cannot be "stock behaving differently", it is
    // a lost result. `PerSide` exempts the stock side from the *outcome*
    // judgement, never from having an outcome at all.
    let Some(inv) = inv else {
        // A repayment legitimately needs no invocation only when the very
        // snapshot this run captured proves the liability was already zero.
        // Anything else — nonzero, missing holder, malformed map — fails closed.
        if let Step::RepayAll { from, asset } = &step.step {
            return match captured_liability(before, *from, *asset) {
                Ok(0) => Vec::new(),
                Ok(d) => vec![policy_store(
                    policy,
                    format!(
                        "{from:?} {asset:?} RepayAll recorded no invocation while the \
                         captured snapshot still shows liability {d}"
                    ),
                )],
                Err(e) => vec![policy_store(
                    policy,
                    format!(
                        "{from:?} {asset:?} RepayAll recorded no invocation without \
                         well-formed zero-debt proof: {e}"
                    ),
                )],
            };
        }
        if should_journal(&step.step) {
            return vec![policy_store(
                policy,
                format!("expected an invocation for {:?}", step.step),
            )];
        }
        return Vec::new();
    };
    // Custody and consistency, before any `PerSide` exemption and before any
    // expectation-specific matching. An `Invocation` deserialised from retained
    // evidence can claim `ok` with an error, fail without explaining itself, or
    // carry the unclassified transport failure that `invoke` now refuses to emit.
    // None of those certify an outcome, and stock's outcome exemption must not
    // turn one into a pass. The sending side's own predicate decides, so send and
    // replay can never drift apart.
    if inv.ok == inv.error.is_some() || !definite_outcome(inv) {
        return vec![policy_store(
            policy,
            format!(
                "inconsistent invocation for {:?}: ok={} {}",
                step.step,
                inv.ok,
                error_debug_opt(policy, inv.error.as_ref())
            ),
        )];
    }
    let want = match step.expect {
        Expect::PerSide => {
            if profile == Profile::StockVsFork && side == 'a' {
                return Vec::new();
            }
            fork_expect(step.label, &step.step)
        }
        e => e,
    };
    let mut out = Vec::new();
    match want {
        Expect::Ok if !inv.ok => out.push(policy_store(
            policy,
            format!(
                "expected ok, got {}: {}",
                error_debug_opt(policy, inv.error.as_ref()),
                Safe::first_error_line(policy, &inv.stderr)
            ),
        )),
        Expect::Err(code) if inv.error != Some(ErrorKind::Contract(code)) => {
            out.push(policy_store(
                policy,
                format!(
                    "expected Error(Contract, #{code}), got ok={} {}",
                    inv.ok,
                    error_debug_opt(policy, inv.error.as_ref())
                ),
            ))
        }
        Expect::AnyErr if inv.ok => out.push(Safe::literal("expected an error, call succeeded")),
        _ => {}
    }
    out
}

/// The actor's liability for `asset`, read strictly out of a captured snapshot.
/// The reserve index and the repayment rate come from that same row, so the
/// no-op decision and the later replay check cannot disagree with a second,
/// unpersisted view. `Err` means the proof was never observed or is malformed
/// and may never be read as a zero; an absent key inside a genuine
/// `Map<u32, i128>` is `Ok(0)`, which is what absence means in that map.
fn captured_liability(before: Option<&Snapshot>, who: Who, asset: Tok) -> Result<i128, String> {
    let before = before.ok_or_else(|| "no pre-invocation snapshot was captured".to_string())?;
    let reserve = before
        .reserves
        .get(&asset)
        .ok_or_else(|| format!("reserve {asset:?} was not captured"))?;
    let index = u32::try_from(
        try_num(&reserve["config"]["index"])
            .ok_or_else(|| format!("reserve {asset:?} config.index is missing/malformed"))?,
    )
    .map_err(|_| format!("reserve {asset:?} config.index is not a u32"))?;
    let who = who.as_str();
    let liabilities = before
        .positions
        .get(who)
        .and_then(|p| p.get("liabilities"))
        .ok_or_else(|| format!("positions[{who}].liabilities was not captured"))?;
    let entries = strict_map(liabilities)
        .ok_or_else(|| format!("positions[{who}].liabilities is not a map"))?;
    match entries.into_iter().find(|(k, _)| *k == index.to_string()) {
        Some((_, v)) => try_num(&v)
            .ok_or_else(|| format!("positions[{who}].liabilities[{index}] is not an i128")),
        None => Ok(0),
    }
}

/// Whether retained evidence terminally blocks later work in its scenario. The
/// judgement is recomputed from the typed record — the invocation actually
/// retained, the before-snapshot actually captured, and the expectation compiled
/// into the current scenario — never from the diagnostic strings the record
/// carries, which are this judgement's output and not its input.
///
/// Canonical production steps always drive both sides, so every retained side is
/// judged: a mutating step whose invocation record was deleted fails the shared
/// checker exactly as a live half-step does, and a label the compiled scenario
/// list no longer describes blocks rather than falling through, because nothing
/// left on disk attests what it was supposed to do. Operator evidence is
/// namespaced under `OPERATOR_SCENARIO` and never reaches this judgement.
fn evidence_blocks_scenario(policy: Policy, m: &PairManifest, s: &StepEvidence) -> bool {
    let Some(step) = crate::scenario::current_step(&s.label) else {
        return true;
    };
    for (side, r) in [('a', &s.a), ('b', &s.b)] {
        if !check_expectation(
            policy,
            m.profile,
            side,
            step,
            r.invocation.as_ref(),
            r.before.as_ref(),
        )
        .is_empty()
        {
            return true;
        }
        if r.invocation.as_ref().is_some_and(|inv| inv.ok) {
            if let Step::NewAuction { user, .. } = &step.step {
                if crate::observe::auction_observed(policy, &r.snapshot, *user).is_some() {
                    return true;
                }
            }
        }
    }
    false
}

impl<'a, E: Executor + Sync> Runner<'a, E> {
    /// Returns `Ok(false)` when the operator aborted or skipped the wait.
    fn wait(&self, label: &str, step: &Step) -> anyhow::Result<bool> {
        match step {
            Step::WaitSeconds(n) => self.nap(Duration::from_secs(*n), &|left| Ev::Waiting {
                label: label.into(),
                now: 0,
                target: 0,
                secs_left: left,
            }),
            Step::WaitLedgersSince {
                auction_of,
                min_delta,
            } => {
                let target = self
                    .sides()
                    .iter()
                    .filter_map(|s| {
                        self.state
                            .auction_blocks
                            .get(&s.tag.to_string())
                            .and_then(|m| m.get(auction_of))
                    })
                    .max()
                    .copied()
                    .context("no recorded auction block to wait on")?
                    + min_delta;
                loop {
                    let now = self.st.latest_ledger()?;
                    if now >= target {
                        return Ok(true);
                    }
                    let remaining = (target - now) as u64;
                    let slice = (5 * remaining).clamp(10, 60);
                    if !self.nap(Duration::from_secs(slice), &|left| Ev::Waiting {
                        label: label.into(),
                        now,
                        target,
                        secs_left: left + 5 * remaining.saturating_sub(slice / 5),
                    })? {
                        return Ok(false);
                    }
                }
            }
            _ => Ok(true),
        }
    }
}

/// Immutable single-side execution view (plan §6): everything one
/// single-side invocation reads, none of what it mutates.
pub struct SideView<'a> {
    /// Pair 'a'/'b' or a market-mode side tag.
    pub tag: char,
    /// The side's deployed contracts, reserves, and label.
    pub m: &'a SideManifest,
    /// Actor identity addresses (`frodo`/`sam`/`user` etc.), shared per run
    /// manifest. The primitive resolves signers only through this view.
    pub identities: &'a BTreeMap<Who, String>,
}

impl<'a> SideView<'a> {
    /// The recorded address for one actor. Fails closed, exactly as the pair
    /// path did before this seam existed (`PairManifest::addr` indexed the
    /// manifest directly): an identity never recorded refuses the invocation
    /// before the executor sees any argument, for pair and market alike.
    pub fn addr(&self, who: Who) -> &str {
        self.identities
            .get(&who)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("{who:?} has no recorded identity address"))
    }
}

/// Caller-owned custody and state effects for one single-side invocation
/// (plan §6). The primitive itself never journals, persists, sanitizes,
/// compares, chooses sides, or marks completion: every effect routes through
/// these hooks. The pair runner implements them over `RunState` minted/auction
/// tracking and gate-visible waiting, byte-for-byte as before; a market runner
/// supplies market journaling and sanitized presentation.
pub trait SideHooks {
    /// The caller's captured pre-invocation snapshot for the side being
    /// driven. `RepayAll`'s zero-liability proof reads only this snapshot;
    /// absence fails closed.
    fn captured(&self) -> Option<&Snapshot>;
    /// A `Mint` landed; record the issued amount for later invariant credit.
    fn mint_recorded(&mut self, side: char, token: Tok, amount: i64) -> anyhow::Result<()>;
    /// A `NewAuction` landed at `block`; record it for ledger waits.
    fn auction_recorded(&mut self, side: char, user: Who, block: u32) -> anyhow::Result<()>;
    /// Gate-visible wait in slices; `Ok(false)` = operator aborted/skipped, so
    /// the pending mutating call is never fired.
    fn wait(&mut self, secs: u64, label: &str) -> anyhow::Result<bool>;
}

/// One classified single-side result, before any comparison or persistence.
#[derive(Clone, Debug)]
pub enum SideOutcome {
    /// The step produced exactly one raw classified invocation.
    Invocation(Invocation),
    /// The operator declined a firing gate; nothing was sent.
    Declined,
    /// The step's semantics required no invocation (zero-liability repay,
    /// wait, or assert step).
    Unmutating,
}

fn submit_view<E: Executor>(
    st: &Stellar<'_, E>,
    side: &SideView<'_>,
    from: Who,
    requests: String,
) -> anyhow::Result<Invocation> {
    let a = side.addr(from);
    st.invoke(
        &side.m.contracts.pool,
        &from.key_name(),
        "submit",
        &[
            kv("from", a),
            kv("spender", a),
            kv("to", a),
            kv("requests", requests),
        ],
    )
}

fn user_balance_view<E: Executor>(
    st: &Stellar<'_, E>,
    side: &SideView<'_>,
    who: Who,
) -> anyhow::Result<Value> {
    st.view(
        &side.m.contracts.backstop,
        &who.key_name(),
        "user_balance",
        &[
            kv("pool", &side.m.contracts.pool),
            kv("user", side.addr(who)),
        ],
    )
}

fn queued_view<E: Executor>(
    st: &Stellar<'_, E>,
    side: &SideView<'_>,
    who: Who,
) -> anyhow::Result<i128> {
    let ub = user_balance_view(st, side, who)?;
    Ok(ub["q4w"]
        .as_array()
        .and_then(|q| q.first())
        .map(|e| num(&e["amount"]))
        .unwrap_or(0))
}

fn trap_view<E: Executor>(
    st: &Stellar<'_, E>,
    side: &SideView<'_>,
    t: TrapCall,
) -> anyhow::Result<Invocation> {
    let c = &side.m.contracts;
    let user = Who::User.key_name();
    let ua = side.addr(Who::User);
    Ok(match t {
        TrapCall::BackstopDistribute => st.invoke(&c.backstop, &user, "distribute", &[])?,
        TrapCall::BackstopGulpEmissions => {
            st.invoke(&c.backstop, &user, "gulp_emissions", &[kv("pool", &c.pool)])?
        }
        TrapCall::BackstopAddReward => {
            st.invoke(&c.backstop, &user, "add_reward", &[kv("to_add", &c.pool)])?
        }
        TrapCall::BackstopRemoveReward => st.invoke(
            &c.backstop,
            &user,
            "remove_reward",
            &[kv("to_remove", &c.pool)],
        )?,
        TrapCall::BackstopClaim => st.invoke(
            &c.backstop,
            &user,
            "claim",
            &[
                kv("from", ua),
                kv("pool_addresses", json!([c.pool])),
                kv("min_lp_tokens_out", 0),
            ],
        )?,
        TrapCall::BackstopDrop => st.invoke(&c.backstop, &user, "drop", &[])?,
        TrapCall::PoolUpdatePool => st.invoke(
            &c.pool,
            &user,
            "update_pool",
            &[
                kv("backstop_take_rate", 0),
                kv("max_positions", 6),
                kv("min_collateral", 10_000_000),
            ],
        )?,
        TrapCall::PoolFlashLoan => st.invoke(
            &c.pool,
            &user,
            "flash_loan",
            &[
                kv("from", ua),
                kv(
                    "flash_loan",
                    json!({"contract": c.pool, "asset": c.stable, "amount": "1"}),
                ),
                kv("requests", "[]"),
            ],
        )?,
        TrapCall::PoolSetEmissionsConfig => st.invoke(
            &c.pool,
            &user,
            "set_emissions_config",
            &[kv(
                "res_emission_metadata",
                json!([{"res_index": 0, "res_type": 1, "share": 1}]),
            )],
        )?,
    })
}

/// The shared single-side invocation primitive (plan §6). Inputs are the
/// `Stellar` choke point, immutable side/identity views, one `Step`, and the
/// caller-owned custody/state hooks; the output is exactly one classified
/// invocation or decline, and it is produced only after the existing
/// retry/definite-outcome rules inside `Stellar` have run. It never compares,
/// persists, sanitizes, chooses sides, or marks completion. The pair adapter
/// drives it through `PairSideHooks`, whose state and gate effects reproduce
/// the retired pair runner exactly (AC9); `MarketRunner` supplies market
/// journaling and sanitized presentation through its own hooks (plan §6).
pub fn invoke_side<E: Executor>(
    st: &Stellar<'_, E>,
    side: &SideView<'_>,
    step: &Step,
    hooks: &mut dyn SideHooks,
) -> anyhow::Result<SideOutcome> {
    let c = &side.m.contracts;
    let pool = c.pool.as_str();
    let inv = match step {
        Step::Mint { token, to, amount } => {
            let inv = st.invoke(
                c.token(*token),
                &Who::Admin.key_name(),
                "mint",
                &[kv("to", side.addr(*to)), kv("amount", amount)],
            )?;
            if inv.ok {
                hooks.mint_recorded(side.tag, *token, i64::try_from(*amount)?)?;
            }
            inv
        }
        Step::LpTransfer { from, to, amount } => st.invoke(
            &c.lp,
            &from.key_name(),
            "transfer",
            &[
                kv("from", side.addr(*from)),
                kv("to", side.addr(*to)),
                kv("amount", amount),
            ],
        )?,
        Step::Submit { from, reqs } => submit_view(st, side, *from, request_json(reqs, c))?,
        Step::RepayAll { from, asset } => {
            // Every quantity here comes from the caller's captured
            // pre-invocation snapshot, under strict parsing: a second
            // permissive view could turn a required amount into a silent zero.
            // Missing/malformed proof fails closed.
            let d_tokens = captured_liability(hooks.captured(), *from, *asset)
                .map_err(|e| anyhow::anyhow!("{asset:?} RepayAll for {from:?}: {e}"))?;
            if d_tokens == 0 {
                return Ok(SideOutcome::Unmutating);
            }
            let before = hooks
                .captured()
                .ok_or_else(|| anyhow::anyhow!("{asset:?} RepayAll has no captured baseline"))?;
            let d_rate = try_num(&before.reserves[asset]["data"]["d_rate"]).ok_or_else(|| {
                anyhow::anyhow!("{asset:?} RepayAll: captured d_rate is missing/malformed")
            })?;
            // d_rate accrues between this snapshot and inclusion; overpay by 0.1% plus one
            // unit. The pool refunds any excess (`apply_repay`), so this is exact.
            let debt_assets = (d_tokens * d_rate + 999_999_999_999) / 1_000_000_000_000;
            let amount = debt_assets + debt_assets / 1_000 + 1;
            let reqs = json!([{"request_type": req::REPAY, "address": c.token(*asset), "amount": amount.to_string()}]);
            submit_view(st, side, *from, reqs.to_string())?
        }
        Step::FillAuction {
            filler,
            victim,
            percent,
        } => {
            let reqs = json!([{"request_type": req::FILL_USER_LIQ, "address": side.addr(*victim), "amount": percent.to_string()}]);
            submit_view(st, side, *filler, reqs.to_string())?
        }
        Step::SetPrices(prices) => set_prices(st, c, prices)?,
        Step::SetPricesAt {
            prices,
            offset_secs,
        } => {
            // Wall clock, not ledger time: testnet close times track it within
            // seconds and the offsets here are hours, so the margin is ample.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let at = (now + offset_secs).max(0) as u64;
            let p: Vec<String> = prices.iter().map(|x| x.to_string()).collect();
            st.invoke(
                &c.oracle,
                &Who::Admin.key_name(),
                "set_price",
                &[kv("prices", serde_json::json!(p)), kv("timestamp", at)],
            )?
        }
        Step::SetStatus(n) => st.invoke(
            pool,
            &Who::Admin.key_name(),
            "set_status",
            &[kv("pool_status", n)],
        )?,
        Step::UpdateStatus => st.invoke(pool, &Who::User.key_name(), "update_status", &[])?,
        Step::BackstopDeposit { who, amount } => st.invoke(
            &c.backstop,
            &who.key_name(),
            "deposit",
            &[
                kv("from", side.addr(*who)),
                kv("pool_address", pool),
                kv("amount", amount),
            ],
        )?,
        Step::BackstopQueuePct { who, pct } => {
            let ub = user_balance_view(st, side, *who)?;
            let amount = num(&ub["shares"]) * i128::from(*pct) / 100;
            st.invoke(
                &c.backstop,
                &who.key_name(),
                "queue_withdrawal",
                &[
                    kv("from", side.addr(*who)),
                    kv("pool_address", pool),
                    kv("amount", amount),
                ],
            )?
        }
        Step::BackstopDequeueHalf { who } => {
            let amount = queued_view(st, side, *who)? / 2;
            st.invoke(
                &c.backstop,
                &who.key_name(),
                "dequeue_withdrawal",
                &[
                    kv("from", side.addr(*who)),
                    kv("pool_address", pool),
                    kv("amount", amount),
                ],
            )?
        }
        Step::BackstopWithdrawQueued { who } => {
            let amount = queued_view(st, side, *who)?;
            st.invoke(
                &c.backstop,
                &who.key_name(),
                "withdraw",
                &[
                    kv("from", side.addr(*who)),
                    kv("pool_address", pool),
                    kv("amount", amount),
                ],
            )?
        }
        Step::NewAuction {
            user,
            bid,
            lot,
            percent,
        } => {
            let bid: Vec<&str> = bid.iter().map(|t| c.token(*t)).collect();
            let lot: Vec<&str> = lot.iter().map(|t| c.token(*t)).collect();
            let inv = st.invoke(
                pool,
                &Who::Frodo.key_name(),
                "new_auction",
                &[
                    kv("auction_type", 0),
                    kv("user", side.addr(*user)),
                    kv("bid", json!(bid)),
                    kv("lot", json!(lot)),
                    kv("percent", percent),
                ],
            )?;
            if let Some(block) = inv.value.as_ref().and_then(|v| v["block"].as_u64()) {
                hooks.auction_recorded(side.tag, *user, block as u32)?;
            }
            inv
        }
        Step::DelAuction { user } => st.invoke(
            pool,
            &Who::User.key_name(),
            "del_auction",
            &[kv("auction_type", 0), kv("user", side.addr(*user))],
        )?,
        Step::DelAuctionWhenStale { user, code } => {
            let args = [kv("auction_type", 0), kv("user", side.addr(*user))];
            loop {
                let probe = st.try_view(pool, &Who::User.key_name(), "del_auction", &args)?;
                if probe.ok || probe.error != Some(ErrorKind::Contract(*code)) {
                    break;
                }
                if !hooks.wait(60, "del_auction_when_stale")? {
                    // The operator aborted or skipped the wait: never fire the
                    // real mutating del_auction they just declined.
                    return Ok(SideOutcome::Declined);
                }
            }
            st.invoke(pool, &Who::User.key_name(), "del_auction", &args)?
        }
        Step::BadDebt { user } => st.invoke(
            pool,
            &Who::User.key_name(),
            "bad_debt",
            &[kv("user", side.addr(*user))],
        )?,
        Step::Gulp { asset } => st.invoke(
            pool,
            &Who::User.key_name(),
            "gulp",
            &[kv("asset", c.token(*asset))],
        )?,
        Step::QueueSetReserve { asset, modify } => {
            let cfg = if *modify {
                let mut v = reserve_config(*asset, side.m.reserves[asset].index, None);
                v["c_factor"] = serde_json::json!(1_000_000);
                v
            } else {
                reserve_config(*asset, side.m.reserves.len() as u32, None)
            };
            st.invoke(
                pool,
                &Who::Admin.key_name(),
                "queue_set_reserve",
                &[kv("asset", c.token(*asset)), kv("metadata", cfg)],
            )?
        }
        Step::Trap(t) => trap_view(st, side, *t)?,
        Step::WaitLedgersSince { .. }
        | Step::WaitSeconds(_)
        | Step::AssertRatesIncreased { .. } => return Ok(SideOutcome::Unmutating),
    };
    Ok(SideOutcome::Invocation(inv))
}

/// The pair runner's custody/state hooks over `RunState`: the minted credit,
/// auction-block record, and gate-visible waiting reproduce the retired pair
/// runner's effects byte-for-byte.
struct PairSideHooks<'r, 'a, 'b, E: Executor + Sync> {
    runner: &'r mut Runner<'a, E>,
    captured: &'b Snapshot,
}

impl<E: Executor + Sync> SideHooks for PairSideHooks<'_, '_, '_, E> {
    fn captured(&self) -> Option<&Snapshot> {
        Some(self.captured)
    }

    fn mint_recorded(&mut self, side: char, token: Tok, amount: i64) -> anyhow::Result<()> {
        *self
            .runner
            .state
            .minted
            .entry(side.to_string())
            .or_default()
            .entry(token)
            .or_insert(0) += amount;
        Ok(())
    }

    fn auction_recorded(&mut self, side: char, user: Who, block: u32) -> anyhow::Result<()> {
        self.runner
            .state
            .auction_blocks
            .entry(side.to_string())
            .or_default()
            .insert(user, block);
        self.runner.state.save(self.runner.out)
    }

    fn wait(&mut self, secs: u64, label: &str) -> anyhow::Result<bool> {
        let label = label.to_string();
        let ok = self
            .runner
            .nap(Duration::from_secs(secs), &|left| Ev::Waiting {
                label: label.clone(),
                now: 0,
                target: 0,
                secs_left: left,
            })?;
        if !ok {
            // The operator declined the firing gate: record the abort exactly
            // as the retired pair runner did, so the pending mutation is never
            // sent and the step's custody journal reads `declined`.
            self.runner.step_abort = true;
        }
        Ok(ok)
    }
}

/// The outcome label for one record under a policy. The pair arm stores the
/// same bytes the retired `String` version produced (AC9); the market arm
/// sanitizes before storage.
fn outcome(policy: Policy, r: &StepRecord) -> Safe {
    match &r.invocation {
        None => Safe::literal("-"),
        Some(i) if i.ok => Safe::literal("ok"),
        Some(i) => match &i.error {
            Some(ErrorKind::Contract(c)) => match crate::stellar::error_name(*c) {
                Some(n) => policy_store(policy, format!("{n}({c})")),
                None => policy_store(policy, format!("err#{c}")),
            },
            Some(k) => Safe::error_summary(policy, k),
            None => Safe::literal("err"),
        },
    }
}

fn should_journal(step: &Step) -> bool {
    !matches!(
        step,
        Step::WaitLedgersSince { .. } | Step::WaitSeconds(_) | Step::AssertRatesIncreased { .. }
    )
}

/// Labels that complete by waiting and so legitimately retain no evidence
/// directory and no invocation.
fn is_wait(step: &Step) -> bool {
    matches!(step, Step::WaitLedgersSince { .. } | Step::WaitSeconds(_))
}

fn requested_labels(scenario: &Scenario) -> Vec<String> {
    scenario
        .steps
        .iter()
        .map(|s| s.label.to_string())
        .chain(traps().iter().map(|s| s.label.to_string()))
        .collect()
}

/// Where a step's evidence directory lives: under the run dir's
/// `evidence/<scenario>/`, one numbered directory per label.
fn evidence_step_dir(out: &Path, scenario: &str, index: usize, label: &str) -> PathBuf {
    out.join("evidence")
        .join(scenario)
        .join(format!("{index:03}-{label}"))
}

#[cfg(test)]
mod custody_regressions {
    use super::*;
    use crate::manifest::{Contracts, ReserveRef, Tok};
    use crate::scenario::scenarios;
    use crate::stellar::fake::FakeExecutor;
    use crate::stellar::CmdOutput;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Mutex, MutexGuard};

    const A: &str = "CPOOL_A";
    const B: &str = "CPOOL_B";

    fn out_id(args: &[String], want: &str) -> bool {
        args.iter()
            .position(|a| a == "--id")
            .and_then(|i| args.get(i + 1))
            .is_some_and(|id| id == want)
    }

    fn fun(args: &[String]) -> &'_ str {
        args.iter()
            .position(|a| a == "--")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
            .unwrap_or_default()
    }

    fn arg_val<'x>(args: &'x [String], flag: &str) -> &'x str {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// The fixture manifest registers Stable=1, Xlm=2, Weth=3; the fake
    /// echoes the requested token so the typed identity checks hold.
    fn reserve_index(asset: &str) -> u32 {
        if asset.contains("STABLE") {
            1
        } else if asset.contains("XLM") {
            2
        } else {
            3
        }
    }

    fn cmd_ok(stdout: &str, stderr: &str) -> CmdOutput {
        crate::stellar::fake::FakeExecutor::ok(stdout, stderr)
    }

    /// The shared stellar.rs absence diagnostic, referenced (not
    /// retyped) so the fake and the matcher cannot drift.
    const EXACT_ABSENT: &str = crate::stellar::CLI_MISSING_ENTRY;

    fn cmd_fail(stderr: &str) -> CmdOutput {
        crate::stellar::fake::FakeExecutor::fail(stderr)
    }

    /// Deterministic CLI seam: views are dispatched by contract/function, while
    /// chain mutations are FIFO-scripted and separately counted.
    struct CustodyExecutor {
        mutations: Mutex<FakeExecutor>,
        status: [AtomicU32; 2],
        fail_a_after_set: bool,
        snapshot_failed: AtomicBool,
        /// Temporary auction storage model: raw `--durability temporary`
        /// reads and `get_auction` answer from this one flag, so the probe
        /// and the typed view can never disagree on existence.
        auction_present: AtomicBool,
        /// Injects an unexpected (non-absence) contract-read failure.
        read_failed: AtomicBool,
        /// Injects a positions view whose liabilities map is not a map at all:
        /// the strict repayment proof must fail closed on it, never read it as zero.
        positions_malformed: AtomicBool,
    }

    impl CustodyExecutor {
        fn new(mutations: Vec<CmdOutput>, fail_a_after_set: bool) -> Self {
            Self {
                mutations: Mutex::new(FakeExecutor::new(mutations)),
                status: [AtomicU32::new(3), AtomicU32::new(3)],
                fail_a_after_set,
                snapshot_failed: AtomicBool::new(false),
                auction_present: AtomicBool::new(false),
                read_failed: AtomicBool::new(false),
                positions_malformed: AtomicBool::new(false),
            }
        }

        fn mutations(&self) -> MutexGuard<'_, FakeExecutor> {
            self.mutations.lock().unwrap_or_else(|p| p.into_inner())
        }

        fn mutation_calls(&self) -> Vec<Vec<String>> {
            self.mutations().calls.borrow().clone()
        }

        fn view_parts(&self, args: &[String]) -> (bool, String, String) {
            let side_a = out_id(args, A);
            if side_a && self.snapshot_failed.load(Ordering::SeqCst) {
                return (false, String::new(), "simulated snapshot failure".into());
            }
            match fun(args) {
                "get_positions" if self.positions_malformed.load(Ordering::SeqCst) => (
                    true,
                    json!({"supply":[],"collateral":[],"liabilities":null}).to_string(),
                    String::new(),
                ),
                "get_positions" => (
                    true,
                    json!({"supply":[],"collateral":[],"liabilities":[]}).to_string(),
                    String::new(),
                ),
                "get_reserve" => (
                    true,
                    json!({
                        "asset": arg_val(args, "--asset"),
                        "config": { "index": reserve_index(arg_val(args, "--asset")) },
                        "data": {
                            "b_supply": "0", "d_supply": "0",
                            "b_rate": 1_000_000_000_000_000_000_u64,
                            "d_rate": 1_000_000_000_000_000_000_u64,
                            "backstop_credit": "0"
                        }
                    })
                    .to_string(),
                    String::new(),
                ),
                "get_config" => (
                    true,
                    json!({
                        "bstop_rate": 0,
                        "oracle": if side_a { "CORACLE_A" } else { "CORACLE_B" },
                        "status": self.status[if side_a { 0 } else { 1 }].load(Ordering::SeqCst)
                    })
                    .to_string(),
                    String::new(),
                ),
                "pool_data" => (true, r#"{"shares":"0","tokens":"0"}"#.into(), String::new()),
                "user_balance" => (true, r#"{"shares":"0","q4w":[]}"#.into(), String::new()),
                "reward_zone" => (true, "[]".into(), String::new()),
                "balance" => (true, "0".into(), String::new()),
                // Absence comes from the raw temporary probe, not a guessed
                // error code: with a row present, `get_auction` must answer
                // like the real contract does (it `unwrap_optimized()`s the
                // temporary read, so it only traps when the row is absent).
                "get_auction" if self.auction_present.load(Ordering::SeqCst) => (
                    true,
                    json!({ "block": 10, "bid": {}, "lot": {} }).to_string(),
                    String::new(),
                ),
                "get_auction" => (
                    false,
                    String::new(),
                    "HostError: Error(Contract, #1)".into(),
                ),
                _ => (
                    false,
                    String::new(),
                    "unexpected custody view function".into(),
                ),
            }
        }
    }

    impl Executor for CustodyExecutor {
        fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput> {
            let cmd = args.first().map(String::as_str);
            let sub = args.get(1).map(String::as_str);
            if cmd == Some("--version") {
                return Ok(cmd_ok("stellar 26.0.0", ""));
            }
            if cmd == Some("xdr") {
                // The in-tree fake has no JSON parser: recognize the canonical
                // fixture keys by their stable JSON prefixes and return valid
                // ScVal XDR whose decoded form matches what read_entry checks.
                let json = std::fs::read_to_string(args.last().expect("xdr input")).unwrap();
                let encoded = if json.contains("\"ResData\"") {
                    "AAAAFQwAAAAAAAAACFJlc0RhdGFRVVZtTUFBQT0AAAA="
                } else {
                    "AAAAAA=="
                };
                return Ok(cmd_ok(encoded, ""));
            }
            if cmd == Some("contract") && sub == Some("read") {
                if self.read_failed.load(Ordering::SeqCst) {
                    return Ok(cmd_fail("error: unexpected ledger read failure"));
                }
                let durability = arg_val(args, "--durability");
                let key = arg_val(args, "--key-xdr");
                // A configured reserve's persistent `ResData` row must be
                // present for every snapshot; absence is a real invariant
                // violation. Echo the requested key with the ledger columns.
                if durability == "persistent" && !key.is_empty() {
                    return Ok(cmd_ok(&format!("{key},AAAAEw==,AEfhxg==,AFAORw=="), ""));
                }
                // Temporary durability is the auction-existence probe; with
                // a row present the CLI echoes the requested key plus value
                // and ledger columns, exactly like a real `contract read`.
                if durability == "temporary" && self.auction_present.load(Ordering::SeqCst) {
                    return Ok(cmd_ok(&format!("{key},AAAAEw==,AEfhxg==,AFAORw=="), ""));
                }
                return Ok(cmd_fail(EXACT_ABSENT));
            }
            if cmd == Some("contract") && sub == Some("invoke") {
                if args.iter().any(|a| a == "--send" || a == "no") {
                    let (ok, stdout, stderr) = self.view_parts(args);
                    return Ok(if ok {
                        cmd_ok(&stdout, &stderr)
                    } else {
                        cmd_fail(&stderr)
                    });
                }
                let f = fun(args);
                let out = self.mutations().run(args)?;
                if f == "set_status" && out.ok() && out_id(args, A) && self.fail_a_after_set {
                    self.snapshot_failed.store(true, Ordering::SeqCst);
                }
                return Ok(out);
            }
            Err(anyhow::anyhow!(
                "unexpected custody executor argv: {args:?}"
            ))
        }
    }

    fn manifest() -> PairManifest {
        let mut identities = BTreeMap::new();
        for who in Who::ALL {
            identities.insert(who, format!("G{}", who.as_str().to_uppercase()));
        }
        let side = |tag: char, pool: &str, oracle: &str, backstop: &str| {
            let mut contracts = Contracts::default();
            for t in Tok::ALL {
                contracts.set_token(t, format!("T{}_{pool}", t.as_str().to_uppercase()));
            }
            contracts.oracle = oracle.into();
            contracts.pool = pool.into();
            contracts.backstop = backstop.into();
            contracts.emitter = format!("EMITTER_{pool}");
            SideManifest {
                label: format!("side-{tag}"),
                deploy_ledger: 0,
                wasm: Default::default(),
                contracts,
                reserves: Tok::RESERVES
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        (
                            *t,
                            ReserveRef {
                                index: i as u32 + 1,
                                decimals: 7,
                            },
                        )
                    })
                    .collect(),
                minted: Default::default(),
            }
        };
        PairManifest {
            profile: Profile::StockVsFork,
            description: None,
            net: crate::stellar::Net {
                rpc_url: "http://127.0.0.1:1/rpc".into(),
                passphrase: "custody-test".into(),
                config_dir: std::env::temp_dir(),
                headers: Vec::new(),
            },
            identities,
            a: side('a', A, "CORACLE_A", "CBACKSTOP_A"),
            b: side('b', B, "CORACLE_B", "CBACKSTOP_B"),
        }
    }

    fn out_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("abtest-custody-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn scenario(name: &str) -> Scenario {
        scenarios().into_iter().find(|s| s.name == name).unwrap()
    }

    fn stop_hook(e: Ev) -> Act {
        match e {
            Ev::StepDone { .. } | Ev::Halt { .. } => Act::Abort,
            _ => Act::Continue,
        }
    }

    thread_local! {
        /// Records each synchronous test's halt evidence without sharing it
        /// with concurrently running tests.
        static HALT_EVENTS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
    }

    /// The oracle probe whose rejection must desync the scenario.
    const ORACLE_PROBE: &str = "oracle_set_future_price";

    fn with_halt_log<R>(f: impl FnOnce(&mut Vec<(String, String)>) -> R) -> R {
        HALT_EVENTS.with_borrow_mut(f)
    }

    fn halt_recorded() -> bool {
        with_halt_log(|log| {
            log.iter()
                .any(|(kind, label)| kind == "stepdone" && label == ORACLE_PROBE)
        })
    }

    fn record_halt(e: Ev) -> Act {
        match &e {
            Ev::StepDone {
                evidence,
                desynced: true,
                ..
            } => with_halt_log(|log| log.push(("stepdone".into(), evidence.label.clone()))),
            Ev::StepSkipped { label, why, .. } => {
                with_halt_log(|log| log.push((why.clone(), label.clone())))
            }
            Ev::StepStarted { .. } if halt_recorded() => return Act::Abort,
            _ => {}
        }
        Act::Continue
    }

    #[test]
    fn future_oracle_debt_is_repaid_before_shared_price_reset() {
        #[derive(Default)]
        struct OracleSide {
            minted: i128,
            collateral: i128,
            debt: i128,
            future: bool,
            accepted_future: bool,
            repaid: bool,
        }

        // The same argv can carry the flag twice: `balance` view passes
        // `--id <token>` as the invoked contract and `--id <holder>` as
        // the function argument. Only the latter names the balance holder.
        fn fun_arg<'x>(args: &'x [String], flag: &str) -> &'x str {
            let sep = args.iter().position(|a| a == "--").unwrap_or(0);
            args[sep..]
                .iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(sep + i + 1))
                .map(String::as_str)
                .unwrap_or_default()
        }

        // Only the oracle prefix needs state: delegate raw reads and unrelated
        // views to the custody fixture, rather than model another pool.
        struct OracleExecutor<'a> {
            base: CustodyExecutor,
            manifest: &'a PairManifest,
            sides: Mutex<[OracleSide; 2]>,
            reset_with_debt: AtomicBool,
        }

        impl Executor for OracleExecutor<'_> {
            fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput> {
                if args.first().map(String::as_str) != Some("contract")
                    || args.get(1).map(String::as_str) != Some("invoke")
                {
                    return self.base.run(args);
                }
                let a = &self.manifest.a.contracts;
                let i = usize::from(
                    !out_id(args, &a.pool)
                        && !out_id(args, &a.oracle)
                        && !Tok::ALL.iter().any(|t| out_id(args, a.token(*t))),
                );
                let c = if i == 0 {
                    a
                } else {
                    &self.manifest.b.contracts
                };
                let frodo = self.manifest.addr(Who::Frodo);
                let mut sides = self.sides.lock().unwrap_or_else(|p| p.into_inner());
                let shared_debt = sides.iter().any(|s| s.debt != 0);
                let s = &mut sides[i];
                if args.iter().any(|arg| arg == "--send" || arg == "no") {
                    // Views the wrapper does not model answer straight from
                    // the base fixture, so an unexpected read fails at its
                    // own layer rather than inside this parser.
                    let mut value: serde_json::Value = match fun(args) {
                        "get_config" | "get_positions" | "get_reserve" | "balance" => {
                            serde_json::from_str(&self.base.view_parts(args).1).map_err(|e| {
                                anyhow::anyhow!("custody fixture view returned bad json: {e}")
                            })?
                        }
                        _ => return self.base.run(args),
                    };
                    match fun(args) {
                        "get_config" => value["oracle"] = json!(c.oracle),
                        "get_positions" if arg_val(args, "--address") == frodo => {
                            value["collateral"] = if s.collateral == 0 {
                                json!({})
                            } else {
                                json!({"1": s.collateral.to_string()})
                            };
                            value["liabilities"] = if s.debt == 0 {
                                json!({})
                            } else {
                                json!({"2": s.debt.to_string()})
                            };
                        }
                        "get_reserve" => {
                            // Serve reserve identity from the manifest, not
                            // from the asset string's shape: the deployed
                            // contracts are checksum-valid StrKeys.
                            let side = if i == 0 {
                                &self.manifest.a
                            } else {
                                &self.manifest.b
                            };
                            let asset = arg_val(args, "--asset");
                            if let Some(t) = Tok::RESERVES
                                .iter()
                                .find(|t| side.contracts.token(**t) == asset)
                            {
                                if let Some(r) = side.reserves.get(t) {
                                    value["asset"] = json!(asset);
                                    value["config"]["index"] = json!(r.index);
                                    value["config"]["decimals"] = json!(r.decimals);
                                }
                            }
                            if asset == c.token(Tok::Stable) {
                                value["data"]["b_supply"] = json!(s.collateral.to_string());
                            } else if asset == c.token(Tok::Xlm) {
                                value["data"]["d_supply"] = json!(s.debt.to_string());
                                value["data"]["d_rate"] = json!(1_000_000_000_000_u64);
                            }
                        }
                        "balance" => {
                            let holder = fun_arg(args, "--id");
                            let amount = if out_id(args, c.token(Tok::Stable)) {
                                if holder == frodo {
                                    s.minted - s.collateral
                                } else if holder == c.pool {
                                    s.collateral
                                } else {
                                    0
                                }
                            } else if out_id(args, c.token(Tok::Xlm)) {
                                if holder == frodo {
                                    s.debt
                                } else if holder == c.pool {
                                    1_000_000_000_000 - s.debt
                                } else {
                                    0
                                }
                            } else {
                                0
                            };
                            value = json!(amount.to_string());
                        }
                        _ => {}
                    }
                    return Ok(cmd_ok(&value.to_string(), ""));
                }
                match fun(args) {
                    "mint" => s.minted += arg_val(args, "--amount").parse::<i128>()?,
                    "set_price" | "set_price_stable" => {
                        // `SetPrices` routes through `deploy::set_prices`'
                        // `set_price_stable` with no `--timestamp`: an absent
                        // timestamp is a present, good price.
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_secs();
                        let future = arg_val(args, "--timestamp")
                            .parse::<u64>()
                            .is_ok_and(|at| at > now + 1_800);
                        if s.future && !future && shared_debt {
                            self.reset_with_debt.store(true, Ordering::SeqCst);
                        }
                        s.future = future;
                    }
                    "submit" => {
                        let requests: Value = serde_json::from_str(arg_val(args, "--requests"))?;
                        let request = &requests[0];
                        let amount = num(&request["amount"]);
                        match request["request_type"].as_u64().unwrap() as u32 {
                            req::SUPPLY_COLLATERAL => s.collateral += amount,
                            req::BORROW if i == 1 && s.future => {
                                return Ok(cmd_fail("error: HostError: Error(Contract, #1210)"));
                            }
                            req::BORROW => {
                                s.debt += amount;
                                s.accepted_future |= s.future;
                                let borrow = json!([
                                    {"symbol": "borrow"},
                                    {"address": c.token(Tok::Xlm)},
                                    {"address": frodo}
                                ]);
                                let transfer = json!([
                                    {"symbol": "transfer"},
                                    {"address": c.pool},
                                    {"address": frodo}
                                ]);
                                let debt = json!({"vec": [
                                    {"i128": amount.to_string()}, {"i128": amount.to_string()}
                                ]});
                                let assets = json!({"i128": amount.to_string()});
                                return Ok(cmd_ok(
                                    "",
                                    &format!(
                                        "{} - Success - Event: {borrow} = {debt}\n\
                                         {} - Success - Event: {transfer} = {assets}",
                                        c.pool,
                                        c.token(Tok::Xlm)
                                    ),
                                ));
                            }
                            req::REPAY => {
                                anyhow::ensure!(amount >= s.debt, "repayment underfunded");
                                s.debt = 0;
                                s.repaid = true;
                            }
                            kind => anyhow::bail!("unexpected oracle request {kind}"),
                        }
                    }
                    function => anyhow::bail!("unexpected oracle mutation {function}"),
                }
                Ok(cmd_ok("", ""))
            }
        }

        // A checksum-valid StrKey contract emitter (version 0x10,
        // seed-derived payload, LE CRC16-XMODEM, base32): the CLI event parser
        // admits only contracts `ContractId::from_str` accepts, and the
        // comparator matches the executed borrow/transfer contracts against
        // exactly these manifest addresses. Checksumless padding fails the
        // parser, so the authorized footprint is never recorded.
        fn caddr(seed: &str) -> String {
            let mut bytes = [0u8; 35];
            bytes[0] = 0x10;
            for (i, b) in seed.bytes().cycle().take(32).enumerate() {
                bytes[1 + i] = b;
            }
            let mut crc: u16 = 0;
            for b in &bytes[..33] {
                crc ^= (*b as u16) << 8;
                for _ in 0..8 {
                    crc = if crc & 0x8000 != 0 {
                        (crc << 1) ^ 0x1021
                    } else {
                        crc << 1
                    };
                }
            }
            bytes[33..].copy_from_slice(&crc.to_le_bytes());
            const ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
            let (mut buf, mut bits, mut out) = (0u32, 0u32, String::with_capacity(56));
            for b in bytes {
                buf = (buf << 8) | b as u32;
                bits += 8;
                while bits >= 5 {
                    bits -= 5;
                    out.push(ALPHA[((buf >> bits) & 0x1f) as usize] as char);
                }
            }
            out
        }

        let mut m = manifest();
        for side in [&mut m.a, &mut m.b] {
            let tag = side.label.as_str();
            side.contracts.pool = caddr(&format!("{tag} pool"));
            for t in Tok::RESERVES {
                side.contracts
                    .set_token(t, caddr(&format!("{tag} {}", t.as_str())));
            }
            side.minted.insert(Tok::Xlm, 1_000_000_000_000);
        }
        let out = out_dir("oracle-cleanup-before-reset");
        let fx = OracleExecutor {
            base: CustodyExecutor::new(vec![], false),
            manifest: &m,
            sides: Mutex::new(std::array::from_fn(|_| OracleSide::default())),
            reset_with_debt: AtomicBool::new(false),
        };
        let reached_reset = AtomicBool::new(false);
        let hook = |event: Ev| -> Act {
            match event {
                Ev::StepDone {
                    evidence,
                    violations,
                    desynced,
                } => {
                    assert!(!desynced, "{} desynced", evidence.label);
                    assert_eq!(
                        violations, 0,
                        "{}: a={:?}, b={:?}, diff={:?}",
                        evidence.label, evidence.a.violations, evidence.b.violations, evidence.diff
                    );
                    if evidence.label == "oracle_reset_after_future" {
                        reached_reset.store(true, Ordering::SeqCst);
                        assert!(!fx.reset_with_debt.load(Ordering::SeqCst));
                        let sides = fx.sides.lock().unwrap_or_else(|p| p.into_inner());
                        assert!(sides[0].accepted_future && !sides[1].accepted_future);
                        assert!(sides.iter().all(|s| s.repaid && s.debt == 0));
                        return Act::Abort;
                    }
                }
                Ev::Halt { label, reason, .. } => panic!("{label}: {reason}"),
                _ => {}
            }
            Act::Continue
        };
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        runner.hook = Some(&hook);
        runner.run(Some("strict_oracle"), None).unwrap();
        assert!(
            reached_reset.load(Ordering::SeqCst),
            "shared reset was never observed"
        );
        std::fs::remove_dir_all(out).unwrap();
    }

    #[test]
    fn failed_oracle_setup_is_retained_and_fails() {
        with_halt_log(|log| log.clear());
        let m = manifest();
        let out = out_dir("failed-oracle");
        let fx = CustodyExecutor::new(
            vec![
                cmd_fail("error: HostError: Error(Contract, #1200)"),
                cmd_fail("error: HostError: Error(Contract, #1200)"),
            ],
            false,
        );
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        runner.hook = Some(&record_halt);
        // Start at the probe so the scripted failures belong to it, not to the
        // scenario's mint/supply setup.
        runner
            .run(Some("strict_oracle"), Some("oracle_set_future_price"))
            .unwrap();
        assert_eq!(
            fx.mutation_calls().len(),
            2,
            "both sides must have attempted the oracle write"
        );

        with_halt_log(|events| {
            assert!(
                events
                    .iter()
                    .any(|(kind, label)| kind == "stepdone" && label == "oracle_set_future_price"),
                "a rejected mutating call must desync the run: {events:?}"
            );
            assert!(
                events.iter().any(
                    |(kind, label)| kind == "desynced" && label == "oracle_borrow_future_price"
                ),
                "the desync must keep dependent steps from executing: {events:?}"
            );
        });

        let index = scenario("strict_oracle")
            .steps
            .iter()
            .position(|s| s.label == "oracle_set_future_price")
            .unwrap();
        // Retained as a judged outcome — evidence on disk, pending guard clear,
        // so a resume does not mistake it for an unresolved chain write.
        assert!(
            evidence_step_dir(&out, "strict_oracle", index, "oracle_set_future_price").exists()
        );
        assert!(
            !pending_step_dir(&out, "strict_oracle", index, "oracle_set_future_price").exists()
        );
        // What the step recorded is a judged outcome: the rejected invocation and
        // the one expectation it failed are both retained.
        let steps = crate::report::load_steps(&out, &m).unwrap();
        let ev = steps
            .iter()
            .find(|s| s.label == "oracle_set_future_price")
            .unwrap();
        for (tag, r) in [('a', &ev.a), ('b', &ev.b)] {
            let inv = r
                .invocation
                .as_ref()
                .unwrap_or_else(|| panic!("{tag} must retain the rejected invocation"));
            assert!(!inv.ok, "{tag} must retain the rejected oracle write");
            assert_eq!(
                inv.error,
                Some(ErrorKind::Contract(1200)),
                "{tag} must retain the contract error"
            );
            assert_eq!(
                r.violations.len(),
                1,
                "{tag} must hold exactly one failed expectation: {:?}",
                r.violations
            );
            assert!(
                format!("{}", r.violations[0]).starts_with("expected "),
                "{tag}: {:?}",
                r.violations
            );
        }

        // The barrier is a recomputation over the retained invocation, not a
        // replay of stored diagnostics: erase the recorded messages and the
        // resume must still refuse every dependent step.
        for side in ["a.json", "b.json"] {
            let p = evidence_step_dir(&out, "strict_oracle", index, "oracle_set_future_price")
                .join(side);
            let mut j: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            j["violations"] = json!([]);
            std::fs::write(&p, serde_json::to_string(&j).unwrap()).unwrap();
        }

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let mut runner2 = Runner::new(&st2, &m, &out)
            .expect("a judged failure must not block resume as unresolved");
        with_halt_log(|log| log.clear());
        runner2.hook = Some(&resume_watch);
        runner2.run(Some("strict_oracle"), None).unwrap();
        // The barrier is the whole point of retaining the evidence: a completed
        // setup that FAILED must never be skipped as merely done, and its
        // dependents must not be driven against state they never established.
        assert!(
            fx2.mutation_calls().is_empty(),
            "a resumed run may not send anything after a failed setup: {:?}",
            fx2.mutation_calls()
        );
        with_halt_log(|log| {
            assert!(
                log.iter()
                    .any(|(why, label)| why == "desynced" && label == "oracle_borrow_future_price"),
                "the dependent step must be refused on resume: {log:?}"
            );
        });
        let dependent = scenario("strict_oracle")
            .steps
            .iter()
            .position(|s| s.label == "oracle_borrow_future_price")
            .unwrap();
        assert!(
            !evidence_step_dir(
                &out,
                "strict_oracle",
                dependent,
                "oracle_borrow_future_price"
            )
            .exists(),
            "a refused dependent step may not record evidence either"
        );
    }

    /// `PerSide` exempts the stock side from the outcome judgement, never from
    /// having an outcome: a missing mutation result is a lost result, while an
    /// observation-only step legitimately has no invocation at all.
    #[test]
    fn per_side_stock_side_may_not_lack_the_invocation() {
        let mutating = LabeledStep {
            label: "per_side_probe",
            expect: Expect::PerSide,
            step: Step::SetPrices(vec![1]),
            delta_dependent: false,
        };
        assert_eq!(
            check_expectation(
                Policy::Pair,
                Profile::StockVsFork,
                'a',
                &mutating,
                None,
                None,
            )
            .iter()
            .map(|f| format!("{f}"))
            .collect::<Vec<String>>(),
            vec![format!("expected an invocation for {:?}", mutating.step)],
            "stock evidence must not be silently absent"
        );

        let observed = LabeledStep {
            label: "per_side_rates",
            expect: Expect::PerSide,
            step: Step::AssertRatesIncreased { asset: Tok::Stable },
            delta_dependent: false,
        };
        assert!(
            check_expectation(
                Policy::Pair,
                Profile::StockVsFork,
                'a',
                &observed,
                None,
                None,
            )
            .is_empty(),
            "an observation step has no invocation to require"
        );

        let m = manifest();
        let out = out_dir("absent-stock-mutation");
        m.save(&out).unwrap();
        let fx = CustodyExecutor::new(vec![cmd_ok("", "")], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        let s = scenario("freeze");
        let index = s
            .steps
            .iter()
            .position(|x| x.label == "freeze_set_status_0_rejected")
            .unwrap();
        // One-sided execution: the stock result is never recorded at all.
        runner
            .step_once(
                "freeze",
                index,
                &s.steps[index],
                &mut BTreeMap::new(),
                false,
                Sides::B,
            )
            .unwrap();
        let steps = crate::report::load_steps(&out, &m).unwrap();
        let ev = steps
            .iter()
            .find(|x| x.label == "freeze_set_status_0_rejected")
            .unwrap();
        assert!(
            ev.findings.iter().any(|f| {
                f.starts_with("a expected-check:") && f.contains("expected an invocation")
            }),
            "an absent stock mutation must fail the replayed verdict: {:?}",
            ev.findings
        );
        assert!(
            !ev.findings
                .iter()
                .any(|f| f.starts_with("b expected-check:") && f.contains("expected an invocation")),
            "the executed fork side did record its outcome: {:?}",
            ev.findings
        );
    }

    #[test]
    fn interrupted_mutation_keeps_outcome_durable_and_blocks_resume() {
        let m = manifest();
        let out = out_dir("interrupted-mutation");
        let fx1 = CustodyExecutor::new(vec![cmd_ok("", "SENTINEL-A")], true);
        let st1 = Stellar::new(&m.net, &fx1, Policy::Pair);
        let mut r1 = Runner::new(&st1, &m, &out).unwrap();
        let first = scenario("statuses").steps[0].label;
        assert!(r1.run(Some("statuses"), None).is_err());
        assert_eq!(fx1.mutation_calls().len(), 1);
        assert!(pending_step_dir(&out, "statuses", 0, first)
            .join("a.result.json")
            .exists());
        assert!(!evidence_step_dir(&out, "statuses", 0, first).exists());
        assert!(RunState::load(&out).unwrap().completed.is_empty());

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let err = Runner::new(&st2, &m, &out).err().unwrap().to_string();
        assert!(err.contains("unresolved side mutation"), "{err}");
        assert!(fx2.mutation_calls().is_empty());
    }

    #[test]
    fn completed_label_and_evidence_are_immutable_under_forced_replay() {
        let m = manifest();
        let out = out_dir("forced-replay");
        let fx1 = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st1 = Stellar::new(&m.net, &fx1, Policy::Pair);
        let mut r1 = Runner::new(&st1, &m, &out).unwrap();
        r1.hook = Some(&stop_hook);
        r1.run(Some("statuses"), None).unwrap();
        let first = scenario("statuses").steps[0].label;
        assert_eq!(r1.state.completed["statuses"], [first]);
        r1.mark("statuses", first).unwrap();
        assert_eq!(
            r1.state.completed["statuses"].len(),
            1,
            "mark must be idempotent"
        );

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let mut r2 = Runner::new(&st2, &m, &out).unwrap();
        let err = r2
            .run(Some("statuses"), Some(first))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing"), "{err}");
        assert!(fx2.mutation_calls().is_empty());
    }

    #[test]
    fn requested_scope_is_exact_and_operator_is_excluded() {
        let m = manifest();
        let out = out_dir("requested-scope");
        let fx = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        runner.hook = Some(&stop_hook);
        runner.run(Some("statuses"), None).unwrap();
        let statuses = scenario("statuses");
        assert_eq!(
            runner.state.requested["statuses"],
            requested_labels(&statuses)
        );

        let mut last = BTreeMap::new();
        runner
            .operator_step(
                &LabeledStep {
                    label: "operator_wait",
                    expect: Expect::Any,
                    step: Step::WaitSeconds(0),
                    delta_dependent: false,
                },
                Sides::Both,
                &mut last,
            )
            .unwrap();
        let state = RunState::load(&out).unwrap();
        assert!(!state.requested.contains_key(OPERATOR_SCENARIO));
        assert!(!state.completed.contains_key(OPERATOR_SCENARIO));
        assert!(out
            .join("evidence")
            .join(OPERATOR_SCENARIO)
            .join("001-operator_wait")
            .exists());
    }

    /// Drives a scenario up to one held-back label: that label is skipped and the
    /// run stops at the first trap probe, so every earlier step is genuinely
    /// completed with its own evidence retained.
    fn hold_back(e: Ev) -> Act {
        match &e {
            Ev::StepStarted { label, .. } if label.as_str() == "interest_rates_increased" => {
                Act::Skip
            }
            Ev::StepStarted { label, .. } if label.starts_with("trap_") => Act::Abort,
            _ => Act::Continue,
        }
    }

    /// A resumed run may not pass the rates assertion vacuously: the assertion is
    /// judged against the mutations driven in THIS session, and it may skip an
    /// earlier label only because that label's evidence is still retained.
    #[test]
    fn resume_without_mutation_cannot_pass_rate_assertion() {
        let m = manifest();
        let out = out_dir("vacuous-rate");
        let interest = scenario("interest");
        let fx1 = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st1 = Stellar::new(&m.net, &fx1, Policy::Pair);
        let mut r1 = Runner::new(&st1, &m, &out).unwrap();
        r1.sleep = |_| {};
        r1.hook = Some(&hold_back);
        r1.run(Some("interest"), None).unwrap();
        assert_eq!(
            fx1.mutation_calls().len(),
            2,
            "only the scenario's own supply may be submitted"
        );
        assert_eq!(
            RunState::load(&out).unwrap().completed["interest"],
            ["interest_wait", "interest_supply"],
            "the assertion step must be the only one left undriven"
        );

        let fx = CustodyExecutor::new(vec![], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        with_halt_log(|log| log.clear());
        runner.hook = Some(&resume_watch);
        runner.run(Some("interest"), None).unwrap();
        assert!(
            fx.mutation_calls().is_empty(),
            "a resumed run may not replay a completed mutation"
        );
        with_halt_log(|log| {
            assert!(
                log.iter()
                    .any(|(why, label)| why == "already completed" && label == "interest_supply"),
                "retained evidence is what licenses the skip: {log:?}"
            );
        });
        let index = interest
            .steps
            .iter()
            .position(|s| s.label == "interest_rates_increased")
            .unwrap();
        let steps = crate::report::load_steps(&out, &m).unwrap();
        let ev = steps.iter().find(|s| s.index == index).unwrap();
        let expected: Vec<crate::stellar::Safe> = serde_json::from_value(serde_json::json!([
            "rates not established by a mutation in this session"
        ]))
        .unwrap();
        assert_eq!(
            ev.a.violations, expected,
            "side a must fail rates assertion"
        );
        assert_eq!(
            ev.b.violations, expected,
            "side b must fail rates assertion"
        );
    }

    /// Temporary auction storage is one fact: when the raw probe reports a
    /// row, the typed `get_auction` must answer — probe and view may never
    /// disagree on existence.
    #[test]
    fn temporary_auction_presence_keeps_probe_and_view_consistent() {
        let m = manifest();
        let out = out_dir("auction-present");
        let fx = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        fx.auction_present.store(true, Ordering::SeqCst);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        runner.hook = Some(&stop_hook);
        runner.run(Some("statuses"), None).unwrap();
        let steps = crate::report::load_steps(&out, &m).unwrap();
        let ev = steps.iter().find(|s| s.scenario == "statuses").unwrap();
        assert!(
            ev.a.snapshot.auctions.values().all(|v| !v.is_null()),
            "a present temporary row must not be recorded as absence"
        );
    }

    /// Any read failure other than the exact CLI absence diagnostic fails
    /// the run closed — before a single mutation is submitted.
    #[test]
    fn unexpected_read_failure_aborts_before_mutation() {
        let m = manifest();
        let out = out_dir("read-failure");
        let fx = CustodyExecutor::new(vec![], false);
        fx.read_failed.store(true, Ordering::SeqCst);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        let err = runner.run(Some("statuses"), None).unwrap_err().to_string();
        assert!(err.contains("contract read failed"), "{err}");
        assert!(
            fx.mutation_calls().is_empty(),
            "no value may move behind an unclassified read failure"
        );
    }

    /// Empty or corrupt evidence files are not a durable completion record:
    /// an unresolved journal must block resume no matter what step
    /// directories exist, and the pending layout must survive for the user.
    #[test]
    fn corrupt_evidence_cannot_clear_pending_or_authorize_resume() {
        let m = manifest();
        let out = out_dir("corrupt-evidence");
        let first = scenario("statuses").steps[0].label;
        let dir = evidence_step_dir(&out, "statuses", 0, first);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.json", "b.json", "diff.json"] {
            std::fs::write(dir.join(name), "{ corrupt").unwrap();
        }
        begin_pending(
            &out,
            &PendingIntent {
                scenario: "statuses".into(),
                index: 0,
                label: first.into(),
                side: 'a',
            },
        )
        .unwrap();

        let fx = CustodyExecutor::new(vec![], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let err = Runner::new(&st, &m, &out).err().unwrap().to_string();
        assert!(err.contains("unresolved side mutation"), "{err}");
        let pending = pending_step_dir(&out, "statuses", 0, first);
        assert!(pending.join("a.intent.json").exists(), "{pending:?}");
        assert_eq!(
            RunState::load(&out).unwrap().completed.get("statuses"),
            None,
            "corrupt evidence must not infer completion"
        );
        assert!(
            fx.mutation_calls().is_empty(),
            "no chain call before the audit passes"
        );
    }

    /// Wiring coverage: a successful `new_auction` whose temporary row the raw
    /// probe cannot see must fail the side closed, desync the scenario live, and
    /// keep it blocked for every later session; with row and typed view present
    /// the same step must pass cleanly.
    #[test]
    fn new_auction_violation_tracks_the_temporary_row() {
        let step = crate::scenario::current_step("liq_alt_new_auction").unwrap();
        let drive = |present: bool, name: &str| {
            let m = manifest();
            let out = out_dir(name);
            let fx = CustodyExecutor::new(
                vec![cmd_ok(r#"{"block":10}"#, ""), cmd_ok(r#"{"block":10}"#, "")],
                false,
            );
            fx.auction_present.store(present, Ordering::SeqCst);
            let st = Stellar::new(&m.net, &fx, Policy::Pair);
            let mut runner = Runner::new(&st, &m, &out).unwrap();
            let mut last = BTreeMap::new();
            let outcome = runner
                .step_once("liquidation", 0, step, &mut last, false, Sides::Both)
                .unwrap();
            (outcome, m, out)
        };
        let (missed, m, out) = drive(false, "auction-missed");
        let violations = &missed.evidence.as_ref().unwrap().a.violations;
        assert!(
            violations
                .iter()
                .any(|v| format!("{v}").contains("auction for")
                    && format!("{v}").contains("observed absent")),
            "unobserved auction must fail closed: {violations:?}"
        );
        assert!(missed.desynced, "a missing witness must desync live");

        // The live desync must survive as the durable barrier it armed: a fresh
        // session may not drive this scenario's dependents against an auction
        // that never appeared.
        let mut seeded = RunState::load(&out).unwrap();
        seeded.requested.insert(
            "liquidation".into(),
            requested_labels(&scenario("liquidation")),
        );
        seeded.save(&out).unwrap();
        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let mut r2 = Runner::new(&st2, &m, &out).unwrap();
        assert!(r2.blocked.contains("liquidation"), "{:?}", r2.blocked);
        with_halt_log(|log| log.clear());
        r2.hook = Some(&resume_watch);
        r2.run(Some("liquidation"), None).unwrap();
        assert!(
            fx2.mutation_calls().is_empty(),
            "a scenario blocked by a missing auction witness may not submit dependents"
        );
        with_halt_log(|log| {
            assert!(
                log.iter().any(|(why, _)| why == "desynced"),
                "dependents must be refused as desynced: {log:?}"
            );
        });

        let (seen, _, _) = drive(true, "auction-seen");
        assert!(
            seen.evidence.unwrap().a.violations.is_empty(),
            "row + typed view present must satisfy the witness cleanly"
        );
        assert!(!seen.desynced, "a witnessed auction must not desync");
    }

    /// Stripping a mutating step's invocation from retained evidence is the same
    /// lost result the live runner refuses to finalize: the scenario must block
    /// again on resume, and its dependents may not be submitted. Only this rule
    /// can fire here — the evidence directory itself survives, so the
    /// completed-label rule sees nothing missing.
    #[test]
    fn deleted_invocation_cannot_reauthorize_dependents() {
        let m = manifest();
        let out = out_dir("deleted-invocation");
        let fx1 = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st1 = Stellar::new(&m.net, &fx1, Policy::Pair);
        let mut r1 = Runner::new(&st1, &m, &out).unwrap();
        r1.hook = Some(&stop_hook);
        r1.run(Some("statuses"), None).unwrap();
        let first = scenario("statuses").steps[0].label;
        let dir = evidence_step_dir(&out, "statuses", 0, first);
        for side in ["a.json", "b.json"] {
            let path = dir.join(side);
            let mut j: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            j["invocation"] = Value::Null;
            std::fs::write(&path, serde_json::to_string(&j).unwrap()).unwrap();
        }

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let mut r2 = Runner::new(&st2, &m, &out).unwrap();
        assert!(
            r2.blocked.contains("statuses"),
            "a step stripped of its invocation must re-block its scenario: {:?}",
            r2.blocked
        );
        with_halt_log(|log| log.clear());
        r2.hook = Some(&resume_watch);
        r2.run(Some("statuses"), None).unwrap();
        assert!(
            fx2.mutation_calls().is_empty(),
            "a re-blocked scenario may not submit any dependent"
        );
        with_halt_log(|log| {
            assert!(
                log.iter()
                    .any(|(why, label)| why == "desynced" && label != first),
                "dependents must be refused as desynced: {log:?}"
            );
        });
    }

    /// A completed label whose evidence directory was deleted is unknown history:
    /// replaying the mutation could apply it twice, trusting it could not prove
    /// it ran. The scenario blocks instead of skipping the label as completed.
    /// The typed rule cannot see this case — there is no record left to judge.
    #[test]
    fn deleted_evidence_cannot_reauthorize_dependents() {
        let m = manifest();
        let out = out_dir("deleted-evidence");
        let fx1 = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st1 = Stellar::new(&m.net, &fx1, Policy::Pair);
        let mut r1 = Runner::new(&st1, &m, &out).unwrap();
        r1.hook = Some(&stop_hook);
        r1.run(Some("statuses"), None).unwrap();
        let first = scenario("statuses").steps[0].label;
        assert!(
            RunState::load(&out).unwrap().completed["statuses"].contains(&first.to_string()),
            "the step must be completed for its deletion to be the test"
        );
        let corrupt_fx = CustodyExecutor::new(vec![], false);
        let corrupt_st = Stellar::new(&m.net, &corrupt_fx, Policy::Pair);
        std::fs::write(
            evidence_step_dir(&out, "statuses", 0, first).join("a.json"),
            b"{",
        )
        .unwrap();
        assert!(Runner::new(&corrupt_st, &m, &out).is_err());
        assert!(corrupt_fx.mutation_calls().is_empty());
        std::fs::remove_dir_all(evidence_step_dir(&out, "statuses", 0, first)).unwrap();

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let mut r2 = Runner::new(&st2, &m, &out).unwrap();
        assert!(
            r2.blocked.contains("statuses"),
            "deleted evidence must not authorize the step it documents: {:?}",
            r2.blocked
        );
        with_halt_log(|log| log.clear());
        r2.hook = Some(&resume_watch);
        r2.run(Some("statuses"), None).unwrap();
        assert!(
            fx2.mutation_calls().is_empty(),
            "deleted evidence must not authorize a resend or a dependent"
        );
        with_halt_log(|log| {
            assert!(
                log.iter()
                    .any(|(why, label)| why == "desynced" && label == first),
                "the evidence-less label must not be skipped as completed: {log:?}"
            );
        });
    }

    /// A hook that records skips and stops at the first trap probe: the trap
    /// block is legitimate post-scenario work, so a resume test only needs the
    /// runner to have refused every dependent step before reaching it.
    fn resume_watch(e: Ev) -> Act {
        match &e {
            Ev::StepSkipped { label, why, .. } => {
                with_halt_log(|log| log.push((why.clone(), label.clone())))
            }
            Ev::StepStarted { label, .. } if label.starts_with("trap_") => return Act::Abort,
            _ => {}
        }
        Act::Continue
    }

    /// A mutating CLI call whose failure leaves inclusion unknown is custody,
    /// not an outcome: one attempt, no evidence, no completion, pending intact.
    #[test]
    fn ambiguous_send_failure_is_never_finalized_as_a_completed_mutation() {
        let m = manifest();
        let out = out_dir("ambiguous-send");
        let fx = CustodyExecutor::new(vec![cmd_fail("error: request timed out")], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        let first = scenario("statuses").steps[0].label;
        let err = runner.run(Some("statuses"), None).unwrap_err().to_string();
        assert!(err.contains("unresolved"), "{err}");
        assert_eq!(
            fx.mutation_calls().len(),
            1,
            "a send that may have landed must not be replayed"
        );
        assert!(
            pending_step_dir(&out, "statuses", 0, first)
                .join("a.result.json")
                .exists(),
            "the unresolved intent must survive"
        );
        assert!(
            !evidence_step_dir(&out, "statuses", 0, first).exists(),
            "an unresolved send may not publish completed evidence"
        );
        assert!(
            RunState::load(&out).unwrap().completed.is_empty(),
            "an unresolved send may not mark the label completed"
        );

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let err = Runner::new(&st2, &m, &out).err().unwrap().to_string();
        assert!(err.contains("unresolved side mutation"), "{err}");
        assert!(fx2.mutation_calls().is_empty());
    }

    /// Drives a genuine zero-liability repayment: the captured well-formed map
    /// proves the no-op, so no repayment is submitted and none is demanded.
    fn drive_zero_debt_repay(name: &str) -> (PairManifest, PathBuf) {
        let m = manifest();
        let out = out_dir(name);
        let fx = CustodyExecutor::new(vec![], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        let step = crate::scenario::current_step("oracle_repay_all").unwrap();
        let mut last = BTreeMap::new();
        let ev = runner
            .step_once("strict_oracle", 0, step, &mut last, false, Sides::Both)
            .unwrap()
            .evidence
            .expect("a certified no-op is still a recorded step");
        assert!(
            fx.mutation_calls().is_empty(),
            "well-formed zero debt must not submit a repayment"
        );
        assert!(ev.a.invocation.is_none() && ev.b.invocation.is_none());
        // Only the repayment judgement is under test here: the sparse fixture's
        // reserve/position cross-checks are the report's own business.
        assert!(
            ev.a.violations
                .iter()
                .all(|v| !format!("{v}").contains("RepayAll")),
            "{:?}",
            ev.a.violations
        );
        (m, out)
    }

    #[test]
    fn zero_debt_repay_all_is_certified_by_the_captured_snapshot() {
        let (m, out) = drive_zero_debt_repay("repay-zero");
        let steps = crate::report::load_steps(&out, &m).unwrap();
        assert!(
            !steps
                .iter()
                .any(|s| s.findings.iter().any(|f| f.contains("expected-check:"))),
            "a well-formed captured zero must certify the absent invocation: {:?}",
            steps.iter().map(|s| &s.findings).collect::<Vec<_>>()
        );
    }

    /// The ways a recorded no-op repayment can lose its proof, each tested against
    /// otherwise-valid evidence: a nonzero captured liability, a missing holder
    /// row, a missing captured baseline, and a retained-but-unresolved submission.
    /// None of them may read as zero debt or as an outcome.
    #[test]
    fn repay_all_noop_only_a_captured_zero_proves_it() {
        let (m, out) = drive_zero_debt_repay("repay-nonzero");
        let dir = evidence_step_dir(&out, "strict_oracle", 0, "oracle_repay_all");
        let pristine: BTreeMap<String, String> = ["a.json", "b.json"]
            .iter()
            .map(|f| (f.to_string(), std::fs::read_to_string(dir.join(f)).unwrap()))
            .collect();
        // The baseline the negative arms are measured against: unmodified, this
        // record certifies its own no-op.
        assert!(
            !load_repay_findings(&out, &m)
                .iter()
                .any(|f| f.contains("expected-check:")),
            "the pristine record must certify its own no-op"
        );

        for side in ["a.json", "b.json"] {
            rewrite_side(&dir, &pristine, side, |j| {
                j["before"]["positions"]["frodo"]["liabilities"] = json!([["2", "5"]]);
            });
        }
        assert_repay_finding(
            &out,
            &m,
            "RepayAll recorded no invocation while",
            "nonzero captured liability",
        );

        for side in ["a.json", "b.json"] {
            rewrite_side(&dir, &pristine, side, |j| {
                j["before"]["positions"]
                    .as_object_mut()
                    .unwrap()
                    .remove("frodo");
            });
        }
        assert_repay_finding(
            &out,
            &m,
            "without well-formed zero-debt proof",
            "absent holder row",
        );

        for side in ["a.json", "b.json"] {
            rewrite_side(&dir, &pristine, side, |j| {
                j["before"] = Value::Null;
            });
        }
        assert_repay_finding(
            &out,
            &m,
            "without well-formed zero-debt proof",
            "absent snapshot",
        );

        // An unresolved submission is not an outcome either, even beside an
        // otherwise-valid zero-debt baseline: the runtime refuses to emit this
        // shape, so replay must refuse to certify it.
        for side in ["a.json", "b.json"] {
            rewrite_side(&dir, &pristine, side, |j| {
                j["invocation"] = serde_json::json!({
                    "ok": false,
                    "value": null,
                    "error": {"Other": "request timed out"},
                    "events": [],
                    "stderr": "error: request timed out"
                });
                let unresolved: Invocation = serde_json::from_value(j["invocation"].clone())
                    .expect("fixture must be a valid retained invocation");
                j["invocation"] = serde_json::to_value(&unresolved).unwrap();
            });
        }
        assert_repay_finding(
            &out,
            &m,
            "inconsistent invocation",
            "retained unresolved submission",
        );
    }

    /// Re-writes one retained side record from its pristine bytes, so each arm of
    /// the proof tests exactly one property and nothing accumulates between arms.
    fn rewrite_side(
        dir: &Path,
        pristine: &BTreeMap<String, String>,
        side: &str,
        break_it: impl Fn(&mut Value),
    ) {
        let mut j: Value = serde_json::from_str(&pristine[side]).unwrap();
        break_it(&mut j);
        std::fs::write(dir.join(side), serde_json::to_string(&j).unwrap()).unwrap();
    }

    fn load_repay_findings(out: &Path, m: &PairManifest) -> Vec<String> {
        crate::report::load_steps(out, m)
            .unwrap()
            .iter()
            .find(|s| s.label == "oracle_repay_all")
            .expect("the repayment step is retained")
            .findings
            .clone()
    }

    /// Both sides must call the tampered record exactly what it is.
    fn assert_repay_finding(out: &Path, m: &PairManifest, needle: &str, tag: &str) {
        let findings = load_repay_findings(out, m);
        for side in ['a', 'b'] {
            assert!(
                findings.iter().any(
                    |f| f.starts_with(&format!("{side} expected-check:")) && f.contains(needle)
                ),
                "{tag}: side {side} must be refused ({needle:?}): {findings:?}"
            );
        }
    }

    /// A malformed liabilities map is not a zero: the runner must refuse to
    /// decide, leave the intent pending, and move nothing.
    #[test]
    fn repay_all_malformed_proof_is_never_read_as_zero_debt() {
        let m = manifest();
        let out = out_dir("repay-malformed");
        let fx = CustodyExecutor::new(vec![], false);
        fx.positions_malformed.store(true, Ordering::SeqCst);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        let step = LabeledStep {
            label: "repay_all_malformed",
            expect: Expect::Ok,
            step: Step::RepayAll {
                from: Who::Frodo,
                asset: Tok::Xlm,
            },
            delta_dependent: false,
        };
        let mut last = BTreeMap::new();
        let err = runner
            .step_once("t", 0, &step, &mut last, false, Sides::Both)
            .err()
            .expect("malformed repayment proof must be a tool error")
            .to_string();
        let pending_result = pending_step_dir(&out, "t", 0, step.label).join("a.result.json");
        let outcome: ManifestSideOutcome =
            serde_json::from_slice(&std::fs::read(&pending_result).unwrap()).unwrap();
        assert!(
            matches!(outcome, ManifestSideOutcome::ToolError(_)),
            "{outcome:?}"
        );
        assert!(
            fx.mutation_calls().is_empty(),
            "malformed proof must not be mistaken for zero debt: {err}"
        );
        assert!(
            !evidence_step_dir(&out, "t", 0, "repay_all_malformed").exists(),
            "an undecided step may not publish evidence"
        );
        assert!(
            RunState::load(&out).unwrap().completed.is_empty(),
            "an undecided repayment may not mark the label completed"
        );
        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let resume_err = Runner::new(&st2, &m, &out).err().unwrap().to_string();
        assert!(
            resume_err.contains("unresolved side mutation"),
            "the durable tool error must be what refuses resume: {resume_err}"
        );
        assert!(
            !pending_step_dir(&out, "t", 0, step.label)
                .join("b.result.json")
                .exists(),
            "side a's refusal must short-circuit the step before side b is attempted"
        );
        assert!(fx2.mutation_calls().is_empty());
    }

    /// `ok` and an error at once certifies neither outcome, and `PerSide` must
    /// not shelter that contradiction on the side it exempts from judgement.
    #[test]
    fn contradictory_invocation_is_rejected_before_perside_exemption() {
        let step = LabeledStep {
            label: "oracle_set_future_price",
            expect: Expect::PerSide,
            step: Step::SetPricesAt {
                prices: vec![1],
                offset_secs: 3_600,
            },
            delta_dependent: false,
        };
        let contradiction: Invocation = serde_json::from_value(serde_json::json!({
            "ok": true,
            "value": null,
            "error": {"Contract": 1200},
            "events": [],
            "stderr": ""
        }))
        .unwrap();
        let findings = check_expectation(
            Policy::Pair,
            Profile::StockVsFork,
            'a',
            &step,
            Some(&contradiction),
            None,
        );
        assert!(
            findings
                .iter()
                .any(|f| format!("{f}").starts_with("inconsistent invocation")),
            "the stock exemption may not bless ok+error: {findings:?}"
        );
        let unexplained: Invocation = serde_json::from_value(serde_json::json!({
            "ok": false,
            "value": null,
            "error": null,
            "events": [],
            "stderr": ""
        }))
        .unwrap();
        let findings = check_expectation(
            Policy::Pair,
            Profile::StockVsFork,
            'b',
            &step,
            Some(&unexplained),
            None,
        );
        assert!(
            findings
                .iter()
                .any(|f| format!("{f}").starts_with("inconsistent invocation")),
            "a failure with no error certifies nothing: {findings:?}"
        );
        // A retained but unexplained failure is the shape `invoke` now refuses to
        // emit. Stock's `PerSide` exemption covers the outcome judgement, not
        // custody, so replay must refuse to certify it as well.
        let unresolved: Invocation = serde_json::from_value(serde_json::json!({
            "ok": false,
            "value": null,
            "error": {"Other": "request timed out"},
            "events": [],
            "stderr": "error: request timed out"
        }))
        .unwrap();
        let findings = check_expectation(
            Policy::Pair,
            Profile::StockVsFork,
            'a',
            &step,
            Some(&unresolved),
            None,
        );
        assert!(
            findings
                .iter()
                .any(|f| format!("{f}").starts_with("inconsistent invocation")),
            "stock's exemption may not certify an unresolved submission: {findings:?}"
        );
        // And the framings the retained corpus actually recorded must stay
        // sanctioned outcomes: a host-level rejection is not unknown inclusion.
        let host_rejected: Invocation = serde_json::from_value(serde_json::json!({
            "ok": false,
            "value": null,
            "error": {
                "Other":
                    "transaction simulation failed: HostError: Error(WasmVm, InvalidAction)"
            },
            "events": [],
            "stderr":
                "❌ error: transaction simulation failed: HostError: Error(WasmVm, InvalidAction)"
        }))
        .unwrap();
        assert!(
            check_expectation(
                Policy::Pair,
                Profile::StockVsFork,
                'a',
                &step,
                Some(&host_rejected),
                None,
            )
            .is_empty(),
            "a sanctioned host rejection must stay an outcome"
        );
    }

    #[test]
    fn prospective_cross_family_scope_is_rejected_before_any_mutation() {
        let m = manifest();
        let out = out_dir("scope-cross-family");
        let fx1 = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st1 = Stellar::new(&m.net, &fx1, Policy::Pair);
        let mut r1 = Runner::new(&st1, &m, &out).unwrap();
        r1.hook = Some(&stop_hook);
        r1.run(Some("statuses"), None).unwrap();
        let state_before = std::fs::read(out.join("state.json")).unwrap();

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let mut r2 = Runner::new(&st2, &m, &out).unwrap();
        let err = r2.run(Some("strict_oracle"), None).unwrap_err().to_string();
        assert!(err.contains("isolated family"), "{err}");
        assert!(
            fx2.mutation_calls().is_empty(),
            "an isolated deployment must not be contaminated to learn the scope is illegal"
        );
        assert_eq!(
            std::fs::read(out.join("state.json")).unwrap(),
            state_before,
            "a rejected scope change may not rewrite the requested campaign"
        );
    }

    #[test]
    fn backwards_scope_extension_is_rejected_before_preflight() {
        let m = manifest();
        let out = out_dir("scope-backwards");
        let fx1 = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st1 = Stellar::new(&m.net, &fx1, Policy::Pair);
        let mut r1 = Runner::new(&st1, &m, &out).unwrap();
        r1.hook = Some(&stop_hook);
        r1.run(Some("statuses"), None).unwrap();

        let fx2 = CustodyExecutor::new(vec![], false);
        let st2 = Stellar::new(&m.net, &fx2, Policy::Pair);
        let mut r2 = Runner::new(&st2, &m, &out).unwrap();
        let err = r2.run(Some("lending"), None).unwrap_err().to_string();
        assert!(err.contains("canonical scenario order"), "{err}");
        assert!(
            fx2.mutation_calls().is_empty(),
            "lending may not be driven after statuses"
        );
    }

    /// The prerequisite law is a property of the cumulative campaign, so an
    /// invocation that is legal alone still cannot complete an incomplete scope.
    #[test]
    fn cumulative_scope_cannot_evade_loss_prerequisites() {
        let m = manifest();
        let out = out_dir("scope-prerequisites");
        let mut state = RunState::default();
        state.scenario_order.push("direct_default".into());
        state.requested.insert(
            "direct_default".into(),
            requested_labels(&scenario("direct_default")),
        );
        state.save(&out).unwrap();

        let fx = CustodyExecutor::new(vec![], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        let err = runner
            .run(Some("liquidation"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("prerequisites"), "{err}");
        assert!(
            fx.mutation_calls().is_empty(),
            "an incomplete cumulative scope may not mutate"
        );
    }

    /// An operator action taken at the completed step's gate must return to
    /// that same decision, never to the canonical step: re-entering execution
    /// would collide with the immutable-evidence guard and surface a fatal
    /// halt for work that already succeeded and was recorded once.
    static POST_STEP_DONE: AtomicU32 = AtomicU32::new(0);
    static POST_STEP_STARTED: AtomicU32 = AtomicU32::new(0);

    fn post_step_control(e: Ev) -> Act {
        match e {
            Ev::StepStarted { label, .. } => {
                POST_STEP_STARTED.fetch_add(1, Ordering::SeqCst);
                if label == "status_update_back_to_active" {
                    Act::Abort
                } else {
                    Act::Continue
                }
            }
            Ev::StepDone { evidence, .. } if evidence.scenario == OPERATOR_SCENARIO => {
                Act::Continue
            }
            Ev::StepDone { .. } => match POST_STEP_DONE.fetch_add(1, Ordering::SeqCst) {
                0 => Act::Snapshot,
                1 => Act::Exec {
                    step: Box::new(LabeledStep {
                        label: "post_gate_wait",
                        expect: Expect::Any,
                        step: Step::WaitSeconds(0),
                        delta_dependent: false,
                    }),
                    sides: Sides::Both,
                },
                _ => Act::Continue,
            },
            Ev::Halt { .. } => Act::Abort,
            _ => Act::Continue,
        }
    }

    #[test]
    fn operator_action_after_a_completed_step_returns_to_the_gate_not_execution() {
        POST_STEP_DONE.store(0, Ordering::SeqCst);
        POST_STEP_STARTED.store(0, Ordering::SeqCst);
        let m = manifest();
        let out = out_dir("post-step-gate");
        let fx = CustodyExecutor::new(vec![cmd_ok("", "A"), cmd_ok("", "B")], false);
        let st = Stellar::new(&m.net, &fx, Policy::Pair);
        let mut runner = Runner::new(&st, &m, &out).unwrap();
        runner.hook = Some(&post_step_control);
        runner.run(Some("statuses"), None).unwrap();

        // The canonical step mutated exactly once per side across both
        // operator actions: the decision loop never re-drove it.
        assert_eq!(
            fx.mutation_calls().len(),
            2,
            "operator actions at the completed-step gate may not re-execute the step: {:?}",
            fx.mutation_calls()
        );
        // Canonical evidence exists, once, unmodified — and the operator
        // snapshot and ad-hoc step are recorded separately under their own
        // disclosed namespace.
        assert!(evidence_step_dir(&out, "statuses", 0, "status_admin_on_ice").exists());
        let op = out.join("evidence").join(OPERATOR_SCENARIO);
        assert!(
            op.join("001-operator_001_snapshot").exists(),
            "the snapshot must land as operator evidence"
        );
        assert!(
            op.join("002-post_gate_wait").exists(),
            "the ad-hoc step must land as operator evidence"
        );
        // Both actions were honoured at the gate (two re-presentations plus
        // the continuation), the canonical step was entered exactly once, and
        // no spurious fatal was produced by the collision the old consuming
        // loop caused.
        assert_eq!(POST_STEP_DONE.load(Ordering::SeqCst), 3);
        assert_eq!(
            POST_STEP_STARTED.load(Ordering::SeqCst),
            2,
            "only the canonical step and the step where the test aborts may start"
        );
        assert!(runner.fatal.is_none(), "spurious fatal: {:?}", runner.fatal);
        let steps = crate::report::load_steps(&out, &m).unwrap();
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.label == "status_admin_on_ice")
                .count(),
            1,
            "the canonical step is recorded exactly once"
        );
    }
}
