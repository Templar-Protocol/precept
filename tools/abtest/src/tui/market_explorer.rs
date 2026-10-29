//! Live market explorer. Watch remains the separate, worker-free entrypoint.
//! Tab selects a pane; arrows browse; Enter edits an argument; / filters functions.
//! r simulates, p prepares, y consumes the current exact confirmation, s reads
//! status (a captured snapshot reopens without a read; s inside refreshes), t
//! fetches a receipt. Esc cancels authority; q joins any in-flight send.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};

use crate::manifest::MarketManifest;
use crate::market_call::{self, PreparedCall};
use crate::market_explorer as catalog;
use crate::market_explorer::{NamedAddress, PublicAddress};
use crate::market_guides::{self, GuideForm};
use crate::market_query::{self, MAX_ARG_VALUE_LEN};
use crate::stellar::{Net, Policy, RealExecutor, Safe, Stellar};
use crate::tui::market_action_form::{request_type_matches, ActionForm, REQUEST_TYPE_LABELS};
use crate::tui::market_status_view::{StatusReport, StatusScreen};
use crate::{market_history, market_status};

// Reuse the terminal's semantic palette and one-cell border/row rhythm.
pub(super) const ACTIVE: Color = Color::Yellow;
pub(super) const FAILURE: Color = Color::LightRed;
pub(super) const SUCCESS: Color = Color::Green;
const MUTED: Color = Color::DarkGray;
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 16;
const ROLE_WIDTH: u16 = 20;
const FORM_HEIGHT: u16 = 6;
const TEXT_CAP: usize = 65_536;
const SEARCH_CAP: usize = 128;

struct Function {
    name: String,
    inputs: Vec<(String, Value)>,
    signature: String,
}

fn functions(spec: &Safe) -> anyhow::Result<Vec<Function>> {
    let value: Value = serde_json::from_str(&spec.to_string())?;
    let entries = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("live spec unavailable"))?;
    let mut functions = Vec::new();
    for entry in entries {
        let Some(function) = entry.get("function_v0") else {
            continue;
        };
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("function name unavailable"))?;
        let inputs = function
            .get("inputs")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("function inputs unavailable"))?;
        let inputs = inputs
            .iter()
            .map(|input| {
                let name = input
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("argument name unavailable"))?;
                let type_ = input
                    .get("type_")
                    .or_else(|| input.get("type"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok((name.to_owned(), type_))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        functions.push(Function {
            name: name.to_owned(),
            inputs,
            signature: display_json(function)?,
        });
    }
    functions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(functions)
}

fn is_address_type(type_: &Value) -> bool {
    type_.as_str() == Some("address")
}

// Pretty JSON keeps data controls escaped, unlike decoding strings into terminal text.
fn display_json(value: &Value) -> anyhow::Result<String> {
    let safe = Safe::render_market_json(value, TEXT_CAP)?;
    let parsed: Value = serde_json::from_str(&safe.to_string())?;
    Ok(serde_json::to_string_pretty(&parsed)?)
}

fn display_safe(safe: &Safe) -> String {
    let text = safe.to_string();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| display_json(&value).ok())
        .unwrap_or(text)
}

fn label(text: &str) -> String {
    // Inline dynamic names cannot introduce layout breaks or terminal controls.
    Safe::render_terminal(text, TEXT_CAP, false).to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Roles,
    Functions,
    Arguments,
    Actions,
    ActionForm,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailContext {
    Generic,
    Guided,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    ReadOnly,
    Preview,
    Awaiting,
    Submitting,
    Submitted,
    Failed,
    Unresolved,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Preview => "preview",
            Self::Awaiting => "awaiting exact confirm",
            Self::Submitting => "submitting / custody in progress",
            Self::Submitted => "submitted",
            Self::Failed => "failed / no confirmation",
            Self::Unresolved => "unresolved / sends blocked",
        }
    }

    fn exit_code(self) -> i32 {
        i32::from(matches!(
            self,
            Self::Failed | Self::Submitting | Self::Unresolved
        ))
    }
}

#[derive(Clone, Copy)]
enum AddressTarget {
    Generic(usize),
    Guided(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selector {
    Asset,
    Source,
}

impl Selector {
    fn name(self) -> &'static str {
        match self {
            Self::Asset => "asset",
            Self::Source => "source",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Asset => "Asset",
            Self::Source => "Source",
        }
    }

    fn unchanged(self) -> &'static str {
        match self {
            Self::Asset => "Asset unchanged.",
            Self::Source => "Source unchanged.",
        }
    }

    fn unmatched(self) -> &'static str {
        match self {
            Self::Asset => "No asset matches; type part of a listed asset or press Esc.",
            Self::Source => "No source matches; type part of a listed source or press Esc.",
        }
    }

    fn ambiguous(self) -> &'static str {
        match self {
            Self::Asset => "Several assets match; use Up/Down or Tab to choose one.",
            Self::Source => "Several sources match; use Up/Down or Tab to choose one.",
        }
    }
}

enum Input {
    Search,
    Argument(usize),
    Form(usize),
    Address {
        target: AddressTarget,
        selected: Option<usize>,
        explicit: bool,
    },
    RequestType {
        index: usize,
        selected: Option<usize>,
    },
    Selector {
        target: Selector,
        selected: Option<usize>,
    },
    SaveAddress(PublicAddress),
    Receipt,
}

struct Explorer {
    roles: Vec<String>,
    role: usize,
    functions: Vec<Function>,
    selected: usize,
    filter: String,
    args: Vec<(String, String)>,
    argument_address: Vec<bool>,
    named_addresses: Vec<NamedAddress>,
    argument: usize,
    pane: Pane,
    input: Option<Input>,
    edit: String,
    detail_context: DetailContext,
    detail: String,
    scroll: (u16, u16),
    scroll_max: (u16, u16),
    phase: Phase,
    message: &'static str,
    signed: bool,
    revision: u64,
    busy: bool,
    load_catalog: bool,
    prepared: Option<PreparedCall>,
    send_failed: bool,
    manifest: Option<MarketManifest>,
    actions: Vec<(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    )>,
    action_selected: usize,
    action_filter: String,
    action_category: String,
    action_name: String,
    action_asset: Option<String>,
    action_source: Option<usize>,
    eligible_assets: Vec<crate::manifest::Tok>,
    action_form: Option<ActionForm>,
    status: StatusScreen,
}

impl Explorer {
    fn new(roles: Vec<String>, signed: bool) -> Self {
        Self {
            roles,
            role: 0,
            functions: Vec::new(),
            selected: 0,
            filter: String::new(),
            args: Vec::new(),
            argument_address: Vec::new(),
            named_addresses: Vec::new(),
            argument: 0,
            pane: Pane::Roles,
            input: None,
            edit: String::new(),
            detail_context: DetailContext::Generic,
            detail: "Select a manifest role to read its live interface.".into(),
            scroll: (0, 0),
            scroll_max: (0, 0),
            phase: Phase::ReadOnly,
            message: "Tab: next pane. Reads never send; p prepares a guarded action.",
            signed,
            revision: 0,
            busy: false,
            load_catalog: true,
            manifest: None,
            actions: market_guides::catalog(),
            action_selected: 0,
            action_filter: String::new(),
            action_category: String::new(),
            action_name: String::new(),
            action_asset: None,
            action_source: None,
            eligible_assets: Vec::new(),
            action_form: None,
            prepared: None,
            send_failed: false,
            status: StatusScreen::default(),
        }
    }

    fn revoke_preparation(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.prepared = None;
    }

    fn exit_status(&self) -> i32 {
        i32::from(self.send_failed) | self.phase.exit_code()
    }

    fn invalidate(&mut self) {
        self.revoke_preparation();
        if self.phase != Phase::Unresolved {
            self.phase = Phase::ReadOnly;
        }
    }

    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        self.functions
            .iter()
            .enumerate()
            .filter(|(_, f)| f.name.to_lowercase().contains(&filter))
            .map(|(i, _)| i)
            .collect()
    }

    fn function(&self) -> Option<&Function> {
        self.visible()
            .get(self.selected)
            .and_then(|&i| self.functions.get(i))
    }
    fn visible_actions(&self) -> Vec<usize> {
        let filter = self.action_filter.to_lowercase();
        self.actions
            .iter()
            .enumerate()
            .filter(|(_, (category, action, _, _, help))| {
                format!("{category}/{action} {help}")
                    .to_lowercase()
                    .contains(&filter)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn select_action(&mut self) -> Option<Job> {
        if self.busy {
            self.message = "A worker request is in flight; guided form remains unchanged.";
            return None;
        }
        let index = *self.visible_actions().get(self.action_selected)?;
        let (category, action, role, _, _) = self.actions[index];
        self.action_category = category.to_owned();
        self.action_name = action.to_owned();
        self.action_asset = None;
        self.action_source = (role == "source").then_some(0);
        self.load_selected_guide()
    }

    fn load_selected_guide(&mut self) -> Option<Job> {
        if self.busy {
            self.message = "A worker request is in flight; guided form remains unchanged.";
            return None;
        }
        self.detail_context = DetailContext::Guided;
        self.invalidate();
        self.action_form = None;
        self.eligible_assets.clear();
        self.message = "Loading guided form from live ABI; this does not prepare or send.";
        Some(Job::GuideForm {
            category: self.action_category.clone(),
            action: self.action_name.clone(),
            asset: self.action_asset.clone(),
            source: self.action_source,
        })
    }
    fn selectors(&self) -> impl Iterator<Item = Selector> {
        [
            (!self.eligible_assets.is_empty()).then_some(Selector::Asset),
            (self.action_form.is_some() && self.action_source.is_some())
                .then_some(Selector::Source),
        ]
        .into_iter()
        .flatten()
    }

    fn form_rows(&self) -> usize {
        self.selectors().count()
            + self
                .action_form
                .as_ref()
                .map_or(0, |form| form.cells().len())
    }

    fn form_index(&self) -> Option<usize> {
        self.argument.checked_sub(self.selectors().count())
    }

    fn selector_choices(&self, target: Selector) -> Vec<String> {
        match target {
            Selector::Asset => self
                .eligible_assets
                .iter()
                .map(|asset| asset.as_str().to_owned())
                .collect(),
            Selector::Source => (0..self
                .manifest
                .as_ref()
                .map_or(0, |manifest| manifest.oracle.sources.len()))
                .map(|index| format!("source.{index}"))
                .collect(),
        }
    }

    fn selector_value(&self, target: Selector) -> Option<String> {
        match target {
            Selector::Asset => self.action_asset.clone(),
            Selector::Source => self.action_source.map(|index| format!("source.{index}")),
        }
    }

    fn selector_display(&self, target: Selector) -> String {
        self.selector_value(target)
            .unwrap_or_else(|| "not selected".into())
    }

    fn selector_matches(&self, target: Selector) -> Vec<usize> {
        let query = self.edit.to_lowercase();
        self.selector_choices(target)
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    fn open_selector_editor(&mut self, target: Selector) {
        if self.busy {
            self.message = "A worker request is in flight; guided form remains unchanged.";
            return;
        }
        self.input = Some(Input::Selector {
            target,
            selected: None,
        });
        self.edit.clear();
        self.invalidate();
        self.message = "Type to filter; Up/Down select, Tab completes, Enter commits, Esc cancels.";
    }

    fn selector_key(&mut self, code: KeyCode) -> Control {
        let (target, selected) = match self.input.as_ref() {
            Some(Input::Selector { target, selected }) => (*target, *selected),
            _ => return Control::None,
        };
        match code {
            KeyCode::Esc => {
                self.input = None;
                self.edit.clear();
                self.invalidate();
                self.message = target.unchanged();
            }
            KeyCode::Enter => {
                if self.busy {
                    self.input = None;
                    self.edit.clear();
                    self.invalidate();
                    self.message = "A worker request is in flight; guided form remains unchanged.";
                    return Control::None;
                }
                let matches = self.selector_matches(target);
                let chosen = match selected.filter(|choice| matches.contains(choice)) {
                    Some(choice) => Some(choice),
                    None if matches.len() == 1 => matches.first().copied(),
                    None if self.edit.is_empty() => {
                        let choices = self.selector_choices(target);
                        self.selector_value(target).and_then(|current| {
                            choices.iter().position(|choice| *choice == current)
                        })
                    }
                    None => None,
                };
                let Some(index) = chosen else {
                    self.invalidate();
                    self.message = if matches.is_empty() {
                        target.unmatched()
                    } else {
                        target.ambiguous()
                    };
                    return Control::None;
                };
                let value = self.selector_choices(target)[index].clone();
                self.input = None;
                self.edit.clear();
                if Some(&value) == self.selector_value(target).as_ref() {
                    self.invalidate();
                    self.message = target.unchanged();
                    return Control::None;
                }
                match target {
                    Selector::Asset => self.action_asset = Some(value),
                    Selector::Source => self.action_source = Some(index),
                }
                if let Some(job) = self.load_selected_guide() {
                    return Control::Job(job);
                }
            }
            KeyCode::Up | KeyCode::Down => {
                let matches = self.selector_matches(target);
                if !matches.is_empty() {
                    let down = code == KeyCode::Down;
                    let current =
                        selected.and_then(|choice| matches.iter().position(|item| *item == choice));
                    let next = match (current, down) {
                        (Some(position), true) => (position + 1) % matches.len(),
                        (Some(0), false) | (None, false) => matches.len() - 1,
                        (Some(position), false) => position - 1,
                        (None, true) => 0,
                    };
                    self.input = Some(Input::Selector {
                        target,
                        selected: matches.get(next).copied(),
                    });
                    self.invalidate();
                }
            }
            KeyCode::Tab => {
                let matches = self.selector_matches(target);
                let choice = selected
                    .filter(|choice| matches.contains(choice))
                    .or_else(|| matches.first().copied());
                if let Some(index) = choice {
                    self.edit = self.selector_choices(target)[index].clone();
                    self.input = Some(Input::Selector {
                        target,
                        selected: Some(index),
                    });
                    self.invalidate();
                    self.message = "Selection completed; press Enter to commit.";
                } else {
                    self.invalidate();
                    self.message = "No value matches; type part of a listed value or press Esc.";
                }
            }
            KeyCode::Backspace => {
                self.edit.pop();
                self.input = Some(Input::Selector {
                    target,
                    selected: None,
                });
                self.invalidate();
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                if self.edit.len() + ch.len_utf8() <= SEARCH_CAP {
                    self.edit.push(ch);
                    self.input = Some(Input::Selector {
                        target,
                        selected: None,
                    });
                    self.invalidate();
                } else {
                    self.message = "Selector query limit reached.";
                }
            }
            _ => {}
        }
        Control::None
    }

    fn open_action_form(&mut self, form: GuideForm) -> anyhow::Result<()> {
        let manifest = self
            .manifest
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("manifest unavailable"))?;
        let mut action_form = ActionForm::new(
            &form.inputs,
            &form.bound,
            &form.spec,
            manifest,
            &self.action_category,
            &self.action_name,
        )?;
        action_form.set_choices(&self.named_addresses);
        self.eligible_assets = form.assets;
        self.action_asset = form.selected_asset.map(|asset| asset.as_str().to_owned());
        self.action_form = Some(action_form);
        self.pane = Pane::ActionForm;
        self.detail_context = DetailContext::Guided;
        self.argument = self
            .selectors()
            .count()
            .min(self.form_rows().saturating_sub(1));
        self.detail = format!(
            "Operation: {}/{}\nStage: guide / live ABI resolved\nManifest bindings are read-only.",
            self.action_category, self.action_name
        );
        self.message = "Typed guided form ready; r simulates one call, p prepares one call.";
        Ok(())
    }

    fn select_function(&mut self) {
        self.detail_context = DetailContext::Generic;
        self.invalidate();
        self.argument = 0;
        self.scroll = (0, 0);
        if let Some(function) = self.function() {
            let args = function
                .inputs
                .iter()
                .map(|(name, _)| (name.clone(), String::new()))
                .collect();
            let argument_address = function
                .inputs
                .iter()
                .map(|(_, type_)| is_address_type(type_))
                .collect();
            self.detail = function.signature.clone();
            self.argument_address = argument_address;
            self.args = args;
        } else {
            self.args.clear();
            self.argument_address.clear();
            self.detail = "No matching functions. Press / to change the filter.".into();
        }
    }

    fn address_value(&self, target: AddressTarget) -> Option<String> {
        match target {
            AddressTarget::Generic(index) => self.args.get(index).map(|(_, value)| value.clone()),
            AddressTarget::Guided(index) => self
                .action_form
                .as_ref()
                .and_then(|form| form.edit_value(index)),
        }
    }

    fn address_matches(&self, query: &str) -> Vec<usize> {
        self.named_addresses
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                query.is_empty()
                    || choice
                        .name
                        .get(..query.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
                    || choice
                        .address
                        .as_str()
                        .get(..query.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn open_address_editor(&mut self, target: AddressTarget) {
        let selected = self.address_value(target).and_then(|value| {
            self.named_addresses
                .iter()
                .position(|choice| choice.address.as_str() == value)
        });
        self.input = Some(Input::Address {
            target,
            selected,
            explicit: false,
        });
        self.edit.clear();
        self.invalidate();
        self.message =
            "Type a name or public ID; arrows select, Tab completes, Enter commits the exact ID.";
    }

    fn open_request_type_editor(&mut self, index: usize) {
        self.input = Some(Input::RequestType {
            index,
            selected: None,
        });
        self.edit.clear();
        self.invalidate();
        self.message =
            "Type a label or 0..=9; arrows select, Tab completes, Enter commits, Esc cancels.";
    }

    fn selected_address_target(&self) -> Option<AddressTarget> {
        match self.pane {
            Pane::Arguments if self.argument_address.get(self.argument) == Some(&true) => {
                Some(AddressTarget::Generic(self.argument))
            }
            Pane::ActionForm => self.form_index().and_then(|index| {
                self.action_form
                    .as_ref()
                    .filter(|form| form.is_address(index))
                    .map(|_| AddressTarget::Guided(index))
            }),
            _ => None,
        }
    }

    fn open_address_save(&mut self) {
        self.invalidate();
        if self.busy {
            self.message = "A worker request is in flight; address saving is unavailable.";
            return;
        }
        let Some(value) = self
            .selected_address_target()
            .and_then(|target| self.address_value(target))
        else {
            self.message = "Select an editable address field to save it by name.";
            return;
        };
        match value.parse::<PublicAddress>() {
            Ok(address) => {
                self.input = Some(Input::SaveAddress(address));
                self.edit.clear();
                self.message = "Name this public address; saving stores no signing authority.";
            }
            Err(_) => {
                self.message = "Only a valid public G/C address can be saved by name.";
            }
        }
    }
    fn guided_call(&mut self, prepare: bool) -> Option<Job> {
        if self.busy {
            self.message = "A worker request is in flight; navigation remains available.";
            return None;
        }
        if prepare && (!self.signed || self.phase == Phase::Unresolved) {
            self.message = "Send disabled: a configured signer and clear custody are required.";
            return None;
        }
        let Some(form) = self.action_form.as_ref() else {
            self.message = "Choose a guided action and load its live form first.";
            return None;
        };
        let args = match form.explicit() {
            Ok(args) => args,
            Err(error) => {
                self.message = "Guided form has a required or invalid field; no call was made.";
                self.detail = label(&error.to_string());
                return None;
            }
        };
        self.detail_context = DetailContext::Guided;
        self.invalidate();
        Some(Job::Guided {
            category: self.action_category.clone(),
            action: self.action_name.clone(),
            asset: self.action_asset.clone(),
            source: self.action_source,
            args,
            prepare,
        })
    }

    fn call(&mut self, prepare: bool) -> Option<Job> {
        if self.busy {
            self.message = "A worker request is in flight; navigation and quit remain available.";
            return None;
        }
        if prepare && (!self.signed || self.phase == Phase::Unresolved) {
            self.message = "Send disabled: a configured signer and clear custody are required.";
            return None;
        }
        let Some(function) = self.function() else {
            self.message = "Choose a callable function first.";
            return None;
        };
        let method = function.name.clone();
        let role = self.roles.get(self.role)?.clone();
        if self.args.iter().any(|(_, value)| value.is_empty()) {
            self.message = "Fill every argument first; use JSON quotes for an empty string.";
            return None;
        }
        self.detail_context = DetailContext::Generic;
        self.invalidate();
        Some(Job::Call {
            role,
            method,
            args: self.args.clone(),
            prepare,
        })
    }

    fn apply(&mut self, revision: u64, reply: Reply) {
        self.busy = false;
        if let Reply::Status(result) = reply {
            if self.status.request == Some(revision) {
                self.status.finish(result);
            }
            return;
        }
        // Results from a cancelled or edited preparation can never open a new gate.
        if revision != self.revision && !matches!(&reply, Reply::Sent { .. }) {
            if matches!(&reply, Reply::Catalog { .. }) && self.functions.is_empty() {
                self.load_catalog = true;
            }
            return;
        }
        self.scroll = (0, 0);
        match reply {
            Reply::Catalog {
                functions,
                addresses,
            } => {
                let address_book_ready = addresses.is_some();
                self.named_addresses = addresses.unwrap_or_default();
                self.functions = functions;
                self.selected = 0;
                if self.pane != Pane::Actions
                    && self.pane != Pane::ActionForm
                    && self.detail_context == DetailContext::Generic
                {
                    self.select_function();
                    self.message = if address_book_ready {
                        "Live ABI loaded. Tab to functions; / searches; Enter opens arguments."
                    } else {
                        "Live ABI loaded; saved public-address book is unavailable."
                    };
                }
            }
            Reply::GuideForm { form, addresses } => {
                let address_book_ready = addresses.is_some();
                self.named_addresses = addresses.unwrap_or_default();
                match self.open_action_form(form) {
                    Ok(()) if address_book_ready => {}
                    Ok(()) => {
                        self.message =
                            "Guided form ready; saved public-address book is unavailable."
                    }
                    Err(error) => {
                        self.fail("Guided form could not be built from the live ABI.");
                        self.detail =
                            format!("Stage: live ABI/form\n{}", Safe::market_error(&error));
                        self.pane = Pane::Detail;
                    }
                }
            }
            Reply::AddressSaved(addresses) => {
                self.named_addresses = addresses;
                if let Some(form) = self.action_form.as_mut() {
                    form.set_choices(&self.named_addresses);
                }
                self.input = None;
                self.edit.clear();
                self.detail = "The run-local public address is saved. It grants no identity or signer authority.".into();
                self.message = "Named address saved; available for autocomplete.";
            }
            Reply::AddressSaveFailed { name, address } => {
                self.input = Some(Input::SaveAddress(address));
                self.edit = name;
                self.detail =
                    "The public address was not saved. Check the name and try again.".into();
                self.message = "Named address save rejected; the address field draft was kept.";
            }
            Reply::Detail(text) => {
                self.detail = text;
                self.pane = Pane::Detail;
                if self.phase != Phase::Unresolved {
                    self.phase = Phase::Preview;
                }
                self.message = if self.detail_context == DetailContext::Guided {
                    "Guided result retained: r simulates or p prepares this recipe; Esc returns to its form."
                } else {
                    "Read-only result. Arrows scroll; Tab returns to browsing."
                };
            }
            Reply::Prepared(prepared) => {
                // The core returns this capability only after a successful simulation.
                // Token stays in the same-process capability, never in display material.
                let preview = serde_json::from_str::<Value>(&prepared.summary().to_string());
                match preview {
                    Ok(mut value) => {
                        if let Some(object) = value.as_object_mut() {
                            object.remove("confirmation");
                        }
                        let operation = format!(
                            "{}.{}",
                            value
                                .get("role")
                                .and_then(Value::as_str)
                                .unwrap_or("market"),
                            value
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or("call")
                        );
                        let operation = label(&operation);
                        match display_json(&value) {
                            Ok(text) => {
                                self.detail = format!("Operation: {operation}\nStage: prepare / successful simulation\n\n{text}");
                                self.pane = Pane::Detail;
                                self.phase = Phase::Awaiting;
                                self.prepared = Some(prepared);
                                self.message = if self.detail_context == DetailContext::Guided {
                                    "Review all fields. y sends this exact call once; Esc returns to the retained form."
                                } else {
                                    "Review all fields. y sends this exact call once; Esc cancels."
                                };
                            }
                            Err(_) => {
                                self.fail("Preview cannot be displayed completely; send blocked.")
                            }
                        }
                    }
                    Err(_) => self.fail("Preview is malformed; send blocked."),
                }
            }
            Reply::Sent { text, phase } => {
                // A confirmed failed send stays sticky for the process exit
                // even if a later navigation or read-only detail rewrites the
                // display phase. Only a later successful confirmed send clears
                // it.
                match phase {
                    Phase::Failed => self.send_failed = true,
                    Phase::Submitted => self.send_failed = false,
                    _ => {}
                }
                self.detail = text;
                self.phase = phase;
                self.pane = Pane::Detail;
                self.message = "Attempt finished. No automatic replay; review outcome and custody.";
            }
            Reply::Status(_) => unreachable!("status replies are applied to the status screen"),
            Reply::Failed {
                stage,
                operation,
                diagnostic,
            } => {
                self.prepared = None;
                if self.phase != Phase::Unresolved {
                    self.phase = Phase::Failed;
                }
                let operation = label(&operation);
                self.detail = format!(
                    "Operation: {operation}\nStage: {stage}\n{}",
                    display_safe(&diagnostic)
                );
                self.message =
                    "Operation failed; no confirmation is available. Review safe stage details.";
                self.pane = Pane::Detail;
            }
        }
    }

    fn fail(&mut self, message: &'static str) {
        self.prepared = None;
        if self.phase != Phase::Unresolved {
            self.phase = Phase::Failed;
        }
        self.message = message;
        self.detail = message.into();
        self.pane = Pane::Detail;
    }

    fn key(&mut self, key: KeyEvent) -> Control {
        if key.kind != KeyEventKind::Press {
            return Control::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.revoke_preparation();
            return Control::Quit;
        }
        if self.status.open {
            if matches!(key.code, KeyCode::Char('q')) {
                self.revoke_preparation();
                return Control::Quit;
            }
            if key.code == KeyCode::Char('s') && !self.busy {
                self.revision = self.revision.wrapping_add(1);
                self.status.start(self.revision);
                return Control::Job(Job::Status);
            }
            if self.status.key(key.code) {
                self.revision = self.revision.wrapping_add(1);
                self.message = "Status closed; prepare again before sending.";
            }
            return Control::None;
        }
        if self.input.is_some() {
            return self.edit_key(key.code);
        }
        match key.code {
            KeyCode::Char('q') => {
                self.revoke_preparation();
                return Control::Quit;
            }
            KeyCode::Esc => {
                let guided_result =
                    self.pane == Pane::Detail && self.detail_context == DetailContext::Guided;
                self.invalidate();
                if guided_result {
                    self.pane = Pane::ActionForm;
                } else if self.pane == Pane::ActionForm {
                    self.pane = Pane::Actions;
                }
                self.message = "Confirmation cancelled. Prepare again before sending.";
            }
            KeyCode::Char('y') => {
                if let Some(prepared) = self.prepared.take() {
                    if !self.busy && self.signed && self.phase == Phase::Awaiting {
                        let confirmation = prepared.confirmation().to_owned();
                        self.phase = Phase::Submitting;
                        return Control::Job(Job::Execute {
                            prepared,
                            confirmation,
                        });
                    }
                }
                self.message = "No current confirmation: y cannot send. Prepare and review first.";
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.invalidate();
                self.detail_context = DetailContext::Generic;
                self.pane = match (self.pane, key.code == KeyCode::BackTab) {
                    (Pane::Roles, false) | (Pane::Arguments, true) => Pane::Functions,
                    (Pane::Functions, false) | (Pane::Detail, true) => Pane::Arguments,
                    (Pane::Arguments, false) | (Pane::Roles, true) => Pane::Detail,
                    (Pane::Actions, _) | (Pane::ActionForm, _) => Pane::Functions,
                    _ => Pane::Roles,
                };
            }
            KeyCode::Char('a') => {
                self.detail_context = DetailContext::Generic;
                self.invalidate();
                self.pane = Pane::Actions;
                self.action_filter.clear();
                self.action_selected = 0;
                self.action_asset = None;
                self.eligible_assets.clear();
                self.message =
                    "Actions catalog; / searches recipes; Enter loads a typed live form.";
            }
            KeyCode::Char('/') => {
                self.invalidate();
                self.input = Some(Input::Search);
                self.edit = if matches!(self.pane, Pane::Actions | Pane::ActionForm) {
                    self.action_filter.clone()
                } else {
                    self.filter.clone()
                };
                if !matches!(self.pane, Pane::Actions | Pane::ActionForm) {
                    self.pane = Pane::Functions;
                }
            }
            KeyCode::Char('n') => self.open_address_save(),
            KeyCode::Char('t') if !self.busy => {
                self.detail_context = DetailContext::Generic;
                self.invalidate();
                self.input = Some(Input::Receipt);
                self.edit.clear();
            }
            KeyCode::Char('s') if !self.busy => {
                // Cached readiness is display only: authority is revoked even
                // when a reopen costs the worker no read at all.
                self.invalidate();
                if self.status.snapshot.is_some() {
                    self.status.reopen();
                    self.message = "Cached read-only snapshot; s inside refreshes.";
                    return Control::None;
                }
                self.status.start(self.revision);
                return Control::Job(Job::Status);
            }
            KeyCode::Char('r') | KeyCode::Char('p') => {
                let prepare = key.code == KeyCode::Char('p');
                let job = match self.pane {
                    Pane::ActionForm => self.guided_call(prepare),
                    Pane::Detail if self.detail_context == DetailContext::Guided => {
                        self.guided_call(prepare)
                    }
                    Pane::Actions => {
                        self.message = "Select a recipe and load its typed form before acting.";
                        None
                    }
                    _ => self.call(prepare),
                };
                if let Some(job) = job {
                    return Control::Job(job);
                }
            }
            KeyCode::Enter | KeyCode::Char('e') => match self.pane {
                Pane::Roles => {
                    self.invalidate();
                    self.load_catalog = true;
                }
                Pane::Functions => {
                    self.pane = Pane::Arguments;
                    self.invalidate();
                }
                Pane::Arguments => {
                    if self.argument_address.get(self.argument) == Some(&true) {
                        self.open_address_editor(AddressTarget::Generic(self.argument));
                    } else if let Some((_, value)) = self.args.get(self.argument) {
                        self.edit = value.clone();
                        self.input = Some(Input::Argument(self.argument));
                        self.invalidate();
                    } else {
                        self.message = "This function has no arguments. r simulates; p prepares.";
                    }
                }
                Pane::Detail => {
                    self.message = "Enter never sends. y requires a fresh prepared confirmation.";
                }
                Pane::Actions => {
                    if let Some(job) = self.select_action() {
                        return Control::Job(job);
                    }
                }
                Pane::ActionForm => {
                    if let Some(target) = self.selectors().nth(self.argument) {
                        self.open_selector_editor(target);
                        return Control::None;
                    }
                    let Some(index) = self.form_index() else {
                        return Control::None;
                    };
                    if let Some(form) = self.action_form.as_mut() {
                        match form.enter(index) {
                            Ok(Some(child)) => {
                                self.invalidate();
                                self.argument = child + self.selectors().count();
                                self.scroll = (0, 0);
                                self.message = "Collection opened; Enter edits a field, +/- on the header changes rows.";
                                return Control::None;
                            }
                            Err(error) => {
                                self.invalidate();
                                self.message = "This collection cannot be edited.";
                                self.detail = label(&error.to_string());
                                return Control::None;
                            }
                            Ok(None) => {}
                        }
                    }
                    let is_address = self
                        .action_form
                        .as_ref()
                        .is_some_and(|form| form.is_address(index));
                    if is_address {
                        self.open_address_editor(AddressTarget::Guided(index));
                    } else if self
                        .action_form
                        .as_ref()
                        .is_some_and(|form| form.is_request_type(index))
                    {
                        self.open_request_type_editor(index);
                    } else if let Some(value) = self
                        .action_form
                        .as_ref()
                        .and_then(|form| form.edit_value(index))
                    {
                        self.edit = value;
                        self.input = Some(Input::Form(index));
                        self.invalidate();
                    } else {
                        self.invalidate();
                        self.message = "This guided field is not editable.";
                    }
                }
            },
            KeyCode::Up | KeyCode::Down => {
                let down = key.code == KeyCode::Down;
                match self.pane {
                    Pane::Roles => {
                        let next = move_selection(self.role, self.roles.len(), down);
                        if next != self.role {
                            self.invalidate();
                            self.role = next;
                            self.functions.clear();
                            self.args.clear();
                            self.argument_address.clear();
                            self.filter.clear();
                            self.load_catalog = true;
                        }
                    }
                    Pane::Functions => {
                        let next = move_selection(self.selected, self.visible().len(), down);
                        if next != self.selected {
                            self.selected = next;
                            self.select_function();
                        }
                    }
                    Pane::Arguments => {
                        let next = move_selection(self.argument, self.args.len(), down);
                        if next != self.argument {
                            self.argument = next;
                            self.invalidate();
                        }
                    }
                    Pane::Actions => {
                        self.action_selected = move_selection(
                            self.action_selected,
                            self.visible_actions().len(),
                            down,
                        );
                        self.action_asset = None;
                        self.eligible_assets.clear();
                        self.invalidate();
                    }
                    Pane::ActionForm => {
                        let count = self.form_rows();
                        let next = move_selection(self.argument, count, down);
                        if next != self.argument {
                            self.argument = next;
                            self.invalidate();
                            self.scroll = (0, 0);
                        }
                    }
                    Pane::Detail => self.scroll_vertical(if down { 1 } else { -1 }),
                }
            }
            KeyCode::Left | KeyCode::Right if self.pane == Pane::ActionForm => {
                let Some(index) = self.form_index() else {
                    return Control::None;
                };
                if let Some(form) = self.action_form.as_mut() {
                    if let Err(error) = form.cycle(index, key.code == KeyCode::Right) {
                        self.message = "This field has no selectable alternatives.";
                        self.detail = label(&error.to_string());
                    } else {
                        self.invalidate();
                    }
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') if self.pane == Pane::ActionForm => {
                let Some(index) = self.form_index() else {
                    return Control::None;
                };
                if let Some(form) = self.action_form.as_mut() {
                    match form.add(index) {
                        Ok(()) => self.invalidate(),
                        Err(error) => {
                            self.message = "This field cannot add a row.";
                            self.detail = label(&error.to_string());
                        }
                    }
                }
            }
            KeyCode::Char('-') if self.pane == Pane::ActionForm => {
                let Some(index) = self.form_index() else {
                    return Control::None;
                };
                if let Some(form) = self.action_form.as_mut() {
                    match form.remove(index) {
                        Ok(()) => self.invalidate(),
                        Err(error) => {
                            self.message = "This field cannot remove a row.";
                            self.detail = label(&error.to_string());
                        }
                    }
                }
            }
            KeyCode::PageDown => self.scroll_vertical(8),
            KeyCode::PageUp => self.scroll_vertical(-8),
            KeyCode::Left if self.pane == Pane::Detail => {
                self.scroll.1 = self.scroll.1.saturating_sub(8)
            }
            KeyCode::Right if self.pane == Pane::Detail => {
                self.scroll.1 = self.scroll.1.saturating_add(8).min(self.scroll_max.1)
            }
            KeyCode::Home if self.pane == Pane::Detail => self.scroll = (0, 0),
            _ => {}
        }
        Control::None
    }

    fn scroll_vertical(&mut self, delta: i16) {
        self.scroll.0 = self
            .scroll
            .0
            .saturating_add_signed(delta)
            .min(self.scroll_max.0);
    }

    fn request_type_key(&mut self, code: KeyCode) -> Control {
        let (index, selected) = match self.input.as_ref() {
            Some(Input::RequestType { index, selected }) => (*index, *selected),
            _ => return Control::None,
        };
        match code {
            KeyCode::Esc => {
                self.input = None;
                self.edit.clear();
                self.invalidate();
                self.message = "Request type unchanged.";
            }
            KeyCode::Enter => {
                let resolved = self
                    .action_form
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("guided form unavailable"))
                    .and_then(|form| form.resolve_request_type(index, &self.edit, selected));
                match resolved {
                    Ok(Some(value)) => {
                        let value = value.to_string();
                        if self
                            .action_form
                            .as_mut()
                            .is_some_and(|form| form.set(index, &value).is_ok())
                        {
                            self.input = None;
                            self.edit.clear();
                            self.message = "Request type committed as an integer opcode.";
                            self.detail =
                                "Typed request type updated; prior confirmation invalidated."
                                    .into();
                        } else {
                            self.message =
                                "Request type field is unavailable; its value was not changed.";
                        }
                    }
                    Ok(None) => {
                        self.input = None;
                        self.edit.clear();
                        self.message = "Request type unchanged.";
                    }
                    Err(error) => {
                        self.message = match error.to_string().as_str() {
                            "request type must be in 0..=9" => {
                                "Request type must be in 0..=9; correct it or press Esc."
                            }
                            "several request types match; select one explicitly" => {
                                "Several request types match; use Up/Down or Tab to choose one."
                            }
                            "enter a request type 0..=9 or a matching label" => {
                                "No request type matches; enter 0..=9 or a label prefix."
                            }
                            _ => "Request type was not changed; choose a listed value.",
                        };
                    }
                }
                self.invalidate();
            }
            KeyCode::Up | KeyCode::Down => {
                let count = request_type_matches(&self.edit).count();
                if count > 0 {
                    let down = code == KeyCode::Down;
                    let current = selected.and_then(|choice| {
                        request_type_matches(&self.edit).position(|item| item == choice)
                    });
                    let next = match (current, down) {
                        (Some(position), true) => (position + 1) % count,
                        (Some(0), false) | (None, false) => count - 1,
                        (Some(position), false) => position - 1,
                        (None, true) => 0,
                    };
                    let selected = request_type_matches(&self.edit).nth(next);
                    self.input = Some(Input::RequestType { index, selected });
                    self.invalidate();
                }
            }
            KeyCode::Tab => {
                let choice = selected
                    .filter(|choice| request_type_matches(&self.edit).any(|item| item == *choice))
                    .or_else(|| request_type_matches(&self.edit).next());
                if let Some(choice) = choice {
                    self.edit = REQUEST_TYPE_LABELS[choice].to_owned();
                    self.input = Some(Input::RequestType {
                        index,
                        selected: Some(choice),
                    });
                    self.invalidate();
                    self.message = "Request type completed; press Enter to commit.";
                } else {
                    self.message = "No request type matches; enter 0..=9 or a label prefix.";
                }
            }
            KeyCode::Backspace => {
                self.edit.pop();
                self.input = Some(Input::RequestType {
                    index,
                    selected: None,
                });
                self.invalidate();
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                if self.edit.len() + ch.len_utf8() <= SEARCH_CAP {
                    self.edit.push(ch);
                    self.input = Some(Input::RequestType {
                        index,
                        selected: None,
                    });
                    self.invalidate();
                } else {
                    self.message = "Request type query limit reached.";
                }
            }
            _ => {}
        }
        Control::None
    }

    fn address_key(&mut self, code: KeyCode) -> Control {
        let (target, selected, explicit) = match self.input.as_ref() {
            Some(Input::Address {
                target,
                selected,
                explicit,
            }) => (*target, *selected, *explicit),
            _ => return Control::None,
        };
        match code {
            KeyCode::Esc => {
                self.input = None;
                self.edit.clear();
                self.invalidate();
            }
            KeyCode::Enter => self.commit_address(target, selected, explicit),
            KeyCode::Up | KeyCode::Down => {
                let matches = self.address_matches(&self.edit);
                if !matches.is_empty() {
                    let down = code == KeyCode::Down;
                    let current =
                        selected.and_then(|index| matches.iter().position(|x| *x == index));
                    let next = match (current, down) {
                        (Some(index), true) => (index + 1) % matches.len(),
                        (Some(0), false) | (None, false) => matches.len() - 1,
                        (Some(index), false) => index - 1,
                        (None, true) => 0,
                    };
                    self.input = Some(Input::Address {
                        target,
                        selected: Some(matches[next]),
                        explicit: true,
                    });
                    self.invalidate();
                }
            }
            KeyCode::Tab => {
                let raw_address = self.edit.parse::<PublicAddress>().ok();
                if raw_address.is_none() {
                    let matches = self.address_matches(&self.edit);
                    let exact = matches.iter().copied().find(|&index| {
                        self.named_addresses[index]
                            .name
                            .eq_ignore_ascii_case(&self.edit)
                    });
                    let chosen = selected
                        .filter(|index| matches.contains(index))
                        .or(exact)
                        .or_else(|| matches.first().copied());
                    if let Some(index) = chosen {
                        self.edit = self.named_addresses[index].name.clone();
                        self.input = Some(Input::Address {
                            target,
                            selected: Some(index),
                            explicit: true,
                        });
                        self.invalidate();
                    } else {
                        self.message = "No saved name matches; enter a public G/C address.";
                    }
                }
            }
            KeyCode::Backspace => {
                self.edit.pop();
                self.input = Some(Input::Address {
                    target,
                    selected: None,
                    explicit: false,
                });
                self.invalidate();
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                if self.edit.len() + ch.len_utf8() <= SEARCH_CAP {
                    self.edit.push(ch);
                    self.input = Some(Input::Address {
                        target,
                        selected: None,
                        explicit: false,
                    });
                    self.invalidate();
                } else {
                    self.message = "Address query limit reached.";
                }
            }
            _ => {}
        }
        Control::None
    }

    fn commit_address(&mut self, target: AddressTarget, selected: Option<usize>, explicit: bool) {
        let explicit_index =
            selected.filter(|index| explicit && *index < self.named_addresses.len());
        if self.edit.is_empty() && explicit_index.is_none() {
            self.input = None;
            self.edit.clear();
            self.message = "Address unchanged.";
            self.invalidate();
            return;
        }
        let raw_address = self.edit.parse::<PublicAddress>().ok();
        let matches = if raw_address.is_some() {
            Vec::new()
        } else {
            self.address_matches(&self.edit)
        };
        let explicit_address = explicit_index
            .filter(|index| matches.contains(index))
            .map(|index| self.named_addresses[index].address.clone());
        let exact_name = matches.iter().copied().find(|&index| {
            self.named_addresses[index]
                .name
                .eq_ignore_ascii_case(&self.edit)
        });
        let address = raw_address
            .or(explicit_address)
            .or_else(|| exact_name.map(|index| self.named_addresses[index].address.clone()))
            .or_else(|| {
                let mut ids = matches
                    .iter()
                    .map(|&index| self.named_addresses[index].address.as_str());
                let first = ids.next()?;
                ids.all(|id| id == first)
                    .then(|| self.named_addresses[matches[0]].address.clone())
            });
        let Some(address) = address else {
            self.message = if matches.len() > 1 {
                "Several names match; use Up/Down or Tab to select a visible address."
            } else {
                "No saved name matches; enter a valid public G/C address."
            };
            return;
        };
        let value = address.as_str().to_owned();
        let updated = match target {
            AddressTarget::Generic(index) => {
                if let Some((_, field)) = self.args.get_mut(index) {
                    *field = value;
                    true
                } else {
                    false
                }
            }
            AddressTarget::Guided(index) => self
                .action_form
                .as_mut()
                .is_some_and(|form| form.set(index, &value).is_ok()),
        };
        if !updated {
            self.message = "This address field is unavailable; its value was not changed.";
            return;
        }
        self.input = None;
        self.edit.clear();
        self.detail = "Address committed as its exact public ID.".into();
        self.message = "Address committed; the exact public ID will be used for this call.";
        self.invalidate();
    }

    fn edit_key(&mut self, code: KeyCode) -> Control {
        if matches!(self.input.as_ref(), Some(Input::Selector { .. })) {
            return self.selector_key(code);
        }
        if matches!(self.input.as_ref(), Some(Input::RequestType { .. })) {
            return self.request_type_key(code);
        }
        if matches!(self.input.as_ref(), Some(Input::Address { .. })) {
            return self.address_key(code);
        }
        match code {
            KeyCode::Esc => {
                self.input = None;
                self.edit.clear();
                self.invalidate();
            }
            KeyCode::Enter => match self.input.take() {
                Some(Input::Search) if matches!(self.pane, Pane::Actions | Pane::ActionForm) => {
                    self.action_filter = std::mem::take(&mut self.edit);
                    self.action_selected = 0;
                    self.action_asset = None;
                    self.eligible_assets.clear();
                    self.pane = Pane::Actions;
                    self.invalidate();
                }
                Some(Input::Search) => {
                    self.filter = std::mem::take(&mut self.edit);
                    self.selected = 0;
                    self.select_function();
                }
                Some(Input::Argument(index)) => {
                    if let Some((_, value)) = self.args.get_mut(index) {
                        *value = std::mem::take(&mut self.edit);
                    }
                    self.invalidate();
                }
                Some(Input::Form(index)) => {
                    match self.action_form.as_mut() {
                        Some(form) => match form.set(index, &self.edit) {
                            Ok(()) => {
                                self.detail =
                                    "Typed field updated; prior confirmation invalidated.".into();
                                self.edit.clear();
                            }
                            Err(error) => {
                                self.input = Some(Input::Form(index));
                                self.detail = label(&error.to_string());
                                self.scroll = (0, 0);
                            }
                        },
                        None => {
                            self.edit.clear();
                            self.detail = "Guided form is unavailable; editor closed.".into();
                        }
                    }
                    self.invalidate();
                }
                Some(Input::SaveAddress(address)) => {
                    let name = std::mem::take(&mut self.edit);
                    if name.is_empty() {
                        self.input = Some(Input::SaveAddress(address));
                        self.message = "Enter a name for this public address or Esc to cancel.";
                        self.detail.clear();
                        self.detail.push_str(self.message);
                        self.scroll = (0, 0);
                    } else {
                        self.invalidate();
                        self.message = "Saving named address...";
                        return Control::Job(Job::SaveNamedAddress { name, address });
                    }
                }
                Some(Input::Receipt) => {
                    return Control::Job(Job::Receipt(std::mem::take(&mut self.edit)));
                }
                Some(
                    Input::Address { .. } | Input::RequestType { .. } | Input::Selector { .. },
                )
                | None => {}
            },
            KeyCode::Backspace => {
                self.edit.pop();
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                let cap = match self.input.as_ref() {
                    Some(Input::Argument(_) | Input::Form(_)) => MAX_ARG_VALUE_LEN,
                    Some(Input::SaveAddress(_)) => 64,
                    Some(Input::Receipt) => 64,
                    _ => SEARCH_CAP,
                };
                if self.edit.len() + ch.len_utf8() <= cap {
                    self.edit.push(ch);
                } else {
                    self.message = "Input limit reached; delete characters before adding more.";
                    if self.pane == Pane::ActionForm && !self.detail.ends_with(self.message) {
                        self.detail.push_str("\n\n");
                        self.detail.push_str(self.message);
                    }
                }
            }
            _ => {}
        }
        Control::None
    }
}

fn move_selection(index: usize, count: usize, down: bool) -> usize {
    if down {
        index.saturating_add(1).min(count.saturating_sub(1))
    } else {
        index.saturating_sub(1)
    }
}

// Bounded single-shot channel messages: each job, reply, and control value
// is constructed once, moved whole between the operator UI and the market
// worker, and dropped after its one delivery or use. No value is held or
// stored idle, and boxing the largest variant would add a heap allocation to
// every job submission and reply on this operator path to shrink a type that
// never exists outside its single move.
#[allow(clippy::large_enum_variant)]
enum Job {
    Catalog(String),
    GuideForm {
        category: String,
        action: String,
        asset: Option<String>,
        source: Option<usize>,
    },
    Call {
        role: String,
        method: String,
        args: Vec<(String, String)>,
        prepare: bool,
    },
    Guided {
        category: String,
        action: String,
        asset: Option<String>,
        source: Option<usize>,
        args: Vec<(String, String)>,
        prepare: bool,
    },
    SaveNamedAddress {
        name: String,
        address: PublicAddress,
    },
    Execute {
        prepared: PreparedCall,
        confirmation: String,
    },
    Status,
    Receipt(String),
}

// Same single-shot channel message boundary as `Job` above.
#[allow(clippy::large_enum_variant)]
enum Reply {
    Catalog {
        functions: Vec<Function>,
        addresses: Option<Vec<NamedAddress>>,
    },
    GuideForm {
        form: GuideForm,
        addresses: Option<Vec<NamedAddress>>,
    },
    AddressSaved(Vec<NamedAddress>),
    AddressSaveFailed {
        name: String,
        address: PublicAddress,
    },
    Detail(String),
    Prepared(PreparedCall),
    Status(Result<StatusReport, String>),
    Sent {
        text: String,
        phase: Phase,
    },
    Failed {
        stage: &'static str,
        operation: String,
        diagnostic: Safe,
    },
}

// Same single-shot channel message boundary as `Job` above: boxing would add
// heap allocations to the operator path without any value outliving its move.
#[allow(clippy::large_enum_variant)]
enum Control {
    None,
    Quit,
    Job(Job),
}

fn failed(stage: &'static str, operation: String, error: &anyhow::Error) -> Reply {
    Reply::Failed {
        stage,
        operation,
        diagnostic: Safe::market_error(error),
    }
}

fn worker(
    out: PathBuf,
    manifest: MarketManifest,
    net: Net,
    exec: RealExecutor,
    signer: Option<String>,
    jobs: Receiver<(u64, Job)>,
    replies: Sender<(u64, Reply)>,
) {
    let st = Stellar::new(&net, &exec, Policy::Market);
    while let Ok((revision, job)) = jobs.recv() {
        let reply = match job {
            Job::Catalog(role) => match catalog::inspect(&st, &out, &manifest, &role, None)
                .and_then(|spec| functions(&spec))
            {
                Ok(functions) => Reply::Catalog {
                    functions,
                    addresses: crate::market_explorer::named_addresses(&manifest, &out).ok(),
                },
                Err(error) => failed("live ABI", role, &error),
            },
            Job::GuideForm {
                category,
                action,
                asset,
                source,
            } => match market_guides::form(
                &st,
                &out,
                &manifest,
                &category,
                &action,
                asset.as_deref(),
                source,
            ) {
                Ok(form) => Reply::GuideForm {
                    form,
                    addresses: crate::market_explorer::named_addresses(&manifest, &out).ok(),
                },
                Err(error) => failed("guide / live ABI", format!("{category}/{action}"), &error),
            },
            Job::Call {
                role,
                method,
                args,
                prepare,
            } => {
                let operation = format!("{role}.{method}");
                if prepare {
                    match signer.as_deref() {
                        Some(alias) => match market_call::prepare(
                            &st, &out, &manifest, &role, &method, &args, alias,
                        ) {
                            Ok(prepared) => Reply::Prepared(prepared),
                            Err(error) => failed("prepare / simulation", operation, &error),
                        },
                        None => {
                            let error = anyhow::anyhow!("signer required");
                            failed("prepare", operation, &error)
                        }
                    }
                } else {
                    match market_query::query(&st, &out, &manifest, &role, &method, &args)
                        .and_then(|value| display_json(&value))
                    {
                        Ok(text) => Reply::Detail(format!(
                            "Operation: {operation}\nStage: read-only simulation\n\n{text}"
                        )),
                        Err(error) => failed("read-only simulation", operation, &error),
                    }
                }
            }
            Job::Guided {
                category,
                action,
                asset,
                source,
                args,
                prepare,
            } => {
                let operation = format!("{category}/{action}");
                match market_guides::guide(
                    &st,
                    &out,
                    &manifest,
                    &category,
                    &action,
                    asset.as_deref(),
                    source,
                    &args,
                ) {
                    Err(error) => failed("guide / live ABI validation", operation, &error),
                    Ok((role, method, args)) if prepare => match signer.as_deref() {
                        Some(alias) => match market_call::prepare(
                            &st, &out, &manifest, &role, &method, &args, alias,
                        ) {
                            Ok(prepared) => Reply::Prepared(prepared),
                            Err(error) => failed("prepare / simulation", operation, &error),
                        },
                        None => {
                            let error = anyhow::anyhow!("signer required");
                            failed("prepare", operation, &error)
                        }
                    },
                    Ok((role, method, args)) => {
                        match market_query::query(&st, &out, &manifest, &role, &method, &args)
                            .and_then(|value| display_json(&value))
                        {
                            Ok(text) => Reply::Detail(format!(
                                "Operation: {operation}\nStage: read-only simulation\n\n{text}"
                            )),
                            Err(error) => failed("read-only simulation", operation, &error),
                        }
                    }
                }
            }
            Job::SaveNamedAddress { name, address } => {
                let retry_address = address.clone();
                match crate::market_explorer::save_named_address(&manifest, &out, &name, address) {
                    Ok(addresses) => Reply::AddressSaved(addresses),
                    Err(_) => Reply::AddressSaveFailed {
                        name,
                        address: retry_address,
                    },
                }
            }
            Job::Status => Reply::Status(
                market_status::status(&st, &out, &manifest)
                    .and_then(|(safe, _)| StatusReport::parse(&safe))
                    .map_err(|error| error.to_string()),
            ),
            Job::Receipt(hash) => match market_history::receipt(&st, &hash) {
                Ok(safe) => Reply::Detail(display_safe(&safe)),
                Err(error) => failed("receipt read", "receipt".into(), &error),
            },
            Job::Execute {
                prepared,
                confirmation,
            } => {
                let summary = serde_json::from_str::<Value>(&prepared.summary().to_string()).ok();
                let operation = summary.as_ref().map(|value| {
                    format!(
                        "{}.{}",
                        value
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("market"),
                        value
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or("call")
                    )
                });
                let (text, phase) = match market_call::execute(&st, &out, &manifest, prepared, &confirmation) {
                    Ok(result) => {
                        let value = json!({"stage": "execute", "operation": operation, "outcome": result.outcome, "value": result.value});
                        let text = display_json(&value).unwrap_or_else(|_| "Result exceeds display limit; consult retained custody.".into());
                        (text, if result.accepted { Phase::Submitted } else { Phase::Failed })
                    }
                    Err(error) if market_status::journal_status(&out).get("state").and_then(Value::as_str)
                        == Some(market_status::STATE_PENDING_FULL) => (
                        format!("Operation: {}\nStage: execute\n{}\nJournal scan capacity reached. No successful submission is claimed. Preserve old custody and use a fresh disposable run for further actions.", operation.as_deref().unwrap_or("market"), Safe::market_error(&error)), Phase::Failed),
                    Err(error) if !market_status::journal_clear_for_action(&out) => (
                        format!("Operation: {}\nStage: execute\n{}\nUnresolved custody: submission may have landed. Further sends are blocked. Do not replay; inspect custody and receipts.", operation.as_deref().unwrap_or("market"), Safe::market_error(&error)), Phase::Unresolved),
                    Err(error) => (format!("Operation: {}\nStage: execute\n{}", operation.as_deref().unwrap_or("market"), Safe::market_error(&error)), Phase::Failed),
                };
                Reply::Sent { text, phase }
            }
        };
        if replies.send((revision, reply)).is_err() {
            break;
        }
    }
}

pub(super) fn block(title: &str, active: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if active { ACTIVE } else { MUTED }))
}

fn list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    active: bool,
    rows: Vec<String>,
    selected: usize,
) {
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(rows)
            .block(block(title, active))
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn scroll_limits(paragraph: &Paragraph<'_>, area: Rect, wraps: bool) -> (u16, u16) {
    let vertical = paragraph
        .line_count(area.width.saturating_sub(2))
        .saturating_sub(area.height as usize)
        .min(u16::MAX as usize) as u16;
    let horizontal = if wraps {
        0
    } else {
        paragraph
            .line_width()
            .saturating_sub(area.width as usize)
            .min(u16::MAX as usize) as u16
    };
    (vertical, horizontal)
}

fn render_address_editor(
    state: &Explorer,
    frame: &mut Frame,
    target: AddressTarget,
    selected: Option<usize>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new("PUBLIC ADDRESS AUTOCOMPLETE / aliases store no signing authority")
            .style(Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(rows[1]);
    let matches = state.address_matches(&state.edit);
    let names = matches
        .iter()
        .map(|&index| label(&state.named_addresses[index].name))
        .collect::<Vec<_>>();
    let visible_selected =
        selected.and_then(|index| matches.iter().position(|&item| item == index));
    let mut list_state = ListState::default().with_selected(visible_selected);
    frame.render_stateful_widget(
        List::new(names)
            .block(block(" names / Up Down select ", true))
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        columns[0],
        &mut list_state,
    );
    let current = state.address_value(target);
    let display_id = |value: &str| {
        let value = label(value);
        if value.len() > 28 {
            format!("{}\n{}", &value[..28], &value[28..])
        } else {
            value
        }
    };
    let inspected = selected
        .and_then(|index| state.named_addresses.get(index))
        .map(|choice| {
            format!(
                "Selected name: {}\nFull public ID:\n{}",
                label(&choice.name),
                display_id(choice.address.as_str())
            )
        })
        .unwrap_or_else(|| {
            let current = current.as_deref().unwrap_or("(empty)");
            format!(
                "Current public ID:\n{}\n\n{} matching saved name(s). Use Up/Down to inspect a full ID.",
                display_id(current),
                matches.len()
            )
        });
    let detail = Paragraph::new(inspected)
        .wrap(Wrap { trim: false })
        .block(block(" selected exact address ", false));
    frame.render_widget(detail, columns[1]);
    frame.render_widget(
        Paragraph::new(format!("Address query: {}", label(&state.edit)))
            .style(Style::default().fg(ACTIVE)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(label(state.message)).style(Style::default().fg(ACTIVE)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new("Up/Down inspect / Tab complete / Enter commit / Esc cancel / raw G/C / n saves outside"),
        rows[4],
    );
}

fn render_request_type_editor(
    state: &Explorer,
    frame: &mut Frame,
    index: usize,
    selected: Option<usize>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new("REQUEST TYPE AUTOCOMPLETE")
            .style(Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    let choices = request_type_matches(&state.edit)
        .map(|choice| format!("{choice} {}", REQUEST_TYPE_LABELS[choice]));
    let selected_row = selected
        .and_then(|choice| request_type_matches(&state.edit).position(|item| item == choice));
    let mut list_state = ListState::default().with_selected(selected_row);
    frame.render_stateful_widget(
        List::new(choices)
            .block(block(" opcodes / Up Down select ", true))
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        rows[1],
        &mut list_state,
    );
    let current = state
        .action_form
        .as_ref()
        .and_then(|form| form.edit_value(index))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            "Request type query: {}  Current: {}",
            label(&state.edit),
            label(&current)
        ))
        .style(Style::default().fg(ACTIVE)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(label(state.message)).style(Style::default().fg(ACTIVE)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new("Up/Down select  Tab complete  Enter commit  Esc cancel"),
        rows[4],
    );
}

fn render_selector_editor(
    state: &Explorer,
    frame: &mut Frame,
    target: Selector,
    selected: Option<usize>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new(format!("{} SELECTOR", target.title().to_uppercase()))
            .style(Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    let matches = state.selector_matches(target);
    let choices = state.selector_choices(target);
    let names = matches.iter().map(|&index| label(&choices[index]));
    let selected_row = selected.and_then(|choice| matches.iter().position(|item| *item == choice));
    let mut list_state = ListState::default().with_selected(selected_row);
    let title = format!(" {} / Up Down select ", target.name());
    frame.render_stateful_widget(
        List::new(names)
            .block(block(&title, true))
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        rows[1],
        &mut list_state,
    );
    frame.render_widget(
        Paragraph::new(format!(
            "{} query: {}  Current: {}",
            target.title(),
            label(&state.edit),
            label(&state.selector_display(target))
        ))
        .style(Style::default().fg(ACTIVE)),
        rows[2],
    );
    // Filtering is live, so an empty result is explained before Enter.
    let message = if matches.is_empty() && !state.edit.is_empty() {
        target.unmatched()
    } else {
        state.message
    };
    frame.render_widget(
        Paragraph::new(label(message)).style(Style::default().fg(ACTIVE)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new("Up/Down select  Tab complete  Enter commit  Esc cancel"),
        rows[4],
    );
}

fn render(state: &mut Explorer, frame: &mut Frame) {
    let area = frame.area();
    if state.status.open {
        state.status.render(frame);
        return;
    }
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(if state.input.is_some() {
                "Resize to at least 60x16; Ctrl-C quits safely."
            } else {
                "Resize to at least 60x16; q quits safely."
            })
            .style(Style::default().fg(ACTIVE)),
            area,
        );
        return;
    }
    if let Some(Input::Selector { target, selected }) = state.input.as_ref() {
        render_selector_editor(state, frame, *target, *selected);
        return;
    }
    if let Some(Input::RequestType {
        index, selected, ..
    }) = state.input.as_ref()
    {
        render_request_type_editor(state, frame, *index, *selected);
        return;
    }
    if let Some(Input::Address {
        target, selected, ..
    }) = state.input.as_ref()
    {
        render_address_editor(state, frame, *target, *selected);
        return;
    }
    let editing = state.input.is_some();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let color = match state.phase {
        Phase::Failed | Phase::Unresolved => FAILURE,
        Phase::Submitted => SUCCESS,
        _ => ACTIVE,
    };
    let header = format!(
        "MARKET / testnet / {} / {}{}",
        if state.signed {
            "signer configured"
        } else {
            "unsigned"
        },
        state.phase.label(),
        if state.busy { " / working" } else { "" }
    );
    frame.render_widget(
        Paragraph::new(header).style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    if state.pane == Pane::Detail {
        let title = if editing {
            " detail / input active "
        } else if state.detail_context == DetailContext::Guided {
            " guided result / Esc returns to form / arrows scroll / Home resets "
        } else {
            " detail / arrows scroll / Home resets / Tab returns "
        };
        let paragraph = Paragraph::new(state.detail.as_str()).block(block(title, true));
        state.scroll_max = scroll_limits(&paragraph, rows[1], false);
        state.scroll.0 = state.scroll.0.min(state.scroll_max.0);
        state.scroll.1 = state.scroll.1.min(state.scroll_max.1);
        frame.render_widget(paragraph.scroll(state.scroll), rows[1]);
    } else if matches!(state.pane, Pane::Actions | Pane::ActionForm) {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(rows[1]);
        if state.pane == Pane::Actions {
            let visible = state.visible_actions();
            let names = visible
                .iter()
                .map(|&index| {
                    let (category, action, _, _, _) = state.actions[index];
                    format!("{}/{}", label(category), label(action))
                })
                .collect();
            list(
                frame,
                columns[0],
                " Actions / recipes ",
                true,
                names,
                state.action_selected,
            );
            let selected = visible
                .get(state.action_selected)
                .map(|&index| state.actions[index]);
            let mut detail = selected
                .map(|(category, action, role, method, help)| {
                    format!("{category}/{action}\nRole: {role}\nMethod: {method}\n\n{help}")
                })
                .unwrap_or_else(|| "No recipe matches this filter.".into());
            detail.push_str(&format!(
                "\n\nAsset: {}\nAvailable assets: {}\nSource: {}\n\nEnter loads the typed form; choose asset and source there.",
                state.action_asset.as_deref().unwrap_or("not selected"),
                state
                    .eligible_assets
                    .iter()
                    .map(|asset| asset.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                state
                    .action_source
                    .map(|index| index.to_string())
                    .unwrap_or_else(|| "not selected".into()),
            ));
            let paragraph =
                Paragraph::new(Safe::render_terminal(&detail, TEXT_CAP, true).to_string())
                    .wrap(Wrap { trim: false })
                    .block(block(" recipe help and selectors ", false));
            state.scroll_max = scroll_limits(&paragraph, columns[1], true);
            state.scroll.0 = state.scroll.0.min(state.scroll_max.0);
            state.scroll.1 = state.scroll.1.min(state.scroll_max.1);
            frame.render_widget(paragraph.scroll(state.scroll), columns[1]);
        } else {
            let selectors = state.selectors().collect::<Vec<_>>();
            let cells = state
                .action_form
                .as_ref()
                .map(ActionForm::cells)
                .unwrap_or_default();
            let mut rows = selectors
                .iter()
                .map(|target| {
                    format!(
                        "{} = {}",
                        target.name(),
                        label(&state.selector_display(*target))
                    )
                })
                .collect::<Vec<_>>();
            rows.extend(
                cells
                    .iter()
                    .map(|(path, value, _)| format!("{} = {}", label(path), label(value))),
            );
            list(
                frame,
                columns[0],
                if editing {
                    " selectors and typed fields "
                } else {
                    " selectors and typed fields / Enter opens chooser or edits / +/- rows "
                },
                true,
                rows,
                state.argument,
            );
            let asset_options = state
                .eligible_assets
                .iter()
                .map(|asset| asset.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let state_line = if editing {
                "Input active; action shortcuts inactive."
            } else {
                state.message
            };
            let detail = if let Some(target) = selectors.get(state.argument).copied() {
                format!(
                    "{}\n\n{} = {}\nAvailable: {}\n\nEnter opens the chooser; Up/Down select, Tab completes, Enter commits, Esc cancels.\n\n{}",
                    state.detail,
                    target.name(),
                    state.selector_display(target),
                    state.selector_choices(target).join(", "),
                    state_line
                )
            } else if let Some((path, value, hint)) =
                state.form_index().and_then(|index| cells.get(index))
            {
                format!(
                    "{}\n\n{}\n\n{}\n\n{}\n\nAsset: {}\nAvailable assets: {}\nSource: {}\n\nState: {}",
                    state.detail,
                    path,
                    hint,
                    value,
                    state.action_asset.as_deref().unwrap_or("not selected"),
                    asset_options,
                    state
                        .action_source
                        .map(|index| index.to_string())
                        .unwrap_or_else(|| "not selected".into()),
                    state_line
                )
            } else {
                format!(
                    "{}\n\nRecipe takes no operator fields.\n\nAsset: {}\nAvailable assets: {}\nSource: {}\n\n{}",
                    state.detail,
                    state.action_asset.as_deref().unwrap_or("not selected"),
                    asset_options,
                    state
                        .action_source
                        .map(|index| index.to_string())
                        .unwrap_or_else(|| "not selected".into()),
                    state_line
                )
            };
            let paragraph =
                Paragraph::new(Safe::render_terminal(&detail, TEXT_CAP, true).to_string())
                    .wrap(Wrap { trim: false })
                    .block(block(" selected value / full inspection ", false));
            state.scroll_max = scroll_limits(&paragraph, columns[1], true);
            state.scroll.0 = state.scroll.0.min(state.scroll_max.0);
            state.scroll.1 = state.scroll.1.min(state.scroll_max.1);
            frame.render_widget(paragraph.scroll(state.scroll), columns[1]);
        }
    } else {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(ROLE_WIDTH), Constraint::Min(30)])
            .split(rows[1]);
        list(
            frame,
            columns[0],
            " manifest roles ",
            state.pane == Pane::Roles,
            state.roles.iter().map(|role| label(role)).collect(),
            state.role,
        );
        let content = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(FORM_HEIGHT),
                Constraint::Length(FORM_HEIGHT),
                Constraint::Min(3),
            ])
            .split(columns[1]);
        let visible = state.visible();
        let names = if visible.is_empty() {
            vec![if editing {
                "No functions.".into()
            } else {
                "No functions; / clears filter".into()
            }]
        } else {
            visible
                .iter()
                .map(|&i| label(&state.functions[i].name))
                .collect()
        };
        list(
            frame,
            content[0],
            if editing {
                " live functions "
            } else {
                " live functions / search "
            },
            state.pane == Pane::Functions,
            names,
            state.selected,
        );
        let args = if state.args.is_empty() {
            vec!["No arguments required".into()]
        } else {
            state
                .args
                .iter()
                .map(|(name, value)| format!("{}={}", label(name), label(value)))
                .collect()
        };
        list(
            frame,
            content[1],
            if editing {
                " arguments "
            } else {
                " arguments / Enter edits value "
            },
            state.pane == Pane::Arguments,
            args,
            state.argument,
        );
        let paragraph = Paragraph::new(state.detail.as_str()).block(block(
            if editing {
                " result / input active "
            } else {
                " result / Tab for full detail "
            },
            false,
        ));
        state.scroll_max = scroll_limits(&paragraph, content[2], false);
        state.scroll.0 = state.scroll.0.min(state.scroll_max.0);
        state.scroll.1 = state.scroll.1.min(state.scroll_max.1);
        frame.render_widget(paragraph.scroll(state.scroll), content[2]);
    }
    let input = match &state.input {
        Some(Input::Search) => format!("Filter: {} [Enter apply / Esc cancel]", label(&state.edit)),
        Some(Input::Argument(index)) => format!(
            "{}={} [Enter save / Esc cancel]",
            state
                .args
                .get(*index)
                .map(|(name, _)| label(name))
                .unwrap_or_default(),
            label(&state.edit)
        ),
        Some(Input::Form(index)) => format!(
            "{}={} [Enter save / Esc cancel]",
            state
                .action_form
                .as_ref()
                .and_then(|form| form.cells().get(*index).map(|cell| label(&cell.0)))
                .unwrap_or_default(),
            label(&state.edit)
        ),
        Some(Input::Selector { target, .. }) => {
            format!("{} query: {}", target.title(), label(&state.edit))
        }
        Some(Input::RequestType { .. }) => {
            format!("Request type query: {}", label(&state.edit))
        }
        Some(Input::Address { .. }) => format!("Address query: {}", label(&state.edit)),
        Some(Input::SaveAddress(_)) => format!(
            "Save address as: {} [Enter save / Esc cancel]",
            label(&state.edit)
        ),
        Some(Input::Receipt) => format!(
            "Receipt hash: {} [Enter fetch / Esc cancel]",
            label(&state.edit)
        ),
        None => state.message.into(),
    };
    let editor = Paragraph::new(input).style(Style::default().fg(ACTIVE));
    let editor = if state.input.is_some() {
        let width = label(&state.edit).chars().count().saturating_add(48);
        editor.scroll((
            0,
            width
                .saturating_sub(rows[2].width as usize)
                .min(u16::MAX as usize) as u16,
        ))
    } else {
        editor
    };
    frame.render_widget(editor, rows[2]);
    frame.render_widget(
        Paragraph::new(if editing {
            "r/p/y type text; action shortcuts inactive"
        } else {
            "a Actions  Tab panes  arrows browse  Enter open/edit  / search  n save address"
        }),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new(if editing {
            "Enter completes input  Esc cancels  Ctrl-C quits safely"
        } else if state.prepared.is_some() {
            "y confirms exact call ONCE  Esc cancels  PgUp/PgDn scroll  q quits safely"
        } else if state.pane == Pane::Actions || state.pane == Pane::ActionForm {
            "Actions: Enter asset/source chooser, +/- rows, arrows select, p prepare, y needs preview"
        } else {
            "p prepare  y disabled without preview  PgUp/PgDn scroll  Esc cancel  q quit"
        }),
        rows[4],
    );
}

pub fn main_market_explorer(
    out: PathBuf,
    manifest: MarketManifest,
    net: Net,
    exec: RealExecutor,
    signer: Option<String>,
) -> anyhow::Result<i32> {
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "the market explorer needs a terminal"
    );
    super::recover_gate_geometry()?;
    let roles = catalog::roles(&manifest)
        .into_iter()
        .map(|(role, _)| role)
        .collect();
    let mut state = Explorer::new(roles, signer.is_some());
    state.manifest = Some(manifest.clone());
    let (jobs_tx, jobs_rx) = channel();
    let (replies_tx, replies_rx) = channel();
    let worker =
        std::thread::spawn(move || worker(out, manifest, net, exec, signer, jobs_rx, replies_tx));
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut state, &jobs_tx, &replies_rx);
    ratatui::restore();
    // Closing the queue never detaches a child or releases custody during a send.
    drop(jobs_tx);
    if state.busy {
        eprintln!(
            "finishing the in-flight request and retaining its outcome; do not kill this process"
        );
    }
    let joined = worker.join().is_ok();
    // Quit can race the last worker reply; preserve its failure exit status.
    while let Ok((revision, reply)) = replies_rx.try_recv() {
        state.apply(revision, reply);
    }
    result?;
    anyhow::ensure!(
        joined,
        "market explorer worker stopped unexpectedly; inspect custody"
    );
    Ok(state.exit_status())
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    state: &mut Explorer,
    jobs: &Sender<(u64, Job)>,
    replies: &Receiver<(u64, Reply)>,
) -> anyhow::Result<()> {
    loop {
        match replies.try_recv() {
            Ok((revision, reply)) => {
                if matches!(&reply, Reply::Prepared(_)) {
                    super::drain_stale_input()?;
                }
                state.apply(revision, reply);
            }
            Err(TryRecvError::Disconnected) => {
                anyhow::bail!("market explorer worker unavailable; inspect custody")
            }
            Err(TryRecvError::Empty) => {}
        }
        if state.load_catalog
            && !state.busy
            && state.pane != Pane::Actions
            && state.pane != Pane::ActionForm
            && state.detail_context == DetailContext::Generic
        {
            state.load_catalog = false;
            if let Some(role) = state.roles.get(state.role) {
                jobs.send((state.revision, Job::Catalog(role.clone())))
                    .map_err(|_| anyhow::anyhow!("market explorer worker unavailable"))?;
                state.busy = true;
            }
        }
        terminal.draw(|frame| render(state, frame))?;
        if event::poll(super::TICK)? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('y') {
                    let size = terminal.size()?;
                    if size.width < MIN_WIDTH || size.height < MIN_HEIGHT {
                        state.invalidate();
                        continue;
                    }
                }
                match state.key(key) {
                    Control::Quit => return Ok(()),
                    Control::Job(job) => {
                        jobs.send((state.revision, job))
                            .map_err(|_| anyhow::anyhow!("market explorer worker unavailable"))?;
                        state.busy = true;
                        state.message =
                            "Working; navigation stays responsive. Quit waits for durable custody.";
                    }
                    Control::None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const ADMIN_G: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const RAW_ACCOUNT: &str = "GABQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABEQO";
    const RAW_CONTRACT: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

    fn press(state: &mut Explorer, code: KeyCode) -> Control {
        state.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn state() -> Explorer {
        let mut state = Explorer::new(vec!["pool".into(), "adapter".into()], false);
        state.functions = functions(&Safe::render_market_json(&json!([
            {"function_v0": {"name": "submit", "inputs": [{"name": "requests", "type_": {"vec": {"element_type": {"udt": {"name": "Request"}}}}}]}},
            {"function_v0": {"name": "get_config", "inputs": []}},
            {"function_v0": {"name": "probe_address", "inputs": [{"name": "address", "type_": "address"}, {"name": "memo", "type_": "string"}]}},
            {"function_v0": {"name": "text_address", "inputs": [{"name": "address", "type_": "string"}, {"name": "memo", "type_": "string"}]}}
        ]), TEXT_CAP).unwrap()).unwrap();
        state.select_function();
        state
    }

    fn form_manifest() -> MarketManifest {
        serde_json::from_value(json!({
            "network": {"label":"testnet","passphrase":"Test SDF Network ; September 2015"},
            "admin":"GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
            "rpc_host":"rpc.testnet.example",
            "oracle":{"runtime":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM","governance":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM","adapter":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM","sources":["CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"],"min_sources":1,"max_age_secs":3600,"max_clock_drift_secs":60,"decimals":7,"resolution":300,"base":"USD"},
            "tokens":{"usdc":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"},
            "pool":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
            "backstop":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
            "factory":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
            "comet":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
            "emitter":"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
            "reserves":{},"identities":{"sam":"GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX","frodo":"GAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHV4"},"deploy_ledger":1
        }))
        .unwrap()
    }

    fn address_choice(name: &str, value: &str) -> NamedAddress {
        NamedAddress {
            name: name.into(),
            address: value.parse().unwrap(),
        }
    }

    fn address_book() -> Vec<NamedAddress> {
        let address_shaped_name =
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF".to_lowercase();
        vec![
            address_choice(
                "sam",
                "GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX",
            ),
            address_choice(
                "sam.alias",
                "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53",
            ),
            address_choice(
                "samuel",
                "GAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHV4",
            ),
            address_choice(
                "pilot.alias",
                "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY",
            ),
            address_choice(
                &address_shaped_name,
                "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53",
            ),
        ]
    }

    fn generic_address_state() -> Explorer {
        let mut state = state();
        state.named_addresses = address_book();
        state.filter = "probe_address".into();
        state.select_function();
        state.pane = Pane::Arguments;
        state
    }

    fn guided_state() -> Explorer {
        let mut state = state();
        state.manifest = Some(form_manifest());
        state.named_addresses = address_book();
        state.action_category = "pool".into();
        state.action_name = "submit".into();
        let mut form = ActionForm::new(
            &[("memo".into(), json!("string"))],
            &[],
            &Safe::render_market_json(&json!([]), TEXT_CAP).unwrap(),
            state.manifest.as_ref().unwrap(),
            "pool",
            "submit",
        )
        .unwrap();
        form.set_choices(&state.named_addresses);
        state.action_form = Some(form);
        state
            .action_form
            .as_mut()
            .unwrap()
            .set(0, "retained draft")
            .unwrap();
        state.pane = Pane::ActionForm;
        state.detail_context = DetailContext::Guided;
        state.signed = true;
        state
    }

    fn guided_request_state() -> Explorer {
        let mut state = guided_state();
        let spec = Safe::render_market_json(&json!([]), TEXT_CAP).unwrap();
        let mut form = ActionForm::new(
            &[(
                "requests".into(),
                json!({"vec":{"element_type":{"udt":{"name":"Request"}}}}),
            )],
            &[],
            &spec,
            state.manifest.as_ref().unwrap(),
            "pool",
            "submit",
        )
        .unwrap();
        form.set_choices(&state.named_addresses);
        form.add(0).unwrap();
        form.set(2, RAW_ACCOUNT).unwrap();
        form.set(3, "1").unwrap();
        state.action_form = Some(form);
        state.argument = 1;
        state
    }
    fn eligible_guide_form(selected: crate::manifest::Tok) -> GuideForm {
        GuideForm {
            role: "proxy-runtime".into(),
            method: "refresh".into(),
            spec: Safe::render_market_json(&json!([]), TEXT_CAP).unwrap(),
            bound: vec![],
            inputs: vec![("memo".into(), json!("string"))],
            assets: vec![
                crate::manifest::Tok::Usdc,
                crate::manifest::Tok::Weth,
                crate::manifest::Tok::Xlm,
                crate::manifest::Tok::Stable,
            ],
            selected_asset: Some(selected),
        }
    }

    fn multi_source_manifest() -> MarketManifest {
        let mut value = serde_json::to_value(form_manifest()).unwrap();
        value["oracle"]["sources"] = json!([
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
            "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53",
            "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY"
        ]);
        serde_json::from_value(value).unwrap()
    }

    /// Loads an eligible oracle/refresh form; `with_source` also selects a
    /// source, so the source selector row becomes available.
    fn eligible_asset_state(with_source: bool) -> Explorer {
        let mut state = guided_state();
        state.action_category = "oracle".into();
        state.action_name = "refresh".into();
        if with_source {
            state.action_source = Some(0);
            state.manifest = Some(multi_source_manifest());
        }
        let revision = state.revision;
        state.apply(
            revision,
            Reply::GuideForm {
                form: eligible_guide_form(crate::manifest::Tok::Usdc),
                addresses: None,
            },
        );
        state
    }

    fn open_asset_chooser(state: &mut Explorer) {
        press(state, KeyCode::Up);
        press(state, KeyCode::Enter);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::Selector {
                target: Selector::Asset,
                selected: None
            })
        ));
    }

    fn type_text(state: &mut Explorer, text: &str) {
        for ch in text.chars() {
            press(state, KeyCode::Char(ch));
        }
    }

    #[test]
    fn guided_asset_options_are_visible_choosable_and_stale_forms_cannot_restore_them() {
        let mut state = guided_state();
        state.action_category = "oracle".into();
        state.action_name = "refresh".into();
        let initial_revision = state.revision;
        state.apply(
            initial_revision,
            Reply::GuideForm {
                form: eligible_guide_form(crate::manifest::Tok::Usdc),
                addresses: None,
            },
        );
        assert_eq!(
            state.eligible_assets,
            [
                crate::manifest::Tok::Usdc,
                crate::manifest::Tok::Weth,
                crate::manifest::Tok::Xlm,
                crate::manifest::Tok::Stable
            ]
        );
        assert_eq!(state.action_asset.as_deref(), Some("usdc"));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let screen = (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for visible in [
            "asset = usdc",
            "Asset: usdc",
            "Available assets",
            "usdc",
            "weth",
            "xlm",
            "stable",
        ] {
            assert!(screen.contains(visible), "missing {visible} from {screen}");
        }
        for removed in ["[ ] cycle eligible asset", ", . cycle source"] {
            assert!(!screen.contains(removed), "{removed} must be gone");
        }

        for (expected, tok) in [
            ("weth", crate::manifest::Tok::Weth),
            ("xlm", crate::manifest::Tok::Xlm),
            ("stable", crate::manifest::Tok::Stable),
            ("usdc", crate::manifest::Tok::Usdc),
        ] {
            open_asset_chooser(&mut state);
            type_text(&mut state, expected);
            match press(&mut state, KeyCode::Enter) {
                Control::Job(Job::GuideForm {
                    asset: Some(asset), ..
                }) => assert_eq!(asset, expected),
                _ => panic!("the asset chooser reloads the selected eligible asset"),
            }
            state.apply(
                state.revision,
                Reply::GuideForm {
                    form: eligible_guide_form(tok),
                    addresses: None,
                },
            );
            assert_eq!(state.action_asset.as_deref(), Some(expected));
        }
        state.prepared = Some(prepared());
        let stale_revision = state.revision;
        open_asset_chooser(&mut state);
        type_text(&mut state, "weth");
        match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::GuideForm {
                asset: Some(asset), ..
            }) => assert_eq!(asset, "weth"),
            _ => panic!("the asset chooser reloads the selected eligible asset"),
        }
        assert!(state.prepared.is_none());
        assert!(state.eligible_assets.is_empty());
        state.apply(
            stale_revision,
            Reply::GuideForm {
                form: eligible_guide_form(crate::manifest::Tok::Usdc),
                addresses: None,
            },
        );
        assert!(state.eligible_assets.is_empty());
        assert_eq!(state.action_asset.as_deref(), Some("weth"));
        assert!(state.prepared.is_none());
    }

    #[test]
    fn guided_form_focus_starts_on_the_first_typed_field_and_reaches_selectors() {
        let mut state = eligible_asset_state(true);
        assert_eq!(
            state.selectors().collect::<Vec<_>>(),
            [Selector::Asset, Selector::Source]
        );
        assert_eq!(state.argument, 2, "focus starts on the first typed field");

        press(&mut state, KeyCode::Up);
        assert_eq!(state.argument, 1, "Up reaches the source row");
        press(&mut state, KeyCode::Up);
        assert_eq!(state.argument, 0, "Up reaches the asset row");
        press(&mut state, KeyCode::Up);
        assert_eq!(state.argument, 0, "the selector rows top out the list");

        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.argument, 2);
        press(&mut state, KeyCode::Enter);
        assert!(matches!(state.input.as_ref(), Some(Input::Form(_))));

        // Row-only keys are inert on the selector rows.
        let mut state = eligible_asset_state(true);
        state.argument = 0;
        press(&mut state, KeyCode::Char('+'));
        press(&mut state, KeyCode::Char('-'));
        press(&mut state, KeyCode::Char('n'));
        assert!(state.input.is_none());
        assert!(state.message.contains("Select an editable address field"));
        assert_eq!(state.argument, 0);
    }

    #[test]
    fn asset_chooser_filter_and_enter_queue_exactly_one_guide_reload() {
        let mut state = eligible_asset_state(false);
        open_asset_chooser(&mut state);
        type_text(&mut state, "wet");
        match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::GuideForm {
                asset: Some(asset), ..
            }) => assert_eq!(asset, "weth"),
            _ => panic!("a unique filter commits with one guide reload"),
        }
        assert!(state.input.is_none());
        assert_eq!(state.action_asset.as_deref(), Some("weth"));
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
    }

    #[test]
    fn asset_chooser_recommitting_the_current_value_queues_no_reload() {
        let mut state = eligible_asset_state(false);
        open_asset_chooser(&mut state);
        type_text(&mut state, "usdc");
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(state.input.is_none());
        assert_eq!(state.action_asset.as_deref(), Some("usdc"));
        assert!(state.message.contains("Asset unchanged"));

        // A blank query with no selection also keeps the current value.
        press(&mut state, KeyCode::Enter);
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(state.input.is_none());
        assert_eq!(state.action_asset.as_deref(), Some("usdc"));
    }

    #[test]
    fn asset_chooser_keeps_the_editor_open_for_ambiguous_and_missing_matches() {
        let mut state = eligible_asset_state(false);
        open_asset_chooser(&mut state);
        press(&mut state, KeyCode::Char('s'));
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(matches!(state.input.as_ref(), Some(Input::Selector { .. })));
        assert!(state.message.contains("Several assets match"));
        assert_eq!(state.action_asset.as_deref(), Some("usdc"));

        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::Selector {
                target: Selector::Asset,
                selected: Some(3)
            })
        ));
        match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::GuideForm {
                asset: Some(asset), ..
            }) => assert_eq!(asset, "stable"),
            _ => panic!("an explicit selection commits"),
        }

        let mut state = eligible_asset_state(false);
        open_asset_chooser(&mut state);
        type_text(&mut state, "zzz");
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(matches!(state.input.as_ref(), Some(Input::Selector { .. })));
        assert!(state.message.contains("No asset matches"));
        assert_eq!(state.edit, "zzz");
    }

    #[test]
    fn asset_chooser_escape_and_busy_commit_keep_the_current_asset() {
        let mut state = eligible_asset_state(false);
        open_asset_chooser(&mut state);
        type_text(&mut state, "weth");
        assert!(matches!(press(&mut state, KeyCode::Esc), Control::None));
        assert!(state.input.is_none());
        assert_eq!(state.action_asset.as_deref(), Some("usdc"));
        assert!(state.message.contains("Asset unchanged"));

        press(&mut state, KeyCode::Enter);
        assert!(matches!(state.input.as_ref(), Some(Input::Selector { .. })));
        type_text(&mut state, "xlm");
        state.busy = true;
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(state.input.is_none());
        assert_eq!(state.action_asset.as_deref(), Some("usdc"));
        assert!(state.message.contains("worker request is in flight"));
    }

    #[test]
    fn asset_chooser_types_action_keys_as_literal_query_text() {
        let mut state = eligible_asset_state(false);
        open_asset_chooser(&mut state);
        for ch in ['r', 'p', 'y', 's'] {
            press(&mut state, KeyCode::Char(ch));
        }
        assert_eq!(state.edit, "rpys");
        assert!(matches!(state.input.as_ref(), Some(Input::Selector { .. })));
        assert!(!state.status.open);
    }

    #[test]
    fn source_chooser_commits_the_selected_source_index() {
        let mut state = eligible_asset_state(true);
        press(&mut state, KeyCode::Up);
        assert_eq!(state.argument, 1, "the source row sits below the asset row");
        press(&mut state, KeyCode::Enter);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::Selector {
                target: Selector::Source,
                selected: None
            })
        ));
        type_text(&mut state, "source.2");
        match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::GuideForm {
                source: Some(2), ..
            }) => {}
            _ => panic!("the source chooser reloads with the chosen source index"),
        }
        assert_eq!(state.action_source, Some(2));

        state.apply(
            state.revision,
            Reply::GuideForm {
                form: eligible_guide_form(crate::manifest::Tok::Usdc),
                addresses: None,
            },
        );
        press(&mut state, KeyCode::Up);
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.edit, "source.0");
        match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::GuideForm {
                source: Some(0), ..
            }) => {}
            _ => panic!("Tab completes the first match and Enter commits it"),
        }

        state.apply(
            state.revision,
            Reply::GuideForm {
                form: eligible_guide_form(crate::manifest::Tok::Usdc),
                addresses: None,
            },
        );
        press(&mut state, KeyCode::Up);
        press(&mut state, KeyCode::Enter);
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert_eq!(state.action_source, Some(0));
        assert!(state.message.contains("Source unchanged"));
    }

    #[test]
    fn search_and_argument_editor_produce_native_query_arguments() {
        let mut state = state();
        press(&mut state, KeyCode::Char('/'));
        for ch in "submit".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Enter);
        let value = r#"[{"amount":"42","request_type":0}]"#;
        for ch in value.chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        press(&mut state, KeyCode::Enter);
        match press(&mut state, KeyCode::Char('r')) {
            Control::Job(Job::Call {
                role,
                method,
                args,
                prepare,
            }) => {
                assert_eq!(role, "pool");
                assert_eq!(method, "submit");
                assert_eq!(args, vec![("requests".into(), value.into())]);
                assert!(!prepare);
            }
            _ => panic!("edited call must reach read-only query"),
        }
    }

    #[test]
    fn raw_address_literal_precedes_an_address_shaped_alias_in_both_editors() {
        let mut generic = state();
        generic.named_addresses = address_book();
        generic.filter = "probe_address".into();
        generic.selected = 0;
        generic.select_function();
        generic.pane = Pane::Arguments;
        press(&mut generic, KeyCode::Enter);
        for ch in ADMIN_G.chars() {
            press(&mut generic, KeyCode::Char(ch));
        }
        press(&mut generic, KeyCode::Enter);
        assert_eq!(generic.args[0].1, ADMIN_G);

        let mut guided = guided_state();
        guided.action_name = "propose-admin".into();
        let mut form = ActionForm::new(
            &[("new_admin".into(), json!("address"))],
            &[],
            &Safe::render_market_json(&json!([]), TEXT_CAP).unwrap(),
            guided.manifest.as_ref().unwrap(),
            "pool",
            "propose-admin",
        )
        .unwrap();
        form.set_choices(&guided.named_addresses);
        guided.action_form = Some(form);
        guided.argument = 0;
        press(&mut guided, KeyCode::Enter);
        for ch in ADMIN_G.chars() {
            press(&mut guided, KeyCode::Char(ch));
        }
        press(&mut guided, KeyCode::Enter);
        assert_eq!(
            guided
                .action_form
                .as_ref()
                .unwrap()
                .edit_value(0)
                .as_deref(),
            Some(ADMIN_G),
        );
    }

    #[test]
    fn address_completion_prefers_exact_sam_and_requires_a_choice_for_ambiguous_prefixes() {
        let mut current = generic_address_state();
        current.args[0].1 = "GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX".into();
        press(&mut current, KeyCode::Enter);
        assert_eq!(current.edit, "");
        assert!(matches!(
            current.input.as_ref(),
            Some(Input::Address {
                selected: Some(_),
                explicit: false,
                ..
            })
        ));
        press(&mut current, KeyCode::Enter);
        assert_eq!(
            current.args[0].1,
            "GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX"
        );

        let mut typed = generic_address_state();
        press(&mut typed, KeyCode::Enter);
        for ch in "sa".chars() {
            press(&mut typed, KeyCode::Char(ch));
        }
        press(&mut typed, KeyCode::Enter);
        assert_eq!(typed.args[0].1, "");
        assert!(matches!(typed.input.as_ref(), Some(Input::Address { .. })));
        assert!(typed.message.contains("Several names match"));

        press(&mut typed, KeyCode::Down);
        let selected = match typed.input.as_ref() {
            Some(Input::Address {
                selected: Some(index),
                explicit: true,
                ..
            }) => typed.named_addresses[*index].address.as_str().to_owned(),
            _ => panic!("Down visibly selects one ambiguous match"),
        };
        press(&mut typed, KeyCode::Enter);
        assert_eq!(typed.args[0].1, selected);

        press(&mut typed, KeyCode::Enter);
        for ch in "SAM".chars() {
            press(&mut typed, KeyCode::Char(ch));
        }
        press(&mut typed, KeyCode::Enter);
        assert_eq!(
            typed.args[0].1,
            "GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX"
        );

        press(&mut typed, KeyCode::Enter);
        for ch in "pilot".chars() {
            press(&mut typed, KeyCode::Char(ch));
        }
        press(&mut typed, KeyCode::Enter);
        assert_eq!(
            typed.args[0].1,
            "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY"
        );
    }

    #[test]
    fn explicit_choice_overrides_exact_name_on_enter_and_tab() {
        let expected = "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53";
        let mut enter = generic_address_state();
        press(&mut enter, KeyCode::Enter);
        for ch in "sam".chars() {
            press(&mut enter, KeyCode::Char(ch));
        }
        press(&mut enter, KeyCode::Down);
        press(&mut enter, KeyCode::Down);
        press(&mut enter, KeyCode::Enter);
        assert_eq!(enter.args[0].1, expected);

        let mut tab = generic_address_state();
        press(&mut tab, KeyCode::Enter);
        for ch in "sam".chars() {
            press(&mut tab, KeyCode::Char(ch));
        }
        press(&mut tab, KeyCode::Down);
        press(&mut tab, KeyCode::Down);
        press(&mut tab, KeyCode::Tab);
        assert_eq!(tab.edit, "sam.alias");
        press(&mut tab, KeyCode::Enter);
        assert_eq!(tab.args[0].1, expected);
        let mut guided = guided_state();
        let mut form = ActionForm::new(
            &[("new_admin".into(), json!("address"))],
            &[],
            &Safe::render_market_json(&json!([]), TEXT_CAP).unwrap(),
            guided.manifest.as_ref().unwrap(),
            "pool",
            "propose-admin",
        )
        .unwrap();
        form.set_choices(&guided.named_addresses);
        guided.action_form = Some(form);
        guided.action_name = "propose-admin".into();
        press(&mut guided, KeyCode::Enter);
        for ch in "sam".chars() {
            press(&mut guided, KeyCode::Char(ch));
        }
        press(&mut guided, KeyCode::Down);
        press(&mut guided, KeyCode::Down);
        press(&mut guided, KeyCode::Enter);
        assert_eq!(
            guided
                .action_form
                .as_ref()
                .unwrap()
                .edit_value(0)
                .as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn raw_account_and_contract_literals_are_accepted_and_rpy_stay_text() {
        let mut generic = generic_address_state();
        for raw in [RAW_ACCOUNT, RAW_CONTRACT] {
            let previous = generic.args[0].1.clone();
            press(&mut generic, KeyCode::Enter);
            for ch in "rpy".chars() {
                assert!(matches!(
                    press(&mut generic, KeyCode::Char(ch)),
                    Control::None
                ));
            }
            press(&mut generic, KeyCode::Enter);
            assert_eq!(generic.args[0].1, previous);
            for _ in 0..3 {
                press(&mut generic, KeyCode::Backspace);
            }
            for ch in raw.chars() {
                assert!(matches!(
                    press(&mut generic, KeyCode::Char(ch)),
                    Control::None
                ));
            }
            press(&mut generic, KeyCode::Enter);
            assert_eq!(generic.args[0].1, raw);
        }

        let mut text = state();
        text.filter = "text_address".into();
        text.selected = 0;
        text.select_function();
        text.pane = Pane::Arguments;
        assert_eq!(text.argument_address, vec![false, false]);
        press(&mut text, KeyCode::Enter);
        assert!(matches!(text.input.as_ref(), Some(Input::Argument(0))));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut text, frame)).unwrap();
        let editing = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(editing.contains("r/p/y type text") && editing.contains("shortcuts inactive"));
        assert!(!editing.contains("Enter edits value"));
        assert!(!editing.contains("a Actions"));
        assert!(!editing.contains("p prepare"));
        assert!(!editing.contains("y confirms"));
        for ch in "rpy".chars() {
            press(&mut text, KeyCode::Char(ch));
        }
        press(&mut text, KeyCode::Enter);
        assert_eq!(text.args[0].1, "rpy");
    }

    #[test]
    fn guided_request_type_editor_resolves_labels_numbers_and_recovers_drafts() {
        let mut state = guided_request_state();
        state.argument = 1;
        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;
        let stale_entry = state.revision;
        press(&mut state, KeyCode::Enter);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::RequestType {
                index: 1,
                selected: None,
            })
        ));
        assert!(state.prepared.is_none());
        state.apply(stale_entry, Reply::Prepared(prepared()));
        assert!(state.prepared.is_none());

        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;
        let stale_edit = state.revision;
        press(&mut state, KeyCode::Char('B'));
        assert!(state.prepared.is_none());
        state.apply(stale_edit, Reply::Prepared(prepared()));
        assert!(state.prepared.is_none());
        for ch in "orrow".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        assert_eq!(state.edit, "Borrow");
        press(&mut state, KeyCode::Enter);
        assert!(state.input.is_none());
        assert_eq!(
            serde_json::from_str::<Value>(
                &state.action_form.as_ref().unwrap().explicit().unwrap()[0].1
            )
            .unwrap()[0]["request_type"],
            4
        );

        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Char('5'));
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            serde_json::from_str::<Value>(
                &state.action_form.as_ref().unwrap().explicit().unwrap()[0].1
            )
            .unwrap()[0]["request_type"],
            5
        );
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(1).as_deref(),
            Some("5")
        );

        press(&mut state, KeyCode::Enter);
        for ch in "Supply".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        press(&mut state, KeyCode::Enter);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::RequestType { .. })
        ));
        assert!(state.message.contains("Several request types match"));
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(1).as_deref(),
            Some("5")
        );
        press(&mut state, KeyCode::Tab);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::RequestType {
                selected: Some(0),
                ..
            })
        ));
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            serde_json::from_str::<Value>(
                &state.action_form.as_ref().unwrap().explicit().unwrap()[0].1
            )
            .unwrap()[0]["request_type"],
            0
        );

        press(&mut state, KeyCode::Enter);
        for ch in "10".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        press(&mut state, KeyCode::Enter);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::RequestType { .. })
        ));
        assert!(state.message.contains("0..=9"));
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(1).as_deref(),
            Some("0")
        );
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(1).as_deref(),
            Some("1")
        );

        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;
        let stale_cancel = state.revision;
        press(&mut state, KeyCode::Enter);
        assert!(state.prepared.is_none());
        state.apply(stale_cancel, Reply::Prepared(prepared()));
        assert!(state.prepared.is_none());
        for ch in "rpy".chars() {
            assert!(matches!(
                press(&mut state, KeyCode::Char(ch)),
                Control::None
            ));
        }
        assert_eq!(state.edit, "rpy");
        press(&mut state, KeyCode::Esc);
        assert!(state.input.is_none());
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(1).as_deref(),
            Some("1")
        );
    }

    #[test]
    fn request_type_selection_revokes_preparation_and_renders_at_supported_sizes() {
        let mut state = guided_request_state();
        state.argument = 1;
        press(&mut state, KeyCode::Enter);
        for ch in "Borrow".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;
        let stale_selection = state.revision;
        press(&mut state, KeyCode::Down);
        assert!(state.prepared.is_none());
        assert_ne!(state.revision, stale_selection);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::RequestType {
                selected: Some(4),
                ..
            })
        ));
        state.apply(stale_selection, Reply::Prepared(prepared()));
        assert!(state.prepared.is_none());

        for (width, height) in [(60, 16), (110, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut state, frame)).unwrap();
            let screen = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains("REQUEST TYPE AUTOCOMPLETE"));
            assert!(screen.contains("Request type query: Borrow"));
            assert!(screen.contains("> 4 Borrow"));
        }
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            serde_json::from_str::<Value>(
                &state.action_form.as_ref().unwrap().explicit().unwrap()[0].1
            )
            .unwrap()[0]["request_type"],
            4
        );

        press(&mut state, KeyCode::Enter);
        for _ in 0..10 {
            press(&mut state, KeyCode::Down);
        }
        for (width, height) in [(60, 16), (110, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut state, frame)).unwrap();
            let screen = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains("> 9 Delete liquidation auction"));
        }
        let stale_cancel = state.revision;
        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;
        press(&mut state, KeyCode::Esc);
        assert!(state.prepared.is_none());
        state.apply(stale_cancel, Reply::Prepared(prepared()));
        assert!(state.prepared.is_none());
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(1).as_deref(),
            Some("4")
        );
    }

    #[test]
    fn guided_request_flash_loan_and_address_vector_leaves_use_the_shared_editor() {
        let mut state = guided_state();
        let spec = Safe::render_market_json(
            &json!([
                {"udt_struct_v0":{"name":"Request","fields":[
                    {"name":"request_type","type_":"u32"},
                    {"name":"address","type_":"address"},
                    {"name":"amount","type_":"i128"}]}},
                {"udt_struct_v0":{"name":"FlashLoan","fields":[
                    {"name":"contract","type_":"address"},
                    {"name":"asset","type_":"address"},
                    {"name":"amount","type_":"i128"}]}}
            ]),
            TEXT_CAP,
        )
        .unwrap();
        let mut form = ActionForm::new(
            &[
                (
                    "requests".into(),
                    json!({"vec":{"element_type":{"udt":{"name":"Request"}}}}),
                ),
                ("flash_loan".into(), json!({"udt":{"name":"FlashLoan"}})),
                ("actors".into(), json!({"vec":{"element_type":"address"}})),
            ],
            &[],
            &spec,
            state.manifest.as_ref().unwrap(),
            "auction",
            "fill-liquidation",
        )
        .unwrap();
        form.set_choices(&state.named_addresses);
        state.action_form = Some(form);
        state.pane = Pane::ActionForm;
        for (header, child) in [(0, 2), (4, 5), (8, 9)] {
            for key in [KeyCode::Enter, KeyCode::Char('e')] {
                state.argument = header;
                state.prepared = Some(prepared());
                let stale_revision = state.revision;
                press(&mut state, key);
                assert_eq!(state.argument, child);
                assert!(
                    state.input.is_none(),
                    "enter the collection, not a JSON text editor"
                );
                assert!(state.prepared.is_none());
                state.apply(stale_revision, Reply::Prepared(prepared()));
                assert!(state.prepared.is_none());
            }
        }
        let form = state.action_form.as_mut().unwrap();
        for index in [2, 5, 6, 9] {
            assert!(form.is_address(index), "address leaf at cell {index}");
        }
        assert!(!form.is_address(1), "fixed request type stays locked");
        assert!(!form.is_address(4), "record aggregate is not an address");
        assert!(!form.is_address(7), "amount stays numeric");
        assert!(!form.is_address(8), "vector aggregate is not an address");
        assert!(form.set(1, "5").is_err());
        state.argument = 1;
        press(&mut state, KeyCode::Enter);
        assert!(
            state.input.is_none(),
            "fixed request types must not open an editor"
        );

        for index in [2, 5, 6, 9] {
            state.argument = index;
            press(&mut state, KeyCode::Enter);
            assert!(matches!(
                state.input.as_ref(),
                Some(Input::Address {
                    target: AddressTarget::Guided(cell),
                    ..
                }) if *cell == index
            ));
            for ch in "pilot".chars() {
                press(&mut state, KeyCode::Char(ch));
            }
            press(&mut state, KeyCode::Enter);
            assert_eq!(
                state
                    .action_form
                    .as_ref()
                    .unwrap()
                    .edit_value(index)
                    .as_deref(),
                Some("CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY")
            );
        }
        let form = state.action_form.as_mut().unwrap();
        form.set(3, "1").unwrap();
        form.set(7, "1").unwrap();
        let args = form.explicit().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&args[0].1).unwrap()[0]["address"],
            "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY"
        );
        let loan = serde_json::from_str::<Value>(&args[1].1).unwrap();
        assert_eq!(loan["contract"], loan["asset"]);
        assert_eq!(
            serde_json::from_str::<Value>(&args[2].1).unwrap(),
            json!(["CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY"])
        );
    }

    #[test]
    fn invalid_name_keeps_address_draft_and_stale_preparation_cannot_restore_authority() {
        let mut state = generic_address_state();
        state.args[0].1 = RAW_ACCOUNT.into();
        state.prepared = Some(prepared());
        let stale_revision = state.revision;
        press(&mut state, KeyCode::Enter);
        assert!(state.prepared.is_none());
        assert_ne!(state.revision, stale_revision);
        for ch in "unknown.name".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.args[0].1, RAW_ACCOUNT);
        assert!(matches!(state.input.as_ref(), Some(Input::Address { .. })));
        assert!(state.message.contains("No saved name matches"));
        assert!(!state.message.contains("unknown.name"));

        state.apply(stale_revision, Reply::Prepared(prepared()));
        assert!(state.prepared.is_none());
        assert!(matches!(state.input.as_ref(), Some(Input::Address { .. })));
        assert_eq!(state.args[0].1, RAW_ACCOUNT);
    }

    #[test]
    fn address_save_failure_keeps_field_and_name_draft_then_success_refreshes_book() {
        let mut state = generic_address_state();
        state.args[0].1 = RAW_ACCOUNT.into();
        state.prepared = Some(prepared());
        press(&mut state, KeyCode::Char('n'));
        assert!(state.prepared.is_none());
        state.scroll = (5, 4);
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(matches!(state.input.as_ref(), Some(Input::SaveAddress(_))));
        assert_eq!(state.args[0].1, RAW_ACCOUNT);
        assert_eq!(state.scroll, (0, 0));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Enter a name"));
        assert!(screen.contains("r/p/y type text"));
        for ch in "duplicate".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        let (name, address) = match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::SaveNamedAddress { name, address }) => (name, address),
            _ => panic!("save name starts the existing worker path"),
        };
        let revision = state.revision;
        state.apply(
            revision,
            Reply::AddressSaveFailed {
                name: name.clone(),
                address: address.clone(),
            },
        );
        assert_eq!(state.args[0].1, RAW_ACCOUNT);
        assert_eq!(state.edit, "duplicate");
        assert!(matches!(state.input.as_ref(), Some(Input::SaveAddress(_))));
        assert!(state.message.contains("address field draft was kept"));

        for _ in 0..state.edit.len() {
            press(&mut state, KeyCode::Backspace);
        }
        for ch in "saved.addr".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        let (saved_name, saved_address) = match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::SaveNamedAddress { name, address }) => (name, address),
            _ => panic!("retry starts a new worker save"),
        };
        assert_eq!(saved_address.as_str(), RAW_ACCOUNT);
        let mut book = address_book();
        book.push(address_choice(&saved_name, saved_address.as_str()));
        state.apply(state.revision, Reply::AddressSaved(book));
        assert_eq!(state.args[0].1, RAW_ACCOUNT);
        assert!(state
            .named_addresses
            .iter()
            .any(|choice| choice.name == "saved.addr"));
        assert!(state.input.is_none());
        assert_eq!(
            state.message,
            "Named address saved; available for autocomplete."
        );
    }

    #[test]
    fn save_key_revokes_real_preview_and_pending_prepare_replies() {
        let mut preview = state();
        let revision = preview.revision;
        preview.apply(revision, Reply::Prepared(prepared()));
        assert_eq!(preview.phase, Phase::Awaiting);
        assert!(preview.prepared.is_some());
        let stale_reply_revision = preview.revision;
        let preview_detail = preview.detail.clone();
        press(&mut preview, KeyCode::Char('n'));
        assert!(preview.prepared.is_none());
        assert_eq!(preview.phase, Phase::ReadOnly);
        preview.apply(stale_reply_revision, Reply::Prepared(prepared()));
        assert!(preview.prepared.is_none());
        assert_eq!(preview.detail, preview_detail);
        assert_eq!(preview.phase, Phase::ReadOnly);

        let mut pending = generic_address_state();
        pending.signed = true;
        pending.args[0].1 = RAW_ACCOUNT.into();
        pending.args[1].1 = "note".into();
        assert!(matches!(
            press(&mut pending, KeyCode::Char('p')),
            Control::Job(Job::Call { prepare: true, .. })
        ));
        let stale_prepare_revision = pending.revision;
        pending.busy = true;
        press(&mut pending, KeyCode::Char('n'));
        assert!(pending.prepared.is_none());
        assert!(pending.revision != stale_prepare_revision);
        pending.apply(stale_prepare_revision, Reply::Prepared(prepared()));
        assert!(pending.prepared.is_none());
        assert_eq!(pending.args[0].1, RAW_ACCOUNT);
    }

    #[test]
    fn selected_full_public_id_is_reachable_at_minimum_and_wide_sizes() {
        let mut state = generic_address_state();
        state.args[0].1 = RAW_ACCOUNT.into();
        press(&mut state, KeyCode::Enter);
        for ch in "pilot".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        press(&mut state, KeyCode::Tab);
        assert!(matches!(
            state.input.as_ref(),
            Some(Input::Address {
                selected: Some(_),
                explicit: true,
                ..
            })
        ));
        let expected = "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY";
        for (width, height) in [(60, 16), (110, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut state, frame)).unwrap();
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(8),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(Rect::new(0, 0, width, height));
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(rows[1]);
            let buffer = terminal.backend().buffer();
            let mut displayed = String::new();
            for y in columns[1].y + 1..columns[1].bottom() - 1 {
                for x in columns[1].x + 1..columns[1].right() - 1 {
                    displayed.push_str(buffer[(x, y)].symbol());
                }
            }
            let compact = displayed
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            assert!(
                compact.contains(expected),
                "selected public ID clipped at {width}x{height}"
            );
            assert!(displayed.contains("pilot.alias"));
        }
    }

    #[test]
    fn guided_result_prepare_reuses_the_same_recipe_and_generic_detail_stays_generic() {
        let mut guided = guided_state();
        match press(&mut guided, KeyCode::Char('r')) {
            Control::Job(Job::Guided {
                category,
                action,
                args,
                prepare: false,
                ..
            }) => {
                assert_eq!((category.as_str(), action.as_str()), ("pool", "submit"));
                assert_eq!(args, vec![("memo".into(), "retained draft".into())]);
            }
            _ => panic!("guided simulation must use the selected recipe"),
        }
        guided.apply(
            guided.revision,
            Reply::Detail("Operation: pool/submit\nStage: read-only simulation".into()),
        );
        assert_eq!(guided.pane, Pane::Detail);
        assert_eq!(guided.detail_context, DetailContext::Guided);
        match press(&mut guided, KeyCode::Char('p')) {
            Control::Job(Job::Guided {
                category,
                action,
                args,
                prepare: true,
                ..
            }) => {
                assert_eq!((category.as_str(), action.as_str()), ("pool", "submit"));
                assert_eq!(args, vec![("memo".into(), "retained draft".into())]);
            }
            _ => panic!("guided result prepare must return to the exact recipe"),
        }
        match press(&mut guided, KeyCode::Char('r')) {
            Control::Job(Job::Guided {
                category,
                action,
                args,
                prepare: false,
                ..
            }) => {
                assert_eq!((category.as_str(), action.as_str()), ("pool", "submit"));
                assert_eq!(args, vec![("memo".into(), "retained draft".into())]);
            }
            _ => panic!("guided result simulation must keep its operation context"),
        }

        let mut generic = state();
        generic.signed = true;
        generic.pane = Pane::Detail;
        generic.detail_context = DetailContext::Generic;
        match press(&mut generic, KeyCode::Char('p')) {
            Control::Job(Job::Call {
                method,
                prepare: true,
                ..
            }) => assert_eq!(method, "get_config"),
            _ => panic!("generic ABI detail must keep generic call routing"),
        }
    }

    #[test]
    fn rejected_guided_prepare_returns_to_editable_draft_without_reloading() {
        let mut state = guided_state();
        assert!(matches!(
            press(&mut state, KeyCode::Char('p')),
            Control::Job(Job::Guided { prepare: true, .. })
        ));
        let revision = state.revision;
        state.apply(
            revision,
            Reply::Failed {
                stage: "prepare / simulation",
                operation: "pool/submit".into(),
                diagnostic: Safe::literal("rejected"),
            },
        );
        assert_eq!(state.pane, Pane::Detail);
        assert_eq!(state.detail_context, DetailContext::Guided);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.pane, Pane::ActionForm);
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
            Some("retained draft")
        );
        press(&mut state, KeyCode::Enter);
        state.edit = "edited draft".into();
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            state.action_form.as_ref().unwrap().explicit().unwrap(),
            vec![("memo".into(), "edited draft".into())]
        );
    }

    #[test]
    fn stale_initial_catalog_defers_reload_and_preserves_guided_draft() {
        let mut startup = Explorer::new(vec!["pool".into()], false);
        assert!(startup.functions.is_empty());
        let startup_revision = startup.revision;
        startup.load_catalog = false;
        startup.busy = true;
        press(&mut startup, KeyCode::Char('a'));
        assert_eq!(startup.pane, Pane::Actions);
        startup.apply(
            startup_revision,
            Reply::Catalog {
                functions: vec![Function {
                    name: "stale_catalog".into(),
                    inputs: vec![],
                    signature: String::new(),
                }],
                addresses: Some(vec![]),
            },
        );
        assert_eq!(startup.pane, Pane::Actions);
        assert!(
            startup.functions.is_empty(),
            "stale Catalog must not replace functions"
        );
        assert!(
            startup.load_catalog,
            "empty stale Catalog must defer a reload"
        );
        assert!(!startup.busy);
        let catalog_dispatch_allowed = |state: &Explorer| {
            state.load_catalog
                && !state.busy
                && state.pane != Pane::Actions
                && state.pane != Pane::ActionForm
                && state.detail_context == DetailContext::Generic
        };
        assert!(
            !catalog_dispatch_allowed(&startup),
            "Actions must defer dispatch"
        );
        press(&mut startup, KeyCode::Tab);
        assert_eq!(startup.pane, Pane::Functions);
        assert!(
            startup.load_catalog,
            "deferred reload must survive returning to Functions"
        );
        assert!(
            catalog_dispatch_allowed(&startup),
            "Functions permits dispatch"
        );

        let mut state = guided_state();
        let original_detail = state.detail.clone();
        let stale_revision = state.revision;
        state.busy = true;
        press(&mut state, KeyCode::Char('a'));
        state.apply(
            stale_revision,
            Reply::Catalog {
                functions: vec![],
                addresses: Some(vec![]),
            },
        );
        assert_eq!(state.pane, Pane::Actions);
        assert_eq!(state.detail, original_detail);
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
            Some("retained draft")
        );

        let mut state = guided_state();
        state.busy = true;
        state.pane = Pane::Actions;
        state.action_asset = Some("usdc".into());
        state.action_source = Some(0);
        let form = state.action_form.as_ref().unwrap().edit_value(0);
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(state.select_action().is_none());
        state.pane = Pane::ActionForm;
        state.eligible_assets = vec![crate::manifest::Tok::Usdc];
        state.argument = 0;
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(state.input.is_none());
        assert!(state.message.contains("worker request is in flight"));
        for target in [Selector::Asset, Selector::Source] {
            state.input = Some(Input::Selector {
                target,
                selected: Some(0),
            });
            assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        }
        assert_eq!(state.action_form.as_ref().unwrap().edit_value(0), form);
        assert_eq!(state.action_asset.as_deref(), Some("usdc"));
        assert_eq!(state.action_source, Some(0));
    }

    #[test]
    fn unsigned_and_cancelled_states_never_honor_confirmation_keys() {
        let mut state = state();
        assert!(matches!(
            press(&mut state, KeyCode::Char('p')),
            Control::None
        ));
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
        state.pane = Pane::ActionForm;
        assert!(matches!(
            press(&mut state, KeyCode::Char('p')),
            Control::None
        ));
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
        state.signed = true;
        let revision = state.revision;
        state.busy = true;
        press(&mut state, KeyCode::Esc);
        state.apply(revision, Reply::Detail("stale result".into()));
        assert!(!state.detail.contains("stale result"));
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
    }

    #[test]
    fn argument_edits_are_byte_bounded_and_escape_cancels_changes() {
        let mut state = state();
        state.selected = 1;
        state.select_function();
        state.pane = Pane::Arguments;
        press(&mut state, KeyCode::Enter);
        state.edit = "x".repeat(MAX_ARG_VALUE_LEN - 1);
        press(&mut state, KeyCode::Char('界'));
        assert_eq!(state.edit.len(), MAX_ARG_VALUE_LEN - 1);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.args[0].1, "");
        assert!(matches!(
            press(&mut state, KeyCode::Char('r')),
            Control::None
        ));
    }

    #[test]
    fn dynamic_text_is_escaped_before_terminal_rendering() {
        let rendered = label("line one\n\u{1b}[31mhidden");
        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.contains("\\u001b"));
    }

    #[test]
    fn narrow_terminal_clips_values_without_hiding_controls_and_detail_scrolls() {
        let mut state = state();
        state.detail = format!("{}end-marker\nsecond row", "x".repeat(160));
        state.pane = Pane::Detail;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let dump = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let screen = dump(&terminal);
        assert!(!screen.contains("end-marker"));
        assert!(screen.contains("q quit"));
        press(&mut state, KeyCode::Char('t'));
        press(&mut state, KeyCode::Char('q'));
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let editing = dump(&terminal);
        assert!(editing.contains("Receipt hash: q"));
        assert!(editing.contains("r/p/y type text") && editing.contains("shortcuts inactive"));
        assert!(editing.contains("Enter completes"));
        assert!(!editing.contains("q quit"));
        assert!(!editing.contains("arrows scroll"));
        assert!(!editing.contains("Home resets"));
        assert!(!editing.contains("Tab returns"));

        let mut undersized = Terminal::new(TestBackend::new(59, 15)).unwrap();
        undersized.draw(|frame| render(&mut state, frame)).unwrap();
        let warning = undersized
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(warning.contains("Ctrl-C quits"));
        assert!(!warning.contains("q quits"));

        press(&mut state, KeyCode::Esc);
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        assert!(dump(&terminal).contains("q quit"));
        for _ in 0..16 {
            press(&mut state, KeyCode::Right);
        }
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        for _ in 0..100 {
            press(&mut state, KeyCode::Right);
        }
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        assert_eq!(state.scroll.1, state.scroll_max.1);
        assert!(dump(&terminal).contains("end-marker"));
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Home);
        assert_eq!(state.scroll, (0, 0));
    }

    #[test]
    fn guided_field_contents_remain_reachable_at_minimum_and_wide_terminal_sizes() {
        let mut state = guided_state();
        state.action_form = Some(
            ActionForm::new(
                &[
                    ("first".into(), json!("string")),
                    ("middle".into(), json!("string")),
                    ("final_field".into(), json!("string")),
                ],
                &[],
                &Safe::render_market_json(&json!([]), TEXT_CAP).unwrap(),
                state.manifest.as_ref().unwrap(),
                "pool",
                "submit",
            )
            .unwrap(),
        );
        let value = format!("START-{}-ENDMARKER", "x".repeat(240));
        state.action_form.as_mut().unwrap().set(2, &value).unwrap();
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        for (width, height) in [(60, 16), (110, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let dump = |terminal: &Terminal<TestBackend>| {
                let buffer = terminal.backend().buffer();
                (0..height)
                    .map(|y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let value_visible = |terminal: &Terminal<TestBackend>| {
                let columns = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                    .split(Rect::new(0, 1, width, height - 4));
                let column = columns[1];
                let buffer = terminal.backend().buffer();
                (column.y + 1..column.bottom() - 1)
                    .map(|y| {
                        (column.x + 1..column.right() - 1)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                            .trim_end()
                            .to_owned()
                    })
                    .collect::<String>()
                    .contains("ENDMARKER")
            };
            terminal.draw(|frame| render(&mut state, frame)).unwrap();
            let initial = dump(&terminal);
            assert!(initial.contains("final_field"));
            assert!(initial.contains("START-"));
            if width == 110 {
                assert_eq!(
                    state.scroll.0, 0,
                    "resize clamps offsets to rendered content"
                );
            }
            let mut saw_end = value_visible(&terminal);
            for _ in 0..4 {
                press(&mut state, KeyCode::PageDown);
                terminal.draw(|frame| render(&mut state, frame)).unwrap();
                saw_end |= value_visible(&terminal);
            }
            assert!(state.scroll.0 <= state.scroll_max.0);
            assert!(
                saw_end,
                "the selected field's complete value must be scroll-reachable at {width}x{height}"
            );
        }
    }

    fn prepared() -> PreparedCall {
        use crate::stellar::fake::FakeExecutor;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let out = std::env::temp_dir().join(format!(
            "abtest-explorer-gate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&out).unwrap();
        std::fs::create_dir(out.join("market-pending")).unwrap();
        let contract = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
        let account = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
        let passphrase = "Test SDF Network ; September 2015";
        let manifest: MarketManifest = serde_json::from_value(json!({
            "network": {"label": "testnet", "passphrase": passphrase},
            "admin": account, "rpc_host": "test.invalid",
            "oracle": {
                "runtime": contract, "governance": contract, "adapter": contract,
                "sources": [contract], "min_sources": 1, "max_age_secs": 3600,
                "max_clock_drift_secs": 60, "decimals": 7, "resolution": 300, "base": "USD"
            },
            "tokens": {}, "pool": contract, "backstop": contract,
            "factory": contract, "comet": contract, "emitter": contract,
            "reserves": {}, "identities": {}, "deploy_ledger": 1
        }))
        .unwrap();
        let net = Net {
            rpc_url: "https://test.invalid".into(),
            passphrase: passphrase.into(),
            config_dir: out.join("config"),
            headers: vec![],
        };
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::ok(r#"{"passphrase":"Test SDF Network ; September 2015"}"#, ""),
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::ok(r#"{"passphrase":"Test SDF Network ; September 2015"}"#, ""),
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::ok(
                r#"[{"function_v0":{"name":"get_config","inputs":[],"outputs":[]}}]"#,
                "",
            ),
            FakeExecutor::ok(account, ""),
            FakeExecutor::ok("null", ""),
        ]);
        let st = Stellar::new(&net, &fx, Policy::Market);
        let result =
            market_call::prepare(&st, &out, &manifest, "pool", "get_config", &[], "operator")
                .unwrap();
        assert!(fx.calls.borrow().iter().all(|call| {
            !call
                .windows(2)
                .any(|pair| pair[0] == "--send" && pair[1] == "yes")
        }));
        std::fs::remove_dir_all(out).unwrap();
        result
    }

    #[test]
    fn stale_preparation_cannot_open_gate_and_fresh_gate_is_one_use() {
        let mut state = state();
        state.signed = true;
        let stale_revision = state.revision;
        press(&mut state, KeyCode::Esc);
        state.apply(stale_revision, Reply::Prepared(prepared()));
        assert!(state.prepared.is_none());
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));

        let prepared = prepared();
        let token = prepared.confirmation().to_owned();
        let preview: Value = serde_json::from_str(&prepared.summary().to_string()).unwrap();
        state.apply(state.revision, Reply::Prepared(prepared));
        for field in [
            "role",
            "method",
            "target",
            "spec_hash",
            "args",
            "signer",
            "fee_envelope",
            "risk",
            "simulated",
        ] {
            assert!(
                state.detail.contains(field),
                "preview must visibly disclose {field}"
            );
            assert!(
                preview.get(field).is_some(),
                "prepared call summary binds {field}"
            );
        }
        assert!(!state.detail.contains(&token));
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        match press(&mut state, KeyCode::Char('y')) {
            Control::Job(Job::Execute {
                prepared,
                confirmation,
            }) => {
                assert_eq!(confirmation, token);
                assert_eq!(prepared.confirmation(), token);
            }
            _ => panic!("fresh exact confirmation must consume its capability"),
        }
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
    }

    #[test]
    fn navigation_and_editing_revoke_a_prepared_call() {
        for code in [
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::Char('/'),
            KeyCode::Char('t'),
            KeyCode::Char('a'),
        ] {
            let mut state = state();
            state.signed = true;
            state.apply(state.revision, Reply::Prepared(prepared()));
            assert!(state.prepared.is_some());
            press(&mut state, code);
            assert!(state.prepared.is_none());
            assert!(!matches!(
                press(&mut state, KeyCode::Char('y')),
                Control::Job(Job::Execute { .. })
            ));
        }
    }

    #[test]
    fn action_catalog_search_returns_a_guided_recipe_without_send_authority() {
        let mut state = state();
        state.signed = true;
        state.apply(state.revision, Reply::Prepared(prepared()));
        press(&mut state, KeyCode::Char('a'));
        assert_eq!(state.pane, Pane::Actions);
        assert!(state.prepared.is_none());

        press(&mut state, KeyCode::Char('/'));
        for ch in "pool/submit".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let screen = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let editing_screen = screen(&terminal);
        assert!(editing_screen.contains("pool/submit"));
        assert!(editing_screen.contains("r/p/y type text"));
        assert!(editing_screen.contains("shortcuts inactive"));
        assert!(editing_screen.contains("Enter completes"));
        assert!(!editing_screen.contains("a Actions"));
        assert!(!editing_screen.contains("p prepare"));
        assert!(!editing_screen.contains("[ ] cycle eligible asset"));
        assert!(!editing_screen.contains(", . cycle source"));

        press(&mut state, KeyCode::Enter);
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let resumed_screen = screen(&terminal);
        assert!(resumed_screen.contains("a Actions"));
        assert!(resumed_screen.contains("Actions: Enter asset/source chooser"));
        match press(&mut state, KeyCode::Enter) {
            Control::Job(Job::GuideForm {
                category, action, ..
            }) => {
                assert_eq!(category, "pool");
                assert_eq!(action, "submit");
            }
            _ => panic!("selected recipe must request its live guided form"),
        }
        assert!(state.prepared.is_none());
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
        assert!(matches!(press(&mut state, KeyCode::Tab), Control::None));
        assert_eq!(state.pane, Pane::Functions);
        assert!(matches!(
            press(&mut state, KeyCode::Char('s')),
            Control::Job(Job::Status)
        ));
        assert!(state.status.open);
    }

    #[test]
    fn form_editor_treats_action_keys_as_text_and_escape_returns_to_menu() {
        let mut state = state();
        state.pane = Pane::Actions;
        state.input = Some(Input::Search);
        for key in ['a', 'r', 'p', 'y'] {
            assert!(matches!(
                press(&mut state, KeyCode::Char(key)),
                Control::None
            ));
        }
        assert_eq!(state.edit, "arpy");
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));

        state.pane = Pane::ActionForm;
        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.pane, Pane::Actions);
        assert!(state.prepared.is_none());
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
    }

    #[test]
    fn rejected_form_draft_stays_editable_until_commit_or_cancel() {
        let mut unavailable = state();
        unavailable.input = Some(Input::Form(0));
        unavailable.edit = "orphan draft".into();
        let mut state = state();
        state.manifest = Some(form_manifest());
        state.action_category = "governance".into();
        state.action_name = "execute-proposal".into();
        state.action_form = Some(
            ActionForm::new(
                &[("id".into(), json!("u32"))],
                &[],
                &Safe::render_market_json(&json!([]), TEXT_CAP).unwrap(),
                state.manifest.as_ref().unwrap(),
                "governance",
                "execute-proposal",
            )
            .unwrap(),
        );
        state.action_form.as_mut().unwrap().set(0, "7").unwrap();
        state.pane = Pane::ActionForm;
        state.message = "Typed guided form ready; r simulates one call, p prepares one call.";
        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;

        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(matches!(state.input.as_ref(), Some(Input::Form(0))));
        assert!(state.prepared.is_none());
        press(&mut state, KeyCode::Char('x'));
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(matches!(state.input.as_ref(), Some(Input::Form(0))));
        assert_eq!(state.edit, "7x");
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
            Some("7")
        );
        assert!(state.detail.contains("invalid unsigned integer"));
        assert!(state.prepared.is_none());

        for key in ['p', 'r', 'y'] {
            assert!(matches!(
                press(&mut state, KeyCode::Char(key)),
                Control::None
            ));
        }
        assert_eq!(state.edit, "7xpry");
        assert!(matches!(press(&mut state, KeyCode::Tab), Control::None));
        assert!(matches!(state.input.as_ref(), Some(Input::Form(0))));
        assert_eq!(state.pane, Pane::ActionForm);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let screen = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let active = screen(&terminal);
        assert!(active.contains("r/p/y type text") && active.contains("shortcuts inactive"));
        assert!(active.contains("Enter completes"));
        assert!(!active.contains("Typed guided form ready"));
        for hint in [
            "a Actions",
            "p prepare",
            "y confirms",
            "Enter open/edit",
            "+/- rows",
            "[ ] cycle eligible asset",
        ] {
            assert!(!active.contains(hint));
        }
        for _ in 0..4 {
            press(&mut state, KeyCode::Backspace);
        }
        press(&mut state, KeyCode::Char('1'));
        assert_eq!(state.edit, "71");
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        assert!(state.input.is_none());
        assert!(state.edit.is_empty());
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
            Some("71")
        );
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let committed = screen(&terminal);
        assert!(!committed.contains("action shortcuts inactive"));
        assert!(!committed.contains("Enter completes"));
        assert!(committed.contains("a Actions"));
        assert!(committed.contains("Actions: Enter asset/source chooser"));

        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Char('9'));
        press(&mut state, KeyCode::Char('9'));
        press(&mut state, KeyCode::Esc);
        assert!(state.input.is_none());
        assert!(state.edit.is_empty());
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
            Some("71")
        );
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        let cancelled = screen(&terminal);
        assert!(!cancelled.contains("action shortcuts inactive"));
        assert!(!cancelled.contains("Enter completes"));
        assert!(cancelled.contains("a Actions"));
        assert!(cancelled.contains("Actions: Enter asset/source chooser"));
        assert!(matches!(
            press(&mut state, KeyCode::Char('r')),
            Control::Job(Job::Guided { prepare: false, .. })
        ));

        assert!(matches!(
            press(&mut unavailable, KeyCode::Enter),
            Control::None
        ));
        assert!(unavailable.input.is_none());
        assert!(unavailable.edit.is_empty());
        assert!(unavailable.detail.contains("unavailable"));
    }

    #[test]
    fn rejected_form_diagnostic_survives_scroll_and_supported_resizes() {
        let field = "proposal_identifier_value";
        let mut state = state();
        state.manifest = Some(form_manifest());
        state.action_category = "governance".into();
        state.action_name = "execute-proposal".into();
        let mut form = ActionForm::new(
            &[(field.into(), json!("u32"))],
            &[],
            &Safe::render_market_json(&json!([]), TEXT_CAP).unwrap(),
            state.manifest.as_ref().unwrap(),
            "governance",
            "execute-proposal",
        )
        .unwrap();
        form.set(0, "7").unwrap();
        state.action_form = Some(form);
        state.pane = Pane::ActionForm;
        state.detail = "Previous ready detail. ".repeat(500);
        state.message = "Typed guided form ready; r simulates one call, p prepares one call.";
        state.action_asset = Some("usdc".into());
        state.action_source = Some(0);
        // A loaded form focuses its first typed field, below any selector rows.
        state.argument = state.selectors().count();

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(&mut state, frame)).unwrap();
        press(&mut state, KeyCode::PageDown);
        assert!(
            state.scroll.0 > 0,
            "the rejection starts from a scrolled inspector"
        );

        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.edit, "7x");
        state.signed = true;
        state.prepared = Some(prepared());
        state.phase = Phase::Awaiting;
        assert!(matches!(press(&mut state, KeyCode::Enter), Control::None));

        let diagnostic = format!("{field}: invalid unsigned integer");
        assert_eq!(state.scroll, (0, 0));
        assert!(matches!(state.input.as_ref(), Some(Input::Form(0))));
        assert_eq!(state.edit, "7x");
        assert_eq!(
            state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
            Some("7")
        );
        assert!(state.prepared.is_none());

        let compact = |text: &str| {
            text.chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        };
        for (index, (width, height)) in [(120, 40), (60, 18), (80, 24), (60, 16)]
            .into_iter()
            .enumerate()
        {
            if index > 0 {
                terminal.backend_mut().resize(width, height);
            }
            terminal.draw(|frame| render(&mut state, frame)).unwrap();
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(Rect::new(0, 1, width, height - 4));
            let inspector = columns[1];
            let buffer = terminal.backend().buffer();
            let inspector_text = (inspector.y + 1..inspector.bottom() - 1)
                .map(|y| {
                    (inspector.x + 1..inspector.right() - 1)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<String>();
            assert!(
                compact(&inspector_text).contains(&compact(&diagnostic)),
                "diagnostic must remain in the inspector at {width}x{height}"
            );
            let editor = (0..width)
                .map(|x| buffer[(x, height - 3)].symbol())
                .collect::<String>();
            let footer = (0..width)
                .map(|x| buffer[(x, height - 2)].symbol())
                .collect::<String>();
            let controls = (0..width)
                .map(|x| buffer[(x, height - 1)].symbol())
                .collect::<String>();
            assert!(
                compact(&editor).contains(&compact(&format!("{field}=7x"))),
                "rejected draft must remain visible at {width}x{height}"
            );
            assert!(
                editor.contains("Enter") && editor.contains("Esc"),
                "field recovery controls must remain visible at {width}x{height}"
            );
            assert!(
                footer.contains("r/p/y type text") && footer.contains("shortcuts inactive"),
                "active-input guidance must remain visible at {width}x{height}"
            );
            assert!(
                controls.contains("Enter completes")
                    && controls.contains("Esc cancels")
                    && controls.contains("Ctrl-C quits"),
                "modal exit controls must remain visible at {width}x{height}"
            );
            assert!(!inspector_text.contains("Typed guided form ready"));
            assert!(!footer.contains("a Actions"));
            assert!(!controls.contains("p prepare"));
            assert!(!controls.contains("y confirms"));
            assert!(matches!(state.input.as_ref(), Some(Input::Form(0))));
            assert_eq!(state.edit, "7x");
            assert_eq!(
                state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
                Some("7")
            );
            assert!(state.prepared.is_none());
        }
        for (input, cap) in [(Input::Form(0), MAX_ARG_VALUE_LEN), (Input::Receipt, 64)] {
            state.input = Some(input);
            state.detail.clone_from(&diagnostic);
            state.edit = "x".repeat(cap);
            assert!(matches!(
                press(&mut state, KeyCode::Char('y')),
                Control::None
            ));
            assert_eq!(state.edit.len(), cap);
            assert!(state.detail.contains(state.message));
            let detail_len = state.detail.len();
            press(&mut state, KeyCode::Char('z'));
            assert_eq!(state.detail.len(), detail_len);
            terminal.draw(|frame| render(&mut state, frame)).unwrap();
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(Rect::new(0, 1, 60, 12));
            let inspector = columns[1];
            let buffer = terminal.backend().buffer();
            let inspector_text = (inspector.y + 1..inspector.bottom() - 1)
                .map(|y| {
                    (inspector.x + 1..inspector.right() - 1)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<String>();
            assert!(compact(&inspector_text).contains(&compact(&diagnostic)));
            assert!(compact(&inspector_text).contains(&compact(state.message)));
            assert!(matches!(press(&mut state, KeyCode::Esc), Control::None));
            assert!(state.input.is_none());
            assert_eq!(
                state.action_form.as_ref().unwrap().edit_value(0).as_deref(),
                Some("7")
            );
            assert!(state.prepared.is_none());
        }
    }

    #[test]
    fn status_navigation_keeps_reply_and_never_routes_send_actions() {
        let mut state = state();
        state.signed = true;
        state.apply(state.revision, Reply::Prepared(prepared()));
        let revision = match press(&mut state, KeyCode::Char('s')) {
            Control::Job(Job::Status) => state.revision,
            _ => panic!("s opens the read-only status screen"),
        };
        assert!(state.prepared.is_none());
        assert!(state.status.open);
        press(&mut state, KeyCode::Down);
        assert!(matches!(
            press(&mut state, KeyCode::Char('p')),
            Control::None
        ));
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
        let safe = Safe::render_market_json(
            &json!({
                "schema_version": 1, "readiness": "not-ready", "ready": false,
                "reasons": ["network unavailable"], "mutation": "none"
            }),
            TEXT_CAP,
        )
        .unwrap();
        let report = StatusReport::parse(&safe).unwrap();
        state.apply(revision, Reply::Status(Ok(report)));
        assert!(state.status.snapshot.is_some());
        assert!(state.status.open);

        let refresh = match press(&mut state, KeyCode::Char('s')) {
            Control::Job(Job::Status) => state.revision,
            _ => panic!("status s refreshes without enabling actions"),
        };
        state.status.key(KeyCode::Esc);
        state.revision = state.revision.wrapping_add(1);
        state.apply(refresh, Reply::Status(Err("stale".into())));
        assert!(!state.status.open);
        assert!(state.status.snapshot.is_some());
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));
    }

    // Every section root must be observed for a report to establish readiness;
    // a partial report is deliberately downgraded by the parser.
    fn status_report(reasons: &[&str]) -> StatusReport {
        let ready = reasons.is_empty();
        let safe = Safe::render_market_json(
            &json!({
                "schema_version": 1,
                "readiness": if ready { "ready" } else { "not-ready" },
                "ready": ready,
                "reasons": reasons,
                "identity": {"state": "separated"},
                "oracle": {"price": {"state": "observed", "observed": "\"1\""}},
                "pool": {"positions": {"state": "observed", "observed": "null"}},
                "backstop": {"reward_zone": {"state": "observed", "observed": "null"}},
                "token_balances": {"USD": {"pool": {"state": "observed", "observed": "\"0\""}}},
                "journal": {"state": "journal-clear", "pending_count": 0, "truncated": false},
                "mutation": "none"
            }),
            TEXT_CAP,
        )
        .unwrap();
        StatusReport::parse(&safe).unwrap()
    }

    #[test]
    fn cached_status_reopens_without_a_read_and_revokes_send_authority() {
        use crate::tui::market_status_view::Readiness;
        let mut state = state();
        state.signed = true;
        state.apply(state.revision, Reply::Prepared(prepared()));
        assert_eq!(state.phase, Phase::Awaiting);

        let first = match press(&mut state, KeyCode::Char('s')) {
            Control::Job(Job::Status) => state.revision,
            _ => panic!("the first s has no cache and must read"),
        };
        assert_eq!(state.status.request, Some(first));
        state.apply(first, Reply::Status(Ok(status_report(&[]))));
        let cached = state.status.snapshot.as_ref().unwrap().readiness;
        assert_eq!(cached, Readiness::Ready);
        press(&mut state, KeyCode::Esc);
        assert!(!state.status.open);

        state.apply(state.revision, Reply::Prepared(prepared()));
        assert_eq!(state.phase, Phase::Awaiting);
        let armed = state.revision;
        assert!(
            matches!(press(&mut state, KeyCode::Char('s')), Control::None),
            "a cached snapshot must reopen without any worker read"
        );
        assert!(state.status.open);
        assert!(state.status.request.is_none());
        assert_eq!(state.status.snapshot.as_ref().unwrap().readiness, cached);
        assert!(
            state.prepared.is_none(),
            "a cached READY is display only, never transaction authority"
        );
        assert_eq!(state.phase, Phase::ReadOnly);
        state.apply(armed, Reply::Prepared(prepared()));
        assert!(
            state.prepared.is_none(),
            "a stale Prepared cannot reopen the gate"
        );
        assert!(matches!(
            press(&mut state, KeyCode::Char('y')),
            Control::None
        ));

        let refresh = match press(&mut state, KeyCode::Char('s')) {
            Control::Job(Job::Status) => state.revision,
            _ => panic!("s inside the open screen must force a fresh read"),
        };
        assert_eq!(state.status.request, Some(refresh));
        state.busy = true;
        assert!(
            matches!(press(&mut state, KeyCode::Char('s')), Control::None),
            "one in-flight refresh at a time"
        );
        state.apply(
            refresh,
            Reply::Status(Ok(status_report(&["oracle unavailable"]))),
        );
        assert_eq!(
            state.status.snapshot.as_ref().unwrap().readiness,
            Readiness::NotReady,
            "the refreshed report must replace the cached contents"
        );

        let failed = match press(&mut state, KeyCode::Char('s')) {
            Control::Job(Job::Status) => state.revision,
            _ => panic!("a refresh must always be retryable"),
        };
        state.apply(failed, Reply::Status(Err("read failed".into())));
        assert!(state.status.request.is_none(), "a failed read arms nothing");
        assert_eq!(
            state.status.snapshot.as_ref().unwrap().readiness,
            Readiness::NotReady
        );
    }

    #[test]
    fn a_cancelled_or_failed_status_read_caches_nothing_and_stays_retryable() {
        let mut cancelled = state();
        let in_flight = match press(&mut cancelled, KeyCode::Char('s')) {
            Control::Job(Job::Status) => cancelled.revision,
            _ => panic!("the first s has no cache and must read"),
        };
        press(&mut cancelled, KeyCode::Esc);
        assert!(!cancelled.status.open);
        cancelled.apply(in_flight, Reply::Status(Ok(status_report(&[]))));
        assert!(
            !cancelled.status.open,
            "a late reply cannot reopen a closed screen"
        );
        assert!(
            cancelled.status.snapshot.is_none(),
            "a cancelled read must not be cached as a current report"
        );
        assert!(
            matches!(
                press(&mut cancelled, KeyCode::Char('s')),
                Control::Job(Job::Status)
            ),
            "with nothing cached, the next s must read again"
        );

        let mut failing = state();
        let first = match press(&mut failing, KeyCode::Char('s')) {
            Control::Job(Job::Status) => failing.revision,
            _ => panic!("the first s has no cache and must read"),
        };
        failing.apply(first, Reply::Status(Err("unavailable".into())));
        assert!(
            failing.status.snapshot.is_none(),
            "a failed read is never cached"
        );
        press(&mut failing, KeyCode::Esc);
        assert!(
            matches!(
                press(&mut failing, KeyCode::Char('s')),
                Control::Job(Job::Status)
            ),
            "a failed first read must retry rather than reopen an empty cache"
        );
    }

    #[test]
    fn quit_revokes_capability_and_preserves_failed_and_unresolved_outcomes() {
        for (code, modifiers) in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            // A preview-only quit must revoke the capability and stay clean.
            let mut preview = state();
            preview.signed = true;
            preview.apply(preview.revision, Reply::Prepared(prepared()));
            assert!(preview.prepared.is_some());
            assert!(matches!(
                preview.key(KeyEvent::new(code, modifiers)),
                Control::Quit
            ));
            assert!(
                preview.prepared.is_none(),
                "quit must revoke the capability"
            );
            assert!(
                !matches!(preview.phase, Phase::Failed),
                "a cancelled preview must never become Failed on quit"
            );
            assert_eq!(preview.phase.exit_code(), 0);

            // A confirmed failed send that the worker reports must keep a
            // nonzero exit even after navigation or a read-only detail
            // rewrites the display phase.
            let mut confirmed_failed = state();
            confirmed_failed.apply(
                confirmed_failed.revision,
                Reply::Sent {
                    text: "confirmed failure".into(),
                    phase: Phase::Failed,
                },
            );
            assert!(confirmed_failed.send_failed);
            assert_eq!(confirmed_failed.exit_status(), 1);
            press(&mut confirmed_failed, KeyCode::Tab);
            assert!(matches!(confirmed_failed.phase, Phase::ReadOnly));
            assert!(confirmed_failed.send_failed);
            assert_eq!(confirmed_failed.exit_status(), 1);
            assert!(matches!(
                confirmed_failed.key(KeyEvent::new(code, modifiers)),
                Control::Quit
            ));
            assert_eq!(confirmed_failed.exit_status(), 1);

            // A read-only detail following a confirmed failed send must not
            // wash the failure out of the exit classification either.
            let mut detail_after_fail = state();
            detail_after_fail.apply(
                detail_after_fail.revision,
                Reply::Sent {
                    text: "confirmed failure".into(),
                    phase: Phase::Failed,
                },
            );
            detail_after_fail.apply(
                detail_after_fail.revision,
                Reply::Detail("read-only result".into()),
            );
            assert!(matches!(detail_after_fail.phase, Phase::Preview));
            assert!(detail_after_fail.send_failed);
            assert!(matches!(
                detail_after_fail.key(KeyEvent::new(code, modifiers)),
                Control::Quit
            ));
            assert_eq!(detail_after_fail.exit_status(), 1);

            // A confirmed failed send that later transitions to a successful
            // confirmed send clears the sticky flag and restores a clean exit.
            let mut recovery = state();
            recovery.apply(
                recovery.revision,
                Reply::Sent {
                    text: "confirmed failure".into(),
                    phase: Phase::Failed,
                },
            );
            assert!(recovery.send_failed);
            recovery.apply(
                recovery.revision,
                Reply::Sent {
                    text: "confirmed success".into(),
                    phase: Phase::Submitted,
                },
            );
            assert!(!recovery.send_failed);
            assert_eq!(recovery.exit_status(), 0);

            // A definite failed result must survive ordinary quit with a nonzero exit.
            let mut failed = state();
            failed.phase = Phase::Failed;
            assert!(matches!(
                failed.key(KeyEvent::new(code, modifiers)),
                Control::Quit
            ));
            assert!(matches!(failed.phase, Phase::Failed));
            assert_eq!(failed.phase.exit_code(), 1);

            // An unresolved custody outcome remains blocked after quit.
            let mut unresolved = state();
            unresolved.phase = Phase::Unresolved;
            assert!(matches!(
                unresolved.key(KeyEvent::new(code, modifiers)),
                Control::Quit
            ));
            assert!(matches!(unresolved.phase, Phase::Unresolved));
            assert_eq!(unresolved.phase.exit_code(), 1);

            // Quitting while a send is in flight must preserve the outcome gap
            // with a nonzero exit, because the worker may still mutate or the
            // outcome may never have been delivered.
            let mut inflight = state();
            inflight.phase = Phase::Submitting;
            assert!(matches!(
                inflight.key(KeyEvent::new(code, modifiers)),
                Control::Quit
            ));
            assert!(matches!(inflight.phase, Phase::Submitting));
            assert_eq!(inflight.phase.exit_code(), 1);
        }
    }
}
