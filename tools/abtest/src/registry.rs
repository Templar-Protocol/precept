//! The precept deployment-registry step of `precept-abtest market`.
//!
//! A completed market deployment is recorded in the precept registry when a
//! precept checkout is available: the run's `contract` rows come from its
//! published manifest, its `wasm` rows only from frozen release provenance.
//! Recording is additive — a missing checkout skips with one line, and no
//! failure here touches the finished deployment. The run's complete rows land
//! in `registry-records.jsonl` before the registry is read; `add` is handed
//! the filtered `registry-add.jsonl`. A failure before that first write names
//! no file to re-`add`, because none exists yet; any later one names it.

use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde_json::{json, Map, Value};

use crate::deploy::{read_deployment_plan, DeploymentPlan};
use crate::manifest::{MarketManifest, Tok};
use crate::release::CONTRACTS_REPO;
use crate::{ContractId, Safe};

/// The artifact name the registry files each deployed role under.
const REGISTRY_ARTIFACT: [(&str, &str); 10] = [
    ("adapter", "pool_oracle_adapter"),
    ("backstop", "backstop"),
    ("comet", "comet"),
    ("emitter", "emitter"),
    ("factory", "pool_factory"),
    ("pool", "pool"),
    ("proxy-governance", "soroban-proxy-governance"),
    ("proxy-runtime", "soroban-proxy-runtime"),
    ("source", "soroban-price-source"),
    ("token", "token"),
];

/// Where to record: `--registry`, else `PRECEPT_REGISTRY`, else the precept
/// checkout this tool runs in. Returns (checkout or None, one log line); an
/// unusable path is a skip, never a failure.
pub(crate) fn locate(
    flag: Option<&Path>,
    env: Option<std::ffi::OsString>,
    own: &Path,
) -> (Option<PathBuf>, String) {
    let (origin, chosen) = match flag {
        Some(path) => ("--registry", path.to_path_buf()),
        None => match env {
            Some(value) => ("PRECEPT_REGISTRY", PathBuf::from(value)),
            None => ("this checkout", own.to_path_buf()),
        },
    };
    if chosen.join("registry").join("registry.py").is_file() {
        (
            Some(chosen.clone()),
            format!("registry: recording into {} ({origin})", chosen.display()),
        )
    } else {
        (
            None,
            format!(
                "registry: skipped; {origin} {} has no registry/registry.py",
                chosen.display()
            ),
        )
    }
}

/// The operator's own command for rows the registry does not hold yet. The
/// tool never invents a registry row; it names the command that adds the
/// rows this run actually produced.
fn add_command(script: &Path, network: &str, file: &Path) -> String {
    format!(
        "python3 {} add --network {network} {}",
        script.display(),
        file.display()
    )
}

/// The precept checkout this tool lives in: the nearest ancestor of the
/// working directory holding `tools/abtest/Cargo.toml`. Outside a checkout
/// nothing is found and the run is skipped.
fn own_checkout() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.ancestors()
        .find(|dir| dir.join("tools/abtest/Cargo.toml").is_file())
        .map_or(cwd.clone(), Path::to_path_buf)
}

/// The date in the run name (`market-live-20260930`), else today, so a
/// re-run of `add` reproduces the same rows.
pub(crate) fn deployment_date(name: &str, today: &str) -> String {
    let bytes = name.as_bytes();
    for start in 0..bytes.len().saturating_sub(7) {
        let bounded = (start == 0 || !bytes[start - 1].is_ascii_digit())
            && (start + 8 == bytes.len() || !bytes[start + 8].is_ascii_digit());
        if !bounded || !bytes[start..start + 8].iter().all(u8::is_ascii_digit) {
            continue;
        }
        let digits = &name[start..start + 8];
        let month: u32 = digits[4..6].parse().unwrap();
        let day: u32 = digits[6..8].parse().unwrap();
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            return format!("{}-{month:02}-{day:02}", &digits[0..4]);
        }
    }
    today.to_string()
}

fn today() -> String {
    let days = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / 86_400,
    )
    .unwrap_or(0);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[allow(clippy::too_many_arguments)]
fn contract(
    network: &str,
    id: &str,
    role: &str,
    deployment: &str,
    when: &str,
    wasm_hash: Option<&str>,
    refs: Value,
    admin: Option<&str>,
    label: Option<&str>,
    assets: Option<Vec<String>>,
) -> Value {
    let mut row = Map::new();
    row.insert("kind".into(), json!("contract"));
    row.insert("network".into(), json!(network));
    row.insert("id".into(), json!(id));
    row.insert("role".into(), json!(role));
    row.insert("deployment".into(), json!(deployment));
    row.insert("status".into(), json!("active"));
    row.insert(
        "wasm_hash".into(),
        wasm_hash.map_or(Value::Null, |hash| json!(hash)),
    );
    row.insert("refs".into(), refs);
    row.insert("deployed_at".into(), json!(when));
    if let Some(admin) = admin {
        row.insert("admin".into(), json!(admin));
    }
    if let Some(label) = label {
        row.insert("label".into(), json!(label));
    }
    if let Some(assets) = assets {
        row.insert("assets".into(), json!(assets));
    }
    Value::Object(row)
}

fn wasm(network: &str, artifact: &str, hash: &str, version: &str, source: Value) -> Value {
    let mut row = Map::new();
    row.insert("kind".into(), json!("wasm"));
    row.insert("network".into(), json!(network));
    row.insert("hash".into(), json!(hash));
    row.insert("artifact".into(), json!(artifact));
    row.insert("version".into(), json!(version));
    row.insert("source".into(), source);
    Value::Object(row)
}

/// The price-source contracts this run deployed, in instance order: every
/// Lazer source, and nothing else. A reserve may quote an existing SEP-40
/// oracle — the mainnet rehearsal names RedStone's contract — which this run
/// neither deployed nor holds the code of; claiming it would file a
/// third-party contract under our deployment with our digest, so it gets no
/// row.
///
/// The record is `real.lazer_sources`. A manifest written before that field
/// existed carries only the per-reserve source identities, so its first
/// reserve's mirror is read positionally against the same list: `lazer:<feed>`
/// entries are this run's instances and `sep40:<contract>` are someone
/// else's. A real manifest with neither — no list and no proxies — names no
/// source this tool can prove it deployed, and recording none is the honest
/// answer.
fn deployed_lazer_sources(manifest: &MarketManifest) -> Vec<ContractId> {
    let Some(real) = manifest.real.as_ref() else {
        return Vec::new();
    };
    if !real.lazer_sources.is_empty() {
        return real.lazer_sources.clone();
    }
    let Some(mirror) = real.proxies.first() else {
        return Vec::new();
    };
    manifest
        .oracle
        .sources
        .iter()
        .zip(mirror.sources.iter())
        .filter(|(_, identity)| is_lazer_identity(identity))
        .map(|(contract, _)| contract.clone())
        .collect()
}

/// Whether one mirrored source identity (`{"source":"lazer:<feed id>"}` or
/// `{"source":"sep40:<contract id>"}`) is a Lazer instance this run deployed.
fn is_lazer_identity(identity: &Safe) -> bool {
    serde_json::from_str::<Value>(&identity.to_string())
        .ok()
        .and_then(|value| value.get("source")?.as_str().map(str::to_string))
        .is_some_and(|source| source.starts_with("lazer:"))
}

/// The registry lines a completed market run is known to: one `contract`
/// per contract this tool deployed (the faucet is deployed out of band and
/// is not ours to name) and a `wasm` only for code whose release provenance
/// was frozen — a path pin carries no source we could record without
/// fabricating one, and the registry's curated row for those bytes stands.
pub(crate) fn market_records(
    manifest: &MarketManifest,
    plan: &DeploymentPlan,
    deployment: &str,
    when: &str,
) -> anyhow::Result<Vec<Value>> {
    let digest = |role: &str| -> anyhow::Result<&str> {
        plan.artifacts
            .get(role)
            .map(|hash| hash.as_str())
            .with_context(|| format!("the frozen plan has no artifact for {role}"))
    };
    let token = |who: Tok| -> anyhow::Result<&str> {
        manifest
            .tokens
            .get(&who)
            .map(|id| id.as_str())
            .with_context(|| format!("the manifest has no {} token", who.as_str()))
    };
    let network = manifest.network.label.to_string();
    let runtime = manifest.oracle.runtime.as_str();
    let real = manifest.real.as_ref();
    // A real market's synthetic legs are classic-asset contracts, not code
    // of ours; the lab's are the mock token.
    let (blnd, usdc) = match real {
        Some(real) => (real.blnd.sac.as_str(), real.usdc.sac.as_str()),
        None => (token(Tok::Blnd)?, token(Tok::Stable)?),
    };
    let mut rows = Vec::new();
    if let Some(real) = real {
        for (leg, label) in [(&real.blnd, "shim-blnd"), (&real.usdc, "shim-usdc")] {
            rows.push(contract(
                &network,
                leg.sac.as_str(),
                "token",
                deployment,
                when,
                None,
                json!({}),
                None,
                Some(label),
                None,
            ));
        }
    }
    for (who, id) in &manifest.tokens {
        rows.push(contract(
            &network,
            id.as_str(),
            "token",
            deployment,
            when,
            Some(digest("token")?),
            json!({}),
            None,
            Some(who.as_str()),
            None,
        ));
    }
    rows.push(contract(
        &network,
        manifest.comet.as_str(),
        "comet",
        deployment,
        when,
        Some(digest("comet")?),
        json!({}),
        None,
        None,
        None,
    ));
    rows.push(contract(
        &network,
        manifest.emitter.as_str(),
        "emitter",
        deployment,
        when,
        Some(digest("emitter")?),
        json!({}),
        None,
        None,
        None,
    ));
    rows.push(contract(
        &network,
        manifest.backstop.as_str(),
        "backstop",
        deployment,
        when,
        Some(digest("backstop")?),
        json!({
            "blnd_token": blnd,
            "comet": manifest.comet.as_str(),
            "emitter": manifest.emitter.as_str(),
            "pool_factory": manifest.factory.as_str(),
            "usdc_token": usdc,
        }),
        None,
        None,
        None,
    ));
    let assets: Vec<String> = match real {
        Some(real) => real
            .reserves
            .iter()
            .map(|r| r.asset.as_str().to_string())
            .collect(),
        None => Tok::RESERVES
            .iter()
            .map(|who| token(*who).map(str::to_string))
            .collect::<anyhow::Result<_>>()?,
    };
    rows.push(contract(
        &network,
        manifest.pool.as_str(),
        "pool",
        deployment,
        when,
        Some(digest("pool")?),
        json!({
            "backstop": manifest.backstop.as_str(),
            "pool_factory": manifest.factory.as_str(),
            "pool_oracle_adapter": manifest.oracle.adapter.as_str(),
        }),
        Some(manifest.admin.as_str()),
        None,
        Some(assets),
    ));
    rows.push(contract(
        &network,
        manifest.factory.as_str(),
        "pool_factory",
        deployment,
        when,
        Some(digest("factory")?),
        json!({}),
        None,
        None,
        None,
    ));
    rows.push(contract(
        &network,
        manifest.oracle.adapter.as_str(),
        "pool_oracle_adapter",
        deployment,
        when,
        Some(digest("adapter")?),
        json!({"proxy_runtime": runtime}),
        None,
        None,
        None,
    ));
    // Only contracts this run deployed get rows. A real run's are its Lazer
    // instances, filed as `lazer_source` under the code it uploaded; an
    // existing SEP-40 oracle it merely quotes is neither filed nor ref'd. The
    // lab's three mock sources are its own deployments, so they keep their
    // rows.
    if real.is_some() {
        let lazer = digest("lazer-source")?;
        for source in deployed_lazer_sources(manifest) {
            rows.push(contract(
                &network,
                source.as_str(),
                "lazer_source",
                deployment,
                when,
                Some(lazer),
                json!({"proxy_runtime": runtime}),
                None,
                None,
                None,
            ));
        }
    } else {
        let source = digest("source")?;
        for id in &manifest.oracle.sources {
            rows.push(contract(
                &network,
                id.as_str(),
                "price_source",
                deployment,
                when,
                Some(source),
                json!({"proxy_runtime": runtime}),
                None,
                None,
                None,
            ));
        }
    }
    // A run that reused a runtime deployed neither it nor its governance:
    // their rows belong to the deployment that did.
    if plan.artifacts.contains_key("proxy-runtime") {
        if let Some(governance) = &manifest.oracle.governance {
            rows.push(contract(
                &network,
                governance.as_str(),
                "proxy_governance",
                deployment,
                when,
                Some(digest("proxy-governance")?),
                json!({"proxy_runtime": runtime}),
                Some(manifest.admin.as_str()),
                None,
                None,
            ));
        }
        rows.push(contract(
            &network,
            runtime,
            "proxy_runtime",
            deployment,
            when,
            Some(digest("proxy-runtime")?),
            manifest.oracle.governance.as_ref().map_or(
                json!({}),
                |governance| json!({"proxy_governance": governance.as_str()}),
            ),
            None,
            None,
            None,
        ));
    }
    for (role, provenance) in &plan.releases {
        let artifact = REGISTRY_ARTIFACT
            .iter()
            .find(|(named, _)| named == role)
            .map(|(_, artifact)| *artifact)
            .with_context(|| format!("no registry artifact name for role {role}"))?;
        rows.push(wasm(
            &network,
            artifact,
            provenance.sha256.as_str(),
            &provenance.version,
            json!({"repo": CONTRACTS_REPO, "tag": provenance.tag, "asset": provenance.asset}),
        ));
    }
    if real.is_some() {
        // The launch profile's digest pins carry their curated provenance;
        // release-pinned roles were recorded above from what the run froze.
        for entry in &crate::manifest::MARKET_LAUNCH_PROFILE {
            if plan.releases.contains_key(entry.role) || !plan.artifacts.contains_key(entry.role) {
                continue;
            }
            let (artifact, version, source) = entry.registry;
            rows.push(wasm(
                &network,
                artifact,
                entry.wasm_hash,
                version,
                serde_json::from_str(source).context("a launch provenance is not JSON")?,
            ));
        }
    }
    Ok(rows)
}

/// The registry's own table order: contracts by (deployment, role, id),
/// then code by (artifact, hash).
fn sort_key(row: &Value) -> (String, String, String, String) {
    let read = |keys: [&str; 3]| {
        keys.map(|key| {
            row.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
    };
    let [deployment, role, id] = read(["deployment", "role", "id"]);
    if row.get("kind").and_then(Value::as_str) == Some("contract") {
        ("contract".to_string(), deployment, role, id)
    } else {
        let [artifact, hash, _] = read(["artifact", "hash", ""]);
        ("wasm".to_string(), artifact, hash, String::new())
    }
}
fn registry_run(script: &Path, arguments: &[&str], robot: bool) -> anyhow::Result<()> {
    let done = Command::new("python3")
        .arg(script)
        .args(arguments)
        .output()
        .with_context(|| format!("run {}", script.display()))?;
    if done.status.code() != Some(0) {
        bail!(
            "registry {} failed: {}",
            arguments[0],
            String::from_utf8_lossy(&done.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&done.stdout).trim().to_string();
    if !stdout.is_empty() {
        // The registry script's own report (`mainnet: 14 added …`) is a human
        // line: in robot mode it belongs on stderr with the rest of them.
        if robot {
            eprintln!("{stdout}");
        } else {
            println!("{stdout}");
        }
    }
    Ok(())
}

fn identity(row: &Value) -> Option<(String, String)> {
    let kind = row.get("kind")?.as_str()?.to_string();
    let value = row
        .get(if kind == "contract" { "id" } else { "hash" })?
        .as_str()?
        .to_string();
    Some((kind, value))
}

/// What the registry table already describes, as (kind, id-or-hash).
fn registered_identities(table: &Path) -> anyhow::Result<HashSet<(String, String)>> {
    let text =
        std::fs::read_to_string(table).with_context(|| format!("read {}", table.display()))?;
    // The registry's own reader splits on "\n" only; so does this.
    text.split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let row: Value =
                serde_json::from_str(line).context("the registry table is malformed")?;
            identity(&row).context("the registry table has a row without kind and id or hash")
        })
        .collect()
}

/// Atomic: a reader never sees half a file, and a failed write leaves any
/// earlier file untouched.
fn write_records(path: &Path, rows: &[&Value]) -> anyhow::Result<()> {
    let mut body = String::new();
    for row in rows {
        writeln!(body, "{}", serde_json::to_string(row)?)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, body)
        .and_then(|()| std::fs::rename(&temporary, path))
        .with_context(|| format!("write {}", path.display()))
}

/// Write the run's complete records next to its evidence — from the run dir
/// alone, before the registry table is read — then `add` the filtered
/// `registry-add.jsonl` and retire the deployment it replaces. A failure
/// before that first write names no file, because none exists; every later
/// one names the file it was working with. A refused retire is reported,
/// never fatal, because the new deployment is already recorded.
pub(crate) fn record(
    checkout: &Path,
    rows: &[Value],
    out: &Path,
    replaced: Option<&str>,
    network: &str,
    robot: bool,
) -> anyhow::Result<()> {
    // A robot run owns stdout: every line it prints there is one JSON event, so
    // the registry's human report, its retire notice and the pull-request
    // instructions go to stderr.
    let say = |line: String| {
        if robot {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    let name = out
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().to_string());
    let script = checkout.join("registry").join("registry.py");
    let table = checkout.join("registry").join(format!("{network}.jsonl"));
    // The complete run record goes to disk before the table is touched, so a
    // table that cannot be read still leaves a file the operator can re-`add`.
    let records = out.join("registry-records.jsonl");
    let mut all: Vec<&Value> = rows.iter().collect();
    all.sort_by_key(|row| sort_key(row));
    write_records(&records, &all)?;
    let records = records.display().to_string();
    let rerun = |file: &str| {
        format!(
            "records are in {records}; fix and re-run: python3 {} add --network {network} {file}",
            script.display()
        )
    };
    let registered = registered_identities(&table).with_context(|| rerun(&records))?;
    // A code or contract the registry already describes keeps its own row;
    // `add` gets only the rest, in a file of its own, so the complete record
    // above stays exactly what the run described.
    let fresh: Vec<&Value> = all
        .iter()
        .copied()
        .filter(|row| identity(row).is_some_and(|key| !registered.contains(&key)))
        .collect();
    let add = out.join("registry-add.jsonl");
    if fresh.is_empty() {
        // A stale file from an earlier attempt must not outlive this verdict.
        let _ = std::fs::remove_file(&add);
        say(format!("registry: {name} is already recorded"));
        return Ok(());
    }
    write_records(&add, &fresh).with_context(|| rerun(&records))?;
    let add = add.display().to_string();
    registry_run(&script, &["add", "--network", network, &add], robot)
        .with_context(|| rerun(&add))?;
    if let Some(replaced) = replaced {
        if let Err(error) = registry_run(
            &script,
            &["retire", "--network", network, "--deployment", replaced],
            robot,
        ) {
            say(format!(
                "registry: retire {replaced} refused: {error}\n\
                 registry: leave it active or retire individual contracts by hand"
            ));
        }
    }
    let checkout = checkout.display();
    let branch = format!("registry/{name}");
    say(format!(
        "registry updated, not committed. To PR it:\n  \
         git -C {checkout} fetch origin\n  \
         git -C {checkout} switch -c {branch} origin/main\n  \
         git -C {checkout} add registry/{network}.jsonl\n  \
         git -C {checkout} commit -m 'registry: record {name}'\n  \
         git -C {checkout} push -u origin {branch}\n  \
         gh pr create --repo Templar-Protocol/precept --base main --head {branch} \
         --title 'registry: record {name}'"
    ));
    Ok(())
}

/// The registry step of a COMPLETED market deployment: write the run's
/// complete `registry-records.jsonl` and its filtered `registry-add.jsonl`
/// into the run dir, `add` the latter, and retire what the run replaced.
/// Nothing here undoes a finished deployment. Any failure exits non-zero;
/// one after the first write names the files, so the operator can re-`add`.
pub(crate) fn record_completed_market(
    registry: Option<&Path>,
    no_registry: bool,
    out: &Path,
    robot: bool,
) -> i32 {
    // A robot run owns stdout: its notices go to stderr so the process's
    // standard output stays the JSON event stream. Nothing else differs.
    let announce = |line: String| {
        if robot {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    // The frozen manifest comes first: a run that never published has no
    // record to make, and that is a failure whatever else was asked for. The
    // notices below keep their order — `--no-registry` still answers as a
    // skip, and a checkout that cannot be found still names the command that
    // would add the rows.
    let manifest = match MarketManifest::load(out) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!(
                "registry: recording failed; the deployment is complete and \
                 unchanged: {error:#}"
            );
            return 1;
        }
    };
    // The frozen manifest, not the command line, names the network every
    // row and every registry.py call carries.
    let network = manifest.network.label.to_string();
    let (checkout, notice) = if no_registry {
        (None, "registry: skipped; --no-registry".to_string())
    } else {
        locate(registry, std::env::var_os("PRECEPT_REGISTRY"), &own_checkout())
    };
    announce(notice);
    let Some(checkout) = checkout else {
        // Rows that are not in the registry stay the operator's to add;
        // the tool names the command, it never invents the row.
        let records = out.join("registry-records.jsonl");
        if records.is_file() {
            announce(format!(
                "registry: {} is not recorded; run {}",
                out.display(),
                add_command(
                    &own_checkout().join("registry").join("registry.py"),
                    &network,
                    &records
                )
            ));
        }
        return 0;
    };
    let outcome = (|| -> anyhow::Result<()> {
        let Some(plan) = read_deployment_plan(out)? else {
            announce(format!(
                "registry: skipped; {} has no frozen deployment plan",
                out.display()
            ));
            return Ok(());
        };
        let name = out
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let rows = market_records(&manifest, &plan, &name, &deployment_date(&name, &today()))?;
        record(&checkout, &rows, out, None, &network, robot)
    })();
    match outcome {
        Ok(()) => 0,
        Err(error) => {
            eprintln!(
                "registry: recording failed; the deployment is complete and \
                 unchanged: {error:#}"
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::Provenance;

    fn canon(row: &Value) -> String {
        serde_json::to_string(row).expect("canonical json")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abtest-registry-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn checkout_with_table(dir: &Path, table: &[Value]) -> PathBuf {
        let checkout = dir.join("precept");
        std::fs::create_dir_all(checkout.join("registry")).expect("registry dir");
        // A registry.py that records its argv; .exit/.err steer the outcome.
        std::fs::write(
            checkout.join("registry").join("registry.py"),
            "import json, os, sys\n\
             here = os.path.dirname(os.path.abspath(__file__))\n\
             with open(os.path.join(here, '.calls'), 'a') as calls:\n\
             \x20   calls.write(json.dumps(sys.argv[1:]) + '\\n')\n\
             code = int(open(os.path.join(here, '.exit')).read()) \\
                 if os.path.exists(os.path.join(here, '.exit')) else 0\n\
             if code:\n\
             \x20   print(open(os.path.join(here, '.err')).read(), file=sys.stderr)\n\
             sys.exit(code)\n",
        )
        .expect("stub registry");
        std::fs::write(
            checkout.join("registry").join("testnet.jsonl"),
            table
                .iter()
                .map(|row| canon(row) + "\n")
                .collect::<String>(),
        )
        .expect("table");
        checkout
    }

    fn calls(checkout: &Path) -> Vec<Vec<String>> {
        let path = checkout.join("registry").join(".calls");
        match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).expect("recorded argv"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn flag_beats_environment_and_this_checkout() {
        let dir = temp_dir("flag");
        let named = dir.join("flag");
        let env = dir.join("env");
        let sibling = dir.join("sibling");
        for checkout in [&named, &env, &sibling] {
            std::fs::create_dir_all(checkout.join("registry")).unwrap();
            std::fs::write(checkout.join("registry").join("registry.py"), "").unwrap();
        }
        let (found, notice) = locate(Some(&named), Some(env.clone().into()), &sibling);
        assert_eq!(found.as_deref(), Some(named.as_path()));
        assert!(notice.contains("--registry"), "{notice}");
        let (found, _) = locate(None, Some(env.clone().into()), &sibling);
        assert_eq!(found.as_deref(), Some(env.as_path()));
        let (found, notice) = locate(None, None, &sibling);
        assert_eq!(found.as_deref(), Some(sibling.as_path()));
        assert!(notice.contains("this checkout"), "{notice}");
        let (found, notice) = locate(None, None, &dir.join("nowhere"));
        assert_eq!(found, None);
        assert!(notice.contains("skipped"), "{notice}");
        assert!(!notice.contains('\n'), "one line");
    }

    #[test]
    fn deployment_date_comes_from_the_run_name() {
        assert_eq!(
            deployment_date("market-live-20260930", "2026-10-25"),
            "2026-09-30"
        );
        assert_eq!(
            deployment_date("rc-xlmusdc-lazer-20261025T154229Z", "2026-10-25"),
            "2026-10-25"
        );
        assert_eq!(
            deployment_date("market-live-nodate", "2026-10-25"),
            "2026-10-25"
        );
        assert_eq!(
            deployment_date("market-live-20261340", "2026-10-25"),
            "2026-10-25"
        );
        let stamp = today();
        assert!(
            stamp.len() == 10 && stamp[0..4].parse::<i64>().is_ok(),
            "{stamp}"
        );
    }

    #[test]
    fn record_writes_canonical_rows_then_adds_and_retires() {
        let dir = temp_dir("record");
        let known = contract(
            "testnet",
            "CSEED",
            "pool",
            "market-live-20260930",
            "2026-09-30",
            Some("aa"),
            json!({}),
            None,
            None,
            None,
        );
        let checkout = checkout_with_table(
            &dir,
            &[
                known.clone(),
                wasm("testnet", "pool", "bb", "v0.1.0", json!({"note": "already described"})),
            ],
        );
        let out = dir.join("market-live-20261027");
        std::fs::create_dir_all(&out).unwrap();
        let rows = vec![
            known.clone(), // the registry's own row: dropped, its description stands
            contract(
                "testnet",
                "CNEW",
                "backstop",
                "market-live-20261027",
                "2026-10-27",
                Some("cc"),
                json!({"pool": "CSEED"}),
                None,
                None,
                None,
            ),
            wasm("testnet", "pool", "bb", "unreleased", json!({"note": "ours"})), // shared code: dropped
            wasm(
                "testnet",
                "backstop",
                "cc",
                "v0.2.0",
                json!({"repo": "r", "tag": "v0.2.0", "asset": "b.wasm"}),
            ),
        ];
        record(
            &checkout,
            &rows,
            &out,
            Some("market-live-20260930"),
            "testnet",
            false,
        )
        .expect("record");
        let read = |name: &str| -> Vec<Value> {
            std::fs::read_to_string(out.join(name))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).expect("row"))
                .collect()
        };
        // The complete record is exactly what the run described, in the
        // registry's own order: contracts by deployment, then code by artifact.
        assert_eq!(
            read("registry-records.jsonl"),
            vec![
                known,
                contract(
                    "testnet",
                    "CNEW",
                    "backstop",
                    "market-live-20261027",
                    "2026-10-27",
                    Some("cc"),
                    json!({"pool": "CSEED"}),
                    None,
                    None,
                    None
                ),
                wasm(
                    "testnet",
                    "backstop",
                    "cc",
                    "v0.2.0",
                    json!({"repo": "r", "tag": "v0.2.0", "asset": "b.wasm"})
                ),
                wasm("testnet", "pool", "bb", "unreleased", json!({"note": "ours"})),
            ]
        );
        // `add` is handed a separate file with only the rows the registry
        // does not already describe.
        assert_eq!(
            read("registry-add.jsonl"),
            vec![
                contract(
                    "testnet",
                    "CNEW",
                    "backstop",
                    "market-live-20261027",
                    "2026-10-27",
                    Some("cc"),
                    json!({"pool": "CSEED"}),
                    None,
                    None,
                    None
                ),
                wasm(
                    "testnet",
                    "backstop",
                    "cc",
                    "v0.2.0",
                    json!({"repo": "r", "tag": "v0.2.0", "asset": "b.wasm"})
                ),
            ]
        );
        let asked = calls(&checkout);
        assert_eq!(asked.len(), 2);
        assert_eq!(&asked[0][..3], ["add", "--network", "testnet"]);
        assert!(asked[0][3].ends_with("registry-add.jsonl"));
        assert_eq!(
            asked[1],
            [
                "retire",
                "--network",
                "testnet",
                "--deployment",
                "market-live-20260930"
            ]
        );
    }

    #[test]
    fn a_refused_add_names_the_records_file() {
        let dir = temp_dir("refused");
        let checkout = checkout_with_table(&dir, &[]);
        std::fs::write(checkout.join("registry").join(".exit"), "1").unwrap();
        std::fs::write(
            checkout.join("registry").join(".err"),
            "a contract with this id is already registered; use --update\n",
        )
        .unwrap();
        let out = dir.join("market-live-20261027");
        std::fs::create_dir_all(&out).unwrap();
        let error = record(
            &checkout,
            &[contract(
                "testnet",
                "CNEW",
                "pool",
                "market-live-20261027",
                "2026-10-27",
                Some("aa"),
                json!({}),
                None,
                None,
                None,
            )],
            &out,
            None,
            "testnet",
            false,
        )
        .expect_err("the add must fail");
        // The operator needs all three: the registry's own reason, the
        // complete record, and the file that re-`add`ing means.
        let text = format!("{error:#}");
        assert!(text.contains("use --update"), "{text}");
        assert!(text.contains("registry-records.jsonl"), "{text}");
        assert!(text.contains("registry-add.jsonl"), "{text}");
        assert!(out.join("registry-records.jsonl").is_file());
        assert!(out.join("registry-add.jsonl").is_file());
    }

    #[test]
    fn an_unreadable_table_still_leaves_the_records_file() {
        let dir = temp_dir("unreadable");
        let checkout = checkout_with_table(&dir, &[]);
        // The table cannot be read: not missing (that would be a skip at
        // locate time) but present and unparseable as text.
        let table = checkout.join("registry").join("testnet.jsonl");
        std::fs::remove_file(&table).unwrap();
        std::fs::create_dir(&table).unwrap();
        let out = dir.join("market-live-20261027");
        std::fs::create_dir_all(&out).unwrap();
        let rows = [contract(
            "testnet",
            "CNEW",
            "pool",
            "market-live-20261027",
            "2026-10-27",
            Some("aa"),
            json!({}),
            None,
            None,
            None,
        )];
        let error = record(&checkout, &rows, &out, None, "testnet", false)
            .expect_err("the table must fail");
        // Every failure of this step points at a file that exists, and the
        // file belonging to the step that never ran is not there.
        let text = format!("{error:#}");
        assert!(text.contains("registry-records.jsonl"), "{text}");
        assert!(text.contains("add --network testnet"), "{text}");
        let written = std::fs::read_to_string(out.join("registry-records.jsonl")).unwrap();
        assert_eq!(written, format!("{}\n", canon(&rows[0])));
        assert!(!out.join("registry-add.jsonl").exists());
        assert!(calls(&checkout).is_empty(), "nothing may be added");
    }

    /// The completed market run the round-trip proof is made against.
    const MARKET_RUN: &str = "market-live-20260930";

    /// Its frozen inputs, from the evidence tree named by
    /// `PRECEPT_EVIDENCE_DIR`. The one test that needs them is `#[ignore]`d,
    /// so a machine without the tree never reports a false green;
    /// `cargo test -- --ignored` runs it where it exists.
    fn evidence_run() -> Option<(PathBuf, MarketManifest, DeploymentPlan)> {
        let dir = PathBuf::from(std::env::var_os("PRECEPT_EVIDENCE_DIR")?).join(MARKET_RUN);
        if !dir.join("market-manifest.toml").is_file() {
            return None;
        }
        let manifest = MarketManifest::load(&dir).expect("the evidence manifest parses");
        let plan = read_deployment_plan(&dir)
            .expect("the evidence plan reads")
            .expect("the evidence run has a frozen plan");
        Some((dir, manifest, plan))
    }

    /// The registry's own table, read from the checkout named by
    /// `PRECEPT_CHECKOUT`: the file on disk, never `git show <ref>`, so the
    /// answer cannot drift with how recently someone fetched.
    fn registry_rows() -> Option<Vec<Value>> {
        let checkout = PathBuf::from(std::env::var_os("PRECEPT_CHECKOUT")?);
        let text = std::fs::read_to_string(checkout.join("registry").join("testnet.jsonl")).ok()?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    #[test]
    #[ignore = "needs the operator's evidence tree and a real precept checkout: \
                PRECEPT_EVIDENCE_DIR=<evidence> PRECEPT_CHECKOUT=<precept> \
                cargo test -- --ignored"]
    fn the_live_market_run_round_trips_into_the_registry() {
        let Some((_, manifest, plan)) = evidence_run() else {
            panic!("PRECEPT_EVIDENCE_DIR must name the precept-abtest-evidence tree");
        };
        let Some(all) = registry_rows() else {
            panic!("PRECEPT_CHECKOUT must name a checkout holding registry/testnet.jsonl");
        };
        let deployment = MARKET_RUN;
        let mine = market_records(&manifest, &plan, deployment, "2026-09-30").expect("records");
        // The faucet is deployed out of band, never by this tool.
        let mut want: Vec<String> = all
            .iter()
            .filter(|row| {
                row["kind"] == "contract"
                    && row["deployment"] == deployment
                    && row["role"] != "faucet"
            })
            .map(canon)
            .collect();
        want.sort();
        let mut mine: Vec<String> = mine.iter().map(canon).collect();
        mine.sort();
        assert_eq!(mine.len(), want.len(), "line count");
        assert_eq!(mine, want, "every contract line, after canonicalisation");
        // The run's code is path-pinned (it predates release resolution):
        // the emitter records no wasm lines it could not source, and the
        // registry's curated rows for exactly those digests already exist.
        assert!(mine.iter().all(|row| row.contains("\"contract\"")));
        let registered: Vec<&Value> = all.iter().filter(|row| row["kind"] == "wasm").collect();
        for row in plan.artifacts.values() {
            assert!(
                registered
                    .iter()
                    .any(|wasm| wasm["hash"] == json!(row.as_str())),
                "the registry must already describe {}",
                row.as_str()
            );
        }
    }

    /// The market-live run's own manifest, copied in so the release path is
    /// exercised offline and identically everywhere. Contract-row fidelity
    /// against a real registry is the round-trip test's job.
    const MARKET_ADMIN: &str = "GCNQ4ZLK45KOYNIQLIYI7GT6TYWGK2OZEVZEY7A3W5L7QSPBNCBWSJAQ";

    fn fixture_manifest() -> MarketManifest {
        serde_json::from_value(json!({
            "network": {"label": "testnet", "passphrase": "Test SDF Network ; September 2015"},
            "admin": MARKET_ADMIN,
            "rpc_host": "{\"host\":\"soroban-testnet.stellar.org\"}",
            "oracle": {
                "runtime": "CDZIWBTV3S4R6534NJESRCQAXKYWJSCXDHVN6LIHSBCAFOOHTRKE6JEE",
                "governance": "CASM5GWRAUID4XTG4KO3R4L6OJUC755ZNKVI4PFHBIRMAOBGSCGRM5SW",
                "adapter": "CAL44IMB252XGNFY3O3B6HGN6OGLOUVPNGS6SZ7D64P7LJGGN7IPYKVI",
                "sources": [
                    "CBAAHPSQWF7YLQQO3FJFFRX26HIGHHYUVELLM3DSOZ5RSCJNLLOVPSNM",
                    "CAIPFFPYKDJ7YXYYGQ7RPXEBOTV2YGP6O63RM36WXS33CJRTFYA2JCTJ",
                    "CD5PLK3V6KIM5KRXQ5R2GYCYYMUVXJCE74IDUFT2DSHYJQXATL76UD6Y",
                ],
                "min_sources": 3, "max_age_secs": 3600, "max_clock_drift_secs": 60,
                "decimals": 7, "resolution": 300, "base": "USD"
            },
            "tokens": {
                "blnd": "CBYC26I2DE6KLFXPON3AD2OJJY5ZKCU2OWBWFGMJCC43S3Z4X5KBOIPV",
                "usdc": "CDXPSFVFELVSWIHICL2O5BJXQVX47YGIMHXPCLDWNR2XEYIUONR5F75D",
                "weth": "CBOX3I2GVXHHQ74RORKRJVVWMCYZ2JWTE3JYCVKYP7EPN465IQVK4CBI",
                "xlm": "CAWUCL3J4SRFE3MNUHDF5JH4S6MUVUMYWXACPYCBUBRQX4KILKUZAECH",
                "stable": "CB3F4EWMKGBPDRZWXKBFEXK2WVFDD65QUECNKURAUZ372DUGZROC3PX7"
            },
            "pool": "CABLQSAUOCKDXREA7CE55J4PRSHOHVDXNLPA54NUH4GCSLOUDP6CG7NQ",
            "backstop": "CDG62BXK5R7SCGZT4PCJI3OXQW5P4JQIZGBIIEVAXVZGY6QY3O5LNVDQ",
            "factory": "CACHT3KSTIULMHLZ6HPIJINGKMKIXFMICI3OVMLUDOR6KIYTXDXAXB7A",
            "comet": "CDPAWAR5GLCZNKWARR3HRJKRYQ6BKELCF5H5NNYUUZTL4OFX7IJYW4DB",
            "emitter": "CCCTBGR6PXFJVT577NIFM6ITAM552SRLH3OLDLMFE32DRHUQCUOZ3TDL",
            "reserves": {}, "identities": {}, "deploy_ledger": 4951021
        }))
        .expect("fixture market manifest")
    }

    /// The same run's frozen plan: every role the emitter asks for, each
    /// with its own digest. Path-pinned, so it carries no releases.
    fn fixture_plan() -> DeploymentPlan {
        let digest = |byte: u8| format!("{byte:02x}").repeat(32);
        serde_json::from_value(json!({
            "schema": 1, "config_digest": "a".repeat(64), "nonce": "7",
            "network_label": "testnet", "passphrase": "Test SDF Network ; September 2015",
            "admin": MARKET_ADMIN,
            "artifacts": {
                "adapter": digest(1), "backstop": digest(2), "comet": digest(3),
                "emitter": digest(4), "factory": digest(5), "pool": digest(6),
                "proxy-governance": digest(7), "proxy-runtime": digest(8),
                "source": digest(9), "token": digest(10)
            },
            "salt_roles": [], "slots": []
        }))
        .expect("fixture deployment plan")
    }

    /// The registry file is compared line by line, so the field order of a
    /// record is part of the contract. It holds only while `serde_json::Map`
    /// is the sorted `BTreeMap`; enabling `preserve_order` anywhere in the
    /// graph would otherwise reorder a deployment's records silently.
    #[test]
    fn a_record_line_is_the_canonical_field_order() {
        assert_eq!(
            canon(&contract(
                "testnet",
                "CID",
                "pool",
                "market-live-20261027",
                "2026-10-27",
                Some("aa"),
                json!({"backstop": "CBB"}),
                Some("GADMIN"),
                None,
                Some(vec!["CAA".to_string(), "CBB".to_string()]),
            )),
            concat!(
                r#"{"admin":"GADMIN","assets":["CAA","CBB"],"deployed_at":"2026-10-27","#,
                r#""deployment":"market-live-20261027","id":"CID","kind":"contract","#,
                r#""network":"testnet","refs":{"backstop":"CBB"},"role":"pool","#,
                r#""status":"active","wasm_hash":"aa"}"#,
            )
        );
        assert_eq!(
            canon(&wasm(
                "testnet",
                "pool",
                "aa",
                "v0.1.0",
                json!({"repo": "r", "tag": "v0.1.0", "asset": "pool.wasm"}),
            )),
            concat!(
                r#"{"artifact":"pool","hash":"aa","kind":"wasm","network":"testnet","#,
                r#""source":{"asset":"pool.wasm","repo":"r","tag":"v0.1.0"},"version":"v0.1.0"}"#,
            )
        );
    }

    #[test]
    fn a_frozen_release_becomes_an_exact_release_source() {
        let manifest = fixture_manifest();
        let mut plan = fixture_plan();
        plan.releases.insert(
            "pool".into(),
            Provenance {
                artifact: "pool".into(),
                version: "v0.1.1".into(),
                tag: "v0.1.1".into(),
                asset: "pool.wasm".into(),
                sha256: plan.artifacts["pool"].clone(),
                length: 1,
                catalog_commit: "c".repeat(40),
            },
        );
        let rows = market_records(&manifest, &plan, "market-live-20261027", "2026-10-27").unwrap();
        let codes: Vec<&Value> = rows.iter().filter(|row| row["kind"] == "wasm").collect();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0]["artifact"], json!("pool"));
        assert_eq!(codes[0]["version"], json!("v0.1.1"));
        assert_eq!(
            codes[0]["source"],
            json!({"repo": CONTRACTS_REPO, "tag": "v0.1.1", "asset": "pool.wasm"})
        );
        // The contracts it describes are still this tool's own rows.
        assert_eq!(
            rows.iter().filter(|row| row["kind"] == "contract").count(),
            16
        );
    }

    const REAL_BLND_SAC: &str = "CBYC26I2DE6KLFXPON3AD2OJJY5ZKCU2OWBWFGMJCC43S3Z4X5KBOIPV";
    const REAL_USDC_SAC: &str = "CDXPSFVFELVSWIHICL2O5BJXQVX47YGIMHXPCLDWNR2XEYIUONR5F75D";
    const REAL_WETH: &str = "CBOX3I2GVXHHQ74RORKRJVVWMCYZ2JWTE3JYCVKYP7EPN465IQVK4CBI";
    const REAL_XLM: &str = "CAWUCL3J4SRFE3MNUHDF5JH4S6MUVUMYWXACPYCBUBRQX4KILKUZAECH";
    const LAZER_ONE: &str = "CBAAHPSQWF7YLQQO3FJFFRX26HIGHHYUVELLM3DSOZ5RSCJNLLOVPSNM";
    /// A third-party SEP-40 oracle this run quotes and does not deploy — the
    /// mainnet rehearsal's RedStone contract, same shape. Only its strkey
    /// validity matters: the point is that it is not ours to file.
    const REDSTONE_SAC: &str = "CAIPFFPYKDJ7YXYYGQ7RPXEBOTV2YGP6O63RM36WXS33CJRTFYA2JCTJ";

    /// A real market shaped like the testnet rehearsal's: no mock tokens,
    /// shim legs, two real reserves, Lazer sources, and a reused runtime.
    fn real_manifest() -> MarketManifest {
        let mut value = serde_json::to_value(fixture_manifest()).unwrap();
        value["tokens"] = json!({});
        // The first reserve quotes one Lazer instance this run deployed and
        // one existing SEP-40 oracle it did not.
        value["oracle"]["sources"] = json!([LAZER_ONE, REDSTONE_SAC]);
        let adapter = value["oracle"]["adapter"].clone();
        let runtime = value["oracle"]["runtime"].clone();
        value["real"] = json!({
            "pool_name": "rehearsal",
            "reserves": [
                {"label": "weth", "asset": REAL_WETH, "index": 0, "decimals": 18},
                {"label": "xlm", "asset": REAL_XLM, "index": 1, "decimals": 7}
            ],
            "blnd": {"code": "SHIMBLND", "issuer": MARKET_ADMIN, "sac": REAL_BLND_SAC},
            "usdc": {"code": "SHIMUSDC", "issuer": MARKET_ADMIN, "sac": REAL_USDC_SAC},
            "backstop_deposit": 1000,
            "q4w_pct": 50,
            "proxies": [{
                "label": "weth",
                "asset": REAL_WETH,
                "adapter": adapter,
                "runtime": runtime,
                // A `Safe` is the rendered text of the identity object, as
                // the manifest assembler writes it.
                "sources": [
                    "{\"source\":\"lazer:71\"}".to_string(),
                    format!("{{\"source\":\"sep40:{REDSTONE_SAC}\"}}")
                ],
                "min_sources": 2,
                "max_age_secs": 300,
                "max_clock_drift_secs": 60,
                "decimals": 7,
                "base": "{\"Other\":\"USD\"}"
            }],
            "lazer_sources": [LAZER_ONE]
        });
        serde_json::from_value(value).expect("real market manifest")
    }

    /// The same manifest as a run wrote it before the Lazer list existed.
    fn real_manifest_without_lazer_list() -> MarketManifest {
        let mut value = serde_json::to_value(real_manifest()).unwrap();
        value["real"].as_object_mut().expect("real").remove("lazer_sources");
        serde_json::from_value(value).expect("legacy real market manifest")
    }

    /// Freeze an extra artifact into a plan, through the same deserialising
    /// door the plan is read by (its digest newtype is not constructible here).
    fn with_artifact(plan: &DeploymentPlan, role: &str, digest: &str) -> DeploymentPlan {
        let mut value = serde_json::to_value(plan).expect("plan as json");
        value["artifacts"]
            .as_object_mut()
            .expect("artifacts")
            .insert(role.to_string(), json!(digest));
        serde_json::from_value(value).expect("plan with a frozen artifact")
    }

    /// Its frozen plan: the runtime was reused, so there is no proxy-runtime
    /// or proxy-governance artifact; the sources are Lazer sources.
    fn real_plan() -> DeploymentPlan {
        let mut plan = fixture_plan();
        for role in ["proxy-runtime", "proxy-governance", "source", "token"] {
            plan.artifacts.remove(role);
        }
        let digest = |byte: u8| format!("{byte:02x}").repeat(32);
        with_artifact(&plan, "lazer-source", &digest(11))
    }

    fn real_rows() -> Vec<Value> {
        market_records(&real_manifest(), &real_plan(), "market-live-20261027", "2026-10-27")
            .expect("real market records")
    }

    #[test]
    fn a_real_markets_shim_legs_are_label_only_token_rows() {
        let rows = real_rows();
        let tokens: Vec<&Value> = rows.iter().filter(|row| row["role"] == "token").collect();
        assert_eq!(tokens.len(), 2, "the shim legs are the only token rows");
        for (id, label) in [(REAL_BLND_SAC, "shim-blnd"), (REAL_USDC_SAC, "shim-usdc")] {
            let row = tokens
                .iter()
                .find(|row| row["id"] == json!(id))
                .unwrap_or_else(|| panic!("no token row for {id}"));
            assert_eq!(row["label"], json!(label));
            assert_eq!(row["wasm_hash"], Value::Null, "classic assets carry no code of ours");
        }
    }

    #[test]
    fn a_real_markets_pool_assets_are_its_real_reserves() {
        let rows = real_rows();
        let pool = rows.iter().find(|row| row["role"] == "pool").expect("pool row");
        assert_eq!(pool["assets"], json!([REAL_WETH, REAL_XLM]));
        let backstop = rows.iter().find(|row| row["role"] == "backstop").expect("backstop row");
        assert_eq!(backstop["refs"]["blnd_token"], json!(REAL_BLND_SAC));
        assert_eq!(backstop["refs"]["usdc_token"], json!(REAL_USDC_SAC));
    }

    /// The run's own Lazer instance is the one price-source row, filed under
    /// the role the registry's vocabulary names for it and under the code the
    /// run uploaded.
    #[test]
    fn a_real_markets_lazer_source_is_a_lazer_source_row_with_our_digest() {
        let rows = real_rows();
        let want = real_plan().artifacts["lazer-source"].as_str().to_string();
        let sources: Vec<&Value> =
            rows.iter().filter(|row| row["role"] == "lazer_source").collect();
        assert_eq!(sources.len(), 1, "one deployed instance, one row: {sources:?}");
        let row = sources[0];
        assert_eq!(row["id"], json!(LAZER_ONE));
        assert_eq!(row["wasm_hash"], json!(want), "the code this run uploaded");
        assert_eq!(
            row["deployment"],
            json!("market-live-20261027"),
            "the run that created it"
        );
        assert_eq!(
            row["refs"]["proxy_runtime"],
            serde_json::to_value(&real_manifest().oracle.runtime).expect("runtime as json")
        );
    }

    /// The third-party SEP-40 oracle the reserve quotes is not this run's: no
    /// row, no code of ours and no deployment name may be attached to it, and
    /// nothing is left filed under the lab's `price_source` role.
    #[test]
    fn a_real_market_files_no_external_oracle_it_did_not_deploy() {
        let rows = real_rows();
        assert!(
            !rows.iter().any(|row| row["id"] == json!(REDSTONE_SAC)),
            "a quoted oracle is not a contract this run deployed"
        );
        assert!(
            !rows.iter().any(|row| row["role"] == json!("price_source")),
            "a real market's own sources are lazer_source rows: {rows:?}"
        );
        // Every contract row's code is code this run put on chain: the Lazer
        // digest for the source, and the plan's own pins for the rest.
        for row in rows.iter().filter(|row| row["kind"] == json!("contract")) {
            if row["role"] == json!("lazer_source") {
                continue;
            }
            assert_ne!(
                row["wasm_hash"],
                json!(real_plan().artifacts["lazer-source"].as_str()),
                "only the Lazer instance carries the source code: {row}"
            );
        }
    }

    /// A manifest written before the Lazer list is classified positionally
    /// against its first reserve's mirrored identities: the instance this run
    /// deployed is recognised, the quoted oracle is not.
    #[test]
    fn a_manifest_without_a_lazer_list_classifies_its_mirror_positionally() {
        let rows = market_records(
            &real_manifest_without_lazer_list(),
            &real_plan(),
            "market-live-20261027",
            "2026-10-27",
        )
        .expect("legacy real market records");
        let sources: Vec<&Value> =
            rows.iter().filter(|row| row["role"] == json!("lazer_source")).collect();
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0]["id"], json!(LAZER_ONE));
        assert!(!rows.iter().any(|row| row["id"] == json!(REDSTONE_SAC)));
    }

    /// With neither a Lazer list nor a mirrored proxy, nothing proves a
    /// source was deployed, so none is filed.
    #[test]
    fn a_real_manifest_that_names_no_proxy_files_no_source_row() {
        let mut value = serde_json::to_value(real_manifest_without_lazer_list()).unwrap();
        value["real"]["proxies"] = json!([]);
        let manifest: MarketManifest = serde_json::from_value(value).expect("manifest");
        let rows = market_records(&manifest, &real_plan(), "market-live-20261027", "2026-10-27")
            .expect("records");
        assert!(!rows.iter().any(|row| row["role"] == json!("lazer_source")));
        assert!(!rows.iter().any(|row| row["id"] == json!(LAZER_ONE)));
    }

    #[test]
    fn a_real_market_emits_the_launch_profile_wasm_lines_for_unreleased_roles() {
        let mut plan = real_plan();
        // pool is release-pinned: its line comes from the frozen release.
        plan.releases.insert(
            "pool".into(),
            Provenance {
                artifact: "pool".into(),
                version: "v0.1.1".into(),
                tag: "v0.1.1".into(),
                asset: "pool.wasm".into(),
                sha256: plan.artifacts["pool"].clone(),
                length: 1,
                catalog_commit: "c".repeat(40),
            },
        );
        let rows = market_records(&real_manifest(), &plan, "market-live-20261027", "2026-10-27")
            .expect("real market records");
        let wasm_rows: Vec<&Value> = rows.iter().filter(|row| row["kind"] == "wasm").collect();
        let profile = |role: &str| crate::manifest::launch_wasm_file(role).expect(role);
        // Every profile role the plan froze, bar the released pool: the
        // curated digest, never the plan's own bytes.
        for role in ["backstop", "factory", "adapter", "lazer-source", "comet", "emitter"] {
            let entry = profile(role);
            let row = wasm_rows
                .iter()
                .find(|row| row["artifact"] == json!(entry.registry.0))
                .unwrap_or_else(|| panic!("no wasm line for {role}"));
            assert_eq!(row["hash"], json!(entry.wasm_hash), "{role}");
            assert_eq!(row["version"], json!(entry.registry.1), "{role}");
        }
        let pool: Vec<&&Value> = wasm_rows.iter().filter(|row| row["artifact"] == "pool").collect();
        assert_eq!(pool.len(), 1, "the released pool is recorded once, from its release");
        assert_eq!(pool[0]["hash"], json!(plan.artifacts["pool"].as_str()));
        assert_eq!(wasm_rows.len(), 7, "{wasm_rows:?}");
    }

    #[test]
    fn a_reused_runtime_leaves_no_proxy_rows_but_a_deployed_one_does() {
        let rows = real_rows();
        assert!(real_manifest().oracle.governance.is_some(), "the gate must matter");
        for row in &rows {
            assert_ne!(row["role"], "proxy_runtime");
            assert_ne!(row["role"], "proxy_governance");
        }
        // Both are release-pinned as the real launch does; a path-pinned
        // proxy role carries no provenance for the emitter to record.
        let mut plan = real_plan();
        for (role, byte) in [("proxy-runtime", 8u8), ("proxy-governance", 7u8)] {
            plan = with_artifact(&plan, role, &format!("{byte:02x}").repeat(32));
        }
        for role in ["proxy-runtime", "proxy-governance"] {
            plan.releases.insert(
                role.into(),
                Provenance {
                    artifact: role.into(),
                    version: "v0.1.5".into(),
                    tag: "v0.1.5".into(),
                    asset: format!("{role}.wasm"),
                    sha256: plan.artifacts[role].clone(),
                    length: 1,
                    catalog_commit: "c".repeat(40),
                },
            );
        }
        let rows = market_records(&real_manifest(), &plan, "market-live-20261027", "2026-10-27")
            .expect("deployed-runtime records");
        for role in ["proxy_runtime", "proxy_governance"] {
            assert_eq!(rows.iter().filter(|row| row["role"] == role).count(), 1, "{role}");
        }
    }
}
