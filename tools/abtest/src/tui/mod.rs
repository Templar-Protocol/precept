//! Operator terminal: drives a run in a worker thread, or watches one read-only.

pub mod app;
mod market_action_form;
pub mod market_explorer;
pub mod market_status_view;
pub mod palette;
pub mod render;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::deploy::{DeployAct, DeployEvent, MarketDeployer};
use crate::manifest::{MarketManifest, PairManifest, RunLock};
use crate::run::{Act, Ev, Runner, Sides};
use crate::stellar::{Capture, CmdOutput, Executor, Net, Policy, RealExecutor, Safe, Stellar};

use app::{App, MarketApp, Mode, Pending, RunStatus, Tab};
use palette::{choices, Palette, Stage};

const TICK: Duration = Duration::from_millis(250);
const DRAIN_PER_TICK: usize = 500;

// --- Event loop ---

/// Wraps an executor so every CLI call shows up in the stream panel.
struct TapExecutor<E: Executor> {
    inner: E,
    /// `observe::parallel` calls this from up to 8 threads, so the sender is shared
    /// under a mutex. Contention is a handful of sends per second.
    tap: Mutex<Sender<Ev>>,
}

impl<E: Executor> TapExecutor<E> {
    fn tapped(
        &self,
        args: &[String],
        run: impl FnOnce() -> anyhow::Result<CmdOutput>,
    ) -> anyhow::Result<CmdOutput> {
        let started = Instant::now();
        let out = run();
        if let Ok(o) = &out {
            // Form (a) is the Tap argv join plus the 2000-char stderr head,
            // composed inside `stellar.rs` from the module-private stream
            // fields. `pair_tap` reproduces the pair bytes verbatim under
            // `Policy::Pair`, and `to_string` emits them unchanged, so the
            // pair stream pane is byte-identical to before the retype. The
            // transient classification is asked in `app.rs` over these event
            // bytes by the single law in `stellar.rs`; same bytes, same law,
            // same verdict, so the pair retry backoff keeps running
            let (argv, stderr) = Safe::pair_tap(o, args);
            let ev = Ev::Cli {
                argv: argv.to_string(),
                ok: o.ok(),
                ms: started.elapsed().as_millis() as u64,
                stderr: stderr.to_string(),
                transient: o.transient(),
            };
            if let Ok(tx) = self.tap.lock() {
                let _ = tx.send(ev);
            }
        }
        out
    }
}

impl<E: Executor> Executor for TapExecutor<E> {
    fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput> {
        self.tapped(args, || self.inner.run(args))
    }

    fn run_by(
        &self,
        args: &[String],
        start_by: std::time::SystemTime,
    ) -> anyhow::Result<CmdOutput> {
        self.tapped(args, || self.inner.run_by(args, start_by))
    }
}

pub fn main(
    out: &Path,
    stellar_bin: PathBuf,
    net: Net,
    scenario: String,
    from_step: Option<String>,
    watch: bool,
) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "the TUI needs a terminal; run it directly rather than through a pipe or a non-interactive session (headless: `run --json`)"
    );
    let manifest = PairManifest::load(out).context("no manifest; run deploy first")?;
    let lock = if watch { None } else { RunLock::acquire(out)? };
    if !watch && lock.is_none() {
        let pid = RunLock::owner(out).unwrap_or(0);
        eprintln!(
            "run {} is owned by pid {pid}; opening read-only",
            out.display()
        );
    }

    let (ev_tx, ev_rx) = channel::<Ev>();
    let (act_tx, act_rx) = channel::<Act>();
    let driving = lock.is_some();
    let worker = if driving {
        Some(spawn_worker(
            out.to_path_buf(),
            stellar_bin,
            net,
            scenario,
            from_step,
            ev_tx,
            act_rx,
        ))
    } else {
        drop(ev_tx);
        None
    };

    let mut state = App::new(
        out.to_path_buf(),
        manifest,
        if driving { Some(act_tx) } else { None },
    );
    state.poll_disk().ok();

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut state, &ev_rx, worker.as_ref());
    ratatui::restore();
    drop(lock);
    result?;
    Ok(0)
}

fn spawn_worker(
    out: PathBuf,
    bin: PathBuf,
    net: Net,
    scenario: String,
    from_step: Option<String>,
    tx: Sender<Ev>,
    acts: Receiver<Act>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let exec = TapExecutor {
            inner: RealExecutor {
                bin,
                xdg: out.join(".xdg"),
                cwd: out.clone(),
                capture: Capture::Pair,
                rpc_env: Vec::new(),
            },
            tap: Mutex::new(tx.clone()),
        };
        let st = Stellar::new(&net, &exec, Policy::Pair);
        let manifest = match PairManifest::load(&out) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(Ev::RunFinished {
                    error: Some(Safe::error_chain(Policy::Pair, &e)),
                });
                return;
            }
        };
        // The hook reports an event and blocks for the operator's reply at gates.
        let hook = |ev: Ev| -> Act {
            let gate = matches!(
                ev,
                Ev::StepStarted { .. } | Ev::StepDone { .. } | Ev::Halt { .. } | Ev::Waiting { .. }
            );
            if tx.send(ev).is_err() {
                return Act::Abort;
            }
            if !gate {
                return Act::Continue;
            }
            acts.recv().unwrap_or(Act::Abort)
        };
        let mut runner = match Runner::new(&st, &manifest, &out) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Ev::RunFinished {
                    error: Some(Safe::error_chain(Policy::Pair, &e)),
                });
                return;
            }
        };
        runner.hook = Some(&hook);
        let only = Some(scenario.as_str());
        let _ = runner.run(only, from_step.as_deref());
    })
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    state: &mut App,
    rx: &Receiver<Ev>,
    worker: Option<&JoinHandle<()>>,
) -> anyhow::Result<()> {
    let mut last_poll = Instant::now();
    loop {
        for _ in 0..DRAIN_PER_TICK {
            match rx.try_recv() {
                Ok(ev) => {
                    let gate = is_gate(&ev);
                    state.apply(ev);
                    // Auto-continue unless the operator is pausing or the run halted.
                    if gate && !state.watching() {
                        match &state.run {
                            RunStatus::Paused | RunStatus::Halted(_) => {}
                            _ => state.send(Act::Continue),
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if let RunStatus::Running = state.run {
                        state.run = RunStatus::Finished("runner stopped".into());
                    }
                    break;
                }
            }
        }
        if let Some(w) = worker {
            if w.is_finished() && matches!(state.run, RunStatus::Running) {
                state.run = RunStatus::Finished("runner exited".into());
            }
        }
        if state.watching() && last_poll.elapsed() > Duration::from_secs(2) {
            state.poll_disk().ok();
            last_poll = Instant::now();
        }

        terminal.draw(|f| render::render(state, f))?;

        if event::poll(TICK)? {
            if let event::Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if !dispatch(state, key)? {
                    return Ok(());
                }
            }
        }
    }
}

pub(crate) fn is_gate(ev: &Ev) -> bool {
    matches!(
        ev,
        Ev::StepStarted { .. } | Ev::StepDone { .. } | Ev::Halt { .. } | Ev::Waiting { .. }
    )
}

/// `Ok(false)` quits. Quitting is never signalled by an error.
pub fn dispatch(state: &mut App, key: KeyEvent) -> anyhow::Result<bool> {
    match state.mode {
        Mode::Normal => handle_normal(state, key),
        Mode::Help | Mode::Info => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('i') | KeyCode::Char('q')
            ) {
                state.mode = Mode::Normal;
            }
            Ok(true)
        }
        Mode::Palette => {
            handle_palette(state, key);
            Ok(true)
        }
        Mode::Jump => {
            handle_jump(state, key);
            Ok(true)
        }
        Mode::Search => {
            handle_search(state, key);
            Ok(true)
        }
        Mode::Confirm => handle_confirm(state, key),
    }
}

fn handle_normal(state: &mut App, key: KeyEvent) -> anyhow::Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => return quit(state),
        KeyCode::Char('q') => return quit(state),
        KeyCode::Char('?') => state.mode = Mode::Help,
        KeyCode::Char('i') => state.mode = Mode::Info,
        KeyCode::Char(c @ '1'..='8') => {
            state.tab = Tab::ALL[c as usize - '1' as usize];
            state.detail_sel = 0;
        }
        KeyCode::Char('j') | KeyCode::Down => state.detail_sel += 1,
        KeyCode::Char('k') | KeyCode::Up => state.detail_sel = state.detail_sel.saturating_sub(1),
        KeyCode::Char('J') => state.select_down(),
        KeyCode::Char('K') => state.select_up(),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => state.toggle_expand(),
        KeyCode::Char('h') | KeyCode::Left => state.toggle_expand(),
        KeyCode::Char(' ') => {
            if state.pending_acts > 0 {
                state.status_msg = "an action is already queued for the next gate".into();
                return Ok(true);
            }
            // Space resumes both operator stops alike; a Halted run was otherwise
            // left mislabeled "paused" with the worker still waiting.
            if matches!(state.run, RunStatus::Paused | RunStatus::Halted(_)) {
                state.run = RunStatus::Running;
                state.send(Act::Continue);
            } else if state.watching() || matches!(state.run, RunStatus::Finished(_)) {
                state.status_msg = "no live worker: nothing to pause".into();
            } else {
                state.run = RunStatus::Paused;

                state.status_msg = "paused at the next gate".into();
            }
        }
        KeyCode::Char('s') => {
            if state.pending_acts > 0 {
                state.status_msg = "an action is already queued for the next gate".into();
                return Ok(true);
            }
            if state.watching() || matches!(state.run, RunStatus::Finished(_)) {
                state.status_msg = "no live worker: nothing to step".into();
            } else {
                state.run = RunStatus::Paused;

                state.send(Act::Continue);
                state.status_msg = "single step".into();
            }
        }
        KeyCode::Char('n') => {
            if state.pending_acts > 0 {
                state.status_msg = "an action is already queued for the next gate".into();
                return Ok(true);
            }
            state.send(Act::Skip);
            // The operator has decided; the next gate is ordinary again.
            if matches!(state.run, RunStatus::Halted(_)) {
                state.run = RunStatus::Running;
            }
        }
        KeyCode::Char('R') => {
            if let Some(step) = state.selected_step() {
                let label = step.label.clone();
                if let Some(ls) = crate::scenario::scenarios()
                    .into_iter()
                    .flat_map(|s| s.steps)
                    .find(|s| s.label == label)
                {
                    state.pending = Some(Pending::Rerun(Box::new(ls)));
                    state.mode = Mode::Confirm;
                } else {
                    state.status_msg = format!("{label} is not a scenario step");
                }
            }
        }
        KeyCode::Char('g') => {
            state.jump_sel = 0;
            state.mode = Mode::Jump;
        }
        KeyCode::Char('G') => {
            state.follow = true;
            state.selected = state.steps.len().saturating_sub(1);
        }
        KeyCode::Char(':') => {
            state.palette = Palette {
                seq: state.steps.len() + 1,
                ..Default::default()
            };
            state.mode = Mode::Palette;
        }
        KeyCode::Char('p') => state.send(Act::Snapshot),
        KeyCode::Char('a') => state.side = 'a',
        KeyCode::Char('b') => state.side = 'b',
        KeyCode::Char('d') => state.diff_only = !state.diff_only,
        KeyCode::Char('f') => state.follow = !state.follow,
        KeyCode::Char('A') => {
            state.auto_continue = !state.auto_continue;
            state.status_msg = format!("auto-continue {}", state.auto_continue);
        }
        KeyCode::Char('/') => {
            state.search.clear();
            state.mode = Mode::Search;
        }
        KeyCode::Char('F') if state.watching() => {
            state.pending = Some(Pending::ForceLock);
            state.mode = Mode::Confirm;
        }
        KeyCode::Esc => state.search.clear(),
        _ => {}
    }
    Ok(true)
}

fn quit(state: &mut App) -> anyhow::Result<bool> {
    if state.watching() {
        return Ok(false);
    }
    if state.mode == Mode::Confirm {
        return Ok(false);
    }
    state.pending = Some(Pending::Quit);
    state.mode = Mode::Confirm;
    Ok(true)
}

fn handle_confirm(state: &mut App, key: KeyEvent) -> anyhow::Result<bool> {
    match key.code {
        KeyCode::Enter
            if matches!(
                state.pending,
                Some(Pending::Rerun(_)) | Some(Pending::ExecOp { .. })
            ) =>
        {
            // Value-moving steps confirm with 'y' only: the palette trains an
            // Enter rhythm, and a stray third Enter must not move value.
            state.status_msg = "press y to fire, n to discard".into();
        }
        KeyCode::Char('y') | KeyCode::Enter => {
            let pending = state.pending.take();
            state.mode = Mode::Normal;
            match pending {
                Some(Pending::Quit) => {
                    state.send(Act::Abort);
                    return Ok(false);
                }
                Some(Pending::Rerun(step)) => {
                    let sides = state.palette.sides;
                    state.send(Act::Exec { step, sides });
                }
                Some(Pending::ExecOp { step, sides }) => {
                    state.send(Act::Exec { step, sides });
                }
                Some(Pending::ForceLock) => {
                    state.status_msg = "restart with --out to take the lock".into();
                }
                None => {}
            }
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            state.pending = None;
            state.mode = Mode::Normal;
        }
        _ => {}
    }
    Ok(true)
}

fn handle_palette(state: &mut App, key: KeyEvent) {
    let len = match state.palette.stage {
        Stage::Pick => state.palette.matches().len(),
        Stage::Arg(_) => state
            .palette
            .current_arg()
            .map(|a| choices(a).len())
            .unwrap_or(0),
        Stage::Ready => 0,
    };
    match key.code {
        KeyCode::Esc => {
            state.mode = Mode::Normal;
            state.palette = Palette::default();
        }
        KeyCode::Tab => {
            state.palette.sides = match state.palette.sides {
                Sides::Both => Sides::A,
                Sides::A => Sides::B,
                Sides::B => Sides::Both,
            }
        }
        KeyCode::Down => state.palette.sel = (state.palette.sel + 1).min(len.saturating_sub(1)),
        KeyCode::Up => state.palette.sel = state.palette.sel.saturating_sub(1),
        KeyCode::Enter => {
            if state.palette.stage == Stage::Ready {
                if let Some(step) = state.palette.build() {
                    // Firing a real op takes the same confirm as a re-run: a stray
                    // Enter must not move value.
                    state.pending = Some(Pending::ExecOp {
                        step: Box::new(step),
                        sides: state.palette.sides,
                    });
                    state.mode = Mode::Confirm;
                    state.status_msg = "confirm the operator step: y fires, n discards".into();
                }
                state.palette = Palette::default();
            } else if !state.palette.accept() {
                state.status_msg = "invalid argument: expected a number".into();
            }
        }
        KeyCode::Backspace => {
            if state.palette.stage == Stage::Pick {
                state.palette.query.pop();
            } else {
                state.palette.buf.pop();
            }
        }
        KeyCode::Char(c) => {
            if state.palette.stage == Stage::Pick {
                state.palette.query.push(c);
                state.palette.sel = 0;
            } else {
                state.palette.buf.push(c);
            }
        }
        _ => {}
    }
}

fn handle_jump(state: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => state.mode = Mode::Normal,
        KeyCode::Down | KeyCode::Char('j') => {
            state.jump_sel = (state.jump_sel + 1).min(state.plan.len().saturating_sub(1))
        }
        KeyCode::Up | KeyCode::Char('k') => state.jump_sel = state.jump_sel.saturating_sub(1),
        KeyCode::Enter => {
            if let Some(sc) = state.plan.get(state.jump_sel) {
                let name = sc.name.clone();
                state.status_msg = match state.steps.iter().position(|s| s.scenario == name) {
                    Some(i) => {
                        state.selected = i;
                        state.detail_sel = 0;
                        state.follow = false;
                        format!("jumped to {name}")
                    }
                    None => format!("{name} has no steps yet"),
                };
            }
            state.mode = Mode::Normal;
        }
        _ => {}
    }
}

fn handle_search(state: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            state.search.clear();
            state.mode = Mode::Normal;
        }
        KeyCode::Enter => state.mode = Mode::Normal,
        KeyCode::Backspace => {
            state.search.pop();
        }
        KeyCode::Char(c) => state.search.push(c),
        _ => {}
    }
}

// --- Protected market mode ---
//
// Market deployment and recovery are driven from this terminal over the
// same `MarketDeployer` state machine the headless commands run; only the
// presentation differs. This mode starts with no `PairManifest` (none is
// required to exist), never takes or forces a pair `RunLock`, and no
// mutation proceeds until its exact confirmation token arrives from an
// operator `y`.

/// Worker-side gate rule: only the exact token that accompanied a pending
/// confirmation releases it. `Init`, `Continue`, `Halt`, and stale or
/// mismatched tokens cannot satisfy a gate; the deployer side double-checks
/// and a wrong token is terminal there, never a continue.
pub(crate) fn gate_honors(stored: &str, act: &DeployAct) -> bool {
    matches!(act, DeployAct::Confirm { token } if token == stored)
}

/// A terminal that reports no usable geometry — zero rows or columns, or
/// a size the terminal cannot even answer (a detached `script(1)` harness
/// leaves its pty at 0x0) — would give every market frame an empty area:
/// the operator could not see the phases, the stream, or a confirmation
/// gate while a mutating deployment ran. Recover loudly: impose the
/// canonical 80x24 through the system `stty` (a real `TIOCSWINSZ`;
/// crossterm's unix `SetSize` only emits an escape and cannot resize),
/// then re-read; if the terminal still cannot present a screen, stop
/// with a clear diagnostic rather than draw blanks. A real terminal
/// reports a real size, so an operator's window is never resized.
fn recover_gate_geometry() -> anyhow::Result<()> {
    fn degenerate() -> bool {
        match ratatui::crossterm::terminal::size() {
            Ok((w, h)) => w == 0 || h == 0,
            Err(_) => true,
        }
    }
    if degenerate() {
        let _ = std::process::Command::new("stty")
            .args(["rows", "24", "cols", "80"])
            .status();
        if degenerate() {
            eprintln!("the terminal reports no usable size; the market gate cannot be displayed");
            anyhow::bail!(
                "terminal reports no size; market deploy/resume needs a screen for the \
                 confirmation gate (headless: `market deploy` or `market resume`)"
            );
        }
    }
    Ok(())
}

/// Watch renders without spawning anything: no worker, no executor, and
/// no geometry recovery either — that is the whole point of the mode. A
/// terminal that cannot report any size cannot display the read-only
/// summary, so it refuses with a loud error instead of drawing blanks.
fn ensure_watch_geometry() -> anyhow::Result<()> {
    match ratatui::crossterm::terminal::size() {
        Ok((0, _)) | Ok((_, 0)) | Err(_) => anyhow::bail!(
            "terminal reports no size; the read-only market screen cannot be displayed"
        ),
        Ok(_) => Ok(()),
    }
}

/// Entry for `market tui --mode deploy|resume`. The deployer is built at
/// the command boundary under the market policy with the market journal in
/// this run directory; it is moved whole into the worker thread.
pub fn main_market_deploy(deployer: MarketDeployer) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "the market TUI needs a terminal; headless: `market deploy` or `market resume`"
    );
    recover_gate_geometry()?;
    let (ev_tx, ev_rx) = channel::<DeployEvent>();
    let (act_tx, act_rx) = channel::<DeployAct>();
    let worker = std::thread::spawn(move || market_worker(deployer, ev_tx, act_rx));
    let mut state = MarketApp::new();
    let mut terminal = ratatui::init();
    let result = market_event_loop(&mut terminal, &mut state, &ev_rx, &act_tx);
    ratatui::restore();
    // Every exit route — quit included — first joins the worker. While a
    // mutating child is in flight the worker is inside `step`, which does
    // not return until the child has finished and the deployer has durably
    // recorded its classified outcome, so this join is the mechanism that
    // keeps the TUI from releasing anything early. A hard crash instead
    // leaves the intent unresolved for `market resume`; a point-in-time
    // absence is never proof of non-submission.
    eprintln!("finishing the current mutation; do not kill this process");
    let _ = act_tx.send(DeployAct::Halt);
    let worker_panicked = worker.join().is_err();
    if worker_panicked {
        // A dead worker must never masquerade as a clean stop. The panic
        // payload is deliberately not echoed here — market surfaces keep
        // operator-visible text input-free, and the default panic hook has
        // already recorded the payload for the logs. What this call site
        // guarantees is only that the run ends failing and the journal's
        // unresolved intent stays exactly that for `market resume`.
        eprintln!("deployment worker died before completing the run");
    }
    result?;
    if state.unresolved || state.failed.is_some() || worker_panicked {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Entry for `market tui --mode watch`: a completed market run, read
/// only. There is no worker and no execution context, so nothing here can
/// sign or mutate.
pub fn main_market_watch(manifest: MarketManifest) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "the market watch screen needs a terminal"
    );
    let (ev_tx, ev_rx) = channel::<DeployEvent>();
    drop(ev_tx);
    let mut state = MarketApp::watch(&manifest);
    ensure_watch_geometry()?;
    let mut terminal = ratatui::init();
    let result = watch_event_loop(&mut terminal, &mut state, &ev_rx);
    ratatui::restore();
    result?;
    Ok(0)
}

fn market_worker(
    mut deployer: MarketDeployer,
    ui_ev: Sender<DeployEvent>,
    ui_acts: Receiver<DeployAct>,
) {
    let mut act = DeployAct::Init;
    let mut gate: Option<String> = None;
    loop {
        if gate.is_some() {
            // Worker endpoint of the confirmation protocol: this thread
            // blocks until the operator's single matching confirmation or a
            // Halt arrives. Stray acts are discarded here, never queued, so
            // nothing can be consumed by a later gate. `Continue` cannot
            // satisfy a gate at this endpoint either — the auto-continue
            // below is unreachable while a gate is open.
            loop {
                match ui_acts.recv() {
                    Ok(DeployAct::Halt) => {
                        act = DeployAct::Halt;
                        break;
                    }
                    Ok(a @ DeployAct::Confirm { .. }) => {
                        if gate.as_deref().is_some_and(|t| gate_honors(t, &a)) {
                            gate = None;
                            act = a;
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
        let ev = match deployer.step(act) {
            Ok(ev) => ev,
            Err(e) => {
                let _ = ui_ev.send(DeployEvent::Failed(Safe::error_chain(Policy::Market, &e)));
                return;
            }
        };
        let unresolved = matches!(
            &ev,
            DeployEvent::Custody {
                unresolved: true,
                ..
            }
        );
        match &ev {
            DeployEvent::Confirm { token, .. } => gate = Some(token.clone()),
            DeployEvent::Phase { .. } => gate = None,
            DeployEvent::Done(_) | DeployEvent::Failed(_) => {
                let _ = ui_ev.send(ev);
                return;
            }
            DeployEvent::Custody { .. } => {}
        }
        if ui_ev.send(ev).is_err() {
            // The terminal is gone. Stop driving; anything unsettled stays
            // unresolved in the journal for public reconciliation and
            // `market resume` — never silently continued.
            return;
        }
        if unresolved {
            return;
        }
        act = next_act(&ui_acts);
    }
}

fn market_event_loop(
    terminal: &mut DefaultTerminal,
    state: &mut MarketApp,
    rx: &Receiver<DeployEvent>,
    acts: &Sender<DeployAct>,
) -> anyhow::Result<()> {
    let mut gate: Option<String> = None;
    loop {
        let mut worker_gone = false;
        for _ in 0..DRAIN_PER_TICK {
            match rx.try_recv() {
                Ok(ev) => {
                    if let DeployEvent::Confirm { token, .. } = &ev {
                        // Entering a gate first drains stale input: no key
                        // pressed earlier — including an old `y` — can act
                        // as the confirmation.
                        drain_stale_input()?;
                        gate = Some(token.clone());
                    } else if matches!(ev, DeployEvent::Phase { .. }) {
                        gate = None;
                    }
                    state.apply_market(&ev);
                }
                Err(TryRecvError::Empty) => break,
                // The worker endpoint is closed. Everything it managed to
                // emit has been applied; draw the resulting state once so
                // the terminal outcome is visible, then stop. Exiting
                // silently is what turned a dead worker into an empty
                // screen with no explanation.
                Err(TryRecvError::Disconnected) => {
                    worker_gone = true;
                    break;
                }
            }
        }
        terminal.draw(|f| render::render_market(state, f))?;
        if worker_gone {
            return Ok(());
        }
        if event::poll(TICK)? {
            if let event::Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match market_key(state, &mut gate, &key) {
                    MarketKey::Discarded => {}
                    MarketKey::Send(act) => {
                        if acts.send(act).is_err() {
                            return Ok(());
                        }
                    }
                    MarketKey::Quit => {
                        // Quit is a halt request: it can only take effect
                        // between journal steps, i.e. after any in-flight
                        // child has finished and its classified outcome is
                        // durable. The caller joins the worker before exit.
                        state.quit_requested = true;
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn watch_event_loop(
    terminal: &mut DefaultTerminal,
    state: &mut MarketApp,
    rx: &Receiver<DeployEvent>,
) -> anyhow::Result<()> {
    loop {
        match rx.try_recv() {
            Ok(ev) => state.apply_market(&ev),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => {}
        }
        terminal.draw(|f| render::render_market(state, f))?;
        if event::poll(TICK)? {
            if let event::Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if key.code == KeyCode::Char('q') || (key.code == KeyCode::Char('c') && ctrl) {
                    return Ok(());
                }
                // Everything else is inert by construction: watch has no
                // worker and no act channel, so there is nothing to drive.
            }
        }
    }
}

/// Event-boundary decision: if the operator has requested a halt, the
/// next act is that halt, so the deployer records the run's classified
/// halted outcome at the next journal step instead of continuing the
/// graph. Every other act is discarded here — only an exact matching
/// confirmation token advances a gate (enforced above) — and the absence
/// of operator intent keeps advancing the authorized deployment.
pub(crate) fn next_act(acts: &Receiver<DeployAct>) -> DeployAct {
    match acts.try_recv() {
        Ok(DeployAct::Halt) => DeployAct::Halt,
        _ => DeployAct::Continue,
    }
}
pub(crate) enum MarketKey {
    Discarded,
    Send(DeployAct),
    Quit,
}

/// Market-mode key routing. Pair tabs, pair diff/filter controls, skip,
/// single-step, rerun, and the operator palette have no bindings here: the
/// market state machine owns the chain while a deployment is incomplete,
/// and post-deployment actions go through `market play` or the market
/// palette, never through pair controls. The confirmation gate consumes
/// exactly one `y`; Enter alone never fires; anything else is discarded,
/// never queued.
pub(crate) fn market_key(
    state: &mut MarketApp,
    gate: &mut Option<String>,
    key: &KeyEvent,
) -> MarketKey {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => MarketKey::Quit,
        KeyCode::Char('q') => MarketKey::Quit,
        KeyCode::Char('y') => match gate.take() {
            // Exactly one `y` per gate: the token is moved out on the
            // first press, so any second press finds nothing to confirm.
            Some(token) => MarketKey::Send(DeployAct::Confirm { token }),
            None => {
                state.status("nothing is awaiting confirmation");
                MarketKey::Discarded
            }
        },
        KeyCode::Enter => {
            if gate.is_some() {
                state.status("press y to confirm; Enter alone never fires");
            }
            MarketKey::Discarded
        }
        _ => MarketKey::Discarded,
    }
}

/// Drop every input queued before a confirmation gate opened. A failed drain
/// must never convert a stale `y` into authorization to send.
fn drain_stale_input() -> anyhow::Result<()> {
    while event::poll(Duration::ZERO)
        .map_err(|_| anyhow::anyhow!("market terminal input could not be cleared"))?
    {
        event::read().map_err(|_| anyhow::anyhow!("market terminal input could not be cleared"))?;
    }
    Ok(())
}
// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Ev;
    use crate::tui::app::tests::{evidence, manifest};
    use crate::tui::palette::specs;
    use std::sync::mpsc::channel;

    fn driving() -> (App, Receiver<Act>) {
        let (tx, rx) = channel();
        let mut a = App::new(PathBuf::from("/tmp/abtest-keys"), manifest(), Some(tx));
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("lending", "borrow", vec![])),
            violations: 0,
            desynced: false,
        });
        (a, rx)
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn control_keys_send_the_right_actions() {
        let (mut a, rx) = driving();
        dispatch(&mut a, press('n')).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Act::Skip));
        a.apply(Ev::Halt {
            scenario: "lending".into(),
            label: "borrow".into(),
            reason: Safe::literal("violated"),
        });
        dispatch(&mut a, press('p')).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Act::Snapshot));
    }

    #[test]
    fn space_pauses_then_resumes() {
        let (mut a, rx) = driving();
        dispatch(&mut a, press(' ')).unwrap();
        assert_eq!(a.run, RunStatus::Paused);
        assert!(rx.try_recv().is_err(), "pausing sends nothing");
        dispatch(&mut a, press(' ')).unwrap();
        assert_eq!(a.run, RunStatus::Running);
        assert!(matches!(rx.try_recv().unwrap(), Act::Continue));
    }

    #[test]
    fn quit_confirms_then_aborts_without_using_an_error() {
        let (mut a, rx) = driving();
        assert!(
            dispatch(&mut a, press('q')).unwrap(),
            "first q only confirms"
        );
        assert_eq!(a.mode, Mode::Confirm);
        assert!(!dispatch(&mut a, press('y')).unwrap(), "y quits");
        assert!(matches!(rx.try_recv().unwrap(), Act::Abort));
    }

    #[test]
    fn rerun_confirms_and_sends_an_operator_step() {
        let (mut a, rx) = driving();
        dispatch(&mut a, press('R')).unwrap();
        assert_eq!(a.mode, Mode::Confirm);
        dispatch(&mut a, press('y')).unwrap();
        match rx.try_recv().unwrap() {
            Act::Exec { step, .. } => assert_eq!(step.label, "borrow"),
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn enter_does_not_fire_a_value_moving_confirm() {
        let (mut a, rx) = driving();
        dispatch(&mut a, press('R')).unwrap();
        assert_eq!(a.mode, Mode::Confirm);
        // The palette trains an Enter rhythm; a stray Enter must not move value.
        dispatch(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert!(rx.try_recv().is_err(), "Enter must not fire the rerun");
        assert_eq!(a.mode, Mode::Confirm, "still waiting for an explicit y");
        dispatch(&mut a, press('y')).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Act::Exec { .. }));
    }

    #[test]
    fn halted_steps_cannot_dispatch_a_canonical_retry() {
        let (mut a, rx) = driving();
        a.apply(Ev::Halt {
            scenario: "lending".into(),
            label: "borrow".into(),
            reason: Safe::literal("unresolved mutation"),
        });
        dispatch(&mut a, press('r')).unwrap();
        assert!(rx.try_recv().is_err());
    }

    /// A ledger wait must not resume a paused worker.
    #[test]
    fn waiting_gate_keeps_the_pause_armed() {
        let (mut a, rx) = driving();
        dispatch(&mut a, press(' ')).unwrap();
        a.apply(Ev::Waiting {
            label: "liq_wait_zero_bid".into(),
            now: 10,
            target: 400,
            secs_left: 390,
        });
        assert_eq!(a.run, RunStatus::Paused);
        assert!(rx.try_recv().is_err());
    }

    /// RC4-5: while a sent act may still be unconsumed, the other decision
    /// keys must refuse — a second queued act would be consumed by the NEXT
    /// gate (a Skip the operator never asked for).
    #[test]
    fn a_queued_act_locks_the_other_decision_keys() {
        let (mut a, rx) = driving();
        a.apply(Ev::Halt {
            scenario: "lending".into(),
            label: "borrow".into(),
            reason: Safe::literal("boom"),
        });
        dispatch(&mut a, press('n')).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Act::Skip));
        dispatch(&mut a, press('n')).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "the second Skip would queue for the next gate"
        );
        assert!(a.status_msg.contains("queued"));
        // The worker passes a gate: the queued act is spent, keys unlock.
        a.apply(Ev::StepStarted {
            scenario: "lending".into(),
            index: 2,
            label: "repay".into(),
        });
        dispatch(&mut a, press('n')).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Act::Skip));
    }

    #[test]
    fn palette_fires_an_operator_labelled_step() {
        let (mut a, rx) = driving();
        dispatch(&mut a, press(':')).unwrap();
        assert_eq!(a.mode, Mode::Palette);
        for c in "update_status".chars() {
            dispatch(&mut a, press(c)).unwrap();
        }
        dispatch(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        dispatch(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        // A stray Enter only arms the confirm overlay now; nothing has fired.
        assert!(rx.try_recv().is_err(), "a stray Enter must not fire");
        assert_eq!(a.mode, Mode::Confirm);
        dispatch(&mut a, press('y')).unwrap();
        match rx.try_recv().unwrap() {
            Act::Exec { step, .. } => {
                assert!(step.label.starts_with("operator_"), "got {}", step.label)
            }
            other => panic!("expected Exec, got {other:?}"),
        }
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn tabs_and_toggles() {
        let (mut a, _rx) = driving();
        dispatch(&mut a, press('3')).unwrap();
        assert_eq!(a.tab, Tab::Snapshot);
        dispatch(&mut a, press('d')).unwrap();
        assert!(a.diff_only);
        dispatch(&mut a, press('a')).unwrap();
        assert_eq!(a.side, 'a');
        dispatch(&mut a, press('A')).unwrap();
        assert!(a.auto_continue);
        dispatch(&mut a, press('?')).unwrap();
        assert_eq!(a.mode, Mode::Help);
        dispatch(&mut a, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn unknown_keys_do_nothing() {
        let (mut a, rx) = driving();
        let before = (a.tab, a.mode, a.side, a.diff_only);
        for c in ['z', 'Q', '9', '~'] {
            dispatch(&mut a, press(c)).unwrap();
        }
        assert_eq!((a.tab, a.mode, a.side, a.diff_only), before);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn watch_mode_control_keys_are_inert_and_quit_is_immediate() {
        let mut a = App::new(PathBuf::from("/tmp/abtest-watch"), manifest(), None);
        dispatch(&mut a, press('n')).unwrap();
        assert!(a.status_msg.contains("read-only"));
        assert!(
            !dispatch(&mut a, press('q')).unwrap(),
            "watch quits at once"
        );
    }

    #[test]
    fn search_collects_then_closes() {
        let (mut a, _rx) = driving();
        dispatch(&mut a, press('/')).unwrap();
        for c in "b_supply".chars() {
            dispatch(&mut a, press(c)).unwrap();
        }
        assert_eq!(a.search, "b_supply");
        dispatch(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn every_spec_name_is_reachable_from_the_palette() {
        let (mut a, _rx) = driving();
        for spec in specs() {
            a.palette = Palette {
                query: spec.name.into(),
                ..Default::default()
            };
            assert!(
                !a.palette.matches().is_empty(),
                "{} unreachable by query",
                spec.name
            );
        }
    }
}

#[cfg(test)]
mod jump_tests {
    use super::*;
    use crate::run::Ev;
    use crate::tui::app::tests::{evidence, manifest};
    use std::sync::mpsc::channel;

    fn with_steps() -> App {
        let (tx, _rx) = channel();
        let mut a = App::new(PathBuf::from("/tmp/abtest-jump"), manifest(), Some(tx));
        for (scenario, label) in [
            ("lending", "borrow"),
            ("lending", "supply"),
            ("statuses", "status_admin_on_ice"),
            ("backstop", "backstop_deposit"),
        ] {
            a.apply(Ev::StepDone {
                evidence: Box::new(evidence(scenario, label, vec![])),
                violations: 0,
                desynced: false,
            });
        }
        a
    }

    #[test]
    fn jump_moves_the_selection_to_that_scenario() {
        let mut a = with_steps();
        assert_eq!(a.steps.len(), 4);
        dispatch(
            &mut a,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(a.mode, Mode::Jump, "g opens the jump overlay");
        // plan order is scenarios(): lending, interest, statuses, ...
        dispatch(
            &mut a,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        dispatch(
            &mut a,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(a.plan[a.jump_sel].name, "statuses");
        dispatch(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(
            a.selected_step().map(|s| s.scenario.as_str()),
            Some("statuses"),
            "jump must select a step of the chosen scenario"
        );
    }

    #[test]
    fn jumping_to_a_scenario_with_no_evidence_says_so() {
        let mut a = with_steps();
        let before = a.selected;
        dispatch(
            &mut a,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        )
        .unwrap();
        let freeze = a.plan.iter().position(|s| s.name == "freeze").unwrap();
        a.jump_sel = freeze;
        dispatch(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert_eq!(a.selected, before, "selection must not move");
        assert!(
            a.status_msg.contains("no steps"),
            "must not claim it jumped: {:?}",
            a.status_msg
        );
    }
}

#[cfg(test)]
mod market_gate_tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn only_the_exact_token_opens_the_gate() {
        assert!(gate_honors(
            "aabbccddeeff0011",
            &DeployAct::Confirm {
                token: "aabbccddeeff0011".into()
            }
        ));
        assert!(!gate_honors(
            "aabbccddeeff0011",
            &DeployAct::Confirm {
                token: "stale-token-00000".into()
            }
        ));
        assert!(!gate_honors("aabbccddeeff0011", &DeployAct::Continue));
        assert!(!gate_honors("aabbccddeeff0011", &DeployAct::Init));
        assert!(!gate_honors("aabbccddeeff0011", &DeployAct::Halt));
    }

    #[test]
    fn confirm_consumes_exactly_one_y_and_enter_never_fires() {
        let mut s = MarketApp::new();
        let mut gate = Some("aabbccddeeff0011".to_string());
        // The palette trains an Enter rhythm; it must not fire a mutation.
        assert!(matches!(
            market_key(&mut s, &mut gate, &key(KeyCode::Enter)),
            MarketKey::Discarded
        ));
        assert!(gate.is_some(), "Enter must not consume the gate");
        assert!(s.status_msg.contains("Enter alone never fires"));
        match market_key(&mut s, &mut gate, &key(KeyCode::Char('y'))) {
            MarketKey::Send(DeployAct::Confirm { token }) => {
                assert_eq!(
                    token, "aabbccddeeff0011",
                    "the act carries exactly the gate's token"
                )
            }
            _ => panic!("expected a confirm send"),
        }
        assert!(gate.is_none(), "the token is spent on the first y");
        assert!(
            matches!(
                market_key(&mut s, &mut gate, &key(KeyCode::Char('y'))),
                MarketKey::Discarded
            ),
            "a second y sends nothing"
        );
    }

    #[test]
    fn y_without_a_gate_sends_nothing() {
        let mut s = MarketApp::new();
        let mut gate = None;
        assert!(matches!(
            market_key(&mut s, &mut gate, &key(KeyCode::Char('y'))),
            MarketKey::Discarded
        ));
        assert!(s.status_msg.contains("nothing is awaiting confirmation"));
    }

    #[test]
    fn pair_controls_are_inert_in_market_mode() {
        let mut s = MarketApp::new();
        let mut gate = None;
        for c in [
            '1', '3', 'd', 'a', 'b', 'n', 'R', ' ', 's', 'p', ':', 'g', 'F', 'f', 'j', 'k',
        ] {
            assert!(
                matches!(
                    market_key(&mut s, &mut gate, &key(KeyCode::Char(c))),
                    MarketKey::Discarded
                ),
                "pair control {c:?} must be inert while the market machine owns the chain"
            );
        }
        assert!(matches!(
            market_key(&mut s, &mut gate, &key(KeyCode::Char('q'))),
            MarketKey::Quit
        ));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            market_key(&mut s, &mut gate, &ctrl_c),
            MarketKey::Quit
        ));
    }

    #[test]
    fn halt_is_honored_at_the_next_event_boundary() {
        let (tx, rx) = channel::<DeployAct>();
        assert!(
            matches!(next_act(&rx), DeployAct::Continue),
            "with no operator intent the authorized deployment proceeds"
        );
        tx.send(DeployAct::Halt).unwrap();
        assert!(
            matches!(next_act(&rx), DeployAct::Halt),
            "a pending halt must become the next act, not another continue"
        );
        assert!(
            matches!(next_act(&rx), DeployAct::Continue),
            "the halt is consumed once; the queue cannot re-halt"
        );
        tx.send(DeployAct::Continue).unwrap();
        assert!(
            matches!(next_act(&rx), DeployAct::Continue),
            "stray continues are discarded at this boundary"
        );
        tx.send(DeployAct::Init).unwrap();
        assert!(
            matches!(next_act(&rx), DeployAct::Continue),
            "a stray init cannot restart anything from this boundary"
        );
        tx.send(DeployAct::Confirm {
            token: "not-from-the-gate".into(),
        })
        .unwrap();
        assert!(
            matches!(next_act(&rx), DeployAct::Continue),
            "a confirmation not held by a gate cannot pass this boundary"
        );
    }
}
