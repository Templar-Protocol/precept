//! Rendering. Free functions only; they never mutate domain state.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::compare::Class;
use crate::timeline::Verdict;

use super::app::{App, Mode, Pending, RunStatus, StepStatus, Tab, TreeRow};
use super::palette::{arg_prompt, choices, specs, Stage};

/// Below this the ratatui layout has no room to be meaningful.
const MIN_W: u16 = 60;
const MIN_H: u16 = 12;

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    if area.width < MIN_W || area.height < MIN_H {
        frame.render_widget(
            Paragraph::new("terminal too small").style(Style::default().fg(Color::Yellow)),
            area,
        );
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(u16::from(!app.watch_read_errors.is_empty())),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(26), Constraint::Min(30)])
        .split(rows[3]);

    render_header(app, frame, rows[0]);
    render_progress(app, frame, rows[1]);
    render_strip(app, frame, rows[2]);
    render_plan(app, frame, body[0]);
    render_detail(app, frame, body[1]);
    if let Some(error) = app.watch_read_errors.first() {
        frame.render_widget(
            Paragraph::new(error.as_str()).style(Style::default().fg(Color::LightRed)),
            rows[4],
        );
    }
    render_status(app, frame, rows[5]);
    render_keys(app, frame, rows[6]);

    match app.mode {
        Mode::Help => render_help(frame),
        Mode::Info => render_info(app, frame),
        Mode::Palette => render_palette(app, frame),
        Mode::Jump => render_jump(app, frame),
        Mode::Confirm => render_confirm(app, frame),
        Mode::Search | Mode::Normal => {}
    }
}

pub fn verdict_color(v: Verdict) -> Color {
    match v {
        Verdict::NoCall => Color::DarkGray,
        Verdict::Match => Color::Green,
        Verdict::Expected => Color::Yellow,
        Verdict::Unexpected => Color::Magenta,
        Verdict::Violation => Color::Red,
    }
}

/// One cell per step across the whole run, so where divergence *started* is visible
/// without scrubbing. Scenario boundaries get a dim rule; the selected step is
/// highlighted, which makes this a map of the run as well as a verdict.
fn render_strip(app: &mut App, frame: &mut Frame, area: Rect) {
    let cells = &app.timeline.cells;
    if cells.is_empty() {
        frame.render_widget(
            Paragraph::new(" no steps recorded yet").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    let count = |v: Verdict| cells.iter().filter(|c| c.verdict == v).count();
    let tail = format!(
        " {}✖ {}! {}◆ {}▪",
        count(Verdict::Violation),
        count(Verdict::Unexpected),
        count(Verdict::Expected),
        count(Verdict::Match)
    );
    // (glyph, colour, index of the step it stands for)
    let mut slots: Vec<(char, Color, Option<usize>)> = Vec::new();
    let mut scenario = String::new();
    for (i, c) in cells.iter().enumerate() {
        if c.scenario != scenario {
            if !scenario.is_empty() {
                slots.push(('│', Color::DarkGray, None));
            }
            scenario = c.scenario.clone();
        }
        slots.push((c.verdict.glyph(), verdict_color(c.verdict), Some(i)));
    }
    let room = (area.width as usize).saturating_sub(tail.chars().count() + 1);
    let here = slots
        .iter()
        .position(|(_, _, i)| *i == Some(app.selected))
        .unwrap_or(0);
    let start = crate::tui::app::window(app.strip_offset, here, room, slots.len());
    app.strip_offset = start;
    let mut spans: Vec<Span> = Vec::new();
    for (n, (g, col, idx)) in slots.iter().enumerate().skip(start).take(room) {
        let mut st = Style::default().fg(*col);
        if *idx == Some(app.selected) {
            st = st.add_modifier(Modifier::REVERSED);
        }
        let _ = n;
        spans.push(Span::styled(g.to_string(), st));
    }
    spans.push(Span::styled(tail, Style::default().fg(Color::DarkGray)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let dir = app
        .out
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mode = if app.watching() { "WATCH" } else { "DRIVE" };
    let (label, color) = run_label(&app.run);
    let (done, total, _) = app.progress();
    let spans = vec![
        Span::styled(
            " precept-abtest ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{dir}  {:?}  ", app.manifest.profile)),
        Span::styled(mode, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(if total == 0 {
            "  scope ?".to_string()
        } else {
            format!("  {done}/{total}")
        }),
        Span::raw(
            app.waiting
                .as_ref()
                .map(|w| format!("  ⏳ {w}"))
                .unwrap_or_default(),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A bar plus "done/total  scenario · step", so the run's position is always on screen.
fn render_progress(app: &App, frame: &mut Frame, area: Rect) {
    let (done, total, here) = app.progress();
    let width = area.width as usize;
    let bar_w = width.saturating_sub(34).clamp(10, 40);
    // No recorded requested scope means there is nothing honest to divide by:
    // say so instead of inventing a denominator (or a 0% that reads as one).
    let known = total > 0;
    let filled = if known { done * bar_w / total } else { 0 };
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_w.saturating_sub(filled));
    let color = match app.run {
        RunStatus::Halted(_) => Color::Red,
        RunStatus::Paused | RunStatus::Retrying { .. } => Color::Yellow,
        RunStatus::Watching => Color::Cyan,
        _ => Color::Green,
    };
    let counter = if known {
        format!(" {done}/{total} ({}%)  ", done * 100 / total)
    } else {
        " scope unknown  ".to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(color)),
            Span::raw(counter),
            Span::styled(here, Style::default().add_modifier(Modifier::BOLD)),
        ])),
        area,
    );
}

pub fn run_label(run: &RunStatus) -> (String, Color) {
    match run {
        RunStatus::Idle => ("IDLE".into(), Color::Gray),
        RunStatus::Running => ("▶ RUNNING".into(), Color::Green),
        RunStatus::Paused => ("⏸ PAUSED".into(), Color::Yellow),
        RunStatus::Retrying { n, .. } => (format!("retry {n}/5"), Color::Yellow),
        RunStatus::Halted(_) => ("⏸ HALTED".into(), Color::Red),
        RunStatus::Finished(m) if m == "complete" => ("FINISHED".into(), Color::Green),
        RunStatus::Finished(_) => ("FINISHED (error)".into(), Color::Red),
        RunStatus::Watching => ("WATCHING".into(), Color::Cyan),
    }
}

pub fn step_icon(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "○",
        StepStatus::Running => "▶",
        StepStatus::Done => "✓",
        StepStatus::Failed => "✖",
        StepStatus::Skipped => "⋯",
    }
}

pub fn step_color(s: StepStatus) -> Color {
    match s {
        StepStatus::Pending => Color::DarkGray,
        StepStatus::Running => Color::Cyan,
        StepStatus::Done => Color::Green,
        StepStatus::Failed => Color::Red,
        StepStatus::Skipped => Color::DarkGray,
    }
}

pub fn class_color(c: &Class) -> Color {
    match c {
        Class::Match => Color::Green,
        Class::Expected(_) => Color::Yellow,
        Class::Violation(_) => Color::Red,
    }
}

pub fn status_color(s: &str) -> Color {
    match s {
        "PASS" => Color::Green,
        "FAIL" => Color::Red,
        "PARTIAL" => Color::Yellow,
        // Never gray/green: an uncertifiable run must not read as a pass.
        "INCOMPLETE" => Color::Yellow,
        "NOT REQUESTED" => Color::DarkGray,
        "N/A" => Color::DarkGray,
        _ => Color::Gray,
    }
}

fn render_plan(app: &mut App, frame: &mut Frame, area: Rect) {
    let current = app
        .selected_step()
        .map(|s| (s.scenario.clone(), s.label.clone()));
    // Build every row first, then window around the selected one: the plan outgrows the
    // panel within a couple of scenarios, and without this the tail is unreachable.
    let mut rows: Vec<(String, Style, bool)> = Vec::new();
    for sc in &app.plan {
        let progress = app.scenario_progress(&sc.name);
        let worst = if sc.steps.iter().any(|s| s.status == StepStatus::Failed) {
            StepStatus::Failed
        } else if sc.steps.iter().any(|s| s.status == StepStatus::Running) {
            StepStatus::Running
        } else if progress.is_some_and(|(done, total)| done == total && total > 0) {
            StepStatus::Done
        } else {
            StepStatus::Pending
        };
        rows.push((
            match progress {
                Some((done, total)) => {
                    format!("{} {:<13}{done}/{total}", step_icon(worst), sc.name)
                }
                None => format!("{} {:<13}unscoped", step_icon(worst), sc.name),
            },
            Style::default()
                .fg(step_color(worst))
                .add_modifier(Modifier::BOLD),
            false,
        ));
        for st in sc.steps.iter().filter(|s| s.status != StepStatus::Pending) {
            let selected = current
                .as_ref()
                .is_some_and(|(s, l)| *s == sc.name && *l == st.label);
            let mut style = Style::default().fg(step_color(st.status));
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            rows.push((
                format!(
                    "{}{} {}",
                    if selected { " ▸" } else { "  " },
                    step_icon(st.status),
                    st.label
                ),
                style,
                selected,
            ));
        }
    }

    let height = area.height.saturating_sub(1) as usize; // the block title takes one row
    let sel = rows.iter().position(|(_, _, s)| *s).unwrap_or(0);
    let start = crate::tui::app::window(app.plan_offset, sel, height, rows.len());
    app.plan_offset = start;
    let (above, below) = (start, rows.len().saturating_sub(start + height));

    // Counts live in the title, so an indicator never costs a row of content and the
    // numbers cannot disagree with what is drawn.
    let mut title = String::from("PLAN");
    if above > 0 {
        title.push_str(&format!(" ▲{above}"));
    }
    if below > 0 {
        title.push_str(&format!(" ▼{below}"));
    }
    let items: Vec<ListItem> = rows
        .into_iter()
        .skip(start)
        .take(height)
        .map(|(text, style, _)| ListItem::new(Line::from(Span::styled(text, style))))
        .collect();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::RIGHT).title(title)),
        area,
    );
}

/// Eight tabs do not fit 34 columns, so the bar slides to keep the active one visible
/// and marks the tabs it cut with chevrons.
pub fn tab_bar(active: Tab, width: u16) -> Line<'static> {
    let labels: Vec<String> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| format!(" [{}]{} ", i + 1, t.title()))
        .collect();
    let total: usize = labels.iter().map(|l| l.chars().count()).sum();
    let here = Tab::ALL.iter().position(|t| *t == active).unwrap_or(0);
    let width = width as usize;
    let (mut first, mut last) = (0usize, labels.len() - 1);
    if total > width {
        // widen outward from the active tab for as long as there is room
        let room = width.saturating_sub(2); // the two chevrons
        first = here;
        last = here;
        let mut used = labels[here].chars().count();
        loop {
            let grew = [last < labels.len() - 1, first > 0];
            let mut moved = false;
            if grew[0] && used + labels[last + 1].chars().count() <= room {
                last += 1;
                used += labels[last].chars().count();
                moved = true;
            }
            if grew[1] && used + labels[first - 1].chars().count() <= room {
                first -= 1;
                used += labels[first].chars().count();
                moved = true;
            }
            if !moved {
                break;
            }
        }
    }
    let mut spans: Vec<Span> = Vec::new();
    if first > 0 {
        spans.push(Span::styled("‹", Style::default().fg(Color::DarkGray)));
    }
    for (i, label) in labels.iter().enumerate().take(last + 1).skip(first) {
        let sel = Tab::ALL[i] == active;
        spans.push(Span::styled(
            label.clone(),
            if sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        ));
    }
    if last < labels.len() - 1 {
        spans.push(Span::styled("›", Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn render_detail(app: &mut App, frame: &mut Frame, area: Rect) {
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(2)])
        .split(area);
    frame.render_widget(Paragraph::new(tab_bar(app.tab, split[0].width)), split[0]);

    app.stream_width = split[1].width as usize;
    let rows = app.tree_rows();
    let sel = app.detail_sel.min(rows.len().saturating_sub(1));
    let height = split[1].height as usize;
    let start = crate::tui::app::window(app.detail_offset, sel, height, rows.len());
    app.detail_offset = start;
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(i, r)| tree_item(app, r, i == sel))
        .collect();
    frame.render_widget(List::new(items), split[1]);
}

fn tree_item<'a>(app: &App, r: &'a TreeRow, selected: bool) -> ListItem<'a> {
    let indent = "  ".repeat(r.indent);
    let marker = if r.expandable {
        if app.expanded.contains(&r.path) {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        ""
    };
    let mut style = Style::default();
    if let Some(c) = &r.class {
        style = style.fg(class_color(c));
    } else if app.tab == Tab::Invariants {
        let status = r.value.split_whitespace().next().unwrap_or("");
        style = style.fg(status_color(status));
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    let text = if r.key.is_empty() {
        format!("{indent}{}", r.value)
    } else {
        format!("{indent}{marker}{}  {}", r.key, r.value)
    };
    ListItem::new(Line::from(Span::styled(text, style)))
}

fn render_status(app: &App, frame: &mut Frame, area: Rect) {
    let mut text = match app.selected_step() {
        Some(s) => {
            // The strip glyph is terse by design, so name it here for whatever is selected.
            let verdict = app
                .timeline
                .cells
                .iter()
                .find(|c| c.scenario == s.scenario && c.label == s.label)
                .map(|c| format!("{} {}", c.verdict.glyph(), c.verdict.name()))
                .unwrap_or_default();
            format!(
                "{}/{}  A {}  B {}  diffs {}  {verdict}",
                s.scenario,
                s.label,
                outcome(&s.a),
                outcome(&s.b),
                s.diff.len()
            )
        }
        None => "no steps yet".into(),
    };
    // The authoritative verdict from the same analysis `report` writes.
    if !app.verdict.is_empty() {
        text.push_str(&format!("  verdict {}", app.verdict));
    }
    let broken = app.timeline.violations();
    if broken > 0 {
        text.push_str(&format!("  ADR checks failing: {broken}"));
    }
    if let RunStatus::Halted(why) = &app.run {
        text.push_str(&format!("  HALTED: {why}"));
    }
    if !app.status_msg.is_empty() {
        text.push_str(&format!("  {}", app.status_msg));
    }
    if app.watching() && app.status_msg.is_empty() {
        text.push_str("   read-only — to drive a run: precept-abtest --out <dir> tui");
    }
    if app.mode == Mode::Search {
        text = format!("/{}", app.search);
    }
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::White).bg(Color::DarkGray)),
        area,
    );
}

fn outcome(r: &crate::observe::StepRecord) -> String {
    match &r.invocation {
        None => "-".into(),
        Some(i) if i.ok => "ok".into(),
        Some(i) => match &i.error {
            Some(crate::stellar::ErrorKind::Contract(c)) => match crate::stellar::error_name(*c) {
                Some(n) => format!("{n}({c})"),
                None => format!("err#{c}"),
            },
            Some(crate::stellar::ErrorKind::Auth) => "auth".into(),
            _ => "err".into(),
        },
    }
}

/// Only advertise keys that do something in the current mode. Run controls are inert
/// while watching, and showing them made the TUI look broken.
fn render_keys(app: &App, frame: &mut Frame, area: Rect) {
    let keys = if app.watching() {
        "q quit  1-8 tab  J/K step  j/k line  h/l expand  g jump  / search  a/b side  i info  ? help"
    } else {
        "q quit  ␣ pause/resume  s one-step  n skip  R rerun  1-8 tab  J/K step  g jump  i info  ? help"
    };
    frame.render_widget(
        Paragraph::new(keys).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

pub fn centered_rect(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

pub const HELP_KEYS: &[(&str, &str)] = &[
    ("q / Ctrl-C", "quit (confirms while driving)"),
    ("?", "this help"),
    ("i", "what the selected experiment tests"),
    ("1-4", "per-step tabs: invariants, diff, snapshot, stream"),
    ("5-8", "run tabs: flow, rates, traps, health"),
    (
        "",
        "5 FLOW    every b/d_supply move and the event explaining it",
    ),
    (
        "",
        "6 RATES   b_rate history; only a default may pull it down",
    ),
    (
        "",
        "7 TRAPS   ADR 0011 exports and the pool traps, on one screen",
    ),
    ("", "8 HEALTH  utilisation and supply-cap headroom"),
    (
        "",
        "strip     ▪ match  ◆ expected  ! off-scenario  ✖ violation",
    ),
    ("J / K", "next / previous STEP"),
    ("j / k", "move down / up within the panel"),
    ("h / l", "collapse / expand node"),
    ("Enter", "toggle node"),
    (
        "space",
        "pause at the next gate; resume a halted or paused run (driving only)",
    ),
    (
        "s",
        "one step: run exactly one step, then pause (driving only)",
    ),
    ("n", "skip the step the runner is waiting on (driving only)"),
    (
        "R",
        "re-run the selected step as a disclosed operator action (driving only)",
    ),
    ("g", "jump to scenario"),
    ("G", "newest step and follow"),
    (":", "command palette"),
    ("p", "force snapshot of both sides"),
    ("a / b", "inspect side A / B"),
    ("d", "filter: diff-only, or loss events only on FLOW"),
    ("f", "toggle follow"),
    ("A", "auto-continue on violation"),
    ("/", "search"),
    ("Esc", "close overlay"),
    ("F", "force a stale lock (watch mode)"),
];

fn render_help(frame: &mut Frame) {
    let area = centered_rect(60, 80, frame.area());
    frame.render_widget(Clear, area);
    let lines: Vec<Line> = HELP_KEYS
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!(" {k:<12}"), Style::default().fg(Color::Cyan)),
                Span::raw(*d),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        area,
    );
}

/// What the selected experiment is for. Reference you read once on arriving at a
/// scenario, so it lives behind a key rather than taxing every frame: at 60x12 the
/// body is seven rows, and prose does not fit the 26-column plan pane.
fn render_info(app: &App, frame: &mut Frame) {
    let area = centered_rect(72, 70, frame.area());
    frame.render_widget(Clear, area);
    let measure = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();

    if let Some(d) = &app.manifest.description {
        for l in crate::tui::app::wrap(d, measure) {
            lines.push(Line::from(Span::raw(l)));
        }
        lines.push(Line::from(""));
    }

    match app.selected_step() {
        Some(step) => {
            let sc = &step.scenario;
            lines.push(Line::from(Span::styled(
                sc.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            let desc = crate::scenario::describe(sc);
            if desc.is_empty() {
                lines.push(Line::from(Span::styled(
                    "no description recorded for this scenario",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for l in crate::tui::app::wrap(desc, measure) {
                    lines.push(Line::from(Span::raw(l)));
                }
            }
            lines.push(Line::from(""));
            if let Some(p) = app.plan.iter().find(|p| p.name == *sc) {
                let done = p
                    .steps
                    .iter()
                    .filter(|s| s.status == StepStatus::Done)
                    .count();
                let failed = p
                    .steps
                    .iter()
                    .filter(|s| s.status == StepStatus::Failed)
                    .count();
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} steps · {done} passed · {failed} with violations",
                        p.steps.len()
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines.push(Line::from(Span::styled(
                format!("selected step: {}", step.label),
                Style::default().fg(Color::DarkGray),
            )));
        }
        None => lines.push(Line::from(Span::styled(
            "no step selected yet",
            Style::default().fg(Color::DarkGray),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" experiment ")),
        area,
    );
}

fn render_palette(app: &App, frame: &mut Frame) {
    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(Clear, area);
    let p = &app.palette;
    let (title, items): (String, Vec<ListItem>) = match p.stage {
        Stage::Pick => (
            format!(" run step: {}_ ", p.query),
            p.matches()
                .iter()
                .enumerate()
                .map(|(i, &s)| row(specs()[s].name.to_string(), i == p.sel))
                .collect(),
        ),
        Stage::Arg(_) => {
            let arg = p.current_arg();
            let prompt = arg.map(arg_prompt).unwrap_or("");
            let opts = arg.map(choices).unwrap_or_default();
            if opts.is_empty() {
                (
                    format!(" {prompt}: {}_ ", p.buf),
                    vec![ListItem::new("type a value, Enter to accept")],
                )
            } else {
                (
                    format!(" {prompt} "),
                    opts.iter()
                        .enumerate()
                        .map(|(i, v)| row(v.label(), i == p.sel))
                        .collect(),
                )
            }
        }
        Stage::Ready => (
            " Enter to fire, Tab switches side ".into(),
            vec![ListItem::new(
                p.build()
                    .map(|s| s.label.to_string())
                    .unwrap_or_else(|| "incomplete".into()),
            )],
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{title}[{:?}]", p.sides));
    frame.render_widget(List::new(items).block(block), area);
}

fn row(text: String, selected: bool) -> ListItem<'static> {
    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    ListItem::new(Line::from(Span::styled(text, style)))
}

fn render_jump(app: &App, frame: &mut Frame) {
    let area = centered_rect(40, 50, frame.area());
    frame.render_widget(Clear, area);
    let items: Vec<ListItem> = app
        .plan
        .iter()
        .enumerate()
        .map(|(i, sc)| row(sc.name.clone(), i == app.jump_sel))
        .collect();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(" jump ")),
        area,
    );
}

fn render_confirm(app: &App, frame: &mut Frame) {
    let area = centered_rect(60, 30, frame.area());
    frame.render_widget(Clear, area);
    let text = match &app.pending {
        Some(Pending::Quit) => "Quit? The run aborts at the next step.  [y/n]".to_string(),
        Some(Pending::Rerun(s)) => format!(
            "Re-run `{}` against {:?}?\nThis moves real value and is recorded as an operator action.  [y/n]",
            s.label, app.palette.sides
        ),
        Some(Pending::ExecOp { step, sides }) => format!(
            "Fire `{}` against {sides:?}?\nThis moves real value and is recorded as an operator action.  [y/n]",
            step.label
        ),
        Some(Pending::ForceLock) => {
            "Take the lock from the recorded owner? Two writers corrupt state.json.  [y/n]".into()
        }
        None => "?".into(),
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" confirm ")),
        area,
    );
}

// --- Protected market screen ---
//
// Market-mode screens: the phase checklist, the rendered CLI stream,
// pending/unresolved custody, gates awaiting confirmation (summary only —
// the bound token is never rendered material), and the final public
// summary. Pair tabs, diff controls, skip/rerun, and the operator palette
// never render here and their keys are unbound until a deployment has
// completed; after completion, play goes through `market play` or the
// market palette, not the pair surface.

use super::app::{MarketApp, MarketPhaseState};

pub fn render_market(app: &MarketApp, frame: &mut Frame) {
    let area = frame.area();
    if area.width < MIN_W || area.height < MIN_H {
        frame.render_widget(
            Paragraph::new("terminal too small").style(Style::default().fg(Color::Yellow)),
            area,
        );
        return;
    }
    let header = if app.watching {
        "READ-ONLY: completed protected market - testnet only, no A/B verdict"
    } else if app.done {
        "PROTECTED MARKET: deployment complete - testnet only, no A/B verdict"
    } else {
        "PROTECTED MARKET DEPLOYMENT - Stellar testnet only - mutations gated by confirmation token"
    };
    let head_style = Style::default()
        .fg(if app.watching {
            Color::Cyan
        } else if app.done {
            Color::Green
        } else {
            Color::LightRed
        })
        .add_modifier(Modifier::BOLD);
    let top = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Min(6),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(Paragraph::new(header).style(head_style), top[0]);

    let mut phase_items: Vec<ListItem> = Vec::new();
    for (id, label, st) in &app.phases {
        let (mark, style) = match st {
            MarketPhaseState::Running => (
                "[>] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            MarketPhaseState::Failed => (
                "[!] ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        };
        phase_items.push(ListItem::new(Line::from(vec![
            Span::styled(mark, style),
            Span::raw(format!("phase {id}: {label}")),
        ])));
    }
    if phase_items.is_empty() {
        phase_items.push(ListItem::new(Span::styled(
            "no journal phase has started",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        List::new(phase_items).block(Block::default().borders(Borders::ALL).title(" phases ")),
        top[1],
    );

    let stream = app.stream.iter().map(|line| {
        ListItem::new(Span::styled(
            line.as_str(),
            Style::default().fg(Color::DarkGray),
        ))
    });
    frame.render_widget(
        List::new(stream).block(Block::default().borders(Borders::ALL).title(" progress ")),
        top[2],
    );

    let mut alert: Vec<Line> = Vec::new();
    if let Some(s) = &app.failed {
        alert.push(Line::styled(
            format!("failed: {s}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(s) = &app.halted {
        alert.push(Line::styled(
            format!("halted at: {s}"),
            Style::default().fg(Color::Red),
        ));
    }
    for c in &app.custody {
        alert.push(Line::styled(
            c.clone(),
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.unresolved {
        alert.push(Line::styled(
            "unresolved custody: reconciliation is public; market resume only",
            Style::default().fg(Color::LightRed),
        ));
    }
    if app.done && !app.watching {
        alert.push(Line::styled(
            "complete; post-deployment actions: `market play` or the market palette",
            Style::default().fg(Color::Green),
        ));
    }
    if alert.is_empty() {
        alert.push(Line::styled(
            "nothing to report",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if !app.status_msg.is_empty() {
        alert.push(Line::styled(
            format!("operator status: {}", app.status_msg),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(
        Paragraph::new(alert).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" custody and status "),
        ),
        top[3],
    );
    let footer = if app.watching {
        "q quit - read-only, nothing here signs"
    } else if app.confirm_summary.is_some() {
        "y confirms exactly once   q halts before it proceeds   Enter alone never fires"
    } else {
        "q quit (halts after the current mutation finishes)"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(if app.confirm_summary.is_some() {
            Color::Yellow
        } else {
            Color::Cyan
        })),
        top[4],
    );

    if let Some(summary) = &app.confirm_summary {
        // The token binds the journal phase/action id, the exact testnet,
        // the freshly re-resolved signer G-address, the target, and the
        // redacted payload digest. Only the rendered summary is shown here;
        // the token never appears on any screen.
        // Most of the screen, so a real summary (phase/action id, testnet,
        // two 56-char strkeys, 64-hex digest) is readable whole at 80x24 —
        // but only above the custody-and-status panel: the gate is modal,
        // and the operator's keypress feedback plus any open custody must
        // stay on screen while it waits for `y`.
        let stage = Rect {
            height: top[3].y.saturating_sub(area.y),
            ..area
        };
        let pop = centered_rect(90, 80, stage);
        frame.render_widget(Clear, pop);
        frame.render_widget(
            Paragraph::new(format!(
                "MUTATION AWAITING CONFIRMATION\n\n{summary}\n\npress y once to confirm this mutation only; q halts before it proceeds; Enter alone never fires"
            ))
            .wrap(Wrap { trim: true })
            .style(
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL).title(" confirm ")),
            pop,
        );
    } else if app.done {
        let sum = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Min(10),
                Constraint::Length(1),
            ])
            .split(area);
        let body: Vec<Line> = app.summary.iter().map(|l| Line::from(l.clone())).collect();
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: true }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" final summary - public addresses "),
            ),
            sum[1],
        );
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Ev;
    use crate::tui::app::tests::{app, evidence};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn dump(b: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                s.push_str(b[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn drawn(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(app, f)).unwrap();
        dump(t.backend().buffer())
    }

    fn loaded() -> App {
        let mut a = app();
        a.apply(Ev::RunStarted {
            plan: vec![("lending".into(), vec!["borrow".into()])],
            resumed: 0,
        });
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("lending", "borrow", vec![])),
            violations: 0,
            desynced: false,
        });
        a
    }

    #[test]
    fn unreadable_evidence_stays_visible_with_good_neighbors_and_search() {
        let dir = std::env::temp_dir().join(format!(
            "abtest-watch-error-render-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::manifest::RunState::default().save(&dir).unwrap();
        crate::report::write_step(&dir, &evidence("lending", "borrow", vec![])).unwrap();
        let bad = dir.join("evidence/lending/001-partial_repay");
        std::fs::create_dir_all(&bad).unwrap();
        for name in ["a.json", "b.json", "diff.json"] {
            std::fs::write(bad.join(name), "{}").unwrap();
        }
        let mut a = app();
        a.out = dir.clone();
        a.poll_disk().unwrap();
        for (w, h) in [(80, 24), (120, 40), (60, 18)] {
            for mode in [Mode::Normal, Mode::Search] {
                a.mode = mode;
                a.search = "borrow".into();
                let screen = drawn(&mut a, w, h);
                assert!(screen.contains("Evidence unavailable"), "{w}x{h}: {screen}");
                assert!(screen.contains("rereading files"), "{w}x{h}: {screen}");
                assert_eq!(a.selected_step().unwrap().label, "borrow");
                if mode == Mode::Search {
                    assert!(screen.contains("/borrow"), "{w}x{h}: {screen}");
                } else {
                    assert!(screen.contains("lending/borrow"), "{w}x{h}: {screen}");
                }
            }
        }
        std::fs::remove_dir_all(bad).unwrap();
        a.poll_disk().unwrap();
        assert!(!drawn(&mut a, 80, 24).contains("Evidence unavailable"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn frame_shows_header_plan_and_tabs() {
        let mut a = loaded();
        let out = drawn(&mut a, 120, 40);
        assert!(out.contains("precept-abtest"));
        assert!(out.contains("WATCH"), "no driver in the test app");
        for t in Tab::ALL {
            assert!(out.contains(t.title()), "missing tab {}", t.title());
        }
        assert!(out.contains("lending"));
        assert!(out.contains("borrow"));
    }

    #[test]
    fn each_tab_renders_its_own_content() {
        let mut a = loaded();
        a.tab = Tab::Invariants;
        assert!(drawn(&mut a, 140, 40).contains("ADR 0008"));
        // the shared fixture's snapshots are equal, so the Diff panel's row
        // must come from a step that actually diverged
        let mut ev = evidence("lending", "repay", vec![]);
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
        assert!(drawn(&mut a, 140, 40).contains("b_supply"));
        a.tab = Tab::Snapshot;
        assert!(drawn(&mut a, 140, 40).contains("reserves"));
        a.tab = Tab::Stream;
        assert!(drawn(&mut a, 140, 40).contains("lending/borrow"));
        a.tab = Tab::Flow;
        assert!(drawn(&mut a, 140, 40).contains("EXPLAINED CHANGE"));
        a.tab = Tab::Rates;
        assert!(drawn(&mut a, 140, 40).contains("b_rate"));
        a.tab = Tab::Traps;
        assert!(drawn(&mut a, 140, 40).contains("trap_backstop_claim"));
        a.tab = Tab::Health;
        assert!(drawn(&mut a, 140, 40).contains("util"));
    }

    #[test]
    fn the_divergence_strip_marks_the_selected_step_and_counts_the_run() {
        // The strip is honest: a step only counts as a violation when its
        // snapshots actually diverge, so give the selected step a real Violation.
        let mut a = app();
        a.apply(Ev::RunStarted {
            plan: vec![("lending".into(), vec!["borrow".into()])],
            resumed: 0,
        });
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
        let screen = drawn(&mut a, 140, 40);
        assert!(screen.contains('✖'), "{screen}");
        assert!(screen.contains("1✖"), "{screen}");
    }

    #[test]
    fn the_strip_says_so_rather_than_rendering_blank_before_any_step() {
        let mut a = app();
        assert!(drawn(&mut a, 140, 40).contains("no steps recorded yet"));
    }

    #[test]
    fn the_tab_bar_keeps_the_active_tab_visible_when_it_cannot_fit_them_all() {
        // At the 60-column floor the detail pane is 34 wide; eight titles need ~90.
        for (i, tab) in Tab::ALL.iter().enumerate() {
            let line = tab_bar(*tab, 34);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.contains(&format!("[{}]{}", i + 1, tab.title())),
                "{tab:?} fell out of a 34-column bar: {text}"
            );
            assert!(text.chars().count() <= 34, "bar overflowed: {text}");
        }
        // Wide enough for everything means no chevrons.
        let wide: String = tab_bar(Tab::Flow, 200)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!wide.contains('\u{2039}') && !wide.contains('\u{203a}'));
    }

    #[test]
    fn the_run_level_tabs_answer_before_a_step_is_selected() {
        // TRAPS and RATES read the whole run, so an empty selection is not an excuse.
        let mut a = app();
        a.tab = Tab::Traps;
        assert!(drawn(&mut a, 140, 40).contains("trap_pool_flash_loan"));
    }

    #[test]
    fn retry_counter_and_halt_reach_the_screen() {
        let mut a = loaded();
        for _ in 0..3 {
            a.apply(Ev::Cli {
                argv: "contract invoke".into(),
                ok: false,
                ms: 1,
                stderr: "502 Bad Gateway".into(),
                transient: true,
            });
        }
        assert!(drawn(&mut a, 120, 40).contains("retry 3/5"));
        a.apply(Ev::Halt {
            scenario: "lending".into(),
            label: "borrow".into(),
            reason: crate::stellar::Safe::literal("boom"),
        });
        let out = drawn(&mut a, 120, 40);
        assert!(out.contains("HALTED") && out.contains("boom"));
    }

    #[test]
    fn info_overlay_explains_the_selected_experiment() {
        let mut a = loaded();
        a.mode = Mode::Info;
        let out = drawn(&mut a, 120, 40);
        assert!(out.contains("experiment"), "titled: {out}");
        assert!(out.contains("lending"), "names the scenario");
        // the real description text, wrapped rather than clipped
        assert!(
            out.contains("Ordinary lending end to end"),
            "shows the description: {out}"
        );
        assert!(
            out.contains("steps ·"),
            "summarises progress in that scenario"
        );
        // and it costs nothing when closed
        a.mode = Mode::Normal;
        assert!(!drawn(&mut a, 120, 40).contains("Ordinary lending end to end"));
    }

    #[test]
    fn overlays_render() {
        let mut a = loaded();
        a.mode = Mode::Help;
        let help = drawn(&mut a, 120, 40);
        for (k, _) in HELP_KEYS.iter().take(8) {
            assert!(help.contains(k), "help missing {k}");
        }
        a.mode = Mode::Palette;
        a.palette.query = "mint".into();
        assert!(drawn(&mut a, 120, 40).contains("mint"));
        a.mode = Mode::Confirm;
        a.pending = Some(Pending::Quit);
        assert!(drawn(&mut a, 120, 40).contains("Quit?"));
    }

    #[test]
    fn waiting_countdown_is_visible() {
        let mut a = loaded();
        a.apply(Ev::Waiting {
            label: "liq_wait_101".into(),
            now: 10,
            target: 111,
            secs_left: 130,
        });
        assert!(drawn(&mut a, 120, 40).contains("liq_wait_101"));
    }

    /// ratatui panics on zero-area rects and impossible constraints, so every size
    /// and overlay combination must at least survive a draw.
    #[test]
    fn renders_at_every_size_without_panicking() {
        for (w, h) in [(200, 60), (120, 40), (80, 24), (60, 12), (20, 5)] {
            for mode in [
                Mode::Normal,
                Mode::Help,
                Mode::Palette,
                Mode::Jump,
                Mode::Search,
                Mode::Confirm,
            ] {
                for tab in Tab::ALL {
                    let mut a = loaded();
                    a.mode = mode;
                    a.tab = tab;
                    a.pending = Some(Pending::Quit);
                    let out = drawn(&mut a, w, h);
                    if w < MIN_W || h < MIN_H {
                        assert!(out.contains("too small"), "{w}x{h} should refuse to draw");
                    }
                }
            }
        }
    }

    /// A jump only reads as "working" if the plan pane shows where you landed.
    #[test]
    fn plan_pane_marks_the_selected_step() {
        // No RunStarted here: that filters the plan to the named scenarios.
        let mut a = app();
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("lending", "borrow", vec![])),
            violations: 0,
            desynced: false,
        });
        a.apply(Ev::StepDone {
            evidence: Box::new(evidence("statuses", "status_admin_on_ice", vec![])),
            violations: 0,
            desynced: false,
        });
        a.selected = 0; // back to lending/borrow
        assert!(drawn(&mut a, 120, 40).contains("▸✓ borrow"));
        a.selected = 1; // statuses
        let out = drawn(&mut a, 120, 40);
        assert!(out.contains("▸✓ status_admin_on_ice"));
        assert!(!out.contains("▸✓ borrow"), "only one step is marked");
    }

    /// The plan outgrows a short panel immediately; a step near the end must still be
    /// reachable, and the hidden counts have to say so.
    #[test]
    fn plan_pane_scrolls_to_the_selected_step() {
        let mut a = app();
        for (scenario, label) in [
            ("lending", "borrow"),
            ("lending", "supply"),
            ("interest", "interest_supply"),
            ("statuses", "status_admin_on_ice"),
            ("backstop", "backstop_deposit"),
            ("backstop", "backstop_queue_30pct"),
            ("liquidation", "liq_frodo_collateral"),
            ("stale_auction", "stale_mint_sam"),
        ] {
            a.apply(Ev::StepDone {
                evidence: Box::new(evidence(scenario, label, vec![])),
                violations: 0,
                desynced: false,
            });
        }
        // a short panel cannot hold every row
        a.selected = 0;
        let top = drawn(&mut a, 120, 16);
        assert!(top.contains("borrow"), "first selection visible");
        assert!(top.contains("▼"), "more rows below are advertised: {top}");

        a.selected = a.steps.len() - 1;
        let bottom = drawn(&mut a, 120, 16);
        assert!(
            bottom.contains("stale_mint_sam"),
            "last selection must scroll into view: {bottom}"
        );
        assert!(bottom.contains("▲"), "hidden rows above are advertised");
        // redrawing without moving must not shift the view
        let again = drawn(&mut a, 120, 16);
        assert_eq!(again, bottom, "a redraw with no input must be identical");

        // a tall panel needs no scrolling at all
        let tall = drawn(&mut a, 120, 60);
        assert!(
            !tall.contains("▲ ") && !tall.contains("▼ "),
            "no indicators when it all fits"
        );
    }

    #[test]
    fn empty_app_renders() {
        let mut a = app();
        assert!(drawn(&mut a, 100, 30).contains("no steps yet"));
    }
}

#[cfg(test)]
mod market_render_tests {
    use super::*;
    use crate::deploy::DeployEvent;
    use crate::manifest::{MarketManifest, MarketNetworkRecord, OracleStackManifest};
    use crate::stellar::{AccountId, ContractId, Safe};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use std::collections::BTreeMap;
    use std::str::FromStr;

    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const C_CONTRACT: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";

    fn dump(b: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                s.push_str(b[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn drawn_market(app: &mut MarketApp, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render_market(app, f)).unwrap();
        dump(t.backend().buffer())
    }

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
    fn confirm_screen_shows_the_summary_never_the_token() {
        let mut m = MarketApp::new();
        m.apply_market(&DeployEvent::Confirm {
            token: "feedfacecafebeef".into(),
            summary: Safe::literal("MUTATION PENDING on testnet, digest 0a1b"),
        });
        let out = drawn_market(&mut m, 120, 40);
        assert!(out.contains("MUTATION PENDING"));
        assert!(
            !out.contains("feedfacecafebeef"),
            "the bound token is not display material"
        );
        assert!(out.contains("Enter alone never fires"));
        for t in Tab::ALL {
            assert!(
                !out.contains(t.title()),
                "pair tab {t:?} must not render in market mode"
            );
        }
    }
    #[test]
    fn gate_guidance_is_readable_at_the_smallest_supported_size() {
        // The safety guidance ("… Enter alone never fires") must reach the
        // screen at the size a real session runs, not only at wide test
        // sizes. A prior over-long footer clipped it at 80x24, leaving an
        // operator at a common terminal unable to read how to confirm or
        // halt a mutation — a silent regression, so it is pinned here.
        let mut m = MarketApp::new();
        m.apply_market(&DeployEvent::Confirm {
            token: "feedfacecafebeef".into(),
            summary: Safe::literal("MUTATION PENDING on testnet, digest 0a1b"),
        });
        let at_80x24 = drawn_market(&mut m, 80, 24);
        assert!(
            at_80x24.contains("Enter alone never fires"),
            "gate guidance must be visible at 80x24"
        );
        // Enter-while-gated guidance is recorded feedback for a keypress
        // that did nothing; it must render too, in the status panel.
        m.status("press y to confirm; Enter alone never fires");
        let with_status = drawn_market(&mut m, 80, 24);
        assert!(
            with_status.contains("press y to confirm"),
            "keypress feedback must render at 80x24"
        );
    }

    #[test]
    fn confirmation_popup_shows_full_addresses_at_80x24() {
        // A real gate summary names the journal phase/action, the exact
        // testnet, the freshly re-resolved signer, the target and the
        // redacted payload digest. The popup used to be a fixed 7-row box
        // about 40 columns wide, which hard-split the two 56-char strkeys
        // across rows: at a common 80x24 terminal the operator could not
        // read back what they were about to sign. Each needle must appear
        // intact on one rendered row.
        const SIGNER: &str = "GC4D2KYVHQDNSUC2BPELVDJZ3IUVMJIB53GXIVVANFUHPVGYGSWLQEZO";
        const TARGET: &str = "CCJNXIEWKTW4I53RVF7PD26ZE25DEVQOAVEA2QT3NFUJ6DIS2PLFGAKI";
        const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const SUMMARY: &str = "phase 4 action install_oracle_adapter\n\
             network testnet (Test SDF Network ; September 2015)\n\
             signer GC4D2KYVHQDNSUC2BPELVDJZ3IUVMJIB53GXIVVANFUHPVGYGSWLQEZO\n\
             target CCJNXIEWKTW4I53RVF7PD26ZE25DEVQOAVEA2QT3NFUJ6DIS2PLFGAKI\n\
             digest 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut m = MarketApp::new();
        m.apply_market(&DeployEvent::Confirm {
            token: "feedfacecafebeef".into(),
            summary: Safe::literal(SUMMARY),
        });
        let out = drawn_market(&mut m, 80, 24);
        assert!(out.contains("MUTATION AWAITING CONFIRMATION"), "{out}");
        assert!(
            out.contains(SIGNER),
            "the signer strkey was clipped, not rendered whole at 80x24:\n{out}"
        );
        assert!(
            out.contains(TARGET),
            "the target strkey was clipped, not rendered whole at 80x24:\n{out}"
        );
        assert!(
            out.contains(DIGEST),
            "the payload digest was clipped at 80x24:\n{out}"
        );
        assert!(
            !out.contains("feedfacecafebeef"),
            "the bound token is not display material"
        );
    }

    #[test]
    fn phase_checklist_is_the_only_progress_narrative() {
        let mut m = MarketApp::new();
        assert!(drawn_market(&mut m, 120, 40).contains("no journal phase has started"));
        m.apply_market(&DeployEvent::Phase {
            id: 2,
            label: Safe::literal("install artifacts"),
        });
        let out = drawn_market(&mut m, 120, 40);
        assert!(out.contains("phase 2: install artifacts"));
        assert!(
            !out.contains("precept-abtest"),
            "no pair chrome in market mode"
        );
    }

    #[test]
    fn watch_screen_shows_the_public_summary_read_only() {
        let mut m = MarketApp::watch(&fixture_manifest());
        let out = drawn_market(&mut m, 140, 46);
        assert!(out.contains("READ-ONLY"));
        assert!(out.contains("pool administrator (public G-address)"));
        assert!(out.contains("single-authority synthetic quorum"));
        assert!(out.contains("read-only, nothing here signs"));
        assert!(!out.contains("MUTATION AWAITING"));
    }
}
