use crate::market_explorer::{NamedAddress as AddressChoice, PublicAddress};

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

use crate::manifest::MarketManifest;
use crate::stellar::Safe;

#[derive(Clone)]
enum Kind {
    Text,
    Address,
    Integer { signed: bool, bits: u32 },
    RequestType,
    Bool,
    Vector(Box<Kind>),
    Record(Vec<Field>),
}

#[derive(Clone)]
struct Field {
    name: String,
    kind: Kind,
    value: String,
    locked: bool,
}

#[derive(Clone)]
struct Argument {
    name: String,
    kind: Kind,
    value: String,
    locked: bool,
    rows: Vec<Vec<Field>>,
}

pub(super) struct ActionForm {
    args: Vec<Argument>,
    choices: Vec<AddressChoice>,
    category: String,
    action: String,
    price_decimals: u32,
}

impl ActionForm {
    pub(super) fn new(
        inputs: &[(String, Value)],
        bound: &[(String, String)],
        spec: &Safe,
        manifest: &MarketManifest,
        category: &str,
        action: &str,
    ) -> Result<Self> {
        let document: Value = serde_json::from_str(&spec.to_string())
            .context("live interface specification is malformed")?;
        let entries = document
            .as_array()
            .ok_or_else(|| anyhow!("live interface has no entry list"))?;
        let mut args = Vec::with_capacity(inputs.len());
        for (name, descriptor) in inputs {
            ensure!(
                crate::market_query::arg_name_is_wellformed(name),
                "input field name is malformed"
            );
            let kind = parse_kind(descriptor, entries, name)?;
            let value = if matches!(&kind, Kind::Address)
                && matches!(name.as_str(), "from" | "caller" | "user")
            {
                manifest.admin.as_str().to_owned()
            } else if category == "oracle" && action == "publish-price" && name == "timestamp" {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|time| time.as_secs().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let rows = if matches!(kind, Kind::Record(_)) {
                vec![make_row(&kind, category, action)]
            } else {
                Vec::new()
            };
            args.push(Argument {
                name: name.clone(),
                kind,
                value,
                locked: false,
                rows,
            });
        }
        let mut form = Self {
            choices: Vec::new(),
            args,
            category: category.to_owned(),
            action: action.to_owned(),
            price_decimals: manifest.oracle.decimals,
        };
        for (name, value) in bound {
            ensure!(
                form.args.iter().all(|arg| arg.name != *name),
                "bound argument overlaps editable input"
            );
            form.args.push(Argument {
                name: name.clone(),
                kind: Kind::Text,
                value: value.clone(),
                locked: true,
                rows: Vec::new(),
            });
        }
        Ok(form)
    }

    pub(super) fn set_choices(&mut self, choices: &[AddressChoice]) {
        self.choices = choices.to_vec();
    }

    /// Cell eligibility follows the ABI kind, never the displayed field name.
    pub(super) fn is_address(&self, index: usize) -> bool {
        let mut cell = 0;
        for arg in &self.args {
            if cell == index {
                return !arg.locked && matches!(arg.kind, Kind::Address);
            }
            cell += 1;
            for row in &arg.rows {
                for field in row {
                    if cell == index {
                        return !field.locked && matches!(field.kind, Kind::Address);
                    }
                    cell += 1;
                }
            }
        }
        false
    }

    pub(super) fn is_request_type(&self, index: usize) -> bool {
        let mut cell = 0;
        for arg in &self.args {
            if cell == index {
                return !arg.locked && matches!(arg.kind, Kind::RequestType);
            }
            cell += 1;
            for field in arg.rows.iter().flatten() {
                if cell == index {
                    return !field.locked && matches!(field.kind, Kind::RequestType);
                }
                cell += 1;
            }
        }
        false
    }

    pub(super) fn resolve_request_type(
        &self,
        index: usize,
        query: &str,
        selected: Option<usize>,
    ) -> Result<Option<u32>> {
        ensure!(
            self.is_request_type(index),
            "selected field is not a request type"
        );
        if let Some(choice) = selected {
            ensure!(
                request_type_matches(query).any(|matched| matched == choice),
                "select a visible request type"
            );
            return Ok(Some(choice as u32));
        }
        if query.is_empty() {
            return Ok(None);
        }
        if query.bytes().all(|byte| byte.is_ascii_digit()) {
            let code = query
                .parse::<u32>()
                .map_err(|_| anyhow!("request type must be in 0..=9"))?;
            ensure!(code <= 9, "request type must be in 0..=9");
            return Ok(Some(code));
        }
        let mut matches = request_type_matches(query);
        let Some(choice) = matches.next() else {
            bail!("enter a request type 0..=9 or a matching label");
        };
        ensure!(
            matches.next().is_none(),
            "several request types match; select one explicitly"
        );
        Ok(Some(choice as u32))
    }

    pub(super) fn cells(&self) -> Vec<(String, String, String)> {
        let mut cells = Vec::new();
        for arg in &self.args {
            let arg_hint = if arg.locked {
                "bound by this guide".to_owned()
            } else if matches!(arg.kind, Kind::Record(_)) {
                "Enter to step into this record; edit its typed fields".to_owned()
            } else if self.category == "oracle"
                && self.action == "publish-price"
                && arg.name == "timestamp"
            {
                "current host Unix seconds; not ledger time".to_owned()
            } else if self.category == "oracle"
                && self.action == "publish-price"
                && arg.name == "price"
            {
                format!(
                    "integer price in smallest units ({} decimals)",
                    self.price_decimals
                )
            } else {
                hint(&arg.kind)
            };
            let value = match &arg.kind {
                Kind::Vector(_) => format!("{} rows", arg.rows.len()),
                Kind::Record(_) => "structured record".to_owned(),
                Kind::Address => self.address_display(&arg.value),
                _ => arg.value.clone(),
            };
            cells.push((arg.name.clone(), value, arg_hint));
            for (row_index, row) in arg.rows.iter().enumerate() {
                for field in row {
                    let field_hint = if field.locked {
                        "fixed by this recipe".to_owned()
                    } else {
                        hint(&field.kind)
                    };
                    cells.push((
                        format!("{}[{row_index}].{}", arg.name, field.name),
                        if matches!(&field.kind, Kind::Address) {
                            self.address_display(&field.value)
                        } else {
                            field.value.clone()
                        },
                        field_hint,
                    ));
                }
            }
        }
        cells
    }

    fn address_display(&self, value: &str) -> String {
        self.choices
            .iter()
            .find(|choice| choice.address.as_str() == value)
            .map(|choice| format!("{} ({})", choice.name, choice.address.as_str()))
            .unwrap_or_else(|| value.to_owned())
    }

    pub(super) fn enter(&mut self, index: usize) -> Result<Option<usize>> {
        let mut cell = 0;
        for arg in &mut self.args {
            if cell == index && matches!(arg.kind, Kind::Vector(_) | Kind::Record(_)) {
                ensure!(!arg.locked, "this field is fixed by the guide");
                if let Kind::Vector(element) = &arg.kind {
                    if arg.rows.is_empty() {
                        let row = make_row(element, &self.category, &self.action);
                        ensure!(
                            row.iter().any(|field| !field.locked),
                            "this collection has no editable fields"
                        );
                        arg.rows.push(row);
                    }
                }
                return arg
                    .rows
                    .iter()
                    .flatten()
                    .position(|field| !field.locked)
                    .map(|offset| Some(cell + 1 + offset))
                    .ok_or_else(|| anyhow!("this collection has no editable fields"));
            }
            cell += 1 + arg.rows.iter().map(Vec::len).sum::<usize>();
        }
        Ok(None)
    }

    pub(super) fn edit_value(&self, index: usize) -> Option<String> {
        let mut cell = 0;
        for arg in &self.args {
            if cell == index {
                return (!arg.locked && !matches!(arg.kind, Kind::Vector(_) | Kind::Record(_)))
                    .then(|| arg.value.clone());
            }
            cell += 1;
            for row in &arg.rows {
                for field in row {
                    if cell == index {
                        return (!field.locked).then(|| field.value.clone());
                    }
                    cell += 1;
                }
            }
        }
        None
    }
    pub(super) fn set(&mut self, index: usize, text: &str) -> Result<()> {
        ensure!(
            !text.chars().any(char::is_control),
            "field value contains control characters"
        );
        let mut cell = 0;
        for arg in &mut self.args {
            if cell == index {
                ensure!(!arg.locked, "this field is fixed by the guide");
                ensure!(
                    !matches!(arg.kind, Kind::Vector(_) | Kind::Record(_)),
                    "select the typed fields below instead of editing an aggregate"
                );
                validate_value(&arg.kind, text, &arg.name)?;
                arg.value = text.to_owned();
                return Ok(());
            }
            cell += 1;
            for row in &mut arg.rows {
                for field in row {
                    if cell == index {
                        ensure!(!field.locked, "this field is fixed by the guide");
                        validate_value(&field.kind, text, &format!("{}.{}", arg.name, field.name))?;
                        field.value = text.to_owned();
                        return Ok(());
                    }
                    cell += 1;
                }
            }
        }
        bail!("field index is out of range")
    }

    pub(super) fn add(&mut self, index: usize) -> Result<()> {
        let arg_index = self
            .vector_at(index)
            .ok_or_else(|| anyhow!("selected field is not a vector"))?;
        let arg = &mut self.args[arg_index];
        ensure!(!arg.locked, "this vector is fixed by the guide");
        let Kind::Vector(element) = &arg.kind else {
            unreachable!()
        };
        arg.rows
            .push(make_row(element, &self.category, &self.action));
        Ok(())
    }

    pub(super) fn remove(&mut self, index: usize) -> Result<()> {
        let arg_index = self
            .vector_at(index)
            .ok_or_else(|| anyhow!("selected field is not a vector"))?;
        let arg = &mut self.args[arg_index];
        ensure!(!arg.locked, "this vector is fixed by the guide");
        ensure!(!arg.rows.is_empty(), "vector has no row to remove");
        arg.rows.pop();
        Ok(())
    }

    pub(super) fn cycle(&mut self, index: usize, down: bool) -> Result<()> {
        let choices = &self.choices;
        let mut cell = 0;
        for arg in &mut self.args {
            if cell == index {
                ensure!(
                    matches!(arg.kind, Kind::Address),
                    "selected field is not an address"
                );
                ensure!(!arg.locked, "this field is fixed by the guide");
                cycle_value(&mut arg.value, choices, down);
                return Ok(());
            }
            cell += 1;
            for row in &mut arg.rows {
                for field in row {
                    if cell == index {
                        ensure!(
                            matches!(field.kind, Kind::Address),
                            "selected field is not an address"
                        );
                        ensure!(!field.locked, "this field is fixed by the guide");
                        cycle_value(&mut field.value, choices, down);
                        return Ok(());
                    }
                    cell += 1;
                }
            }
        }
        bail!("field index is out of range")
    }

    fn vector_at(&self, index: usize) -> Option<usize> {
        let mut cell = 0;
        for (arg_index, arg) in self.args.iter().enumerate() {
            if cell == index {
                return matches!(arg.kind, Kind::Vector(_)).then_some(arg_index);
            }
            cell += 1 + arg.rows.iter().map(Vec::len).sum::<usize>();
        }
        None
    }

    pub(super) fn explicit(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(self.args.len());
        for arg in &self.args {
            if arg.locked {
                continue;
            }
            let encoded = match &arg.kind {
                Kind::Vector(element) => {
                    if self.category == "auction" && arg.name == "requests" {
                        ensure!(
                            !arg.rows.is_empty(),
                            "{}: auction recipe needs at least one request",
                            arg.name
                        );
                    }
                    let values = arg
                        .rows
                        .iter()
                        .enumerate()
                        .map(|(index, row)| {
                            encode_row(element, row, &format!("{}[{index}]", arg.name))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Value::Array(values).to_string()
                }
                Kind::Record(_) => encode_row(
                    &arg.kind,
                    arg.rows
                        .first()
                        .ok_or_else(|| anyhow!("{}: missing record fields", arg.name))?,
                    &arg.name,
                )?
                .to_string(),
                _ => encode_scalar(&arg.kind, &arg.value, &arg.name)?.1,
            };
            out.push((arg.name.clone(), encoded));
        }
        Ok(out)
    }
}

pub(super) const REQUEST_TYPE_LABELS: [&str; 10] = [
    "Supply",
    "Withdraw",
    "Supply collateral",
    "Withdraw collateral",
    "Borrow",
    "Repay",
    "Fill user liquidation auction",
    "Fill bad debt auction",
    "Fill interest auction",
    "Delete liquidation auction",
];

pub(super) fn request_type_matches(query: &str) -> impl Iterator<Item = usize> + '_ {
    (0..REQUEST_TYPE_LABELS.len()).filter(move |&index| {
        if query.is_empty() {
            true
        } else if query.bytes().all(|byte| byte.is_ascii_digit()) {
            query
                .parse::<usize>()
                .is_ok_and(|code| code == index && code <= 9)
        } else {
            REQUEST_TYPE_LABELS[index]
                .get(..query.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
        }
    })
}

fn cycle_value(value: &mut String, choices: &[AddressChoice], down: bool) {
    if choices.is_empty() {
        return;
    }
    let next = match (
        choices
            .iter()
            .position(|choice| choice.address.as_str() == value.as_str()),
        down,
    ) {
        (Some(i), true) => (i + 1) % choices.len(),
        (Some(0), false) => choices.len() - 1,
        (Some(i), false) => i - 1,
        (None, _) => 0,
    };
    *value = choices[next].address.as_str().to_owned();
}

fn parse_kind(ty: &Value, entries: &[Value], path: &str) -> Result<Kind> {
    if let Some(name) = ty.as_str() {
        return Ok(match name {
            "address" => Kind::Address,
            "bool" => Kind::Bool,
            "string" => Kind::Text,
            "i32" => Kind::Integer {
                signed: true,
                bits: 32,
            },
            "i64" => Kind::Integer {
                signed: true,
                bits: 64,
            },
            "i128" => Kind::Integer {
                signed: true,
                bits: 128,
            },
            "u32" => Kind::Integer {
                signed: false,
                bits: 32,
            },
            "u64" => Kind::Integer {
                signed: false,
                bits: 64,
            },
            "u128" => Kind::Integer {
                signed: false,
                bits: 128,
            },
            _ => bail!("{path}: unsupported ABI type {name}"),
        });
    }
    if let Some(elem) = ty.get("vec").and_then(|x| x.get("element_type")) {
        return Ok(Kind::Vector(Box::new(parse_kind(elem, entries, path)?)));
    }
    if let Some(name) = ty
        .get("udt")
        .and_then(|x| x.get("name"))
        .and_then(Value::as_str)
    {
        let entry = entries.iter().find_map(|entry| {
            entry
                .get("udt_struct_v0")
                .filter(|def| def.get("name").and_then(Value::as_str) == Some(name))
        });
        if let Some(def) = entry {
            let fields = def
                .get("fields")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("{path}: malformed ABI struct {name}"))?;
            let mut parsed = Vec::with_capacity(fields.len());
            for field in fields {
                let field_name = field
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|x| crate::market_query::arg_name_is_wellformed(x))
                    .ok_or_else(|| anyhow!("{path}: malformed ABI struct field"))?;
                let field_type = field
                    .get("type_")
                    .or_else(|| field.get("type"))
                    .ok_or_else(|| anyhow!("{path}.{field_name}: missing ABI type"))?;
                ensure!(
                    field_type.is_string(),
                    "{path}.{field_name}: nested aggregate ABI field is unsupported"
                );
                let field_kind = if name == "Request" && field_name == "request_type" {
                    ensure!(
                        field_type.as_str() == Some("u32"),
                        "{path}.request_type: Request ABI type must be u32"
                    );
                    Kind::RequestType
                } else {
                    parse_kind(field_type, entries, &format!("{path}.{field_name}"))?
                };
                parsed.push(Field {
                    name: field_name.to_owned(),
                    kind: field_kind,
                    value: String::new(),
                    locked: false,
                });
            }
            return Ok(Kind::Record(parsed));
        }
        if name == "Request" {
            return Ok(Kind::Record(request_fields()));
        }
        if name == "FlashLoan" {
            return Ok(Kind::Record(flash_loan_fields()));
        }
        bail!("{path}: unsupported ABI UDT {name}");
    }
    bail!("{path}: unsupported ABI type")
}

fn request_fields() -> Vec<Field> {
    vec![
        field("request_type", Kind::RequestType),
        field("address", Kind::Address),
        field(
            "amount",
            Kind::Integer {
                signed: true,
                bits: 128,
            },
        ),
    ]
}
fn flash_loan_fields() -> Vec<Field> {
    vec![
        field("contract", Kind::Address),
        field("asset", Kind::Address),
        field(
            "amount",
            Kind::Integer {
                signed: true,
                bits: 128,
            },
        ),
    ]
}
fn field(name: &str, kind: Kind) -> Field {
    Field {
        name: name.into(),
        kind,
        value: String::new(),
        locked: false,
    }
}

fn make_row(kind: &Kind, category: &str, action: &str) -> Vec<Field> {
    match kind {
        Kind::Record(fields) => {
            let mut row = fields.clone();
            if let Some(code) = request_code(category, action) {
                if let Some(req) = row.iter_mut().find(|field| {
                    field.name == "request_type" && matches!(field.kind, Kind::RequestType)
                }) {
                    req.value = code.to_string();
                    req.locked = true;
                }
            }
            row
        }
        _ => vec![field("value", kind.clone())],
    }
}
fn request_code(category: &str, action: &str) -> Option<u32> {
    match (category, action) {
        ("auction", "fill-liquidation") => Some(6),
        ("auction", "fill-bad-debt") => Some(7),
        ("auction", "fill-interest") => Some(8),
        ("auction", "delete-liquidation") => Some(9),
        _ => None,
    }
}

fn encode_row(kind: &Kind, row: &[Field], path: &str) -> Result<Value> {
    match kind {
        Kind::Record(_) => {
            let mut object = Map::new();
            for field in row {
                let field_path = format!("{path}.{}", field.name);
                let mut value = encode_json_scalar(&field.kind, &field.value, &field_path)?;
                if field.name == "amount" {
                    value = Value::String(field.value.clone());
                }
                object.insert(field.name.clone(), value);
            }
            Ok(Value::Object(object))
        }
        _ => {
            let f = row
                .first()
                .ok_or_else(|| anyhow!("{path}: missing vector cell"))?;
            encode_json_scalar(&f.kind, &f.value, path)
        }
    }
}

fn encode_json_scalar(kind: &Kind, text: &str, path: &str) -> Result<Value> {
    let (value, _) = encode_scalar(kind, text, path)?;
    Ok(match kind {
        Kind::Integer {
            signed: true,
            bits: 32 | 64,
        } => json!(text
            .parse::<i64>()
            .map_err(|_| anyhow!("{path}: invalid signed integer"))?),
        Kind::Integer {
            signed: false,
            bits: 32 | 64,
        } => json!(text
            .parse::<u64>()
            .map_err(|_| anyhow!("{path}: invalid unsigned integer"))?),
        _ => value,
    })
}

fn encode_scalar(kind: &Kind, text: &str, path: &str) -> Result<(Value, String)> {
    ensure!(!text.is_empty(), "{path}: required value is empty");
    ensure!(
        !text.chars().any(char::is_control),
        "{path}: value contains control characters"
    );
    match kind {
        Kind::Integer { signed, bits } => {
            let value = if *signed {
                let n: i128 = text
                    .parse()
                    .map_err(|_| anyhow!("{path}: invalid signed integer"))?;
                let min = if *bits == 128 {
                    i128::MIN
                } else {
                    -(1_i128 << (*bits - 1))
                };
                let max = if *bits == 128 {
                    i128::MAX
                } else {
                    (1_i128 << (*bits - 1)) - 1
                };
                ensure!(
                    (min..=max).contains(&n),
                    "{path}: integer is outside ABI range"
                );
                n.to_string()
            } else {
                let n: u128 = text
                    .parse()
                    .map_err(|_| anyhow!("{path}: invalid unsigned integer"))?;
                let max = if *bits == 128 {
                    u128::MAX
                } else {
                    (1_u128 << *bits) - 1
                };
                ensure!(n <= max, "{path}: integer is outside ABI range");
                n.to_string()
            };
            Ok((Value::String(value.clone()), value))
        }
        Kind::RequestType => {
            let code = text
                .parse::<u32>()
                .map_err(|_| anyhow!("{path}: invalid request type"))?;
            ensure!(code <= 9, "{path}: request type must be in 0..=9");
            Ok((json!(code), code.to_string()))
        }
        Kind::Address => {
            let valid = text.parse::<PublicAddress>().is_ok();
            ensure!(valid, "{path}: address must be a valid public G/C address");
            Ok((Value::String(text.to_owned()), text.to_owned()))
        }
        Kind::Bool => {
            ensure!(
                text == "true" || text == "false",
                "{path}: expected true or false"
            );
            Ok((Value::Bool(text == "true"), text.to_owned()))
        }
        Kind::Text => Ok((Value::String(text.to_owned()), text.to_owned())),
        Kind::Vector(_) | Kind::Record(_) => {
            bail!("{path}: nested aggregate ABI type is unsupported")
        }
    }
}

fn validate_value(kind: &Kind, value: &str, path: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    encode_scalar(kind, value, path).map(|_| ())
}
fn hint(kind: &Kind) -> String {
    match kind {
        Kind::Address => "named address autocomplete or valid public G/C literal".into(),
        Kind::RequestType => "request type 0..=9 (number or label autocomplete)".into(),
        Kind::Integer { signed, bits } => format!(
            "{}{} decimal integer",
            if *signed { "i" } else { "u" },
            bits
        ),
        Kind::Bool => "true or false".into(),
        Kind::Text => "text".into(),
        Kind::Vector(_) => {
            "Enter to step into rows (creates the first); +/- add/remove rows".into()
        }
        Kind::Record(_) => "typed ABI record".into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const C: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
    const G: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";

    fn manifest() -> MarketManifest {
        serde_json::from_value(json!({
            "network": {"label":"testnet","passphrase":"Test SDF Network ; September 2015"},
            "admin":G,"rpc_host":"rpc.testnet.example",
            "oracle":{"runtime":C,"governance":C,"adapter":C,"sources":[C],"min_sources":1,"max_age_secs":3600,"max_clock_drift_secs":60,"decimals":7,"resolution":300,"base":"USD"},
            "tokens":{"usdc":C},"pool":C,"backstop":C,"factory":C,"comet":C,"emitter":C,
            "reserves":{},"identities":{},"deploy_ledger":1
        })).unwrap()
    }

    fn spec() -> Safe {
        Safe::render_market_json(&json!([
            {"udt_struct_v0":{"name":"Request","fields":[
                {"name":"request_type","type_":"u32"},{"name":"address","type_":"address"},{"name":"amount","type_":"i128"}]}},
            {"udt_struct_v0":{"name":"FlashLoan","fields":[
                {"name":"contract","type_":"address"},{"name":"asset","type_":"address"},{"name":"amount","type_":"i128"}]}}
        ]), 4096).unwrap()
    }

    #[test]
    fn self_referential_struct_field_is_rejected_without_recursing() {
        let spec = Safe::render_market_json(
            &json!([{"udt_struct_v0":{"name":"Recursive","fields":[
                {"name":"next","type_":{"udt":{"name":"Recursive"}}}
            ]}}]),
            4096,
        )
        .unwrap();
        let error = ActionForm::new(
            &[("recursive".into(), json!({"udt":{"name":"Recursive"}}))],
            &[],
            &spec,
            &manifest(),
            "pool",
            "flash-loan",
        )
        .err()
        .expect("self-referential ABI fields must be unsupported");
        assert_eq!(
            error.to_string(),
            "recursive.next: nested aggregate ABI field is unsupported"
        );
    }

    #[test]
    fn exact_wide_integers_and_recipe_locked_request_vector() {
        let max = "170141183460469231731687303715884105727";
        let kind = Kind::Integer {
            signed: true,
            bits: 128,
        };
        assert_eq!(encode_scalar(&kind, max, "amount").unwrap().1, max);
        assert!(encode_scalar(&kind, "170141183460469231731687303715884105728", "amount").is_err());
        let unsigned = Kind::Integer {
            signed: false,
            bits: 128,
        };
        let unsigned_max = u128::MAX.to_string();
        assert_eq!(
            encode_scalar(&unsigned, &unsigned_max, "amount").unwrap().1,
            unsigned_max
        );
        assert!(encode_scalar(&Kind::RequestType, "10", "requests[0].request_type").is_err());
        assert_eq!(
            encode_scalar(&Kind::RequestType, "9", "requests[0].request_type")
                .unwrap()
                .1,
            "9"
        );
        let mut form = ActionForm::new(
            &[(
                "requests".into(),
                json!({"vec":{"element_type":{"udt":{"name":"Request"}}}}),
            )],
            &[],
            &spec(),
            &manifest(),
            "auction",
            "fill-liquidation",
        )
        .unwrap();
        assert!(
            form.set(0, r#"[{"request_type":4}]"#).is_err(),
            "guided vectors reject operator-assembled JSON"
        );
        form.add(0).unwrap();
        form.remove(0).unwrap();
        assert!(
            form.explicit().is_err(),
            "an empty required vector is incomplete"
        );
        form.add(0).unwrap();
        assert!(
            form.is_address(2),
            "Request.address is an editable address leaf"
        );
        assert!(
            !form.is_address(1),
            "the recipe-locked request type is not editable"
        );
        assert!(!form.is_address(3), "the amount leaf is not an address");
        assert!(
            !form.is_request_type(1),
            "recipe-locked request types are not eligible for the editor"
        );
        assert!(
            form.set(1, "5").is_err(),
            "auction request type is fixed at 6"
        );
        form.set(2, C).unwrap();
        form.set(3, max).unwrap();
        let args = form.explicit().unwrap();
        let requests: Value = serde_json::from_str(&args[0].1).unwrap();
        assert_eq!(requests[0]["request_type"], 6);
        assert_eq!(requests[0]["amount"], max);
        assert_eq!(args[0].0, "requests");
    }

    #[test]
    fn request_type_labels_and_numeric_codes_remain_typed_u32_fields() {
        let mut form = ActionForm::new(
            &[(
                "requests".into(),
                json!({"vec":{"element_type":{"udt":{"name":"Request"}}}}),
            )],
            &[],
            &spec(),
            &manifest(),
            "pool",
            "submit",
        )
        .unwrap();
        form.add(0).unwrap();
        assert!(form.is_request_type(1));
        form.set(2, C).unwrap();
        form.set(3, "1").unwrap();

        assert_eq!(
            form.resolve_request_type(1, "Borrow", None).unwrap(),
            Some(4)
        );
        form.set(1, "4").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&form.explicit().unwrap()[0].1).unwrap()[0]
                ["request_type"],
            4
        );
        assert_eq!(form.resolve_request_type(1, "5", None).unwrap(), Some(5));
        form.set(1, "5").unwrap();
        assert_eq!(
            form.resolve_request_type(1, "Supply", Some(2)).unwrap(),
            Some(2)
        );
        assert!(form.resolve_request_type(1, "4", Some(5)).is_err());
        assert!(form.resolve_request_type(1, "Supply", None).is_err());
        assert!(form.resolve_request_type(1, "10", None).is_err());
        assert_eq!(form.resolve_request_type(1, "", None).unwrap(), None);
        assert_eq!(form.edit_value(1).as_deref(), Some("5"));

        let generic_spec = Safe::render_market_json(
            &json!([{"udt_struct_v0":{"name":"Other","fields":[
                {"name":"request_type","type_":"u32"}
            ]}}]),
            4096,
        )
        .unwrap();
        let mut generic = ActionForm::new(
            &[("other".into(), json!({"udt":{"name":"Other"}}))],
            &[],
            &generic_spec,
            &manifest(),
            "pool",
            "submit",
        )
        .unwrap();
        assert!(!generic.is_request_type(1));
        generic.set(1, "10").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&generic.explicit().unwrap()[0].1).unwrap()
                ["request_type"],
            10
        );
    }

    #[test]
    fn flash_loan_udt_requires_valid_address_and_complete_exact_fields() {
        let mut form = ActionForm::new(
            &[("flash_loan".into(), json!({"udt":{"name":"FlashLoan"}}))],
            &[],
            &spec(),
            &manifest(),
            "pool",
            "flash-loan",
        )
        .unwrap();
        assert!(
            form.set(0, r#"{"contract":"C..."}"#).is_err(),
            "flash-loan records reject operator-assembled JSON"
        );
        assert!(
            form.explicit().is_err(),
            "empty required fields are refused"
        );
        assert!(form.is_address(1), "FlashLoan.contract is an address leaf");
        assert!(form.is_address(2), "FlashLoan.asset is an address leaf");
        assert!(
            !form.is_address(0),
            "the FlashLoan record is not itself an address"
        );
        assert!(!form.is_address(3), "FlashLoan.amount is numeric");
        assert!(form.set(1, "Gnot-a-valid-address").is_err());
        form.set(1, C).unwrap();
        form.set(2, C).unwrap();
        form.set(3, "18446744073709551617").unwrap();
        assert!(form.set(3, "1\u{7}2").is_err());
        let args = form.explicit().unwrap();
        let loan: Value = serde_json::from_str(&args[0].1).unwrap();
        assert_eq!(loan["amount"], "18446744073709551617");
        assert_eq!(loan["contract"], C);
    }

    #[test]
    fn flash_loan_with_no_requests_encodes_an_explicit_empty_collection() {
        let mut form = ActionForm::new(
            &[
                ("flash_loan".into(), json!({"udt":{"name":"FlashLoan"}})),
                (
                    "requests".into(),
                    json!({"vec":{"element_type":{"udt":{"name":"Request"}}}}),
                ),
            ],
            &[],
            &spec(),
            &manifest(),
            "pool",
            "flash-loan",
        )
        .unwrap();
        form.set(1, C).unwrap();
        form.set(2, C).unwrap();
        form.set(3, "7").unwrap();
        let args = form.explicit().unwrap();
        let loan: Value = serde_json::from_str(&args[0].1).unwrap();
        let requests: Value = serde_json::from_str(&args[1].1).unwrap();
        assert_eq!(loan, json!({"contract": C, "asset": C, "amount": "7"}));
        assert_eq!(requests, json!([]));
    }

    #[test]
    fn visible_address_choices_keep_semantics_and_encode_only_the_public_id() {
        let mut form = ActionForm::new(
            &[
                ("from".into(), json!("address")),
                ("asset".into(), json!("address")),
            ],
            &[],
            &spec(),
            &manifest(),
            "pool",
            "submit",
        )
        .unwrap();
        form.set_choices(&[
            AddressChoice {
                name: "pool".into(),
                address: C.parse().unwrap(),
            },
            AddressChoice {
                name: "admin".into(),
                address: G.parse().unwrap(),
            },
        ]);
        let cells = form.cells();
        assert!(cells[0].1.starts_with("admin ("));
        form.cycle(1, true).unwrap();
        let selected = form.edit_value(1).unwrap();
        assert!(form.cells()[1].1.contains("pool"));
        assert!(form.cells()[1].1.contains(selected.as_str()));
        let explicit = form.explicit().unwrap();
        assert_eq!(explicit[0].1, G);
        assert_eq!(explicit[1].1, selected);
    }

    #[test]
    fn empty_pool_request_collection_is_a_present_valid_argument() {
        let form = ActionForm::new(
            &[(
                "requests".into(),
                json!({"vec":{"element_type":{"udt":{"name":"Request"}}}}),
            )],
            &[],
            &spec(),
            &manifest(),
            "pool",
            "submit",
        )
        .unwrap();
        let args = form.explicit().unwrap();
        assert_eq!(args[0].0, "requests");
        assert_eq!(
            serde_json::from_str::<Value>(&args[0].1).unwrap(),
            json!([])
        );
    }
    #[test]
    fn price_is_explicit_and_timestamp_is_host_seconds() {
        let manifest = manifest();
        let mut form = ActionForm::new(
            &[
                ("price".into(), json!("i128")),
                ("timestamp".into(), json!("u64")),
            ],
            &[("asset".into(), format!("{{\"Stellar\":\"{}\"}}", C))],
            &spec(),
            &manifest,
            "oracle",
            "publish-price",
        )
        .unwrap();
        let cells = form.cells();
        assert!(cells[0].2.contains("7 decimals"));
        assert!(cells[1].2.contains("host Unix seconds"));
        assert!(!cells[1].1.is_empty());
        assert!(
            form.explicit().is_err(),
            "price cannot be silently defaulted"
        );
        form.set(0, "1234567").unwrap();
        let explicit = form.explicit().unwrap();
        assert_eq!(explicit.len(), 2);
        assert_eq!(explicit[0], ("price".into(), "1234567".into()));
        assert_eq!(explicit[1].0, "timestamp");
    }
    #[test]
    fn scalar_and_address_vectors_emit_typed_json_rows() {
        let mut form = ActionForm::new(
            &[
                ("ids".into(), json!({"vec":{"element_type":"u32"}})),
                ("actors".into(), json!({"vec":{"element_type":"address"}})),
            ],
            &[],
            &spec(),
            &manifest(),
            "pool",
            "claim",
        )
        .unwrap();
        form.add(0).unwrap();
        form.add(2).unwrap();
        assert!(!form.is_address(1), "u32 vector row is not an address");
        assert!(
            !form.is_address(2),
            "address vector aggregate is not an address"
        );
        assert!(
            form.is_address(3),
            "vector<address> leaf is an editable address"
        );
        form.set(1, "7").unwrap();
        form.set(3, C).unwrap();
        let args = form.explicit().unwrap();
        let ids: Value = serde_json::from_str(&args[0].1).unwrap();
        let actors: Value = serde_json::from_str(&args[1].1).unwrap();
        assert_eq!(ids, json!([7]));
        assert_eq!(actors, json!([C]));
    }

    #[test]
    fn general_request_rows_accept_supply_borrow_and_repay_codes() {
        let mut form = ActionForm::new(
            &[(
                "requests".into(),
                json!({"vec":{"element_type":{"udt":{"name":"Request"}}}}),
            )],
            &[],
            &spec(),
            &manifest(),
            "pool",
            "submit",
        )
        .unwrap();
        assert!(
            form.edit_value(0).is_none(),
            "a collection is not a scalar editor"
        );
        assert_eq!(form.enter(0).unwrap(), Some(1));
        for _ in 0..2 {
            form.add(0).unwrap();
        }
        for (row, code) in [(0, 4), (1, 2), (2, 5)] {
            let start = 1 + row * 3;
            form.set(start, &code.to_string()).unwrap();
            form.set(start + 1, C).unwrap();
            form.set(start + 2, "9007199254740993").unwrap();
        }
        assert_eq!(form.enter(0).unwrap(), Some(1));
        let requests: Value = serde_json::from_str(&form.explicit().unwrap()[0].1).unwrap();
        assert_eq!(
            requests,
            json!([
                {"request_type":4,"address":C,"amount":"9007199254740993"},
                {"request_type":2,"address":C,"amount":"9007199254740993"},
                {"request_type":5,"address":C,"amount":"9007199254740993"}
            ])
        );
        for _ in 0..3 {
            form.remove(0).unwrap();
        }
        assert_eq!(form.explicit().unwrap()[0].1, "[]");
        assert_eq!(form.enter(0).unwrap(), Some(1));
        assert!(
            form.explicit().is_err(),
            "new rows must not reuse removed request values"
        );
    }

    #[test]
    fn text_is_not_json_input_and_control_bytes_are_refused() {
        let mut form = ActionForm::new(
            &[("memo".into(), json!("string"))],
            &[],
            &spec(),
            &manifest(),
            "token",
            "transfer",
        )
        .unwrap();
        assert!(form.set(0, "bad\u{1b}text").is_err());
        form.set(0, "ordinary text with spaces").unwrap();
        assert_eq!(form.explicit().unwrap()[0].1, "ordinary text with spaces");
    }

    #[test]
    fn emissions_metadata_defaults_to_an_empty_vector_and_edits_typed_rows() {
        // The fork's ReserveEmissionMetadata is three scalar ABI fields
        // (`pool/src/emissions/manager.rs`), so the generic vector and record
        // controls already cover it: an empty list is valid native input for
        // the contract's own guard, and rows are edited field by field.
        let spec = Safe::render_market_json(
            &json!([{"udt_struct_v0": {"name": "ReserveEmissionMetadata", "fields": [
                {"name": "res_index", "type_": "u32"},
                {"name": "res_type", "type_": "u32"},
                {"name": "share", "type_": "u64"}
            ]}}]),
            4096,
        )
        .unwrap();
        let inputs = [(
            "res_emission_metadata".to_string(),
            json!({"vec": {"element_type": {"udt": {"name": "ReserveEmissionMetadata"}}}}),
        )];
        let mut form = ActionForm::new(
            &inputs,
            &[],
            &spec,
            &manifest(),
            "pool",
            "set-emissions-config",
        )
        .unwrap();
        assert_eq!(
            form.explicit().unwrap(),
            vec![("res_emission_metadata".to_string(), "[]".to_string())],
            "an empty emission list is a complete call, not a local refusal"
        );
        assert_eq!(form.enter(0).unwrap(), Some(1), "Enter opens the first row");
        form.add(0).unwrap();
        for (cell, value) in [
            (1, "0"),
            (2, "1"),
            (3, "3500000"),
            (4, "3"),
            (5, "0"),
            (6, "6500000"),
        ] {
            form.set(cell, value).unwrap();
        }
        assert!(!form.is_address(1), "reserve index is a plain u32 leaf");
        assert!(
            !form.is_request_type(2),
            "reserve type is not the request-type dropdown"
        );
        assert!(
            form.set(1, "4294967296").is_err(),
            "u32 leaves keep their ABI range"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&form.explicit().unwrap()[0].1).unwrap(),
            json!([
                {"res_index": 0, "res_type": 1, "share": 3500000},
                {"res_index": 3, "res_type": 0, "share": 6500000}
            ])
        );
    }
}
