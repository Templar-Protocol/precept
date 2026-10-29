//! TUI state. `App::apply` is a pure transition over `Ev`, which is what makes the
//! whole interface testable without a terminal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use serde_json::Value;

use crate::manifest::{PairManifest, RunState, Tok};
use crate::observe::map_entries;
use crate::report::{analyze, load_step_dir, Row, StepEvidence};
use crate::run::{Act, Ev};
use crate::scenario::scenarios;
use crate::stellar::{Policy, Safe};
use crate::timeline::{bar, gauges, pct, spark, uncapped, Custody, Timeline, RATE_DP};

use super::palette::Palette;

// --- State ---

pub const STREAM_CAP: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Help,
    Info,
    Palette,
    Jump,
    Search,
    Confirm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Invariants,
    Diff,
    Snapshot,
    Stream,
    Flow,
    Rates,
    Traps,
    Health,
}

impl Tab {
    pub const ALL: [Tab; 8] = [
        Tab::Invariants,
        Tab::Diff,
        Tab::Snapshot,
        Tab::Stream,
        Tab::Flow,
        Tab::Rates,
        Tab::Traps,
        Tab::Health,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Invariants => "INVARIANTS",
            Tab::Diff => "DIFF",
            Tab::Snapshot => "SNAPSHOT",
            Tab::Stream => "STREAM",
            Tab::Flow => "FLOW",
            Tab::Rates => "RATES",
            Tab::Traps => "TRAPS",
            Tab::Health => "HEALTH",
        }
    }
    /// The run-level tabs read the whole timeline, not the selected step, so they still
    /// have something to say before any step is selected.
    pub fn run_level(self) -> bool {
        matches!(self, Tab::Flow | Tab::Rates | Tab::Traps)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunStatus {
    Idle,
    Running,
    Paused,
    Retrying { n: u32, why: String },
    Halted(String),
    Finished(String),
    Watching,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Clone, Debug)]
pub struct PlanStep {
    pub label: String,
    pub status: StepStatus,
}

#[derive(Clone, Debug)]
pub struct PlanScenario {
    pub name: String,
    pub steps: Vec<PlanStep>,
}

/// What the confirmation overlay will do when accepted.
#[derive(Clone, Debug)]
pub enum Pending {
    Quit,
    Rerun(Box<crate::scenario::LabeledStep>),
    /// A palette-built step awaiting the confirm overlay, sides pinned at build.
    ExecOp {
        step: Box<crate::scenario::LabeledStep>,
        sides: crate::run::Sides,
    },
    ForceLock,
}

#[derive(Clone, Debug)]
pub struct TreeRow {
    pub indent: usize,
    pub key: String,
    pub value: String,
    /// Diff classification, when the row comes from a diff.
    pub class: Option<crate::compare::Class>,
    pub expandable: bool,
    pub path: String,
}

/// Small row builder for the run-level tabs. `class` is the existing colour channel:
/// `Match` is green, `Expected` yellow, `Violation` red, which is exactly the
/// pass / info / fail palette these views need.
struct Text {
    rows: Vec<TreeRow>,
    width: usize,
}

impl Text {
    fn new(width: usize) -> Self {
        Self {
            rows: Vec::new(),
            width: width.max(40),
        }
    }
    fn raw(&mut self, indent: usize, value: String, class: Option<crate::compare::Class>) {
        self.rows.push(TreeRow {
            indent,
            key: String::new(),
            value,
            class,
            expandable: false,
            path: String::new(),
        });
    }
    fn head(&mut self, value: String) {
        self.raw(0, value, None);
    }
    fn line(&mut self, indent: usize, value: String) {
        for l in wrap(&value, self.width.saturating_sub(indent * 2 + 2)) {
            self.raw(indent, l, None);
        }
    }
    /// A judged line: the tick or cross carries the verdict, the colour repeats it.
    fn note(&mut self, indent: usize, ok: bool, text: String) {
        use crate::compare::Class;
        let class = Some(if ok {
            Class::Match
        } else {
            Class::Violation(String::new())
        });
        let mark = if ok { "✓" } else { "✖" };
        for (n, l) in wrap(&text, self.width.saturating_sub(indent * 2 + 4))
            .into_iter()
            .enumerate()
        {
            let v = if n == 0 {
                format!("{mark} {l}")
            } else {
                format!("  {l}")
            };
            self.raw(indent, v, class.clone());
        }
    }
    /// Context, not a verdict: no tick, no colour.
    fn dim(&mut self, indent: usize, text: String) {
        use crate::compare::Class;
        for l in wrap(&text, self.width.saturating_sub(indent * 2 + 2)) {
            self.raw(indent, l, Some(Class::Expected(String::new())));
        }
    }
    fn gauge(&mut self, indent: usize, name: &str, bar: &str, tail: &str, ok: bool) {
        use crate::compare::Class;
        let class = Some(if ok {
            Class::Match
        } else {
            Class::Violation(String::new())
        });
        self.raw(indent, format!("{name:<6}{bar}  {tail}"), class);
    }
}

type LoadedSignature = (
    (std::time::SystemTime, u64),
    (std::time::SystemTime, u64),
    bool,
    bool,
);

pub struct App {
    pub out: PathBuf,
    pub manifest: PairManifest,
    pub plan: Vec<PlanScenario>,
    /// Canonical requested scope and completion claims from `state.json`: the
    /// only denominator progress may use. `None` means the scope is unknown.
    scope: Option<RunState>,
    pub steps: Vec<StepEvidence>,
    pub ledger: Vec<Row>,
    /// The authoritative run verdict, from the same analysis `report` writes.
    pub verdict: String,
    /// Custody reasons blocking certification; empty on a certifiable run.
    pub integrity: Vec<String>,
    /// Current-poll failures to read evidence; cleared by a fully readable poll.
    pub watch_read_errors: Vec<String>,
    /// Everything that needs more than one step to see: flows, rates, traps, orphans.
    pub timeline: Timeline,
    pub stream: Vec<String>,
    pub run: RunStatus,
    pub mode: Mode,
    pub tab: Tab,
    pub status_msg: String,
    /// Last unknown-label set announced in `status_msg`, so a steady state does
    /// not overwrite the operator's own feedback on every adopted step.
    last_unknown: String,
    /// Operator actions sent but not yet consumed by a gate: while nonzero the
    /// decision keys must stay locked, or a second press queues an act the
    /// NEXT gate silently consumes.
    pub pending_acts: usize,
    /// Whether the oldest queued act was sent while the worker ran mid-step.
    /// Such an act outlives the current moment and answers the NEXT gate, so
    /// the automatic Continue must not queue behind it: a spare Continue that
    /// reaches an `Ev::Halt` gate would run the runner straight past the halt.
    /// An act sent while the worker waits at the announced gate is consumed
    /// there and needs no such reservation.
    pub pending_midstep: bool,
    /// The worker is believed blocked at the last announced gate. Set by gate
    /// events, cleared by any send.
    pub worker_at_gate: bool,
    pub selected: usize,
    pub detail_sel: usize,
    pub follow: bool,
    pub diff_only: bool,
    pub side: char,
    pub auto_continue: bool,
    pub expanded: BTreeSet<String>,
    pub search: String,
    pub palette: Palette,
    pub jump_sel: usize,
    pub pending: Option<Pending>,
    pub waiting: Option<String>,
    /// `None` in watch mode: the run is owned by another process.
    pub drive: Option<Sender<Act>>,
    /// Execution/render policy fixed at construction. Every pair run is
    /// `Policy::Pair`; the error renderings below route through it and
    /// reproduce pair bytes verbatim.
    pub policy: Policy,
    /// Caches a/b metadata plus incoming replay context; diff.json is derived and ignored by
    /// the report reader.
    loaded: BTreeMap<String, LoadedSignature>,
    /// Consecutive transient CLI failures, for the retry counter.
    transient: u32,
    /// Width of the detail panel, published by the renderer so text wraps to fit.
    pub stream_width: usize,
    /// Scroll offsets, kept across frames so the view does not jump.
    pub plan_offset: usize,
    pub detail_offset: usize,
    pub strip_offset: usize,
}

impl App {
    pub fn new(out: PathBuf, manifest: PairManifest, drive: Option<Sender<Act>>) -> Self {
        // The catalog is the scaffold for browsing when no scope is recorded.
        // Once a canonical requested scope exists, `refresh_scope` narrows the
        // plan to it; progress itself never counts these rows.
        let plan = scenarios()
            .into_iter()
            .map(|sc| PlanScenario {
                name: sc.name.to_string(),
                steps: sc
                    .steps
                    .iter()
                    .map(|s| s.label.to_string())
                    .chain(crate::scenario::traps().iter().map(|s| s.label.to_string()))
                    .map(|label| PlanStep {
                        label,
                        status: StepStatus::Pending,
                    })
                    .collect(),
            })
            .collect();
        let watching = drive.is_none();
        // Built empty rather than defaulted: the trap matrix shows as "not run"
        // before a single step exists, which is itself the honest verdict.
        let timeline = Timeline::build(&[], &manifest, Policy::Pair);
        let mut app = Self {
            out,
            manifest,
            plan,
            scope: None,
            steps: Vec::new(),
            ledger: Vec::new(),
            verdict: String::new(),
            integrity: Vec::new(),
            watch_read_errors: Vec::new(),
            timeline,
            stream: Vec::new(),
            run: if watching {
                RunStatus::Watching
            } else {
                RunStatus::Idle
            },
            mode: Mode::Normal,
            tab: Tab::Invariants,
            status_msg: String::new(),
            last_unknown: String::new(),
            pending_acts: 0,
            pending_midstep: false,
            worker_at_gate: false,
            selected: 0,
            detail_sel: 0,
            follow: true,
            diff_only: false,
            side: 'b',
            auto_continue: false,
            expanded: BTreeSet::new(),
            search: String::new(),
            palette: Palette::default(),
            jump_sel: 0,
            pending: None,
            waiting: None,
            drive,
            policy: Policy::Pair,
            loaded: BTreeMap::new(),
            transient: 0,
            stream_width: 100,
            plan_offset: 0,
            detail_offset: 0,
            strip_offset: 0,
        };
        app.refresh_scope();
        app
    }

    /// Completed steps out of the recorded requested scope, and where the run
    /// currently is. `(0, 0)` means the scope is unknown: there is no honest
    /// denominator, and the catalog never stands in for one.
    pub fn progress(&self) -> (usize, usize, String) {
        let (done, total) = self
            .plan
            .iter()
            .filter_map(|s| self.scenario_progress(&s.name))
            .fold((0, 0), |(done, total), (d, t)| (done + d, total + t));
        let here = self
            .plan
            .iter()
            .find_map(|sc| {
                sc.steps
                    .iter()
                    .find(|s| s.status == StepStatus::Running)
                    .map(|s| format!("{} · {}", sc.name, s.label))
            })
            .or_else(|| {
                self.selected_step()
                    .map(|s| format!("{} · {}", s.scenario, s.label))
            })
            .unwrap_or_else(|| "not started".into());
        (done, total, here)
    }

    pub fn scenario_progress(&self, scenario: &str) -> Option<(usize, usize)> {
        let labels = self.scope.as_ref()?.requested.get(scenario)?;
        Some((
            labels
                .iter()
                .filter(|label| self.step_completed(scenario, label))
                .count(),
            labels.len(),
        ))
    }

    fn step_completed(&self, scenario: &str, label: &str) -> bool {
        let Some(completed) = self
            .scope
            .as_ref()
            .and_then(|st| st.completed.get(scenario))
        else {
            return false;
        };
        if completed.iter().filter(|l| l.as_str() == label).count() != 1 {
            return false;
        }
        let evidence = self
            .steps
            .iter()
            .filter(|s| s.scenario == scenario && s.label == label)
            .count();
        let wait = matches!(
            crate::scenario::current_step(label).map(|s| &s.step),
            Some(
                crate::scenario::Step::WaitSeconds(_)
                    | crate::scenario::Step::WaitLedgersSince { .. }
            )
        );
        evidence == usize::from(!wait)
    }

    fn refresh_scope(&mut self) {
        let canonical = crate::report::canonical_scope();
        self.scope = RunState::load(&self.out).ok().filter(|st| {
            let names: Vec<&str> = st.requested.keys().map(String::as_str).collect();
            !names.is_empty()
                && crate::scenario::family_of(&names).is_some()
                && crate::scenario::loss_prerequisites_ok(&names)
                && st
                    .requested
                    .iter()
                    .all(|(name, labels)| canonical.get(name) == Some(labels))
        });
        let Some(state) = &self.scope else { return };
        // The requested scope *is* the plan. Evidence outside it stays: a
        // label that exists on disk but not in the recorded scope (a renamed
        // or legacy step) must remain navigable, and an observed scenario
        // outside the scope (e.g. `operator`) is disclosed, not hidden.
        // Unevidenced catalog scaffold is dropped — it is not this run.
        let mut observed: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for s in &self.steps {
            observed
                .entry(s.scenario.as_str())
                .or_default()
                .insert(s.label.as_str());
        }
        let previous = std::mem::take(&mut self.plan);
        let mut plan: Vec<PlanScenario> = Vec::new();
        for scenario in scenarios() {
            let Some(labels) = state.requested.get(scenario.name) else {
                continue;
            };
            let old_sc = previous.iter().find(|s| s.name == scenario.name);
            let mut steps: Vec<PlanStep> = labels
                .iter()
                .map(|label| {
                    let old = old_sc
                        .and_then(|s| s.steps.iter().find(|x| &x.label == label))
                        .map(|s| s.status);
                    let status = if self.step_completed(scenario.name, label) {
                        if self
                            .steps
                            .iter()
                            .any(|s| s.scenario == scenario.name && &s.label == label)
                        {
                            self.step_status_after_analysis(scenario.name, label)
                        } else {
                            StepStatus::Done
                        }
                    } else if matches!(
                        old,
                        Some(StepStatus::Running | StepStatus::Skipped | StepStatus::Failed)
                    ) {
                        old.unwrap()
                    } else {
                        StepStatus::Pending
                    };
                    PlanStep {
                        label: label.clone(),
                        status,
                    }
                })
                .collect();
            for label in observed
                .get(scenario.name)
                .cloned()
                .unwrap_or_default()
                .difference(&labels.iter().map(String::as_str).collect())
            {
                steps.push(PlanStep {
                    label: (*label).to_string(),
                    status: self.step_status_after_analysis(scenario.name, label),
                });
            }
            plan.push(PlanScenario {
                name: scenario.name.into(),
                steps,
            });
        }
        plan.extend(previous.into_iter().filter(|s| {
            !state.requested.contains_key(&s.name) && observed.contains_key(s.name.as_str())
        }));
        self.plan = plan;
    }

    pub fn watching(&self) -> bool {
        self.drive.is_none()
    }

    fn set_step_status(&mut self, scenario: &str, label: &str, status: StepStatus) {
        if let Some(sc) = self.plan.iter_mut().find(|s| s.name == scenario) {
            if let Some(st) = sc.steps.iter_mut().find(|s| s.label == label) {
                st.status = status;
            }
        }
    }

    /// The plan status a step earns from the authoritative analysis: stored
    /// violations are evidence, acceptance comes from the recomputed copy
    /// `analyze` left in `self.steps` — findings, stored violations, and the
    /// re-classified diff must all be clean.
    fn step_status_after_analysis(&self, scenario: &str, label: &str) -> StepStatus {
        match self
            .steps
            .iter()
            .find(|s| s.scenario == scenario && s.label == label)
        {
            Some(s) => {
                let failed = !s.findings.is_empty()
                    || !s.a.violations.is_empty()
                    || !s.b.violations.is_empty()
                    || s.diff
                        .iter()
                        .any(|(_, c)| matches!(c, crate::compare::Class::Violation(_)));
                if failed {
                    StepStatus::Failed
                } else {
                    StepStatus::Done
                }
            }
            None => StepStatus::Failed,
        }
    }

    fn push_stream(&mut self, line: String) {
        self.stream.push(line);
        if self.stream.len() > STREAM_CAP {
            let drop = self.stream.len() - STREAM_CAP;
            self.stream.drain(..drop);
        }
    }

    fn refresh_ledger(&mut self) -> i32 {
        // Same authoritative analysis the `report` command runs: recomputed
        // findings, scope-checked ledger rows, timeline failures. The TUI
        // must never show a ledger the report contradicts.
        let a = analyze(&self.out, &self.manifest, &self.steps);
        let verdict_code = crate::report::verdict_code(&a.verdict);
        self.steps = a.steps;
        self.ledger = a.ledger;
        self.verdict = a.verdict;
        self.integrity = a.integrity;
        self.refresh_scope();
        // Steps are already sorted into plan order above; the timeline depends on that,
        // because a delta taken against the wrong predecessor is worse than no delta.
        self.timeline = Timeline::build(&self.steps, &self.manifest, Policy::Pair);
        let unknown = self.timeline.unknown_labels.join(", ");
        if !unknown.is_empty() && unknown != self.last_unknown {
            self.last_unknown = unknown.clone();
            self.status_msg = format!("labels without declared expectations: {unknown}");
        }
        verdict_code
    }

    /// Make sure the plan has a row for this step. Evidence on disk is the truth: a run
    /// recorded before a scenario was renamed still has to be navigable.
    fn ensure_in_plan(&mut self, scenario: &str, label: &str) {
        let sc = match self.plan.iter_mut().position(|s| s.name == scenario) {
            Some(i) => &mut self.plan[i],
            None => {
                self.plan.push(PlanScenario {
                    name: scenario.to_string(),
                    steps: crate::report::canonical_scope()
                        .remove(scenario)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|label| PlanStep {
                            label,
                            status: StepStatus::Pending,
                        })
                        .collect(),
                });
                self.plan.last_mut().expect("just pushed")
            }
        };
        if !sc.steps.iter().any(|s| s.label == label) {
            sc.steps.push(PlanStep {
                label: label.to_string(),
                status: StepStatus::Pending,
            });
        }
        let order = scenarios();
        self.plan.sort_by_key(|s| {
            order
                .iter()
                .position(|sc| sc.name == s.name)
                .unwrap_or(usize::MAX)
        });
    }

    /// Position of a step in the plan, which is the order the run executes in. Evidence
    /// arrives in directory order when loaded from disk, so without this the step list
    /// and the plan pane disagree and stepping through jumps around.
    fn plan_position(&self, scenario: &str, label: &str) -> (usize, usize) {
        match self.plan.iter().position(|s| s.name == scenario) {
            Some(i) => (
                i,
                self.plan[i]
                    .steps
                    .iter()
                    .position(|s| s.label == label)
                    .unwrap_or(usize::MAX),
            ),
            None => (usize::MAX, usize::MAX),
        }
    }

    fn adopt(&mut self, ev: StepEvidence) {
        self.ensure_in_plan(&ev.scenario, &ev.label);
        // keep the selection on the same step across the re-sort below
        let anchor = self
            .selected_step()
            .map(|s| (s.scenario.clone(), s.label.clone()));
        match self
            .steps
            .iter_mut()
            .find(|s| s.label == ev.label && s.scenario == ev.scenario)
        {
            Some(slot) => *slot = ev,
            None => self.steps.push(ev),
        }
        let order: Vec<(usize, usize)> = self
            .steps
            .iter()
            .map(|s| self.plan_position(&s.scenario, &s.label))
            .collect();
        let mut idx: Vec<usize> = (0..self.steps.len()).collect();
        idx.sort_by_key(|i| order[*i]);
        let mut sorted: Vec<Option<StepEvidence>> = self.steps.drain(..).map(Some).collect();
        self.steps = idx
            .into_iter()
            .map(|i| sorted[i].take().expect("each index used once"))
            .collect();

        self.refresh_ledger();
        if self.follow {
            self.selected = self.steps.len().saturating_sub(1);
            self.detail_sel = 0;
        } else if let Some((sc, l)) = anchor {
            if let Some(i) = self
                .steps
                .iter()
                .position(|s| s.scenario == sc && s.label == l)
            {
                self.selected = i;
            }
        }
    }

    /// Pure state transition. Every panel is a function of what this records.
    pub fn apply(&mut self, ev: Ev) {
        // The worker blocks exactly at the events `is_gate` names, so a gate
        // event both locates the worker and witnesses the consumption of one
        // queued action.
        self.worker_at_gate = crate::tui::is_gate(&ev);
        if self.worker_at_gate && self.pending_acts > 0 {
            self.pending_acts -= 1;
            self.pending_midstep = false;
        }
        match ev {
            Ev::RunStarted { plan, resumed } => {
                self.plan = plan
                    .into_iter()
                    .map(|(name, labels)| PlanScenario {
                        name,
                        steps: labels
                            .into_iter()
                            .map(|label| PlanStep {
                                label,
                                status: StepStatus::Pending,
                            })
                            .collect(),
                    })
                    .collect();
                // The runner has just persisted the canonical requested scope, so
                // the plan and progress follow the recorded authority, not this
                // event's convenience copy.
                self.refresh_scope();
                self.run = RunStatus::Running;
                self.status_msg = format!("run started ({resumed} steps already complete)");
            }
            Ev::StepStarted {
                scenario, label, ..
            } => {
                self.waiting = None;
                self.ensure_in_plan(&scenario, &label);
                self.set_step_status(&scenario, &label, StepStatus::Running);
                if !matches!(self.run, RunStatus::Paused | RunStatus::Halted(_)) {
                    self.run = RunStatus::Running;
                }
            }
            Ev::SideDone { tag, ok, err } => {
                if !ok {
                    let why = err.map(|e| e.to_string()).unwrap_or_else(|| "?".into());
                    self.push_stream(format!("side {tag} failed: {why}"));
                }
            }
            Ev::Waiting {
                label,
                now,
                target,
                secs_left,
            } => {
                self.waiting = Some(if target > 0 {
                    format!("{label}: ledger {now}/{target}, ~{secs_left}s")
                } else {
                    format!("{label}: ~{secs_left}s")
                });
            }
            Ev::StepDone {
                evidence,
                violations,
                desynced,
            } => {
                self.waiting = None;
                self.transient = 0;
                let (scenario, label) = (evidence.scenario.clone(), evidence.label.clone());
                let line = format!(
                    "{scenario}/{label} violations={violations}{}",
                    if desynced { " desynced" } else { "" }
                );
                self.adopt(*evidence);
                // The plan tick is the authoritative verdict: the runner's
                // pre-recompute count can only escalate it, never clear a
                // finding the replay derives from the same evidence.
                let status = if violations > 0 {
                    StepStatus::Failed
                } else {
                    self.step_status_after_analysis(&scenario, &label)
                };
                self.set_step_status(&scenario, &label, status);
                self.push_stream(line);
                if status == StepStatus::Failed && !self.auto_continue {
                    self.run = RunStatus::Halted(if violations > 0 {
                        format!("{violations} violation(s)")
                    } else {
                        "replay findings".into()
                    });
                } else if !matches!(self.run, RunStatus::Paused) {
                    self.run = RunStatus::Running;
                }
            }
            Ev::StepSkipped {
                scenario,
                label,
                why,
            } => {
                self.set_step_status(&scenario, &label, StepStatus::Skipped);
                if why != "already completed" {
                    self.push_stream(format!("{scenario}/{label} skipped: {why}"));
                }
            }
            Ev::Halt {
                scenario,
                label,
                reason,
            } => {
                self.waiting = None;
                self.set_step_status(&scenario, &label, StepStatus::Failed);
                self.push_stream(format!("HALT {scenario}/{label}: {reason}"));
                self.run = RunStatus::Halted(reason.to_string());
            }
            Ev::Cli {
                argv,
                ok,
                ms,
                stderr,
                transient,
            } => {
                if ok {
                    self.transient = 0;
                    self.push_stream(format!("⟩ {argv} → ok {ms}ms"));
                } else {
                    // The transient verdict is pre-computed at the executor
                    // from `CmdOutput::transient()` — the single
                    // policy-independent law in `stellar.rs`.
                    if transient {
                        self.transient += 1;
                        // Mirrored CLI chatter must not overwrite a decision
                        // state the operator is looking at.
                        if !matches!(self.run, RunStatus::Paused | RunStatus::Halted(_)) {
                            self.run = RunStatus::Retrying {
                                n: self.transient,
                                why: first_line(&stderr),
                            };
                        }
                    }
                    self.push_stream(format!("✖ {argv} → {} ({ms}ms)", first_line(&stderr)));
                }
            }
            Ev::RunFinished { error } => {
                self.waiting = None;
                self.run = RunStatus::Finished(
                    error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "complete".into()),
                );
            }
        }
    }

    /// Send an operator action; inert while watching.
    pub fn send(&mut self, act: Act) {
        match &self.drive {
            Some(tx) => {
                if tx.send(act).is_err() {
                    self.status_msg = "runner is gone".into();
                } else {
                    // An act sent while the worker runs mid-step outlives the
                    // current moment and answers the NEXT gate; one sent while
                    // the worker waits at the announced gate is consumed there.
                    self.pending_midstep = !self.worker_at_gate;
                    self.pending_acts += 1;
                    self.worker_at_gate = false;
                }
            }
            None => {
                self.status_msg = match crate::manifest::RunLock::owner(&self.out) {
                    Some(pid) => format!("read-only: pid {pid} owns this run"),
                    None => "read-only: opened for browsing".into(),
                }
            }
        }
    }

    /// Watch mode: reconcile cached evidence against the current authoritative inputs.
    pub fn poll_disk(&mut self) -> anyhow::Result<()> {
        let root = self.out.join("evidence");
        let selected = self
            .selected_step()
            .map(|s| (s.scenario.clone(), s.label.clone()));
        let following = self.follow;
        let mut errors = Vec::new();
        let mut observed = BTreeSet::new();
        let mut failed_scenarios = BTreeSet::new();
        let mut pending: Vec<(String, PathBuf)> = Vec::new();
        let root_entries = match std::fs::read_dir(&root) {
            Ok(entries) => Some(entries),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.evict_all_loaded();
                None
            }
            Err(e) => {
                self.evict_all_loaded();
                errors.push(format!("cannot scan evidence root {}: {e}", root.display()));
                None
            }
        };
        let mut root_scan_failed = false;
        if let Some(root_entries) = root_entries {
            for scenario in root_entries {
                let scenario = match scenario {
                    Ok(scenario) => scenario,
                    Err(e) => {
                        errors.push(format!(
                            "cannot enumerate evidence root {}: {e}",
                            root.display()
                        ));
                        root_scan_failed = true;
                        continue;
                    }
                };
                let sname = scenario.file_name().to_string_lossy().to_string();
                observed.insert(sname.clone());
                let dirs = match std::fs::read_dir(scenario.path()) {
                    Ok(dirs) => dirs,
                    Err(e) => {
                        errors.push(format!(
                            "cannot scan evidence scenario {}: {e}",
                            scenario.path().display()
                        ));
                        failed_scenarios.insert(sname);
                        continue;
                    }
                };
                let mut entries = Vec::new();
                let mut scan_error = None;
                for entry in dirs {
                    match entry {
                        Ok(entry) => entries.push(entry.path()),
                        Err(e) => scan_error = Some(e),
                    }
                }
                if let Some(e) = scan_error {
                    errors.push(format!(
                        "cannot enumerate evidence scenario {}: {e}",
                        scenario.path().display()
                    ));
                    failed_scenarios.insert(sname);
                    continue;
                }
                pending.extend(entries.into_iter().map(|path| (sname.clone(), path)));
            }
        }
        if root_scan_failed {
            self.evict_all_loaded();
        }
        let present: BTreeSet<String> = pending
            .iter()
            .map(|(scenario, path)| {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("{scenario}/{name}")
            })
            .collect();
        let stale: Vec<String> = self
            .loaded
            .keys()
            .filter(|key| {
                let scenario = key.split_once('/').map_or(key.as_str(), |(s, _)| s);
                failed_scenarios.contains(scenario) || !present.contains(*key)
            })
            .cloned()
            .collect();
        for key in stale {
            self.evict_loaded_step(&key);
        }

        let delta: BTreeMap<String, bool> = scenarios()
            .into_iter()
            .flat_map(|s| {
                s.steps
                    .into_iter()
                    .map(|st| (st.label.to_string(), st.delta_dependent))
            })
            .collect();
        let order = crate::report::scenario_replay_order(&self.out, &observed);
        let rank = |n: &str| order.iter().position(|o| o == n).unwrap_or(usize::MAX);
        pending.sort_by(|x, y| {
            (rank(&x.0), x.0.as_str(), x.1.file_name()).cmp(&(
                rank(&y.0),
                y.0.as_str(),
                y.1.file_name(),
            ))
        });
        // Replaying every current step in report order preserves the shared
        // delta/custody context; unchanged a/b pairs still contribute it.
        let mut pool_loss_seen = false;
        let mut freeze_split_seen = false;
        let mut adopted_any = false;
        for (sname, path) in pending {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = name
                .split_once('-')
                .map(|x| x.1.to_string())
                .unwrap_or_default();
            let key = format!("{sname}/{name}");
            let a_path = path.join("a.json");
            let b_path = path.join("b.json");
            let a_sig = disk_file_signature(&a_path);
            let b_sig = disk_file_signature(&b_path);
            let sig = match (a_sig, b_sig) {
                (Ok(a), Ok(b)) => (a, b, pool_loss_seen, freeze_split_seen),
                (a, b) => {
                    self.evict_loaded_step(&key);
                    for (input, result) in [(a_path, a), (b_path, b)] {
                        if let Err(e) = result {
                            errors.push(format!(
                                "cannot read authoritative evidence {}: {e}",
                                input.display()
                            ));
                        }
                    }
                    continue;
                }
            };
            if self.loaded.get(&key) == Some(&sig) {
                if let Some(s) = self
                    .steps
                    .iter()
                    .find(|s| s.scenario == sname && s.label == label)
                {
                    if sname != crate::run::OPERATOR_SCENARIO {
                        pool_loss_seen |= crate::timeline::emits_pool_loss(&s.a, &self.manifest.a)
                            || crate::timeline::emits_pool_loss(&s.b, &self.manifest.b);
                    }
                    if label == crate::scenario::FREEZE_DOWNGRADE_PROBE {
                        freeze_split_seen = true;
                    }
                    continue;
                }
            }
            match load_step_dir(
                &path,
                &sname,
                &self.manifest,
                delta.get(label.as_str()).copied().unwrap_or(false),
                pool_loss_seen,
                freeze_split_seen,
            ) {
                Ok(ev) => {
                    if sname != crate::run::OPERATOR_SCENARIO {
                        pool_loss_seen |= crate::timeline::emits_pool_loss(&ev.a, &self.manifest.a)
                            || crate::timeline::emits_pool_loss(&ev.b, &self.manifest.b);
                    }
                    if label == crate::scenario::FREEZE_DOWNGRADE_PROBE {
                        freeze_split_seen = true;
                    }
                    self.loaded.insert(key, sig);
                    let (scenario, label) = (ev.scenario.clone(), ev.label.clone());
                    self.adopt(ev);
                    let status = self.step_status_after_analysis(&scenario, &label);
                    self.set_step_status(&scenario, &label, status);
                    adopted_any = true;
                }
                Err(e) => {
                    self.evict_loaded_step(&key);
                    errors.push(format!(
                        "cannot read current evidence at {}: {e:#}",
                        path.display()
                    ));
                }
            }
        }

        // Custody-only state changes still refresh analysis on every poll.
        let analysis_verdict = self.refresh_ledger();
        self.watch_read_errors = errors
            .into_iter()
            .map(|error| {
                format!(
                    "Evidence unavailable; rereading files: {error:?}; retrying read-only next poll (do not replay or mutate)"
                )
            })
            .collect();
        if !self.watch_read_errors.is_empty() {
            self.integrity
                .extend(self.watch_read_errors.iter().cloned());
            if analysis_verdict == 0 {
                self.verdict = "INCOMPLETE".into();
            }
            let evidence = self.integrity.join("<br>");
            if let Some(row) = self.ledger.iter_mut().find(|r| r.clause == "run custody") {
                row.status = "INCOMPLETE".into();
                row.evidence = evidence;
            }
        }
        if following && adopted_any {
            self.selected = self.steps.len().saturating_sub(1);
            self.detail_sel = 0;
        } else if let Some((scenario, label)) = selected {
            if let Some(index) = self
                .steps
                .iter()
                .position(|s| s.scenario == scenario && s.label == label)
            {
                self.selected = index;
            } else {
                self.selected = self.selected.min(self.steps.len().saturating_sub(1));
                self.detail_sel = 0;
            }
        } else {
            self.selected = self.selected.min(self.steps.len().saturating_sub(1));
            if self.steps.is_empty() {
                self.detail_sel = 0;
            }
        }
        Ok(())
    }

    fn evict_all_loaded(&mut self) {
        // ponytail: bulk scan-error eviction is O(n²) from per-key retains;
        // batch identity indexing if unusually large evidence sets make this hot.
        let keys: Vec<String> = self.loaded.keys().cloned().collect();
        for key in keys {
            self.evict_loaded_step(&key);
        }
    }

    fn evict_loaded_step(&mut self, key: &str) {
        if self.loaded.remove(key).is_none() {
            return;
        }
        let Some((scenario, name)) = key.split_once('/') else {
            return;
        };
        let Some((index, label)) = name.split_once('-') else {
            return;
        };
        let Ok(index) = index.parse::<usize>() else {
            return;
        };
        let had_step = self
            .steps
            .iter()
            .any(|step| step.scenario == scenario && step.index == index && step.label == label);
        self.steps
            .retain(|step| step.scenario != scenario || step.index != index || step.label != label);
        if had_step {
            self.set_step_status(scenario, label, StepStatus::Pending);
        }
    }

    pub fn selected_step(&self) -> Option<&StepEvidence> {
        self.steps.get(self.selected)
    }

    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.follow = false;
        self.detail_sel = 0;
    }

    pub fn select_down(&mut self) {
        if self.selected + 1 < self.steps.len() {
            self.selected += 1;
            self.detail_sel = 0;
            self.follow = false;
        }
    }

    /// Rows for the active tab, already filtered and flattened. Never empty: an empty
    /// panel is indistinguishable from a broken one, so say why instead.
    pub fn tree_rows(&self) -> Vec<TreeRow> {
        let rows = self.tab_rows();
        if !rows.is_empty() {
            return rows;
        }
        let why = match (self.selected_step().is_some(), self.tab) {
            (false, _) => "no step selected yet",
            (true, Tab::Invariants) => "no invariant rows",
            (true, Tab::Diff) if !self.search.is_empty() => "no diff paths match the search",
            (true, Tab::Diff) if self.diff_only => {
                "no differences on this step (d clears the filter)"
            }
            (true, Tab::Diff) => "no differences on this step",
            (true, Tab::Snapshot) => "snapshot is empty",
            (true, Tab::Stream) => "no events on this step",
            (_, Tab::Flow) if self.diff_only => "no loss events yet (d shows every flow)",
            (_, Tab::Flow) => "no supply moved yet",
            (_, Tab::Rates) => "no rate history yet",
            (_, Tab::Traps) => "no trap steps recorded yet",
            (_, Tab::Health) => "no snapshot to read headroom from",
        };
        vec![TreeRow {
            indent: 0,
            key: String::new(),
            value: why.into(),
            class: None,
            expandable: false,
            path: String::new(),
        }]
    }

    fn tab_rows(&self) -> Vec<TreeRow> {
        if self.tab.run_level() {
            return self.run_rows();
        }
        let Some(step) = self.selected_step() else {
            return Vec::new();
        };
        match self.tab {
            Tab::Invariants => {
                let width = self.stream_width.max(40);
                let mut rows = Vec::new();
                let mut row =
                    |indent: usize, key: String, value: String, clause: &str, exp: bool| {
                        rows.push(TreeRow {
                            indent,
                            key,
                            value,
                            class: None,
                            expandable: exp,
                            path: clause.to_string(),
                        })
                    };
                for r in &self.ledger {
                    // verdict first: that is the column you scan
                    row(
                        0,
                        format!("{:<8}", r.status),
                        r.clause.to_string(),
                        r.clause,
                        true,
                    );
                    for l in wrap(r.what, width - 8) {
                        row(1, String::new(), l, r.clause, false);
                    }
                    if self.expanded.contains(r.clause) {
                        for part in r.evidence.split("<br>") {
                            for (n, l) in wrap(part.trim(), width - 10).into_iter().enumerate() {
                                let text = if n == 0 {
                                    format!("· {l}")
                                } else {
                                    format!("  {l}")
                                };
                                row(2, String::new(), text, r.clause, false);
                            }
                        }
                    }
                }
                rows.into_iter()
                    .filter(|x| {
                        self.search.is_empty()
                            || x.key.contains(&self.search)
                            || x.value.contains(&self.search)
                    })
                    .collect()
            }
            Tab::Diff => {
                use crate::compare::Class;
                let names = address_names(&self.manifest);
                let a_json = serde_json::to_value(&step.a.snapshot).unwrap_or(Value::Null);
                let b_json = serde_json::to_value(&step.b.snapshot).unwrap_or(Value::Null);
                let width = self.stream_width.max(40);
                let mut rows = Vec::new();
                let mut group = String::new();
                for (path, class) in &step.diff {
                    if self.diff_only && matches!(class, Class::Match) {
                        continue;
                    }
                    if !self.search.is_empty() && !path.contains(&self.search) {
                        continue;
                    }
                    // section per top-level area, so the list reads in blocks
                    let area = path.split('.').next().unwrap_or(path).to_string();
                    if area != group {
                        group = area.clone();
                        rows.push(TreeRow {
                            indent: 0,
                            key: area.clone(),
                            value: String::new(),
                            class: None,
                            expandable: false,
                            path: area,
                        });
                    }
                    let rest = path
                        .strip_prefix(&format!("{group}."))
                        .unwrap_or(path)
                        .to_string();
                    let dp = decimals_for(path, &self.manifest);
                    // the values each side actually held, scaled, rather than the
                    // comparator's raw message
                    let side = |j: &Value| {
                        value_at(j, path)
                            .map(|v| scval(&v, &names, dp))
                            .unwrap_or_else(|| "-".into())
                    };
                    let (av, bv) = (side(&a_json), side(&b_json));
                    let value = if av == bv {
                        av
                    } else {
                        format!("A {av}   B {bv}")
                    };
                    rows.push(TreeRow {
                        indent: 1,
                        key: rest,
                        value,
                        class: Some(class.clone()),
                        expandable: false,
                        path: path.clone(),
                    });
                    // keep the comparator's note when it says more than the values do
                    if let Class::Expected(m) | Class::Violation(m) = class {
                        if !m.starts_with("a=") {
                            for l in wrap(m, width - 10) {
                                rows.push(TreeRow {
                                    indent: 2,
                                    key: String::new(),
                                    value: l,
                                    class: Some(class.clone()),
                                    expandable: false,
                                    path: path.clone(),
                                });
                            }
                        }
                    }
                }
                rows
            }
            Tab::Snapshot => {
                let rec = if self.side == 'a' { &step.a } else { &step.b };
                let v = serde_json::to_value(&rec.snapshot).unwrap_or(Value::Null);
                let mut out = Vec::new();
                let m = &self.manifest;
                flatten(
                    &v,
                    "",
                    &self.expanded,
                    &self.search,
                    &|p| decimals_for(p, m),
                    &mut out,
                );
                out
            }
            Tab::Flow | Tab::Rates | Tab::Traps => Vec::new(), // handled by run_rows
            Tab::Health => {
                let rec = if self.side == 'a' { &step.a } else { &step.b };
                let mut t = Text::new(self.stream_width);
                t.head(format!(
                    "headroom on side {} at {} · {}",
                    self.side.to_ascii_uppercase(),
                    step.scenario,
                    step.label
                ));
                for g in gauges(&rec.snapshot, &self.manifest) {
                    let width = (self.stream_width.saturating_sub(46)).clamp(8, 24);
                    t.head(g.tok.as_str().to_string());
                    // ADR 0008 r3: max_util stays below 100% and the sealed cap bounds
                    // the reserve. Both are ceilings you want to see approached, not
                    // discover by a 1207 an hour into a run.
                    t.gauge(
                        1,
                        "util",
                        &bar(g.util_frac(), width),
                        &format!("{} of max {}", pct(g.util), pct(g.max_util)),
                        !g.over_util(),
                    );
                    t.gauge(
                        1,
                        "cap",
                        &bar(g.cap_frac(), width),
                        &if uncapped(g.cap) {
                            format!("{}, uncapped", scaled(&g.supply.to_string(), g.decimals))
                        } else {
                            format!(
                                "{} of {} ({})",
                                scaled(&g.supply.to_string(), g.decimals),
                                scaled(&g.cap.to_string(), g.decimals),
                                pct((g.cap_frac() * 1e7) as i128)
                            )
                        },
                        !g.over_cap(),
                    );
                }
                t.head(String::new());
                t.dim(
                    0,
                    "per-user health factor needs oracle prices, which the snapshot does not carry"
                        .into(),
                );
                t.rows
            }
            Tab::Stream => {
                let names = address_names(&self.manifest);
                let width = self.stream_width.max(40);
                let mut rows = Vec::new();
                let mut line = |indent: usize, value: String| {
                    rows.push(TreeRow {
                        indent,
                        key: String::new(),
                        value,
                        class: None,
                        expandable: false,
                        path: String::new(),
                    })
                };
                for (tag, rec) in [("A", &step.a), ("B", &step.b)] {
                    let Some(i) = &rec.invocation else {
                        line(0, format!("{tag} {}  (no call)", rec.side));
                        continue;
                    };
                    let outcome = match (&i.ok, &i.error) {
                        (true, _) => "ok".to_string(),
                        (_, Some(k)) => Safe::error_explained(self.policy, k).to_string(),
                        _ => "failed".into(),
                    };
                    line(0, format!("── {tag} {} ── {outcome}", rec.side));
                    if let Some(v) = &i.value {
                        for (n, l) in wrap(&scval(v, &names, None), width - 6)
                            .into_iter()
                            .enumerate()
                        {
                            line(
                                1,
                                if n == 0 {
                                    format!("→ {l}")
                                } else {
                                    format!("  {l}")
                                },
                            );
                        }
                    }
                    for e in &i.events {
                        // The asset a topic names tells us how to scale the amounts.
                        let dp = e.topics.iter().find_map(|t| {
                            t.get("address")
                                .and_then(|a| a.as_str())
                                .and_then(|a| decimals_of_address(a, &self.manifest))
                        });
                        let mut topics = e.topics.iter().map(|t| scval(t, &names, None));
                        let name = topics.next().unwrap_or_default();
                        let rest: Vec<String> = topics.collect();
                        let head = if rest.is_empty() {
                            format!("● {name}")
                        } else {
                            format!("● {name}  {}", rest.join("  "))
                        };
                        for (n, l) in wrap(&head, width - 6).into_iter().enumerate() {
                            line(1, if n == 0 { l } else { format!("  {l}") });
                        }
                        for l in wrap(&scval(&e.value, &names, dp), width - 10) {
                            line(2, format!("= {l}"));
                        }
                    }
                    if !i.ok {
                        for d in diagnostics(&pair_render(&i.stderr, STREAM_CAP), &names) {
                            for (n, w) in wrap(&d, width - 10).into_iter().enumerate() {
                                line(2, if n == 0 { w } else { format!("  {w}") });
                            }
                        }
                    }
                    for v in &rec.violations {
                        for (n, l) in wrap(&v.to_string(), width - 8).into_iter().enumerate() {
                            line(
                                1,
                                if n == 0 {
                                    format!("! {l}")
                                } else {
                                    format!("  {l}")
                                },
                            );
                        }
                    }
                }
                if !self.stream.is_empty() {
                    line(0, String::new());
                    line(0, format!("── live ── {} lines", self.stream.len()));
                    for l in self.stream.iter().rev().take(200).rev() {
                        for w in wrap(l, width - 6) {
                            line(1, w);
                        }
                    }
                }
                rows.into_iter()
                    .filter(|r| self.search.is_empty() || r.value.contains(&self.search))
                    .collect()
            }
        }
    }

    /// The three views that read the whole run rather than the selected step.
    fn run_rows(&self) -> Vec<TreeRow> {
        let mut t = Text::new(self.stream_width);
        match self.tab {
            Tab::Flow => self.flow_rows(&mut t),
            Tab::Rates => self.rate_rows(&mut t),
            Tab::Traps => self.trap_rows(&mut t),
            _ => {}
        }
        if self.search.is_empty() {
            t.rows
        } else {
            t.rows
                .into_iter()
                .filter(|r| r.value.contains(&self.search))
                .collect()
        }
    }

    /// ADR 0008 §8, rendered: every `b_supply`/`d_supply` move with the event that
    /// explains it and the residual left over. Restriction 7's loss sequence and
    /// restriction 4's orphan removal are the equations being checked.
    fn flow_rows(&self, t: &mut Text) {
        let orphans = &self.timeline.orphans;
        if orphans.iter().any(|o| o.state != Custody::Never)
            || orphans.iter().any(|o| !o.notes.is_empty())
        {
            t.head("ORPHAN CUSTODY (B side)".into());
            for o in orphans {
                let dp = self
                    .manifest
                    .b
                    .reserves
                    .get(&o.tok)
                    .map(|r| r.decimals)
                    .unwrap_or(7);
                let state = match o.state {
                    Custody::Never => "never held".to_string(),
                    Custody::Held => format!(
                        "held {} since {}",
                        scaled(&o.amount.to_string(), dp),
                        o.since.clone().unwrap_or_default()
                    ),
                    Custody::Settled => format!(
                        "settled at {} (peak {})",
                        o.settled_at.clone().unwrap_or_default(),
                        scaled(&o.amount.to_string(), dp)
                    ),
                };
                t.line(1, format!("{:<8}{state}", o.tok.as_str()));
                for n in &o.notes {
                    t.note(2, n.ok, n.text.to_string());
                }
            }
            t.head(String::new());
        }

        t.head("EXPLAINED CHANGE".into());
        let mut group = String::new();
        let mut shown = 0usize;
        for f in &self.timeline.flows {
            // `d` narrows to the steps that actually carry a loss event or a failure,
            // which is the ADR-7 path; the default shows every move.
            if self.diff_only && f.losses.is_empty() && !f.failed() {
                continue;
            }
            let key = format!("{} · {}", f.scenario, f.label);
            if key != group {
                group = key.clone();
                t.raw(0, key, None);
            }
            shown += 1;
            let sm = if f.side == 'a' {
                &self.manifest.a
            } else {
                &self.manifest.b
            };
            let dp = sm.reserves.get(&f.tok).map(|r| r.decimals).unwrap_or(7);
            let d = |v: i128, dp: u32| {
                if v == 0 {
                    "0".to_string()
                } else {
                    let sign = if v > 0 { "+" } else { "-" };
                    format!("{sign}{}", scaled(&v.abs().to_string(), dp))
                }
            };
            t.line(
                1,
                format!(
                    "{:<8}[{}] b {}  d {}  custody {}  b_rate {}",
                    f.tok.as_str(),
                    f.side,
                    d(f.d_b_supply, dp),
                    d(f.d_d_supply, dp),
                    d(f.d_custody, dp),
                    d(f.d_b_rate, RATE_DP)
                ),
            );
            for l in &f.losses {
                t.line(2, format!("● {}", l.name()));
            }
            for n in &f.notes {
                t.note(2, n.ok, n.text.to_string());
            }
        }
        if shown == 0 && !self.timeline.flows.is_empty() {
            t.line(1, "no loss events in this run (d clears the filter)".into());
        }
    }

    /// ADR 0008 r7: interest only lifts `b_rate`; the one sanctioned fall is a
    /// ceil-rounded default write-down, and the no-denominator case preserves it.
    fn rate_rows(&self, t: &mut Text) {
        let width = self.stream_width.saturating_sub(34).clamp(8, 60);
        for s in &self.timeline.rates {
            if s.points.is_empty() {
                continue;
            }
            let first = s.points.first().expect("non-empty");
            let last = s.points.last().expect("non-empty");
            t.head(format!(
                "{}  side {} ({} steps)",
                s.tok.as_str(),
                s.side,
                s.points.len()
            ));
            let take = |f: &dyn Fn(&crate::timeline::Point) -> i128| -> Vec<i128> {
                s.points.iter().rev().take(width).rev().map(f).collect()
            };
            for (name, vals, a, b) in [
                ("b_rate", take(&|p| p.b_rate), first.b_rate, last.b_rate),
                ("d_rate", take(&|p| p.d_rate), first.d_rate, last.d_rate),
            ] {
                t.raw(
                    1,
                    format!(
                        "{name}  {}  {} -> {}",
                        spark(&vals),
                        scaled(&a.to_string(), RATE_DP),
                        scaled(&b.to_string(), RATE_DP)
                    ),
                    None,
                );
            }
            for p in s.points.iter().filter(|p| p.defaulted) {
                let what = if p.no_denominator {
                    "default with b_supply 0: declared no-denominator deviation, b_rate preserved"
                } else {
                    "default write-down"
                };
                t.line(2, format!("● {} · {what}", p.label));
            }
            if s.notes.is_empty() {
                t.note(2, true, "every b_rate fall is a sanctioned default".into());
            } else {
                for n in &s.notes {
                    t.note(2, n.ok, n.text.to_string());
                }
            }
        }
    }

    /// ADR 0011's six backstop exports plus the three pool traps, on one screen.
    fn trap_rows(&self, t: &mut Text) {
        // Columns are clipped, not wrapped: this view is a matrix you scan down, and a
        // stock-side diagnostic that runs to three lines destroys that.
        let rest = self.stream_width.saturating_sub(44);
        let (wa, wb) = (rest / 2, rest - rest / 2);
        t.head(format!(
            "  {:<32}{:<8}{:<wa$}{:<wb$}",
            "entrypoint", "wants", "A (stock)", "B (fork)"
        ));
        for r in &self.timeline.traps {
            t.raw(
                0,
                format!(
                    "{} {:<32}{:<8}{:<wa$}{:<wb$}",
                    if r.ok { "✓" } else { "✖" },
                    one_line(r.call, 31),
                    format!("#{}", r.want),
                    one_line(&r.a.to_string(), wa.saturating_sub(1)),
                    one_line(&r.b.to_string(), wb)
                ),
                Some(if r.ok {
                    crate::compare::Class::Match
                } else {
                    crate::compare::Class::Violation(String::new())
                }),
            );
        }
        t.head(String::new());
        let runs = self
            .timeline
            .traps
            .iter()
            .map(|r| r.runs)
            .max()
            .unwrap_or(0);
        t.dim(
            0,
            format!(
                "each trap runs once per scenario ({runs} so far); a row is green only if every \
                 run on the fork side returned the code ADR 0011 requires as its first operation"
            ),
        );
    }

    pub fn toggle_expand(&mut self) {
        let rows = self.tree_rows();
        if let Some(r) = rows.get(self.detail_sel) {
            if !self.expanded.remove(&r.path) {
                self.expanded.insert(r.path.clone());
            }
        }
    }
}

// --- Helpers ---

fn disk_file_signature(path: &std::path::Path) -> std::io::Result<(std::time::SystemTime, u64)> {
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.modified()?, metadata.len()))
}

/// Every address in the pair, mapped to the name it is known by: the four identities,
/// the pool and backstop, and each token by ticker. Both sides, since an event carries
/// whichever side's address produced it.
pub fn address_names(m: &PairManifest) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (who, addr) in &m.identities {
        out.insert(addr.clone(), who.as_str().to_string());
    }
    for (tag, side) in [("A", &m.a), ("B", &m.b)] {
        let c = &side.contracts;
        out.insert(c.pool.clone(), format!("pool{tag}"));
        out.insert(c.backstop.clone(), format!("backstop{tag}"));
        out.insert(c.pool_factory.clone(), format!("factory{tag}"));
        out.insert(c.oracle.clone(), format!("oracle{tag}"));
        out.insert(c.emitter.clone(), format!("emitter{tag}"));
        for t in Tok::ALL {
            out.insert(c.token(t).to_string(), t.as_str().to_uppercase());
        }
    }
    out
}

/// Decimals for a token address, when the address is one of the pair's reserves.
fn decimals_of_address(addr: &str, m: &PairManifest) -> Option<u32> {
    Tok::RESERVES.iter().find_map(|t| {
        let hit = m.a.contracts.token(*t) == addr || m.b.contracts.token(*t) == addr;
        hit.then(|| m.a.reserves.get(t).map(|r| r.decimals))
            .flatten()
    })
}

/// Render one decoded ScVal the way a person reads it rather than as JSON:
/// addresses become names, symbols lose their quotes, integers are scaled when the
/// surrounding event says which asset they belong to, and containers nest inline.
pub fn scval(v: &Value, names: &BTreeMap<String, String>, dp: Option<u32>) -> String {
    let short = |a: &str| -> String {
        names.get(a).cloned().unwrap_or_else(|| {
            if a.len() > 12 {
                format!("{}…{}", &a[..5], &a[a.len() - 4..])
            } else {
                a.to_string()
            }
        })
    };
    match v {
        Value::Object(m) if m.len() == 1 => {
            let (k, inner) = m.iter().next().expect("len 1");
            match (k.as_str(), inner) {
                ("symbol" | "string", Value::String(s)) => s.clone(),
                ("address", Value::String(s)) => short(s),
                ("i128" | "u128" | "i64" | "u64" | "i32" | "u32", x) => {
                    let raw = x
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| x.to_string());
                    match dp {
                        Some(d) => scaled(&raw, d),
                        None => raw,
                    }
                }
                ("bool", x) => x.to_string(),
                ("void", _) => "void".into(),
                ("vec", Value::Array(items)) => format!(
                    "[{}]",
                    items
                        .iter()
                        .map(|i| scval(i, names, dp))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                ("map", Value::Array(entries)) => format!(
                    "{{{}}}",
                    entries
                        .iter()
                        .map(|e| {
                            let k = e
                                .get("key")
                                .map(|k| scval(k, names, None))
                                .unwrap_or_default();
                            let val = e
                                .get("val")
                                .map(|x| scval(x, names, dp))
                                .unwrap_or_default();
                            format!("{k}: {val}")
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => scval(inner, names, dp),
            }
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|i| scval(i, names, dp))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::String(s) => match dp {
            Some(d) if s.parse::<i128>().is_ok() => scaled(s, d),
            _ => short(s),
        },
        Value::Number(n) => match dp {
            Some(d) => scaled(&n.to_string(), d),
            None => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// The readable part of a failure: the contract's own diagnostic messages, with the
/// host's boilerplate, duplicate lines and full contract ids stripped out.
pub fn diagnostics(stderr: &str, names: &BTreeMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in stderr.lines() {
        let l = raw.trim();
        if !l.contains("Diagnostic Event") {
            continue;
        }
        // `... contract:C..., topics:[...], data:"the useful bit"`
        let Some(p) = l.find("data:") else { continue };
        let data = l[p + "data:".len()..].trim().trim_matches('"');
        if data.is_empty() || data.contains("failing with contract error") {
            continue;
        }
        let msg = data
            .trim_start_matches('[')
            .trim_end_matches(']')
            .replace("\\\"", "")
            .trim_matches('"')
            .to_string();
        // name the contract that raised it, when we know it
        let who = l
            .find("contract:")
            .map(|c| {
                l[c + "contract:".len()..]
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
            })
            .and_then(|a| names.get(a).cloned());
        let text = match who {
            Some(w) => format!("{w}: {msg}"),
            None => msg,
        };
        if !out.contains(&text) {
            out.push(text);
        }
    }
    if out.is_empty() {
        // no diagnostics: fall back to the first real error line
        if let Some(l) = stderr
            .lines()
            .map(str::trim)
            .find(|l| l.contains("error:") || l.contains("Error"))
        {
            out.push(
                l.trim_start_matches('❌')
                    .trim()
                    .trim_start_matches("error:")
                    .trim()
                    .to_string(),
            );
        }
    }
    out.truncate(4);
    out
}

/// Smallest scroll that keeps `sel` visible. Sticky: the view only moves when the
/// selection would leave it, which is what stops the pane jumping on every keypress.
pub fn window(offset: usize, sel: usize, height: usize, len: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    let max = len - height;
    let mut o = offset.min(max);
    if sel < o {
        o = sel;
    } else if sel >= o + height {
        o = sel + 1 - height;
    }
    o.min(max)
}

/// Walk a dotted path into a JSON document, so a diff path resolves back to the value
/// each side actually held. Soroban maps serialize as arrays of pairs, so those are
/// matched by key rather than by index.
pub fn value_at(doc: &Value, path: &str) -> Option<Value> {
    fn go(cur: &Value, segs: &[&str]) -> Option<Value> {
        let Some((seg, rest)) = segs.split_first() else {
            return Some(cur.clone());
        };
        let next = match cur {
            Value::Object(m) => m.get(*seg)?.clone(),
            Value::Array(_) => map_entries(cur)
                .into_iter()
                .find(|(k, _)| k == seg)
                .map(|(_, v)| v)?,
            _ => return None,
        };
        go(&next, rest)
    }
    go(doc, &path.split('.').collect::<Vec<_>>())
}

/// Split `text` into rows no wider than `width`, breaking on spaces where possible.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.chars().count() <= width {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if word.chars().count() > width {
            for chunk in word.chars().collect::<Vec<_>>().chunks(width) {
                out.push(chunk.iter().collect());
            }
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// Collapse to one line and clip, so a blob cannot blow up a row.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        flat.chars().take(max).collect::<String>() + "…"
    } else {
        flat
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

/// Pair-arm verbatim rendering of retained raw stream text for the pair
/// evidence pane. The pair arm of `Safe::render` stores the retained bytes
/// unchanged; `Display` emits them unchanged, so the pane shows exactly the
/// bytes it showed before the retype. This call site only ever runs on the
/// pair page (`App` holds a `PairManifest`; the market front ends use
/// `MarketApp`, which never reads `StepEvidence`).
pub fn pair_render(raw: &crate::stellar::RawText, cap: usize) -> String {
    Safe::render(Policy::Pair, raw, cap).to_string()
}

/// Render a raw integer string at `dp` decimal places: 1599624110541 at 7 -> 159962.4110541.
pub fn scaled(raw: &str, dp: u32) -> String {
    let Ok(n) = raw.parse::<i128>() else {
        return raw.to_string();
    };
    if dp == 0 {
        return n.to_string();
    }
    let dp = dp as usize;
    let sign = if n < 0 { "-" } else { "" };
    let digits = n.unsigned_abs().to_string();
    let digits = format!("{:0>width$}", digits, width = dp + 1);
    let (int, frac) = digits.split_at(digits.len() - dp);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    }
}

/// Decimal places for a snapshot field, by path. `None` leaves the value raw.
///
/// Three scales are in play: each reserve's own decimals for token quantities, 12 for
/// the b_rate/d_rate fixed point, and 7 for every ratio, price and LP figure. Counters,
/// indices, timestamps, ledger numbers and addresses stay raw.
pub fn decimals_for(path: &str, m: &PairManifest) -> Option<u32> {
    const RATE: u32 = 12;
    const RATIO: u32 = 7;
    let seg: Vec<&str> = path.split('.').collect();

    let by_name = |name: &str| {
        Tok::RESERVES
            .iter()
            .find(|t| t.as_str() == name)
            .and_then(|t| m.a.reserves.get(t))
            .map(|r| r.decimals)
    };
    let by_index = |idx: &str| {
        idx.parse::<u32>().ok().and_then(|i| {
            m.a.reserves
                .values()
                .find(|r| r.index == i)
                .map(|r| r.decimals)
        })
    };
    // Auction bid/lot maps are keyed by contract address, and the two sides have
    // different addresses, so check both.
    let by_address = |addr: &str| {
        Tok::RESERVES.iter().find_map(|t| {
            let hit = m.a.contracts.token(*t) == addr || m.b.contracts.token(*t) == addr;
            hit.then(|| m.a.reserves.get(t).map(|r| r.decimals))
                .flatten()
        })
    };

    match seg.as_slice() {
        // --- reserve data ---
        ["reserves", _, "data", "b_rate" | "d_rate"] => Some(RATE),
        ["reserves", _, "data", "ir_mod"] => Some(RATIO),
        ["reserves", tok, "data", "b_supply" | "d_supply" | "backstop_credit"] => by_name(tok),
        ["reserves", _, "data", "last_time"] => None,
        // --- reserve config: every rate and factor is a 7dp ratio ---
        ["reserves", _, "config", "c_factor" | "l_factor" | "util" | "max_util" | "r_base" | "r_one" | "r_two"
        | "r_three" | "reactivity"] => Some(RATIO),
        ["reserves", tok, "config", "supply_cap"] => by_name(tok),
        ["reserves", _, "config", "index" | "decimals" | "enabled"] => None,
        ["reserves", _, "scalar"] => None,
        // --- positions, keyed by reserve index ---
        ["positions", _, "collateral" | "liabilities" | "supply", idx] => by_index(idx),
        // --- token balances; the Comet LP is 7dp ---
        ["balances", tok, _] => by_name(tok).or(Some(RATIO)),
        // --- pool config ---
        ["config", "bstop_rate"] => Some(RATIO),
        ["config", "min_collateral"] => Some(RATIO),
        ["config", "status" | "max_positions"] => None,
        // --- backstop ---
        ["backstop_pool", "blnd" | "usdc" | "shares" | "tokens" | "q4w_pct" | "token_spot_price"] => {
            Some(RATIO)
        }
        ["backstop_users", _, "shares"] => Some(RATIO),
        ["backstop_users", _, "q4w", _, "amount"] => Some(RATIO),
        ["backstop_users", _, "q4w", _, "exp"] => None,
        // --- auctions: bid is d-tokens, lot is b-tokens, both in reserve decimals ---
        ["auctions", _, "bid" | "lot", addr] => by_address(addr),
        ["auctions", _, "block"] => None,
        _ => None,
    }
}

/// Flatten JSON into display rows, showing children only for expanded paths and
/// keeping ancestors of search matches. Soroban maps arrive in two encodings, so
/// `map_entries` normalizes them.
pub fn flatten(
    v: &Value,
    prefix: &str,
    expanded: &BTreeSet<String>,
    filter: &str,
    scale: &dyn Fn(&str) -> Option<u32>,
    out: &mut Vec<TreeRow>,
) {
    let indent = if prefix.is_empty() {
        0
    } else {
        prefix.matches('.').count() + 1
    };
    let children: Vec<(String, Value)> = match v {
        Value::Object(m) => m.iter().map(|(k, x)| (k.clone(), x.clone())).collect(),
        Value::Array(_) => map_entries(v),
        _ => Vec::new(),
    };
    for (k, child) in children {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        let leaf = !matches!(child, Value::Object(_) | Value::Array(_));
        let rendered = if leaf {
            let raw = child.to_string();
            let raw = raw.trim_matches('"');
            match scale(&path) {
                Some(dp) => scaled(raw, dp),
                None => raw.to_string(),
            }
        } else {
            String::new()
        };
        let matched = filter.is_empty()
            || path.contains(filter)
            || rendered.contains(filter)
            || subtree_matches(&child, filter);
        if !matched {
            continue;
        }
        let is_expanded = expanded.contains(&path) || (!filter.is_empty() && !leaf);
        out.push(TreeRow {
            indent,
            key: k,
            value: rendered,
            class: None,
            expandable: !leaf,
            path: path.clone(),
        });
        if !leaf && is_expanded {
            flatten(&child, &path, expanded, filter, scale, out);
        }
    }
}

fn subtree_matches(v: &Value, filter: &str) -> bool {
    match v {
        Value::Object(_) | Value::Array(_) => {
            let kids: Vec<(String, Value)> = match v {
                Value::Object(m) => m.iter().map(|(k, x)| (k.clone(), x.clone())).collect(),
                _ => map_entries(v),
            };
            kids.iter()
                .any(|(k, c)| k.contains(filter) || subtree_matches(c, filter))
        }
        other => other.to_string().contains(filter),
    }
}

// --- Tests ---

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::manifest::{Contracts, Profile, ReserveRef, SideManifest, Tok, Who};
    use crate::observe::{Snapshot, StepRecord};
    use crate::stellar::{Invocation, Net};
    use serde_json::json;

    pub fn manifest() -> PairManifest {
        let side = SideManifest {
            label: "s".into(),
            deploy_ledger: 1,
            wasm: BTreeMap::new(),
            contracts: Contracts {
                stable: "CSTBL".into(),
                oracle: "CORC".into(),
                ..Contracts::default()
            },
            reserves: [(
                Tok::Stable,
                ReserveRef {
                    index: 0,
                    decimals: 6,
                },
            )]
            .into(),
            minted: BTreeMap::new(),
        };
        PairManifest {
            profile: Profile::StockVsFork,
            description: Some("test pair".into()),
            net: Net {
                rpc_url: "u".into(),
                passphrase: "p".into(),
                config_dir: "/c".into(),
                headers: Vec::new(),
            },
            identities: Who::ALL
                .iter()
                .map(|w| (*w, format!("G{}", w.as_str())))
                .collect(),
            a: side.clone(),
            b: side,
        }
    }

    pub fn snap() -> Snapshot {
        let empty = json!({"collateral": {}, "liabilities": {}, "supply": {}});
        let mut positions = BTreeMap::new();
        for w in ["admin", "frodo", "sam", "user", "pool", "backstop"] {
            positions.insert(w.to_string(), empty.clone());
        }
        let mut balances: BTreeMap<Tok, BTreeMap<String, String>> = Tok::MOCK
            .iter()
            .copied()
            .map(|t| (t, BTreeMap::new()))
            .collect();
        balances.insert(Tok::Lp, [("backstop".to_string(), "0".to_string())].into());
        Snapshot {
            positions,
            reserves: [(
                Tok::Stable,
                json!({"asset": "CSTBL", "config": {"index": 0}, "data": {
                    "b_supply": "0", "d_supply": "0",
                    "b_rate": "1000000000000", "d_rate": "1000000000000",
                    "backstop_credit": "0"
                }}),
            )]
            .into(),
            config: json!({"bstop_rate": 0, "oracle": "CORC"}),
            backstop_pool: json!({"shares": "0", "tokens": "0"}),
            backstop_users: Who::ALL
                .iter()
                .copied()
                .map(|w| (w, json!({"shares": "0", "q4w": []})))
                .collect(),
            reward_zone: json!([]),
            balances,
            auctions: BTreeMap::new(),
            raw: [
                ("pool_emis".to_string(), None),
                (
                    "res_data_stable".to_string(),
                    Some(crate::stellar::RawEntry {
                        key: "res".into(),
                        val: "0".into(),
                        last_modified: 0,
                        live_until: None,
                    }),
                ),
            ]
            .into(),
        }
    }

    pub fn evidence(scenario: &str, label: &str, violations: Vec<String>) -> StepEvidence {
        let stderr: crate::stellar::RawText =
            serde_json::from_value(serde_json::Value::String(String::new()))
                .expect("RawText deserializes any string");
        let ok = Invocation {
            ok: true,
            value: None,
            error: None,
            events: vec![],
            stderr,
        };
        let rec = |side: &str, v: Vec<String>| StepRecord {
            label: label.into(),
            side: side.into(),
            invocation: Some(ok.clone()),
            before: Some(snap()),
            snapshot: snap(),
            violations: v
                .iter()
                .map(|x| {
                    crate::stellar::Safe::error_chain(
                        crate::stellar::Policy::Pair,
                        &anyhow::anyhow!("{x}"),
                    )
                })
                .collect(),
        };
        StepEvidence {
            scenario: scenario.into(),
            index: 0,
            label: label.into(),
            a: rec("s", vec![]),
            b: rec("s", violations),
            // Equal snapshots mean no divergences: a stored diff must mean
            // what the snapshots say. Tests that render a violation carry
            // their own honest diff.
            diff: vec![],
            findings: vec![],
        }
    }

    /// Plan status of one step, by name rather than position.
    pub fn status_of(a: &App, scenario: &str, label: &str) -> StepStatus {
        a.plan
            .iter()
            .find(|s| s.name == scenario)
            .and_then(|s| s.steps.iter().find(|x| x.label == label))
            .map(|s| s.status)
            .expect("step exists in the plan")
    }

    fn diff_class(
        steps: &[StepEvidence],
        scenario: &str,
        label: &str,
        path: &str,
    ) -> crate::compare::Class {
        steps
            .iter()
            .find(|s| s.scenario == scenario && s.label == label)
            .unwrap()
            .diff
            .iter()
            .find(|(p, _)| p == path)
            .unwrap()
            .1
            .clone()
    }

    fn passing_manifest() -> PairManifest {
        let side = |label: &str| SideManifest {
            label: label.into(),
            deploy_ledger: 1,
            wasm: BTreeMap::new(),
            contracts: Contracts {
                blnd: "CBLND".into(),
                usdc: "CUSDC".into(),
                weth: "CWETH".into(),
                xlm: "CXLM".into(),
                stable: "CSTBL".into(),
                lp: "CLP".into(),
                emitter: "CEMIT".into(),
                backstop: "CBSTP".into(),
                pool_factory: "CPF".into(),
                oracle: "CORC".into(),
                pool: "CPOOL".into(),
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
                    Tok::Xlm,
                    ReserveRef {
                        index: 1,
                        decimals: 7,
                    },
                ),
                (
                    Tok::Weth,
                    ReserveRef {
                        index: 2,
                        decimals: 9,
                    },
                ),
            ]
            .into(),
            minted: BTreeMap::new(),
        };
        let mut m = manifest();
        m.description = None;
        m.identities.clear();
        m.a = side("stock");
        m.b = side("fork");
        m
    }

    fn passing_interest_snapshot(b_rate: &str, d_rate: &str) -> Snapshot {
        let mut snapshot = snap();
        for (tok, asset, index) in [
            (Tok::Stable, "CSTBL", 0),
            (Tok::Xlm, "CXLM", 1),
            (Tok::Weth, "CWETH", 2),
        ] {
            snapshot.reserves.insert(
                tok,
                json!({
                    "asset": asset,
                    "config": {"index": index},
                    "data": {
                        "b_supply": "0",
                        "d_supply": "0",
                        "b_rate": b_rate,
                        "d_rate": d_rate,
                        "backstop_credit": "0"
                    }
                }),
            );
            snapshot.raw.insert(
                format!("res_data_{}", tok.as_str()),
                Some(crate::stellar::RawEntry {
                    key: "res".into(),
                    val: "0".into(),
                    last_modified: 0,
                    live_until: None,
                }),
            );
        }
        snapshot
    }
    fn write_passing_interest_run(out: &std::path::Path, m: &PairManifest) {
        m.save(out).unwrap();
        let scope = crate::report::canonical_scope().remove("interest").unwrap();
        let mut state = RunState::default();
        state.requested.insert("interest".into(), scope.clone());
        state.completed.insert("interest".into(), scope);
        state.scenario_order = vec!["interest".into()];
        state.save(out).unwrap();

        let before = passing_interest_snapshot("1000000000000", "1000000000000");
        let after = passing_interest_snapshot("1000000000010", "1000000000010");
        let mut supply = evidence("interest", "interest_supply", vec![]);
        supply.index = 1;
        for record in [&mut supply.a, &mut supply.b] {
            record.before = Some(before.clone());
            record.snapshot = before.clone();
        }
        crate::report::write_step(out, &supply).unwrap();
        let mut rates = evidence("interest", "interest_rates_increased", vec![]);
        rates.index = 2;
        rates.a.invocation = None;
        rates.b.invocation = None;
        for record in [&mut rates.a, &mut rates.b] {
            record.before = Some(before.clone());
            record.snapshot = after.clone();
        }
        crate::report::write_step(out, &rates).unwrap();

        for (n, trap) in crate::scenario::traps().into_iter().enumerate() {
            let call = crate::scenario::TrapCall::ALL
                .iter()
                .copied()
                .find(|call| call.label() == trap.label)
                .unwrap();
            let mut ev = evidence("interest", trap.label, vec![]);
            ev.index = 3 + n;
            ev.b.invocation = Some(
                serde_json::from_value(json!({
                    "ok": false,
                    "value": null,
                    "error": {"Contract": call.fork_error()},
                    "events": [],
                    "stderr": format!("error: Error(Contract, #{})", call.fork_error())
                }))
                .unwrap(),
            );
            for record in [&mut ev.a, &mut ev.b] {
                record.before = Some(after.clone());
                record.snapshot = after.clone();
            }
            crate::report::write_step(out, &ev).unwrap();
        }
    }

    pub fn app() -> App {
        App::new(PathBuf::from("/tmp/abtest-test"), manifest(), None)
    }

    #[test]
    fn step_lifecycle_updates_plan_and_selection() {
        let mut a = app();
        a.apply(Ev::RunStarted {
            plan: vec![("lending".into(), vec!["borrow".into()])],
            resumed: 0,
        });
        assert_eq!(a.run, RunStatus::Running);
        assert_eq!(a.plan.len(), 1, "plan filtered to the selected scenarios");
        a.apply(Ev::StepStarted {
            scenario: "lending".into(),
            index: 0,
            label: "borrow".into(),
        });
        assert_eq!(status_of(&a, "lending", "borrow"), StepStatus::Running);
        a.apply(Ev::SideDone {
            tag: 'a',
            ok: true,
            err: None,
        });
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("lending", "borrow", vec![])),
            violations: 0,
            desynced: false,
        });
        assert_eq!(status_of(&a, "lending", "borrow"), StepStatus::Done);
        assert_eq!(a.steps.len(), 1);
        assert_eq!(a.selected, 0);
        assert!(!a.ledger.is_empty(), "ledger recomputed on StepDone");
    }

    #[test]
    fn violations_halt_unless_auto_continue() {
        let mut a = app();
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("lending", "borrow", vec!["invariant: x".into()])),
            violations: 1,
            desynced: false,
        });
        assert!(matches!(a.run, RunStatus::Halted(_)));
        assert_eq!(status_of(&a, "lending", "borrow"), StepStatus::Failed);

        let mut b = app();
        b.auto_continue = true;
        b.apply(Ev::StepDone {
            evidence: Box::new(evidence("lending", "borrow", vec!["invariant: x".into()])),
            violations: 1,
            desynced: false,
        });
        assert_eq!(b.run, RunStatus::Running);
    }

    #[test]
    fn halt_records_reason() {
        let mut a = app();
        a.apply(Ev::Halt {
            scenario: "lending".into(),
            label: "borrow".into(),
            reason: crate::stellar::Safe::literal("502"),
        });
        assert_eq!(a.run, RunStatus::Halted("502".into()));
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn waiting_replaces_rather_than_accumulates() {
        let mut a = app();
        for left in [130u64, 125] {
            a.apply(Ev::Waiting {
                label: "liq_wait_101".into(),
                now: 10,
                target: 20,
                secs_left: left,
            });
        }
        assert!(a.stream.is_empty(), "waits must not flood the stream");
        assert!(a.waiting.as_ref().unwrap().contains("125"));
        a.apply(Ev::StepStarted {
            scenario: "s".into(),
            index: 0,
            label: "l".into(),
        });
        assert!(a.waiting.is_none());
    }

    #[test]
    fn consecutive_transient_failures_become_a_retry_counter() {
        let mut a = app();
        for _ in 0..3 {
            a.apply(Ev::Cli {
                argv: "contract invoke".into(),
                ok: false,
                ms: 10,
                stderr: "error: Server returned an error status code: 502".into(),
                transient: true,
            });
        }
        assert_eq!(
            a.run,
            RunStatus::Retrying {
                n: 3,
                why: "error: Server returned an error status code: 502".into()
            }
        );
        a.apply(Ev::Cli {
            argv: "contract invoke".into(),
            ok: true,
            ms: 5,
            stderr: String::new(),
            transient: false,
        });
        assert_eq!(a.transient, 0);
    }

    #[test]
    fn stream_is_capped() {
        let mut a = app();
        for i in 0..(STREAM_CAP + 500) {
            a.apply(Ev::Cli {
                argv: format!("call {i}"),
                ok: true,
                ms: 1,
                stderr: String::new(),
                transient: false,
            });
        }
        assert_eq!(a.stream.len(), STREAM_CAP);
        assert!(a.stream[0].contains(&format!("call {}", 500)));
    }

    #[test]
    fn flatten_expands_and_filters() {
        let v = json!({"reserves": {"stable": {"b_supply": "10"}}, "config": {"bstop_rate": 0}});
        let mut rows = Vec::new();
        flatten(&v, "", &BTreeSet::new(), "", &|_| None, &mut rows);
        assert_eq!(rows.len(), 2, "collapsed: only top-level keys");

        let mut expanded = BTreeSet::new();
        expanded.insert("reserves".to_string());
        let mut rows = Vec::new();
        flatten(&v, "", &expanded, "", &|_| None, &mut rows);
        assert!(rows.iter().any(|r| r.key == "stable" && r.indent == 1));

        let mut rows = Vec::new();
        flatten(&v, "", &BTreeSet::new(), "b_supply", &|_| None, &mut rows);
        assert!(rows.iter().any(|r| r.key == "b_supply"));
        assert!(
            rows.iter().any(|r| r.key == "reserves"),
            "ancestors of a match are kept"
        );
        assert!(!rows.iter().any(|r| r.key == "config"));
    }

    #[test]
    fn flatten_handles_both_soroban_map_encodings() {
        let object = json!({"m": {"0": "1"}});
        let array = json!({"m": [[0, "1"]]});
        let mut e = BTreeSet::new();
        e.insert("m".to_string());
        let (mut ra, mut rb) = (Vec::new(), Vec::new());
        flatten(&object, "", &e, "", &|_| None, &mut ra);
        flatten(&array, "", &e, "", &|_| None, &mut rb);
        let keys = |r: &Vec<TreeRow>| r.iter().map(|x| x.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&ra), keys(&rb));
    }

    /// Closest thing to "open a finished run" without a terminal: persist evidence
    /// with the real writer, then load it back through the watch-mode path.
    #[test]
    fn browse_a_persisted_run_from_disk() {
        let dir = std::env::temp_dir().join(format!("abtest-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (i, label) in ["borrow", "partial_repay"].iter().enumerate() {
            let mut ev = evidence("lending", label, vec![]);
            ev.index = i;
            crate::report::write_step(&dir, &ev).unwrap();
        }
        let mut a = App::new(dir.clone(), manifest(), None);
        a.poll_disk().unwrap();
        assert_eq!(a.steps.len(), 2);
        assert!(!a.ledger.is_empty());
        assert_eq!(status_of(&a, "lending", "borrow"), StepStatus::Done);

        // A second poll must not duplicate what is already loaded.
        a.poll_disk().unwrap();
        assert_eq!(a.steps.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_disk_reclassifies_b_only_changes_without_reading_diff() {
        let dir = std::env::temp_dir().join(format!("abtest-poll-b-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ev = evidence("lending", "borrow", vec![]);
        ev.index = 0;
        crate::report::write_step(&dir, &ev).unwrap();
        let step_dir = dir.join("evidence/lending/000-borrow");
        let diff_path = step_dir.join("diff.json");
        std::fs::remove_file(&diff_path).unwrap();

        let mut a = App::new(dir.clone(), manifest(), None);
        a.poll_disk().unwrap();
        assert!(
            a.steps[0].diff.is_empty(),
            "a/b snapshots are authoritative without diff.json"
        );

        let b_path = step_dir.join("b.json");
        let mut b: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&b_path).unwrap()).unwrap();
        b["snapshot"]["config"]["bstop_rate"] = json!(999);
        std::fs::write(&b_path, serde_json::to_vec_pretty(&b).unwrap()).unwrap();
        a.poll_disk().unwrap();
        assert!(
            a.steps[0].diff.iter().any(|(path, class)| {
                path == "config.bstop_rate" && matches!(class, crate::compare::Class::Violation(_))
            }),
            "a b.json-only snapshot change must be reclassified: {:?}",
            a.steps[0].diff
        );
        assert_eq!(status_of(&a, "lending", "borrow"), StepStatus::Failed);
        let changed = a.steps[0].diff.clone();
        a.poll_disk().unwrap();
        assert_eq!(a.steps.len(), 1, "an unchanged pair is not adopted twice");
        assert_eq!(a.steps[0].diff, changed);
        assert!(!diff_path.exists(), "diff.json remains non-authoritative");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_disk_evicts_invalid_and_removed_evidence_then_recovers() {
        let dir = std::env::temp_dir().join(format!("abtest-poll-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut borrow = evidence("lending", "borrow", vec![]);
        borrow.index = 0;
        let mut partial = evidence("lending", "partial_repay", vec![]);
        partial.index = 1;
        crate::report::write_step(&dir, &borrow).unwrap();
        crate::report::write_step(&dir, &partial).unwrap();
        let borrow_dir = dir.join("evidence/lending/000-borrow");
        let a_path = borrow_dir.join("a.json");
        let b_path = borrow_dir.join("b.json");
        let saved_a = std::fs::read(&a_path).unwrap();
        let saved_b = std::fs::read(&b_path).unwrap();

        let mut app = App::new(dir.clone(), manifest(), None);
        app.poll_disk().unwrap();
        app.follow = false;
        app.selected = app
            .steps
            .iter()
            .position(|s| s.scenario == "lending" && s.label == "borrow")
            .unwrap();

        std::fs::write(&a_path, b"malformed a record with changed length").unwrap();
        app.poll_disk().unwrap();
        assert_eq!(
            app.steps.len(),
            1,
            "malformed a.json retires cached evidence"
        );
        assert_eq!(app.selected_step().unwrap().label, "partial_repay");
        assert_eq!(status_of(&app, "lending", "borrow"), StepStatus::Pending);
        assert!(!app
            .timeline
            .cells
            .iter()
            .any(|c| c.scenario == "lending" && c.label == "borrow"));
        assert!(!app.watch_read_errors.is_empty());
        assert_ne!(app.verdict, "PASS");

        std::fs::write(&a_path, &saved_a).unwrap();
        std::fs::remove_file(&b_path).unwrap();
        app.poll_disk().unwrap();
        assert_eq!(
            app.steps.len(),
            1,
            "missing b.json also retires cached evidence"
        );
        assert!(!app.watch_read_errors.is_empty());

        std::fs::write(&b_path, &saved_b).unwrap();
        app.poll_disk().unwrap();
        assert_eq!(
            app.steps.len(),
            2,
            "readable inputs are automatically re-adopted"
        );
        assert!(app.watch_read_errors.is_empty());
        assert_eq!(app.selected_step().unwrap().label, "partial_repay");

        let partial_dir = dir.join("evidence/lending/001-partial_repay");
        std::fs::remove_dir_all(&partial_dir).unwrap();
        app.poll_disk().unwrap();
        assert_eq!(
            app.steps.len(),
            1,
            "a removed step leaves the current analysis"
        );
        assert_eq!(
            status_of(&app, "lending", "partial_repay"),
            StepStatus::Pending
        );

        let evidence_root = dir.join("evidence");
        std::fs::remove_dir_all(&evidence_root).unwrap();
        app.poll_disk().unwrap();
        assert!(
            app.steps.is_empty(),
            "a disappeared evidence root retires cached steps"
        );
        assert!(app.selected_step().is_none());
        assert!(app.watch_read_errors.is_empty());
        assert_eq!(status_of(&app, "lending", "borrow"), StepStatus::Pending);
        crate::report::write_step(&dir, &borrow).unwrap();
        crate::report::write_step(&dir, &partial).unwrap();
        app.poll_disk().unwrap();
        assert_eq!(
            app.steps.len(),
            2,
            "a recreated root is read-only recoverable"
        );

        std::fs::remove_dir_all(&evidence_root).unwrap();
        std::fs::write(&evidence_root, b"not a directory").unwrap();
        app.poll_disk().unwrap();
        assert!(
            app.steps.is_empty(),
            "a root scan failure cannot retain cached steps"
        );
        assert!(app.selected_step().is_none());
        assert!(!app.watch_read_errors.is_empty());
        assert_ne!(app.verdict, "PASS");
        std::fs::remove_file(&evidence_root).unwrap();
        app.poll_disk().unwrap();
        assert!(
            app.watch_read_errors.is_empty(),
            "a missing root is a readable empty state"
        );
        crate::report::write_step(&dir, &borrow).unwrap();
        crate::report::write_step(&dir, &partial).unwrap();
        app.poll_disk().unwrap();
        assert_eq!(app.steps.len(), 2);
        assert!(app.watch_read_errors.is_empty());
        assert!(app.selected_step().is_some());
        let mut neighbor = evidence("interest", "interest_supply", vec![]);
        neighbor.index = 0;
        crate::report::write_step(&dir, &neighbor).unwrap();
        app.poll_disk().unwrap();
        let lending_dir = dir.join("evidence/lending");
        std::fs::remove_dir_all(&lending_dir).unwrap();
        std::fs::write(&lending_dir, b"not a directory").unwrap();
        app.poll_disk().unwrap();
        assert_eq!(
            app.steps.len(),
            1,
            "a failed scenario scan evicts only that scenario"
        );
        assert_eq!(
            app.steps[0].scenario, "interest",
            "readable neighbors remain browsable"
        );
        assert!(!app.watch_read_errors.is_empty());
        assert_eq!(status_of(&app, "lending", "borrow"), StepStatus::Pending);
        std::fs::remove_file(&lending_dir).unwrap();
        crate::report::write_step(&dir, &borrow).unwrap();
        crate::report::write_step(&dir, &partial).unwrap();
        app.poll_disk().unwrap();
        assert_eq!(app.steps.len(), 3);
        assert!(app.watch_read_errors.is_empty());
        assert_eq!(app.steps[0].scenario, "lending");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_disk_reclassifies_cached_successors_when_freeze_context_changes() {
        let dir =
            std::env::temp_dir().join(format!("abtest-poll-freeze-context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = manifest();
        let mut probe = evidence("freeze", crate::scenario::FREEZE_DOWNGRADE_PROBE, vec![]);
        probe.index = 0;
        probe.a.snapshot.config["status"] = json!(0);
        probe.b.snapshot.config["status"] = json!(4);
        let mut successor = evidence("freeze", "freeze_withdraw_allowed", vec![]);
        successor.index = 1;
        successor.a.snapshot.config["status"] = json!(0);
        successor.b.snapshot.config["status"] = json!(4);
        crate::report::write_step(&dir, &probe).unwrap();
        crate::report::write_step(&dir, &successor).unwrap();

        let mut app = App::new(dir.clone(), m.clone(), None);
        app.poll_disk().unwrap();
        assert!(matches!(
            diff_class(
                &app.steps,
                "freeze",
                "freeze_withdraw_allowed",
                "config.status"
            ),
            crate::compare::Class::Expected(_)
        ));

        let probe_dir = dir
            .join("evidence/freeze")
            .join(format!("000-{}", crate::scenario::FREEZE_DOWNGRADE_PROBE));
        std::fs::remove_dir_all(probe_dir).unwrap();
        app.poll_disk().unwrap();
        let report_steps = crate::report::load_steps(&dir, &m).unwrap();
        let report_class = diff_class(
            &report_steps,
            "freeze",
            "freeze_withdraw_allowed",
            "config.status",
        );
        assert!(matches!(
            report_class.clone(),
            crate::compare::Class::Violation(_)
        ));
        assert_eq!(
            diff_class(
                &app.steps,
                "freeze",
                "freeze_withdraw_allowed",
                "config.status",
            ),
            report_class
        );

        crate::report::write_step(&dir, &probe).unwrap();
        app.poll_disk().unwrap();
        let report_steps = crate::report::load_steps(&dir, &m).unwrap();
        let report_class = diff_class(
            &report_steps,
            "freeze",
            "freeze_withdraw_allowed",
            "config.status",
        );
        assert!(matches!(
            report_class.clone(),
            crate::compare::Class::Expected(_)
        ));
        assert_eq!(
            diff_class(
                &app.steps,
                "freeze",
                "freeze_withdraw_allowed",
                "config.status",
            ),
            report_class
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_disk_eviction_is_scoped_to_loaded_directory_identity() {
        let dir = std::env::temp_dir().join(format!("abtest-poll-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut secondary = evidence("lending", "borrow", vec![]);
        secondary.index = 1;
        crate::report::write_step(&dir, &secondary).unwrap();
        let secondary_dir = dir.join("evidence/lending/001-borrow");

        let mut app = App::new(dir.clone(), manifest(), None);
        app.poll_disk().unwrap();
        assert_eq!(app.steps.len(), 1);
        assert_eq!(app.steps[0].index, 1);

        let mut failing = evidence(
            "lending",
            "borrow",
            vec!["invariant: preserve failing primary record".into()],
        );
        failing.index = 0;
        crate::report::write_step(&dir, &failing).unwrap();
        std::fs::remove_file(secondary_dir.join("b.json")).unwrap();
        app.poll_disk().unwrap();
        assert!(!app.watch_read_errors.is_empty());
        assert_eq!(app.steps.len(), 1);
        assert_eq!(app.steps[0].index, 0);
        assert_eq!(app.verdict, "FAIL");

        app.poll_disk().unwrap();
        assert_eq!(app.steps.len(), 1);
        assert_eq!(app.steps[0].index, 0);
        assert_eq!(app.verdict, "FAIL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_disk_read_failure_cannot_hide_a_currently_certifiable_pass() {
        let dir =
            std::env::temp_dir().join(format!("abtest-poll-read-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = passing_manifest();
        write_passing_interest_run(&dir, &m);
        let operator = evidence("operator", "ad_hoc", vec![]);
        crate::report::write_step(&dir, &operator).unwrap();
        let operator_dir = dir.join("evidence/operator/000-ad_hoc");
        let b_path = operator_dir.join("b.json");
        let saved_b = std::fs::read(&b_path).unwrap();

        let mut app = App::new(dir.clone(), m.clone(), None);
        app.poll_disk().unwrap();
        assert_eq!(
            app.verdict,
            "PASS",
            "canonical interest fixture is certifiable; ledger={:?}; integrity={:?}; steps={:?}",
            app.ledger
                .iter()
                .map(|r| (r.clause, &r.status, &r.evidence))
                .collect::<Vec<_>>(),
            app.integrity,
            app.steps
                .iter()
                .map(|s| (
                    &s.scenario,
                    &s.label,
                    &s.findings,
                    &s.a.violations,
                    &s.b.violations,
                    &s.diff,
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(analyze(&dir, &m, &app.steps).verdict, "PASS");

        std::fs::write(&b_path, b"{malformed operator evidence}").unwrap();
        app.poll_disk().unwrap();
        assert_eq!(analyze(&dir, &m, &app.steps).verdict, "PASS");
        assert_eq!(
            app.verdict, "INCOMPLETE",
            "a current read failure must block PASS"
        );
        assert!(
            app.watch_read_errors[0].starts_with("Evidence unavailable; rereading files: "),
            "the renderer receives the current-poll diagnostic: {:?}",
            app.watch_read_errors
        );
        assert!(
            app.steps.iter().all(|s| s.scenario != "operator"),
            "malformed operator evidence is retired, not retained from cache"
        );

        std::fs::write(&b_path, &saved_b).unwrap();
        app.poll_disk().unwrap();
        assert!(app.watch_read_errors.is_empty());
        assert_eq!(app.verdict, "PASS");
        assert!(app.steps.iter().any(|s| s.scenario == "operator"));
        let (done, total, _) = app.progress();
        assert_eq!((done, total), (total, total));
        let supply_dir = dir.join("evidence/interest/001-interest_supply");
        app.follow = false;
        app.selected = app
            .steps
            .iter()
            .position(|s| s.scenario == "interest" && s.label == "interest_supply")
            .unwrap();
        std::fs::remove_dir_all(&supply_dir).unwrap();
        app.poll_disk().unwrap();
        assert_eq!(app.progress().0, total - 1);
        assert_eq!(
            status_of(&app, "interest", "interest_supply"),
            StepStatus::Pending
        );
        assert_eq!(app.ledger[0].status, "INCOMPLETE");
        assert!(!app
            .timeline
            .cells
            .iter()
            .any(|c| c.scenario == "interest" && c.label == "interest_supply"));
        assert_ne!(app.selected_step().unwrap().label, "interest_supply");
        let mut supply = evidence("interest", "interest_supply", vec![]);
        supply.index = 1;
        let clean = passing_interest_snapshot("1000000000000", "1000000000000");
        for record in [&mut supply.a, &mut supply.b] {
            record.before = Some(clean.clone());
            record.snapshot = clean.clone();
        }
        crate::report::write_step(&dir, &supply).unwrap();
        app.poll_disk().unwrap();
        let (recovered_done, recovered_total, _) = app.progress();
        assert_eq!((recovered_done, recovered_total), (total, total));
        assert_eq!(app.verdict, "PASS");
        assert_eq!(
            status_of(&app, "interest", "interest_supply"),
            StepStatus::Done
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_disk_read_failure_preserves_a_genuine_fail() {
        let dir = std::env::temp_dir().join(format!("abtest-poll-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut failed = evidence(
            "lending",
            "borrow",
            vec!["invariant: retained finding".into()],
        );
        failed.index = 0;
        crate::report::write_step(&dir, &failed).unwrap();
        let operator = evidence("operator", "ad_hoc", vec![]);
        crate::report::write_step(&dir, &operator).unwrap();
        std::fs::write(
            dir.join("evidence/operator/000-ad_hoc/a.json"),
            b"malformed operator evidence",
        )
        .unwrap();

        let mut app = App::new(dir.clone(), manifest(), None);
        app.poll_disk().unwrap();
        assert!(!app.watch_read_errors.is_empty());
        assert_eq!(
            app.verdict, "FAIL",
            "an unreadable neighbor cannot erase a genuine failure"
        );
        assert!(app
            .steps
            .iter()
            .any(|s| s.scenario == "lending" && s.label == "borrow"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Custody is part of the disk state: the runner writes complete evidence
    /// before it marks the step and clears `pending/`, so a poll landing in
    /// that window must not freeze INCOMPLETE forever. Removing the pending
    /// entry changes no evidence record, so the verdict must still follow custody.
    #[test]
    fn poll_disk_follows_a_pending_entry_removed_without_a_diff_change() {
        let dir = std::env::temp_dir().join(format!("abtest-poll-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ev = evidence("lending", "borrow", vec![]);
        crate::report::write_step(&dir, &ev).unwrap();
        let pending = dir
            .join("pending")
            .join("lending")
            .join("002-partial_repay");
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::write(pending.join("b.intent.json"), "{}").unwrap();

        let mut a = App::new(dir.clone(), manifest(), None);
        a.poll_disk().unwrap();
        assert_eq!(a.steps.len(), 1);
        assert!(
            a.integrity.iter().any(|i| i.contains("pending")),
            "the unresolved pending step must block certification: {:?}",
            a.integrity
        );

        std::fs::remove_dir_all(&pending).unwrap();
        // Evidence is unchanged; this poll must reflect the cleared custody state.
        a.poll_disk().unwrap();
        assert_eq!(
            a.steps.len(),
            1,
            "polling must not duplicate retained evidence"
        );
        assert_eq!(
            a.verdict,
            analyze(&dir, &manifest(), &a.steps).verdict,
            "the watch verdict must match the report after a custody-only change"
        );
        assert!(
            !a.integrity.iter().any(|i| i.contains("pending")),
            "the cleared pending entry must no longer block: {:?}",
            a.integrity
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The watch and the report must replay in the same order: a scenario dir
    /// that readdir returns first (here `interest`, created first) must not lead
    /// the adoption when the canonical order puts `lending` first — the shared
    /// cross-step context flags (delta, custody) assume it.
    #[test]
    fn poll_disk_replays_in_report_order_not_readdir_order() {
        let dir = std::env::temp_dir().join(format!("abtest-poll-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (scenario, label) in [("interest", "interest_wait"), ("lending", "borrow")] {
            let ev = evidence(scenario, label, vec![]);
            crate::report::write_step(&dir, &ev).unwrap();
        }
        let mut a = App::new(dir.clone(), manifest(), None);
        a.poll_disk().unwrap();
        let seen: Vec<(&str, &str)> = a
            .steps
            .iter()
            .map(|s| (s.scenario.as_str(), s.label.as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![("lending", "borrow"), ("interest", "interest_wait")],
            "watch must adopt in scenario_replay_order, not readdir order"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Diagnostic against a real run directory, skipped when absent.
    #[test]
    fn watch_mode_populates_every_tab() {
        let dir = std::path::PathBuf::from("runs/smoke3");
        if !dir.join("manifest.toml").exists() {
            eprintln!("skip: no runs/smoke3");
            return;
        }
        let m = crate::manifest::PairManifest::load(&dir).unwrap();
        let mut a = App::new(dir, m, None);
        a.poll_disk().expect("poll_disk must not fail");
        eprintln!("steps loaded: {}", a.steps.len());
        eprintln!("selected: {:?}", a.selected_step().map(|s| &s.label));
        for t in Tab::ALL {
            a.tab = t;
            eprintln!("{:?}: {} rows", t, a.tree_rows().len());
        }
        assert!(a.steps.len() > 50, "evidence should load");
        // Every loaded step must be reachable from the plan, including scenarios whose
        // names have since changed in code.
        for st in &a.steps {
            let sc = a
                .plan
                .iter()
                .find(|p| p.name == st.scenario)
                .unwrap_or_else(|| panic!("scenario {} missing from plan", st.scenario));
            assert!(
                sc.steps.iter().any(|x| x.label == st.label),
                "step {}/{} missing from plan",
                st.scenario,
                st.label
            );
        }
        // And selecting between them works.
        a.selected = 0;
        a.select_down();
        assert_eq!(a.selected, 1);
        assert!(!a.follow, "moving off the tail stops following");
        for t in Tab::ALL {
            a.tab = t;
            assert!(!a.tree_rows().is_empty(), "{t:?} tab is empty");
        }
    }

    /// Evidence loaded from disk arrives in directory order. If the step list is not
    /// put back into plan order, stepping through it walks the plan pane backwards and
    /// forwards and the scroll lurches.
    #[test]
    fn steps_are_held_in_plan_order() {
        let mut a = app();
        // adopt in a deliberately wrong order
        for (sc, label) in [
            ("statuses", "status_admin_on_ice"),
            ("lending", "supply"),
            ("backstop", "backstop_deposit"),
            ("lending", "borrow"),
            ("interest", "interest_supply"),
        ] {
            a.apply(Ev::StepDone {
                evidence: Box::new(evidence(sc, label, vec![])),
                violations: 0,
                desynced: false,
            });
        }
        let got: Vec<(String, String)> = a
            .steps
            .iter()
            .map(|s| (s.scenario.clone(), s.label.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("lending".into(), "borrow".into()),
                ("lending".into(), "supply".into()),
                ("interest".into(), "interest_supply".into()),
                ("statuses".into(), "status_admin_on_ice".into()),
                ("backstop".into(), "backstop_deposit".into()),
            ],
            "execution order, not arrival order"
        );

        // and walking the list moves monotonically through the plan
        let mut last = (0usize, 0usize);
        for i in 0..a.steps.len() {
            let s = &a.steps[i];
            let pos = a.plan_position(&s.scenario, &s.label);
            assert!(pos >= last, "step {i} goes backwards in the plan");
            last = pos;
        }
    }

    #[test]
    fn selection_survives_a_resort() {
        let mut a = app();
        for (sc, label) in [("backstop", "backstop_deposit"), ("lending", "borrow")] {
            a.apply(Ev::StepDone {
                evidence: Box::new(evidence(sc, label, vec![])),
                violations: 0,
                desynced: false,
            });
        }
        a.follow = false;
        a.selected = a
            .steps
            .iter()
            .position(|s| s.label == "backstop_deposit")
            .unwrap();
        // a later arrival re-sorts the list; the selection must stay on the same step
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("interest", "interest_supply", vec![])),
            violations: 0,
            desynced: false,
        });
        assert_eq!(
            a.selected_step().map(|s| s.label.as_str()),
            Some("backstop_deposit")
        );
    }

    #[test]
    fn window_is_sticky_and_stays_in_range() {
        // everything fits: never scroll
        assert_eq!(window(0, 5, 20, 10), 0);
        assert_eq!(window(7, 5, 20, 10), 0, "a stale offset is discarded");

        // selection already visible: do not move
        assert_eq!(window(10, 12, 5, 40), 10);
        assert_eq!(
            window(10, 14, 5, 40),
            10,
            "last visible row still does not move"
        );

        // just past the bottom edge: scroll by exactly one
        assert_eq!(window(10, 15, 5, 40), 11);
        // above the top edge: jump straight to it
        assert_eq!(window(10, 3, 5, 40), 3);

        // clamped at the end, never showing blank space past the last row
        assert_eq!(window(999, 39, 5, 40), 35);
        assert_eq!(window(0, 39, 5, 40), 35);

        // degenerate inputs must not panic or underflow
        assert_eq!(window(0, 0, 0, 10), 0);
        assert_eq!(window(0, 0, 5, 0), 0);

        // walking down one row at a time moves the view one row at a time
        let (h, len) = (5, 20);
        let mut off = 0;
        for sel in 0..len {
            off = window(off, sel, h, len);
            assert!(
                sel >= off && sel < off + h,
                "sel {sel} outside {off}..{}",
                off + h
            );
            assert!(off + h <= len);
        }
        // and back up again
        for sel in (0..len).rev() {
            off = window(off, sel, h, len);
            assert!(sel >= off && sel < off + h);
        }
    }

    /// The progress line is the canonical requested scope judged against
    /// certified completion — never the whole catalog, never merely the
    /// evidence dirs that happen to have landed, and never inflatable by an
    /// operator or out-of-scope completion claim.
    #[test]
    fn progress_counts_requested_scope_not_adopted_evidence() {
        // No recorded scope: unknown, never a fake full-suite denominator.
        let (done, total, _) = app().progress();
        assert_eq!(
            (done, total),
            (0, 0),
            "a run without a recorded scope has no honest denominator"
        );

        let dir =
            std::env::temp_dir().join(format!("abtest-progress-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = manifest();
        m.save(&dir).unwrap();
        let want = crate::report::canonical_scope()
            .remove("interest")
            .expect("interest scenario");
        let mut st = RunState::default();
        st.requested.insert("interest".into(), want.clone());
        st.completed.insert("interest".into(), want.clone());
        st.scenario_order = vec!["interest".into()];
        st.save(&dir).unwrap();
        // The wait is certified by `completed` alone and produces no directory;
        // every other label gets its evidence written once, then never touched.
        for (n, label) in want.iter().enumerate() {
            if label == "interest_wait" {
                continue;
            }
            let mut ev = evidence("interest", label, vec![]);
            ev.index = n;
            crate::report::write_step(&dir, &ev).unwrap();
        }

        let mut a = App::new(dir.clone(), m.clone(), None);
        a.poll_disk().unwrap();
        let (done, total, _) = a.progress();
        assert_eq!(
            (done, total),
            (want.len(), want.len()),
            "a fully certified requested scope reaches its own denominator, wait included"
        );
        assert_eq!(
            a.steps.len(),
            want.len() - 1,
            "the wait counts as complete while carrying no evidence"
        );
        assert_eq!(
            a.plan.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["interest"],
            "the plan pane shrinks to the requested scope: no catalog rows, no duplicated scenario"
        );

        // Operator records satisfy neither side of the fraction.
        let mut op = evidence("operator", "ad_hoc", vec![]);
        op.index = 0;
        crate::report::write_step(&dir, &op).unwrap();
        let mut st = RunState::load(&dir).unwrap();
        st.completed
            .insert("operator".into(), vec!["ad_hoc".into()]);
        st.completed
            .get_mut("interest")
            .unwrap()
            .push("never_a_step".into());
        st.save(&dir).unwrap();
        a.poll_disk().unwrap();
        let (done, total, _) = a.progress();
        assert_eq!(
            (done, total),
            (want.len(), want.len()),
            "operator and unknown labels cannot inflate completion"
        );

        assert_eq!(
            a.plan.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["interest", "operator"],
            "observed evidence outside the scope stays disclosed; unevidenced catalog rows do not"
        );

        // Evidence for a label the recorded scope never named must stay
        // navigable without widening the denominator.
        let mut legacy = evidence("interest", "legacy_interest_supply", vec![]);
        legacy.index = 99;
        crate::report::write_step(&dir, &legacy).unwrap();
        a.poll_disk().unwrap();
        assert!(
            a.plan
                .iter()
                .find(|p| p.name == "interest")
                .is_some_and(|p| p.steps.iter().any(|s| s.label == "legacy_interest_supply")),
            "a step on disk that the recorded scope does not name must stay navigable"
        );
        let (done, total, _) = a.progress();
        assert_eq!(
            (done, total),
            (want.len(), want.len()),
            "undeclared evidence cannot widen the denominator"
        );

        // A state-only edit: one certification withdrawn, evidence bytes unchanged.
        let mut st = RunState::load(&dir).unwrap();
        st.completed
            .get_mut("interest")
            .unwrap()
            .retain(|l| l != "interest_supply");
        st.save(&dir).unwrap();
        a.poll_disk().unwrap();
        let (done, total, _) = a.progress();
        assert_eq!(
            (done, total),
            (want.len() - 1, want.len()),
            "only the certification moved, so only progress moves"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn value_at_walks_objects_and_soroban_maps() {
        let doc = json!({"reserves": {"stable": {"data": {"b_supply": "10"}}},
                         "positions": {"sam": {"collateral": [[0, "5"]]}}});
        assert_eq!(
            value_at(&doc, "reserves.stable.data.b_supply"),
            Some(json!("10"))
        );
        assert_eq!(
            value_at(&doc, "positions.sam.collateral.0"),
            Some(json!("5")),
            "pair-encoded maps resolve by key"
        );
        assert_eq!(value_at(&doc, "reserves.nope.x"), None);
    }

    #[test]
    fn diff_tab_shows_both_sides_scaled_and_grouped() {
        let mut a = app();
        let mut ev = evidence("lending", "borrow", vec![]);
        // the comparator reports a path; the panel should resolve it on both sides
        ev.diff = vec![(
            "reserves.stable.data.b_supply".into(),
            crate::compare::Class::Violation("a=10 b=12 rel=2e-1".into()),
        )];
        ev.a.snapshot.reserves.insert(
            Tok::Stable,
            json!({"data": {"b_supply": "10000000", "d_supply": "5", "b_rate": "1"}}),
        );
        ev.b.snapshot.reserves.insert(
            Tok::Stable,
            json!({"data": {"b_supply": "12000000", "d_supply": "5", "b_rate": "1"}}),
        );
        a.apply(Ev::StepDone {
            evidence: Box::new(ev),
            violations: 1,
            desynced: false,
        });
        a.tab = Tab::Diff;
        let rows = a.tree_rows();
        assert_eq!(rows[0].key, "reserves", "grouped by area");
        let leaf = &rows[1];
        assert_eq!(leaf.key, "stable.data.b_supply");
        assert_eq!(
            leaf.value, "A 10   B 12",
            "real values, scaled to the reserve's 6 decimals"
        );
        assert!(matches!(
            leaf.class,
            Some(crate::compare::Class::Violation(_))
        ));
    }

    #[test]
    fn invariants_tab_leads_with_the_verdict_and_expands_evidence() {
        let mut a = app();
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("lending", "borrow", vec![])),
            violations: 0,
            desynced: false,
        });
        a.tab = Tab::Invariants;
        a.stream_width = 100;
        let rows = a.tree_rows();
        let head = &rows[0];
        assert!(
            head.key.starts_with("NOT RUN")
                || head.key.starts_with("NOT REQUESTED")
                || head.key.starts_with("PASS")
                || head.key.starts_with("FAIL")
                || head.key.starts_with("INCOMPLETE"),
            "verdict comes first: {:?}",
            head.key
        );
        assert!(head.expandable, "evidence hides behind an expand");
        let collapsed = rows.len();
        let clause = head.path.clone();
        a.expanded.insert(clause);
        assert!(
            a.tree_rows().len() > collapsed,
            "expanding shows the evidence"
        );
    }

    #[test]
    fn scval_reads_like_a_human_wrote_it() {
        let m = manifest();
        let names = address_names(&m);
        let n = |v: serde_json::Value| scval(&v, &names, None);
        assert_eq!(n(json!({"symbol": "mint"})), "mint");
        assert_eq!(n(json!({"i128": "123"})), "123");
        assert_eq!(n(json!({"bool": true})), "true");
        assert_eq!(n(json!({"vec": [{"symbol": "a"}, {"u32": 2}]})), "[a, 2]");
        assert_eq!(
            n(json!({"map": [{"key": {"symbol": "k"}, "val": {"u32": 7}}]})),
            "{k: 7}"
        );
        // a known identity resolves to its name, an unknown address is elided
        assert_eq!(n(json!({"address": "Gadmin"})), "admin");
        let long = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT";
        assert_eq!(n(json!({"address": long})), "CBEF4…F3AT");
        // amounts scale when the event names an asset
        assert_eq!(scval(&json!({"i128": "2000000"}), &names, Some(6)), "2");
    }

    #[test]
    fn diagnostics_keep_the_contract_message_and_drop_the_noise() {
        let m = manifest();
        let mut names = address_names(&m);
        names.insert("CPOOL".into(), "poolB".into());
        let stderr = "❌ error: transaction simulation failed: HostError: Error(Contract, #1200)\n\
            Event log (newest first):\n\
            \x200: [Diagnostic Event] contract:CPOOL, topics:[error, Error(Contract, #1200)], data:\"escalating error to VM trap from failed host function call: fail_with_error\"\n\
            \x201: [Diagnostic Event] contract:CPOOL, topics:[error, Error(Contract, #1200)], data:[\"failing with contract error\", 1200]";
        let d = diagnostics(stderr, &names);
        assert_eq!(d.len(), 1, "the boilerplate second line is dropped: {d:?}");
        assert!(d[0].starts_with("poolB: "), "contract is named: {:?}", d[0]);
        assert!(d[0].contains("escalating error to VM trap"));
        // with no diagnostics at all, fall back to the error line itself
        let plain = diagnostics("❌ error: request timed out", &names);
        assert_eq!(plain, vec!["request timed out"]);
    }

    #[test]
    fn wrap_breaks_on_spaces_and_never_drops_text() {
        let rows = wrap("alpha beta gamma delta", 11);
        assert_eq!(rows, vec!["alpha beta", "gamma delta"]);
        assert_eq!(wrap("short", 40), vec!["short"]);
        // an unbreakable blob is chunked rather than truncated
        let blob = "x".repeat(25);
        let rows = wrap(&blob, 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.concat(), blob, "wrapping must be lossless");
    }

    #[test]
    fn stream_tab_decodes_events_and_does_not_truncate() {
        let mut a = app();
        let mut ev = evidence("lending", "supply", vec![]);
        ev.b.invocation = Some(crate::stellar::Invocation {
            ok: true,
            value: Some(json!({"i128": "2000000"})),
            error: None,
            events: vec![crate::stellar::Event {
                contract: "C1".into(),
                topics: vec![json!({"symbol": "supply"}), json!({"address": "Gfrodo"})],
                value: json!({"i128": "2000000"}),
            }],
            stderr: serde_json::from_value(serde_json::Value::String(String::new()))
                .expect("RawText deserializes any string"),
        });
        a.apply(Ev::StepDone {
            evidence: Box::new(ev),
            violations: 0,
            desynced: false,
        });
        a.tab = Tab::Stream;
        let text: Vec<String> = a.tree_rows().into_iter().map(|r| r.value).collect();
        let joined = text.join("\n");
        assert!(
            joined.contains("● supply  frodo"),
            "topics decoded: {joined}"
        );
        assert!(!joined.contains("\"symbol\""), "no raw json: {joined}");
        assert!(!joined.contains('…'), "nothing truncated: {joined}");
    }

    #[test]
    fn scaled_renders_decimals() {
        assert_eq!(scaled("1599624110541", 7), "159962.4110541");
        assert_eq!(scaled("40097805571", 6), "40097.805571");
        assert_eq!(scaled("1000000000000", 12), "1");
        assert_eq!(scaled("1000073000000", 12), "1.000073");
        assert_eq!(scaled("5000000000", 9), "5");
        assert_eq!(scaled("1", 7), "0.0000001");
        assert_eq!(scaled("0", 7), "0");
        assert_eq!(scaled("-250000", 7), "-0.025");
        assert_eq!(scaled("9500000", 7), "0.95", "c_factor reads as a ratio");
        assert_eq!(scaled("not a number", 7), "not a number");
    }

    #[test]
    fn decimals_are_resolved_per_field() {
        let m = manifest(); // one reserve: stable, index 0, 6dp
        assert_eq!(decimals_for("reserves.stable.data.b_supply", &m), Some(6));
        assert_eq!(decimals_for("reserves.stable.data.b_rate", &m), Some(12));
        assert_eq!(decimals_for("reserves.stable.config.c_factor", &m), Some(7));
        assert_eq!(decimals_for("positions.sam.collateral.0", &m), Some(6));
        assert_eq!(decimals_for("balances.stable.frodo", &m), Some(6));
        assert_eq!(decimals_for("backstop_pool.shares", &m), Some(7));
        assert_eq!(
            decimals_for("config.status", &m),
            None,
            "status is a plain number"
        );
        assert_eq!(
            decimals_for("reserves.stable.asset", &m),
            None,
            "addresses stay raw"
        );
        // the ones missed on the first pass
        for (path, dp) in [
            ("reserves.stable.data.ir_mod", 7),
            ("reserves.stable.config.r_base", 7),
            ("reserves.stable.config.r_one", 7),
            ("reserves.stable.config.r_two", 7),
            ("reserves.stable.config.r_three", 7),
            ("reserves.stable.config.reactivity", 7),
            ("reserves.stable.config.supply_cap", 6),
            ("config.bstop_rate", 7),
            ("backstop_users.sam.q4w.0.amount", 7),
            ("backstop_pool.q4w_pct", 7),
            ("balances.lp.frodo", 7),
        ] {
            assert_eq!(decimals_for(path, &m), Some(dp), "{path}");
        }
        for raw in [
            "reserves.stable.data.last_time",
            "reserves.stable.config.index",
            "reserves.stable.config.decimals",
            "reserves.stable.scalar",
            "config.max_positions",
            "auctions.sam.block",
            "backstop_users.sam.q4w.0.exp",
        ] {
            assert_eq!(decimals_for(raw, &m), None, "{raw} should stay raw");
        }
    }

    #[test]
    fn snapshot_tab_shows_scaled_values() {
        let mut a = app();
        // the scale reading is about the values shown, so the fixture carries
        // raw values worth scaling instead of leaning on the shared zero state
        let mut ev = evidence("lending", "borrow", vec![]);
        ev.b.snapshot.reserves.insert(
            Tok::Stable,
            json!({"data": {
                "b_supply": "10", "d_supply": "0",
                "b_rate": "1", "d_rate": "1", "backstop_credit": "0"
            }}),
        );
        a.apply(Ev::StepDone {
            evidence: Box::new(ev),
            violations: 0,
            desynced: false,
        });
        a.tab = Tab::Snapshot;
        a.expanded.insert("reserves".into());
        a.expanded.insert("reserves.stable".into());
        a.expanded.insert("reserves.stable.data".into());
        let rows = a.tree_rows();
        let b_supply = rows
            .iter()
            .find(|r| r.key == "b_supply")
            .expect("b_supply row");
        assert_eq!(b_supply.value, "0.00001", "10 raw units at 6dp");
        let b_rate = rows.iter().find(|r| r.key == "b_rate").expect("b_rate row");
        assert_eq!(b_rate.value, "0.000000000001", "1 raw unit at 12dp");
    }

    #[test]
    fn diff_tab_filters_to_violations() {
        let mut a = app();
        // the shared fixture keeps its diff honest (equal snapshots, no
        // divergences); this test renders a violation, so it carries one
        let mut ev = evidence("lending", "borrow", vec![]);
        ev.diff = vec![(
            "reserves.stable.data.b_supply".into(),
            crate::compare::Class::Violation("a=1 b=2".into()),
        )];
        a.apply(Ev::StepDone {
            evidence: Box::new(ev),
            violations: 1,
            desynced: false,
        });
        a.tab = Tab::Diff;
        // one group header plus the leaf
        assert_eq!(a.tree_rows().len(), 2);
        assert_eq!(a.tree_rows()[0].key, "reserves");
        a.diff_only = true;
        assert_eq!(a.tree_rows().len(), 2, "the only leaf is a violation");
    }
}

// --- Protected market mode ---
//
// Market deploy/resume/watch carries no `PairManifest` and never builds a
// pair `App`. The state below holds only journal phase identities,
// market-policy rendered text, custody notes, and the final public summary.
// It is pure: no channels, no executor, no clock.

use crate::deploy::DeployEvent;
use crate::manifest::MarketManifest;

/// Market phase display state. The UI reflects what the journal and the
/// deployer emit; it never asserts settlement on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketPhaseState {
    Running,
    Failed,
}

pub struct MarketApp {
    /// Watch mode: no worker exists, nothing can sign, and only quit is
    /// bound to a key.
    pub watching: bool,
    /// `(phase id, label, state)` checklist rows.
    pub phases: Vec<(u32, String, MarketPhaseState)>,
    pub current: Option<u32>,
    /// Bounded stream of rendered deployment phase events.
    pub stream: Vec<String>,
    /// Summary of the confirmation gate currently awaiting an operator
    /// `y`. The token is deliberately not stored anywhere here: it binds
    /// the journal phase/action id, the exact testnet, the freshly
    /// re-resolved signer G-address, the target, and the redacted payload
    /// digest, and it is never display material.
    pub confirm_summary: Option<String>,
    pub custody: Vec<String>,
    pub unresolved: bool,
    pub failed: Option<String>,
    pub halted: Option<String>,
    pub done: bool,
    pub summary: Vec<String>,
    pub quit_requested: bool,
    pub status_msg: String,
}

impl MarketApp {
    pub fn new() -> Self {
        Self {
            watching: false,
            phases: Vec::new(),
            current: None,
            stream: Vec::new(),
            confirm_summary: None,
            custody: Vec::new(),
            unresolved: false,
            failed: None,
            halted: None,
            done: false,
            summary: Vec::new(),
            quit_requested: false,
            status_msg: String::new(),
        }
    }

    /// Read-only screen for a completed market run
    /// (`market tui --mode watch`). Nothing here can sign: there is no
    /// worker and no execution context.
    pub fn watch(manifest: &MarketManifest) -> Self {
        let mut s = Self::new();
        s.watching = true;
        s.done = true;
        s.summary = market_summary_lines(manifest);
        s
    }

    pub fn status(&mut self, msg: &str) {
        self.status_msg = msg.to_string();
    }

    /// Pure transition over the deployment event grammar. Every `Phase`
    /// event clears any pending confirmation, so a gate never survives a
    /// phase transition and a stale `y` can never fire a later phase.
    pub fn apply_market(&mut self, ev: &DeployEvent) {
        match ev {
            DeployEvent::Phase { id, label } => {
                self.confirm_summary = None;
                let id = *id as u32;
                match self.phases.iter_mut().find(|(p, _, _)| *p == id) {
                    Some(row) => row.2 = MarketPhaseState::Running,
                    None => self
                        .phases
                        .push((id, label.to_string(), MarketPhaseState::Running)),
                }
                self.current = Some(id);
                self.push_stream(format!("phase {id}: {label}"));
            }
            DeployEvent::Confirm { summary, .. } => {
                self.confirm_summary = Some(summary.to_string())
            }
            DeployEvent::Custody { unresolved, note } => {
                self.custody
                    .push(format!("custody unresolved={unresolved}: {note}"));
                if *unresolved {
                    self.unresolved = true;
                }
            }
            DeployEvent::Done(m) => {
                self.confirm_summary = None;
                self.current = None;
                self.done = true;
                self.summary = market_summary_lines(m);
            }
            DeployEvent::Failed(reason) => {
                self.confirm_summary = None;
                if let Some(id) = self.current.take() {
                    if let Some(row) = self.phases.iter_mut().find(|(p, _, _)| *p == id) {
                        row.2 = MarketPhaseState::Failed;
                    }
                }
                self.failed = Some(reason.to_string());
            }
        }
    }

    fn push_stream(&mut self, line: String) {
        self.stream.push(line);
        if self.stream.len() > STREAM_CAP {
            let excess = self.stream.len() - STREAM_CAP;
            self.stream.drain(..excess);
        }
    }
}

/// The public deployment summary: `Safe` text and checksum-validated
/// identifiers only. These are the market's deployed addresses, which are
/// public by design; no alias, config path, or secret can appear here.
pub fn market_summary_lines(m: &MarketManifest) -> Vec<String> {
    let mut v = vec![
        format!("network: {}", m.network.label),
        format!("pool administrator (public G-address): {}", m.admin),
        format!("rpc endpoint (redacted): {}", m.rpc_host),
        format!("proxy runtime: {}", m.oracle.runtime),
    ];
    if let Some(governance) = &m.oracle.governance {
        v.push(format!("proxy governance: {governance}"));
    }
    v.push(format!("oracle adapter (immutable): {}", m.oracle.adapter));
    for (n, s) in m.oracle.sources.iter().enumerate() {
        v.push(format!("oracle source {}: {s}", n + 1));
    }
    v.push(format!("oracle base: {}", m.oracle.base));
    v.push(format!(
        "single-authority synthetic quorum: {} sources, min_sources {}",
        m.oracle.sources.len(),
        m.oracle.min_sources
    ));
    for (tok, id) in &m.tokens {
        v.push(format!("token {tok:?}: {id}"));
    }
    v.push(format!("comet: {}", m.comet));
    v.push(format!("emitter: {}", m.emitter));
    v.push(format!("backstop: {}", m.backstop));
    v.push(format!("factory: {}", m.factory));
    v.push(format!("pool: {}", m.pool));
    for (tok, r) in &m.reserves {
        v.push(format!(
            "reserve {tok:?}: index {} decimals {}",
            r.index, r.decimals
        ));
    }
    for (who, id) in &m.identities {
        v.push(format!("identity {who:?}: {id}"));
    }
    v.push(format!("deploy ledger height: {}", m.deploy_ledger));
    v
}

#[cfg(test)]
mod market_tests {
    use super::*;
    use crate::manifest::{MarketNetworkRecord, OracleStackManifest};
    use crate::stellar::{AccountId, ContractId, Safe};
    use std::str::FromStr;

    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const C_CONTRACT: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";

    fn fixture_manifest() -> MarketManifest {
        let cid = || ContractId::from_str(C_CONTRACT).unwrap();
        MarketManifest {
            real: None,
            network: MarketNetworkRecord {
                label: Safe::literal("testnet"),
                passphrase: Safe::literal("Test SDF Network ; September 2015"),
            },
            admin: AccountId::from_str(G_ADMIN).unwrap(),
            rpc_host: Safe::literal("soroban-testnet.stellar.org"),
            oracle: OracleStackManifest {
                runtime: cid(),
                governance: Some(cid()),
                adapter: cid(),
                sources: vec![cid(), cid(), cid()],
                min_sources: 3,
                max_age_secs: 3600,
                max_clock_drift_secs: 60,
                decimals: 7,
                resolution: 300,
                base: Safe::literal("USD"),
            },
            tokens: BTreeMap::new(),
            pool: cid(),
            backstop: cid(),
            factory: cid(),
            comet: cid(),
            emitter: cid(),
            reserves: BTreeMap::new(),
            identities: BTreeMap::new(),
            deploy_ledger: 1,
        }
    }

    #[test]
    fn pending_confirmation_clears_on_phase_transition() {
        let mut s = MarketApp::new();
        s.apply_market(&DeployEvent::Confirm {
            token: "0123456789abcdef".into(),
            summary: Safe::literal("mutation awaiting confirmation"),
        });
        assert!(s.confirm_summary.is_some());
        s.apply_market(&DeployEvent::Phase {
            id: 3,
            label: Safe::literal("deploy sources"),
        });
        assert!(
            s.confirm_summary.is_none(),
            "a gate cannot survive a phase transition"
        );
    }

    #[test]
    fn terminal_events_clear_the_gate_and_are_recorded() {
        let mut s = MarketApp::new();
        s.apply_market(&DeployEvent::Phase {
            id: 7,
            label: Safe::literal("governance"),
        });
        s.apply_market(&DeployEvent::Failed(Safe::literal("halted by operator")));
        assert!(s.confirm_summary.is_none());
        assert_eq!(s.failed.as_deref(), Some("halted by operator"));
        assert_eq!(
            s.phases.iter().find(|(p, _, _)| *p == 7).map(|r| r.2),
            Some(MarketPhaseState::Failed)
        );
    }

    #[test]
    fn done_publishes_the_public_summary() {
        let mut s = MarketApp::new();
        s.apply_market(&DeployEvent::Done(Box::new(fixture_manifest())));
        assert!(s.done);
        assert!(s.confirm_summary.is_none());
        assert!(s
            .summary
            .iter()
            .any(|l| l.starts_with("pool administrator (public G-address): GAAAA")));
        assert!(s.summary.iter().any(|l| l.starts_with("pool: CAAAA")));
        assert!(s
            .summary
            .iter()
            .any(|l| l.contains("single-authority synthetic quorum")));
    }

    #[test]
    fn unresolved_custody_is_flagged_and_kept() {
        let mut s = MarketApp::new();
        s.apply_market(&DeployEvent::Custody {
            unresolved: true,
            note: Safe::literal("submission outcome unresolved"),
        });
        assert!(s.unresolved);
        assert_eq!(s.custody.len(), 1);
        assert!(s.custody[0].contains("unresolved=true"));
    }

    #[test]
    fn watch_screen_starts_finished() {
        let s = MarketApp::watch(&fixture_manifest());
        assert!(s.watching && s.done && s.confirm_summary.is_none());
    }

    #[test]
    fn market_stream_is_capped() {
        let mut s = MarketApp::new();
        for i in 0..(STREAM_CAP + 500) {
            s.apply_market(&DeployEvent::Phase {
                id: i,
                label: Safe::literal("operator progress"),
            });
        }
        assert_eq!(s.stream.len(), STREAM_CAP);
        assert_eq!(
            s.stream.first().map(String::as_str),
            Some("phase 500: operator progress")
        );
        let final_phase = format!("phase {}: operator progress", STREAM_CAP + 499);
        assert_eq!(
            s.stream.last().map(String::as_str),
            Some(final_phase.as_str())
        );
    }
}
