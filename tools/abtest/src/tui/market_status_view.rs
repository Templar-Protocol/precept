//! Read-only, typed projection of the bounded status wire report. The CLI keeps
//! its original projection; only this worker-boundary parser decodes observations.

use std::borrow::Cow;
use std::cell::Cell;

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};

use super::market_explorer::{block, ACTIVE, FAILURE, SUCCESS};
use crate::stellar::Safe;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Readiness {
    Ready,
    NotReady,
    Unknown,
}

impl Readiness {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::NotReady => "NOT READY",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportState {
    Complete,
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SectionKind {
    Overview,
    Oracle,
    Pool,
    Positions,
    Backstop,
    Balances,
    Actors,
    Journal,
    Governance,
}

const SECTIONS: [SectionKind; 9] = [
    SectionKind::Overview,
    SectionKind::Oracle,
    SectionKind::Pool,
    SectionKind::Positions,
    SectionKind::Backstop,
    SectionKind::Balances,
    SectionKind::Actors,
    SectionKind::Journal,
    SectionKind::Governance,
];

impl SectionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Oracle => "Oracle",
            Self::Pool => "Pool",
            Self::Positions => "Positions",
            Self::Backstop => "Backstop",
            Self::Balances => "Balances",
            Self::Actors => "Actors",
            Self::Journal => "Journal",
            Self::Governance => "Governance",
        }
    }

    fn root(self) -> &'static str {
        match self {
            Self::Overview => "status",
            Self::Oracle => "oracle",
            Self::Pool => "pool",
            Self::Positions => "pool[\"positions\"]",
            Self::Backstop => "backstop",
            Self::Balances => "token_balances",
            Self::Actors => "identity",
            Self::Journal => "journal",
            Self::Governance => "governance",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SectionState {
    Missing,
    Null,
    Present,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservationState {
    Accepted,
    Blocked,
    Unavailable,
    Failed,
    Unknown,
}

impl ObservationState {
    fn parse(state: &str) -> Self {
        match state {
            "observed"
            | "adapter-price-observed"
            | "oracle-cache-accepted"
            | "breaker-clear"
            | "pool-active"
            | "admin-match"
            | "account-bound"
            | "reserve-enabled"
            | "interface-available"
            | "journal-clear" => Self::Accepted,
            "oracle-cache-blocked"
            | "breaker-tripped"
            | "breaker-blocking"
            | "pool-inactive"
            | "oracle-mismatch"
            | "address-mismatch"
            | "admin-actor-identity-collapse"
            | "reserve-disabled"
            | "journal-unresolved"
            | "journal-truncated"
            | "journal-full" => Self::Blocked,
            "null"
            | "missing"
            | "unsupported"
            | "adapter-price-null"
            | "interface-unavailable"
            | "journal-unavailable" => Self::Unavailable,
            "read-failed"
            | "malformed-response"
            | "adapter-price-malformed"
            | "oracle-cache-resolution-failed"
            | "interface-malformed"
            | "interface-mismatch" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted observation",
            Self::Blocked => "blocking observation",
            Self::Unavailable => "unavailable observation",
            Self::Failed => "failed observation",
            Self::Unknown => "unknown observation state",
        }
    }
}

enum ObservedValue {
    Missing,
    Null,
    Decoded(Value),
    Malformed(Value),
}

struct Observation {
    state: ObservationState,
    label: String,
    value: ObservedValue,
}

impl Observation {
    fn parse(object: &serde_json::Map<String, Value>) -> Self {
        let label = object.get("state").and_then(Value::as_str);
        Self {
            state: label
                .map(ObservationState::parse)
                .unwrap_or(ObservationState::Unknown),
            label: label
                .map(|text| literal(&json!(text)))
                .unwrap_or_else(|| "missing/malformed state".into()),
            value: match object.get("observed") {
                None => ObservedValue::Missing,
                Some(Value::Null) => ObservedValue::Null,
                Some(Value::String(text)) => match serde_json::from_str(text) {
                    Ok(value) => ObservedValue::Decoded(value),
                    Err(_) => ObservedValue::Malformed(Value::String(text.clone())),
                },
                Some(Value::Object(value))
                    if label == Some("read-failed")
                        && value.len() == 1
                        && value.get("state").and_then(Value::as_str) == Some("read-failed") =>
                {
                    ObservedValue::Decoded(Value::Object(value.clone()))
                }
                Some(value) => ObservedValue::Malformed(value.clone()),
            },
        }
    }
}

struct Field {
    path: String,
    preview: String,
    value: String,
    line_count: usize,
    width: usize,
    observation: Option<ObservationState>,
}

impl Field {
    fn new(
        path: String,
        value: Option<&Value>,
        context: Option<&str>,
        observation: Option<ObservationState>,
    ) -> Self {
        let kind = match value {
            None => "missing",
            Some(Value::Null) => "null",
            Some(Value::Bool(_)) => "boolean",
            Some(Value::Number(number)) if number.is_i64() || number.is_u64() => "integer",
            Some(Value::Number(_)) => "number",
            Some(Value::String(_)) => "string",
            Some(Value::Array(array)) if array.is_empty() => "empty array",
            Some(Value::Array(_)) => "array",
            Some(Value::Object(object)) if object.is_empty() => "empty object",
            Some(Value::Object(_)) => "object",
        };
        let display = value
            .map(literal)
            .unwrap_or_else(|| "missing (not reported)".into());
        let type_line = format!("Type: {kind}");
        let context_line = context.map(|context| format!("Observation: {context}"));
        let mut full_value = String::with_capacity(
            path.len()
                + type_line.len()
                + context_line.as_ref().map_or(0, String::len)
                + display.len()
                + 4,
        );
        full_value.push_str(&path);
        full_value.push('\n');
        full_value.push_str(&type_line);
        if let Some(context_line) = context_line {
            full_value.push('\n');
            full_value.push_str(&context_line);
        }
        full_value.push_str("\n\n");
        full_value.push_str(&display);
        let text = ratatui::text::Text::from(full_value.as_str());
        let width = text.width();
        let line_count = text.height();
        Self {
            path,
            preview: display,
            value: full_value,
            line_count,
            width,
            observation,
        }
    }
}

struct Section {
    state: SectionState,
    fields: Vec<Field>,
}

pub(super) struct StatusReport {
    pub(super) readiness: Readiness,
    state: ReportState,
    issue: Option<&'static str>,
    reasons: Vec<String>,
    sections: Vec<Section>,
}

// Keep exact JSON values and escape every terminal control, including DEL/C1.
fn literal(value: &Value) -> String {
    Safe::render_market_json(value, usize::MAX)
        .expect("an in-memory bounded status value fits an unbounded display cap")
        .to_string()
}

fn child_path(path: &str, key: &str) -> String {
    format!("{path}[{}]", literal(&json!(key)))
}

fn flatten_value(
    path: String,
    value: &Value,
    context: Option<&str>,
    observation: Option<ObservationState>,
    fields: &mut Vec<Field>,
) {
    match value {
        Value::Object(object) if !object.is_empty() => {
            for (key, child) in object {
                flatten_value(child_path(&path, key), child, context, observation, fields);
            }
        }
        Value::Array(array) if !array.is_empty() => {
            for (index, child) in array.iter().enumerate() {
                flatten_value(
                    format!("{path}[{index}]"),
                    child,
                    context,
                    observation,
                    fields,
                );
            }
        }
        _ => fields.push(Field::new(path, Some(value), context, observation)),
    }
}

fn is_observation(object: &serde_json::Map<String, Value>) -> bool {
    if object.contains_key("observed") {
        return true;
    }
    let Some(state) = object.get("state").and_then(Value::as_str) else {
        return false;
    };
    // Identity and journal panels have a state field but are not observations.
    !matches!(state, "account-bound" | "admin-actor-identity-collapse")
        && !state.starts_with("journal-")
}

fn flatten_wire(path: String, value: &Value, fields: &mut Vec<Field>) {
    match value {
        Value::Object(object) if is_observation(object) => {
            let observation = Observation::parse(object);
            let context = format!("{} / {}", observation.state.label(), observation.label);
            for (key, child) in object {
                if key != "observed" {
                    flatten_wire(child_path(&path, key), child, fields);
                }
            }
            let observed_path = child_path(&path, "observed");
            match observation.value {
                ObservedValue::Missing => fields.push(Field::new(
                    observed_path,
                    None,
                    Some(&format!("{context}; observed value missing")),
                    Some(observation.state),
                )),
                ObservedValue::Null => fields.push(Field::new(
                    observed_path,
                    Some(&Value::Null),
                    Some(&context),
                    Some(observation.state),
                )),
                ObservedValue::Decoded(value) => flatten_value(
                    observed_path,
                    &value,
                    Some(&context),
                    Some(observation.state),
                    fields,
                ),
                ObservedValue::Malformed(raw) => {
                    fields.push(Field::new(
                        observed_path,
                        Some(&raw),
                        Some("malformed observed JSON; showing original value"),
                        Some(ObservationState::Failed),
                    ));
                }
            }
        }
        Value::Object(object) if !object.is_empty() => {
            for (key, child) in object {
                flatten_wire(child_path(&path, key), child, fields);
            }
        }
        Value::Array(array) if !array.is_empty() => {
            for (index, child) in array.iter().enumerate() {
                flatten_wire(format!("{path}[{index}]"), child, fields);
            }
        }
        _ => fields.push(Field::new(path, Some(value), None, None)),
    }
}

impl StatusReport {
    pub(super) fn parse(safe: &Safe) -> anyhow::Result<Self> {
        let value: Value = serde_json::from_str(&safe.to_string())?;
        let root = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("status report is not an object"))?;
        let known_schema = root.get("schema_version").and_then(Value::as_u64) == Some(1);
        let reasons = root.get("reasons").and_then(Value::as_array);
        let reasons_valid = reasons.is_some_and(|items| items.iter().all(Value::is_string));
        let mut readiness = if known_schema && reasons_valid {
            match (
                root.get("ready").and_then(Value::as_bool),
                root.get("readiness").and_then(Value::as_str),
            ) {
                (Some(true), Some("ready")) if reasons.is_some_and(Vec::is_empty) => {
                    Readiness::Ready
                }
                (Some(false), Some("not-ready"))
                    if reasons.is_some_and(|items| !items.is_empty()) =>
                {
                    Readiness::NotReady
                }
                _ => Readiness::Unknown,
            }
        } else {
            Readiness::Unknown
        };
        let mut issue = if !known_schema {
            Some("Unknown/missing status schema; readiness is not established")
        } else if readiness == Readiness::Unknown {
            Some("Malformed readiness/reasons; readiness is not established")
        } else {
            None
        };
        let reasons = reasons.into_iter().flatten().map(literal).collect();
        let mut sections = Vec::with_capacity(SECTIONS.len());
        for kind in SECTIONS {
            let path = kind.root();
            let data = match kind {
                SectionKind::Overview => None,
                SectionKind::Positions => root.get("pool").and_then(|pool| pool.get("positions")),
                _ => root.get(path),
            };
            let state = match data {
                None if kind != SectionKind::Overview => SectionState::Missing,
                Some(Value::Null) => SectionState::Null,
                _ => SectionState::Present,
            };
            let mut fields = Vec::new();
            if kind == SectionKind::Overview {
                const FIRST: [&str; 6] = [
                    "readiness",
                    "ready",
                    "reasons",
                    "mutation",
                    "observed_data",
                    "schema_version",
                ];
                for key in FIRST {
                    if let Some(value) = root.get(key) {
                        flatten_wire(child_path("status", key), value, &mut fields);
                    }
                }
                // Unknown/additive top-level fields remain explorable as well.
                for (key, value) in root {
                    if !FIRST.contains(&key.as_str())
                        && !matches!(
                            key.as_str(),
                            "oracle"
                                | "pool"
                                | "backstop"
                                | "token_balances"
                                | "identity"
                                | "journal"
                                | "governance"
                        )
                    {
                        flatten_wire(child_path("status", key), value, &mut fields);
                    }
                }
            } else if let Some(data) = data {
                if kind == SectionKind::Pool {
                    if let Some(object) = data.as_object() {
                        for (key, value) in object {
                            if key != "positions" {
                                flatten_wire(child_path(path, key), value, &mut fields);
                            }
                        }
                        if object.is_empty() {
                            flatten_wire(path.into(), data, &mut fields);
                        }
                    } else {
                        flatten_wire(path.into(), data, &mut fields);
                    }
                } else if matches!(kind, SectionKind::Actors | SectionKind::Journal) {
                    flatten_value(path.into(), data, None, None, &mut fields);
                } else {
                    flatten_wire(path.into(), data, &mut fields);
                }
            } else {
                fields.push(Field::new(path.into(), None, None, None));
            }
            sections.push(Section { state, fields });
        }
        // Governance is display-only: its absence cannot weaken readiness.
        if readiness == Readiness::Ready
            && SECTIONS.iter().zip(&sections).any(|(kind, section)| {
                *kind != SectionKind::Governance && section.state == SectionState::Missing
            })
        {
            readiness = Readiness::Unknown;
            issue = Some("Partial report cannot establish readiness");
        }

        let state = if issue.is_some() {
            ReportState::Unknown
        } else if sections
            .iter()
            .any(|section| section.state == SectionState::Missing)
        {
            ReportState::Partial
        } else {
            ReportState::Complete
        };
        Ok(Self {
            readiness,
            state,
            issue,
            reasons,
            sections,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Sections,
    Fields,
    Value,
}

pub(super) struct StatusScreen {
    pub(super) open: bool,
    pub(super) request: Option<u64>,
    pub(super) snapshot: Option<StatusReport>,
    error: Option<&'static str>,
    section: usize,
    field: usize,
    focus: Focus,
    top: usize,
    horizontal: usize,
    viewport: Cell<(usize, usize)>,
}

impl Default for StatusScreen {
    fn default() -> Self {
        Self {
            open: false,
            request: None,
            snapshot: None,
            error: None,
            section: 0,
            field: 0,
            focus: Focus::Sections,
            top: 0,
            horizontal: 0,
            viewport: Cell::new((1, 1)),
        }
    }
}

impl StatusScreen {
    pub(super) fn start(&mut self, revision: u64) {
        self.open = true;
        self.request = Some(revision);
        self.error = None;
    }

    // Reopening a captured report is pure projection: no request is started,
    // so a cached snapshot can never be mistaken for a fresh read, and its
    // readiness stays a display value rather than transaction authority. A
    // prior refresh failure remains visible over the retained report.
    pub(super) fn reopen(&mut self) {
        self.open = true;
        self.request = None;
        self.clamp_scroll();
    }

    pub(super) fn finish(&mut self, result: Result<StatusReport, String>) {
        self.request = None;
        match result {
            Ok(report) => {
                let path = self.selected().map(|field| field.path.clone());
                self.field = path
                    .as_deref()
                    .and_then(|path| {
                        report.sections[self.section]
                            .fields
                            .iter()
                            .position(|field| field.path == path)
                    })
                    .unwrap_or(0);
                self.snapshot = Some(report);
                self.error = None;
                self.clamp_scroll();
            }
            Err(_) => self.error = Some("Status read/display failed; s retries; Esc returns"),
        }
    }

    fn selected(&self) -> Option<&Field> {
        self.snapshot
            .as_ref()?
            .sections
            .get(self.section)?
            .fields
            .get(self.field)
    }

    fn clamp_scroll(&mut self) {
        let (height, columns) = self.viewport.get();
        let (lines, width) = self
            .selected()
            .map(|field| (field.line_count, field.width))
            .unwrap_or((0, 0));
        self.top = self.top.min(lines.saturating_sub(height));
        self.horizontal = self.horizontal.min(width.saturating_sub(columns));
    }

    // Returns true only when Esc closes the screen, never a transaction action.
    pub(super) fn key(&mut self, code: KeyCode) -> bool {
        let height = self.viewport.get().0.max(1);
        let fields = self
            .snapshot
            .as_ref()
            .map(|report| report.sections[self.section].fields.len())
            .unwrap_or(0);
        match code {
            KeyCode::Esc if self.focus == Focus::Value => self.focus = Focus::Fields,
            KeyCode::Esc => {
                self.open = false;
                self.request = None;
                return true;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if fields == 0 {
                    Focus::Sections
                } else {
                    match (self.focus, code == KeyCode::BackTab) {
                        (Focus::Sections, false) | (Focus::Value, true) => Focus::Fields,
                        (Focus::Fields, false) | (Focus::Sections, true) => Focus::Value,
                        _ => Focus::Sections,
                    }
                };
            }
            KeyCode::Enter if fields > 0 => {
                self.focus = match self.focus {
                    Focus::Sections => Focus::Fields,
                    _ => Focus::Value,
                };
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => {
                let (index, count) = match self.focus {
                    Focus::Sections => (&mut self.section, SECTIONS.len()),
                    Focus::Fields => (&mut self.field, fields),
                    Focus::Value => {
                        let lines = self.selected().map(|field| field.line_count).unwrap_or(0);
                        (
                            &mut self.top,
                            lines.saturating_sub(height).saturating_add(1),
                        )
                    }
                };
                let old = *index;
                let page = height.saturating_sub(1).max(1);
                *index = match code {
                    KeyCode::Home => 0,
                    KeyCode::End => count.saturating_sub(1),
                    KeyCode::Up => old.saturating_sub(1),
                    KeyCode::PageUp => old.saturating_sub(page),
                    KeyCode::Down => old.saturating_add(1).min(count.saturating_sub(1)),
                    _ => old.saturating_add(page).min(count.saturating_sub(1)),
                };
                if self.focus == Focus::Sections && old != *index {
                    self.field = 0;
                }
                if self.focus != Focus::Value || matches!(code, KeyCode::Home | KeyCode::End) {
                    self.top = 0;
                    self.horizontal = 0;
                    if self.focus == Focus::Value && code == KeyCode::End {
                        self.top = count.saturating_sub(1);
                    }
                }
            }
            KeyCode::Left if self.focus == Focus::Value => {
                self.horizontal = self.horizontal.saturating_sub(8)
            }
            KeyCode::Right if self.focus == Focus::Value => {
                self.horizontal = self.horizontal.saturating_add(8)
            }
            _ => {}
        }
        self.clamp_scroll();
        false
    }

    pub(super) fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 60 || area.height < 16 {
            frame.render_widget(
                Paragraph::new("Resize to 60x16; Esc returns; q quits safely")
                    .style(Style::default().fg(ACTIVE)),
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
                Constraint::Min(8),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        let readiness = self
            .snapshot
            .as_ref()
            .map(|report| report.readiness)
            .unwrap_or(Readiness::Unknown);
        let color = match readiness {
            Readiness::Ready => SUCCESS,
            Readiness::NotReady => FAILURE,
            Readiness::Unknown => ACTIVE,
        };
        let progress = match (
            self.request.is_some(),
            self.error.is_some(),
            self.snapshot.is_some(),
        ) {
            (true, _, true) => "REFRESHING / previous snapshot",
            (true, _, false) => "LOADING",
            (_, true, true) => "STALE / REFRESH FAILED",
            (_, true, false) => "UNAVAILABLE",
            _ => "cached snapshot",
        };
        frame.render_widget(
            Paragraph::new(format!(
                "STATUS / testnet / READ ONLY / {}",
                readiness.label()
            ))
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
            rows[0],
        );
        let first = self
            .snapshot
            .as_ref()
            .and_then(|report| {
                report
                    .issue
                    .map(str::to_owned)
                    .or_else(|| report.reasons.first().cloned())
            })
            .or_else(|| self.error.map(str::to_owned))
            .unwrap_or_else(|| {
                if self.request.is_some() {
                    "Reading status; Esc returns".into()
                } else {
                    "No readiness blockers reported".into()
                }
            });
        frame.render_widget(
            Paragraph::new(clip(&first, rows[1].width as usize)).style(Style::default().fg(color)),
            rows[1],
        );
        let count = self
            .snapshot
            .as_ref()
            .map(|report| report.reasons.len())
            .unwrap_or(0);
        let completeness = self
            .snapshot
            .as_ref()
            .map(|report| match report.state {
                ReportState::Complete => "complete report",
                ReportState::Partial => "partial report; missing sections explicit",
                ReportState::Unknown => "unknown report schema/state",
            })
            .unwrap_or("no snapshot yet");
        frame.render_widget(
            Paragraph::new(format!(
                "{progress} / {count} reasons; all in Overview / {completeness}"
            )),
            rows[2],
        );
        let wide = area.width >= 100 && area.height >= 30;
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(if wide {
                vec![
                    Constraint::Length(14),
                    Constraint::Percentage(40),
                    Constraint::Min(24),
                ]
            } else {
                vec![Constraint::Length(14), Constraint::Min(30)]
            })
            .split(rows[3]);
        let height = columns[1].height.saturating_sub(2) as usize;
        let value_area = if wide { columns[2] } else { columns[1] };
        self.viewport
            .set((height.max(1), value_area.width.saturating_sub(2) as usize));
        let mut selection = ListState::default().with_selected(Some(self.section));
        frame.render_stateful_widget(
            List::new(SECTIONS.iter().map(|kind| kind.label()))
                .block(block(" Sections ", self.focus == Focus::Sections))
                .highlight_symbol("> ")
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            columns[0],
            &mut selection,
        );
        if wide || self.focus != Focus::Value {
            self.render_fields(frame, columns[1]);
        }
        if wide || self.focus == Focus::Value {
            self.render_value(frame, value_area);
        }
        frame.render_widget(
            Paragraph::new("Tab panes / Enter inspect / Esc back / s refresh / q quit"),
            rows[4],
        );
        frame.render_widget(
            Paragraph::new("Caps 64KiB/4KiB/256; arrows PgUp/Dn Home/End; L/R value"),
            rows[5],
        );
    }

    fn render_fields(&self, frame: &mut Frame, area: Rect) {
        let section = self
            .snapshot
            .as_ref()
            .map(|report| &report.sections[self.section]);
        let count = section.map(|section| section.fields.len()).unwrap_or(0);
        let title = format!(
            " {} / field {}/{} ",
            SECTIONS[self.section].label(),
            if count == 0 { 0 } else { self.field + 1 },
            count
        );
        if let Some(section) = section {
            if count == 0 {
                frame.render_widget(
                    Paragraph::new("No entries reported for this section")
                        .block(block(&title, self.focus == Focus::Fields)),
                    area,
                );
            } else {
                let mut selection = ListState::default().with_selected(Some(self.field));
                let available = area.width.saturating_sub(4) as usize;
                let value_width = (available / 2).max(16).min(available.saturating_sub(3));
                let path_width = available.saturating_sub(value_width + 3);
                let items = section.fields.iter().map(|field| {
                    let color = match field.observation {
                        Some(ObservationState::Accepted) => SUCCESS,
                        Some(ObservationState::Blocked | ObservationState::Failed) => FAILURE,
                        Some(ObservationState::Unavailable | ObservationState::Unknown) => ACTIVE,
                        None => Color::Reset,
                    };
                    let label = field
                        .path
                        .strip_prefix(SECTIONS[self.section].root())
                        .filter(|label| !label.is_empty())
                        .unwrap_or(&field.path);
                    let style = Style::default().fg(color);
                    ListItem::new(Line::from(vec![
                        Span::styled(clip_middle(label, path_width), style),
                        Span::raw(" = "),
                        Span::styled(clip(&field.preview, value_width), style),
                    ]))
                });
                frame.render_stateful_widget(
                    List::new(items)
                        .block(block(&title, self.focus == Focus::Fields))
                        .highlight_symbol("> ")
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                    area,
                    &mut selection,
                );
            }
        } else {
            frame.render_widget(
                Paragraph::new("No snapshot yet; s retries")
                    .block(block(&title, self.focus == Focus::Fields)),
                area,
            );
        }
    }

    fn render_value(&self, frame: &mut Frame, area: Rect) {
        let height = area.height.saturating_sub(2) as usize;
        if let Some(field) = self.selected() {
            let top = self.top.min(field.line_count.saturating_sub(height));
            let last = (top + height).min(field.line_count);
            let horizontal = self.horizontal.min(
                field
                    .width
                    .saturating_sub(area.width.saturating_sub(2) as usize),
            );
            let title = format!(
                " Value / lines {}-{}/{} / col {} ",
                top + 1,
                last,
                field.line_count,
                horizontal
            );
            frame.render_widget(
                Paragraph::new(field.value.as_str())
                    .scroll((top as u16, horizontal.min(u16::MAX as usize) as u16))
                    .block(block(&title, self.focus == Focus::Value)),
                area,
            );
        } else {
            frame.render_widget(
                Paragraph::new("No value reported")
                    .block(block(" Value ", self.focus == Focus::Value)),
                area,
            );
        }
    }
}

// Preview-only clipping is always marked; the stored inspector value is whole.
fn clip(text: &str, width: usize) -> Cow<'_, str> {
    let span = Span::raw(text);
    if span.width() <= width {
        return Cow::Borrowed(text);
    }
    if width == 0 {
        return Cow::Borrowed("");
    }
    let mut columns = 0;
    let mut bytes = 0;
    for grapheme in span.styled_graphemes(Style::default()) {
        let next = columns + Span::raw(grapheme.symbol).width();
        if next > width - 1 {
            break;
        }
        columns = next;
        bytes += grapheme.symbol.len();
    }
    Cow::Owned(format!("{}…", &text[..bytes]))
}

fn clip_middle(text: &str, width: usize) -> Cow<'_, str> {
    let span = Span::raw(text);
    let total = span.width();
    if total <= width {
        return Cow::Borrowed(text);
    }
    if width == 0 {
        return Cow::Borrowed("");
    }
    let prefix_columns = (width - 1) / 2;
    let suffix_start_column = total.saturating_sub(width - 1 - prefix_columns);
    let (mut columns, mut bytes, mut prefix_end, mut suffix_start) = (0, 0, 0, 0);
    for grapheme in span.styled_graphemes(Style::default()) {
        let start = columns;
        columns += Span::raw(grapheme.symbol).width();
        bytes += grapheme.symbol.len();
        if columns <= prefix_columns {
            prefix_end = bytes;
        }
        if start < suffix_start_column {
            suffix_start = bytes;
        }
    }
    Cow::Owned(format!("{}…{}", &text[..prefix_end], &text[suffix_start..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn report(oracle: Value) -> StatusReport {
        let value = json!({
            "schema_version": 1,
            "readiness": "not-ready",
            "ready": false,
            "reasons": ["oracle unavailable", "journal unresolved"],
            "identity": {"state": "admin-actor-identity-collapse"},
            "oracle": oracle,
            "pool": {"positions": {"state": "observed", "observed": "{\"u\":{\"amount\":\"922337203685477580812345\"}}"}},
            "backstop": {"reward_zone": {"state": "observed", "observed": "null"}},
            "token_balances": {"USD": {"admin": {"state": "observed", "observed": "0"}, "pool": {"state": "null", "observed": null}}},
            "journal": {"state": "journal-unavailable", "pending_count": null, "truncated": null},
            "mutation": "none"
        });
        let safe = Safe::render_market_json(&value, 65_536).unwrap();
        StatusReport::parse(&safe).unwrap()
    }

    #[test]
    fn unicode_previews_keep_visible_elision_and_complete_graphemes() {
        for width in 0..12 {
            let clipped = clip("界界界e\u{301}ending", width);
            assert!(Span::raw(clipped.as_ref()).width() <= width);
            if width > 0 {
                assert!(clipped.ends_with('…'));
            }
            let middle = clip_middle("[\"界界\"][\"e\u{301}ending\"]", width);
            assert!(Span::raw(middle.as_ref()).width() <= width);
            if width > 0 {
                assert!(middle.contains('…'));
            }
        }
        let middle = clip_middle("[\"stable\"][\"proxy_cache\"][\"timestamp\"]", 26);
        assert!(middle.starts_with("[\"stable"));
        assert!(middle.ends_with("timestamp\"]"));
    }

    #[test]
    fn bounded_read_failure_is_not_a_malformed_observation() {
        let envelope = json!({"state": "read-failed", "observed": {"state": "read-failed"}});
        let observation = Observation::parse(envelope.as_object().unwrap());
        assert_eq!(observation.state, ObservationState::Failed);
        match observation.value {
            ObservedValue::Decoded(value) => assert_eq!(value, json!({"state": "read-failed"})),
            _ => panic!("bounded read failure must remain a reported failure"),
        }
        let malformed = json!({"state": "observed", "observed": {"unexpected": 1}});
        assert!(matches!(
            Observation::parse(malformed.as_object().unwrap()).value,
            ObservedValue::Malformed(_)
        ));
    }

    #[test]
    fn nested_observations_keep_exact_amount_null_zero_and_safe_strings() {
        let report = report(json!({
            "cache": {"state": "oracle-cache-blocked", "observed": "{\"amount\":\"922337203685477580812345\",\"note\":\"\\u001b[31mred\"}"},
            "price": {"state": "null", "observed": null},
            "empty": {"state": "observed", "observed": "{}"}
        }));
        assert_eq!(report.readiness, Readiness::NotReady);
        let oracle = &report.sections[1];
        let amount = oracle
            .fields
            .iter()
            .find(|field| field.path.contains("amount"))
            .unwrap();
        assert!(amount.value.contains("922337203685477580812345"));
        let null = oracle
            .fields
            .iter()
            .find(|field| field.path.contains("price") && field.path.ends_with("[\"observed\"]"))
            .unwrap();
        let zero = report.sections[5]
            .fields
            .iter()
            .find(|field| field.path.contains("admin") && field.path.ends_with("[\"observed\"]"))
            .unwrap();
        assert!(null.preview.contains("null"));
        assert!(zero.preview.contains("0"));
        assert!(oracle
            .fields
            .iter()
            .all(|field| !field.value.chars().any(|ch| ch.is_control() && ch != '\n')));
        assert!(oracle
            .fields
            .iter()
            .any(|field| field.value.contains("\\u001b[31mred")));
        assert!(oracle
            .fields
            .iter()
            .any(|field| field.path.contains("empty") && field.value.ends_with("{}")));
        let pending = report.sections[7]
            .fields
            .iter()
            .find(|field| field.path.contains("pending_count"))
            .unwrap();
        assert!(pending.preview.contains("null"));
    }

    #[test]
    fn early_partial_report_and_invalid_schema_do_not_assume_readiness() {
        let safe = Safe::render_market_json(
            &json!({
                "schema_version": 1, "ready": false, "readiness": "not-ready",
                "reasons": ["network gate unavailable"], "mutation": "none"
            }),
            65_536,
        )
        .unwrap();
        let partial = StatusReport::parse(&safe).unwrap();
        assert_eq!(partial.readiness, Readiness::NotReady);
        assert_eq!(partial.state, ReportState::Partial);
        assert!(partial
            .sections
            .iter()
            .any(|section| section.state == SectionState::Missing));

        let unknown =
            Safe::render_market_json(&json!({"ready": true, "reasons": []}), 65_536).unwrap();
        let unknown = StatusReport::parse(&unknown).unwrap();
        assert_eq!(unknown.readiness, Readiness::Unknown);
        assert!(unknown.issue.is_some());
        let misleading = Safe::render_market_json(
            &json!({
                "schema_version": 1, "ready": true, "readiness": "ready", "reasons": []
            }),
            65_536,
        )
        .unwrap();
        let misleading = StatusReport::parse(&misleading).unwrap();
        assert_eq!(misleading.readiness, Readiness::Unknown);
    }

    #[test]
    fn long_field_values_remain_navigable_at_narrow_and_wide_sizes() {
        let rows: Vec<Value> = (0..30)
            .map(|index| json!({"state": "observed", "observed": index.to_string()}))
            .collect();
        let report = report(json!({
            "long": {"state": "observed", "observed": format!("\"{}end-marker\"", "x".repeat(250))},
            "rows": {"state": "observed", "observed": serde_json::to_string(&rows).unwrap()}
        }));
        let mut screen = StatusScreen::default();
        screen.start(1);
        screen.finish(Ok(report));
        screen.section = 1;
        screen.field = screen.snapshot.as_ref().unwrap().sections[1]
            .fields
            .iter()
            .position(|field| field.path.contains("long") && field.path.ends_with("[\"observed\"]"))
            .unwrap();
        for (width, height) in [(60, 16), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| screen.render(frame)).unwrap();
            assert!(screen.selected().unwrap().value.contains(&"x".repeat(250)));
        }
        screen.focus = Focus::Fields;
        screen.key(KeyCode::End);
        assert!(screen.selected().unwrap().path.contains("[29]"));
        screen.field = screen.snapshot.as_ref().unwrap().sections[1]
            .fields
            .iter()
            .position(|field| field.path.contains("long") && field.path.ends_with("[\"observed\"]"))
            .unwrap();
        screen.focus = Focus::Value;
        screen.key(KeyCode::End);
        for _ in 0..1_000 {
            screen.key(KeyCode::Right);
        }
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|frame| screen.render(frame)).unwrap();
        let rendered = (0..16)
            .map(|row| {
                (0..60)
                    .map(|col| terminal.backend().buffer()[(col, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("end-marker"));
        assert!(!screen.key(KeyCode::Esc));
        assert_eq!(screen.focus, Focus::Fields);
    }

    #[test]
    fn failed_refresh_keeps_snapshot_and_cancel_invalidates_pending_result() {
        let mut screen = StatusScreen::default();
        screen.start(1);
        screen.finish(Ok(report(json!({}))));
        let old = screen.snapshot.as_ref().unwrap().sections[1].fields.len();
        screen.start(2);
        screen.finish(Err("failed".into()));
        assert_eq!(
            screen.snapshot.as_ref().unwrap().sections[1].fields.len(),
            old
        );
        assert!(screen.error.is_some());
        screen.start(3);
        assert!(screen.key(KeyCode::Esc));
        assert!(!screen.open);
        assert!(screen.request.is_none());
    }

    #[test]
    fn cached_reopen_keeps_navigation_and_never_starts_a_request() {
        let mut screen = StatusScreen::default();
        screen.start(1);
        screen.finish(Ok(report(json!({}))));
        for _ in 0..3 {
            screen.key(KeyCode::Down);
        }
        screen.key(KeyCode::Tab);
        screen.key(KeyCode::Down);
        let view = (screen.section, screen.field, screen.focus);
        assert_ne!(view.0, 0, "navigation must have something to preserve");
        assert!(screen.key(KeyCode::Esc));
        assert!(!screen.open);

        screen.reopen();
        assert!(screen.open);
        assert!(screen.request.is_none(), "a cached reopen is not a read");
        assert_eq!((screen.section, screen.field, screen.focus), view);

        // A failed refresh retains the report and stays disclosed afterwards.
        screen.start(2);
        screen.finish(Err("failed".into()));
        let fields = screen.snapshot.as_ref().unwrap().sections[view.0]
            .fields
            .len();
        assert!(screen.key(KeyCode::Esc));
        screen.reopen();
        assert_eq!(
            screen.snapshot.as_ref().unwrap().sections[view.0]
                .fields
                .len(),
            fields
        );
        assert!(screen.error.is_some(), "the failed refresh stays disclosed");
        assert!(screen.request.is_none());

        // Only an explicit request arms the worker, and success clears it.
        screen.start(3);
        assert_eq!(screen.request, Some(3));
        screen.finish(Ok(report(json!({}))));
        assert!(screen.error.is_none());
        assert!(screen.request.is_none());
    }
}
