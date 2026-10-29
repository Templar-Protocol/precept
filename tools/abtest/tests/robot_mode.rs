//! Robot mode seam (`--robot` / `--approve-plan`): the deployment path an
//! agent drives without a terminal. The approval object is the run's frozen
//! `deployment-plan.toml`, not a stream of `y` lines, so these tests pin the
//! two-phase contract against the same injected `stellar` executable the PTY
//! seam uses:
//!
//! * `--robot` alone stops at the first confirmation after printing the plan
//!   and the plan file's own sha256, with zero mutating calls;
//! * a digest that is not the file's digest refuses before anything is sent;
//! * the approved digest releases the same coordinates the interactive path
//!   reaches, in the same order, when the file still hashes to it;
//! * a plan edited after approval — before the run, or mid-run — refuses
//!   before the next send;
//! * `--approve-plan` without `--robot` is a usage error.
//!
//! No new dependencies: `script(1)` is checked at runtime for the interactive
//! comparison only, and every other test drives the binary directly.

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_precept-abtest");

/// Identity material the injected CLI replays; the fixture config's
/// administrator pin is rewritten to it.
const ADMIN_G: &str = "GBDV7KHJMC36EEXW5V75FJCGPIDRPJNEWL7Z7LID3CHY5OAE2NRREWZX";

/// A contract the injected CLI knows — the oracle runtime the fixture quotes —
/// standing in for a curator whose market this run deploys.
const CURATOR_C: &str = "CC64WBDGS6QQP22QTTIACYIXT3WF7BBQEYOQPLTP7GTKYY7PZ74QYGSL";

/// Exit code of a robot run that printed the plan and stopped.
const PLAN_EXIT: i32 = 3;
/// Exit code of a robot run whose approval no longer matches the file.
const REFUSED_EXIT: i32 = 4;

fn script_available() -> bool {
    Command::new("script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Scratch root, per process, without whitespace: the interactive comparison
/// quotes for `script -c`, which cannot survive a space in a path.
fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("abtest-robot-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    assert!(
        !root.to_string_lossy().contains(char::is_whitespace),
        "TMPDIR must not contain whitespace for the interactive comparison"
    );
    root
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut child = Command::new("sha256sum")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("sha256sum");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(bytes)
        .expect("feed sha256sum");
    let out = child.wait_with_output().expect("sha256sum");
    assert!(out.status.success(), "sha256sum failed");
    let text = String::from_utf8(out.stdout).expect("ascii digest");
    text.split_whitespace().next().expect("digest").to_string()
}

/// The frozen market configuration shipped with the tool, rewritten only as
/// far as offline determinism requires: artifact paths move into the scratch
/// tree with deterministic bytes and matching sha256 values so preflight can
/// be satisfied without network access, and the administrator pin becomes the
/// identity the injected CLI resolves.
fn market_config(dir: &Path) -> PathBuf {
    let raw = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/markets/protected-testnet.toml"
    ))
    .expect("markets/protected-testnet.toml must ship with the tool");
    let mut last: Option<PathBuf> = None;
    let mut lines: Vec<String> = Vec::new();
    let mut rewriting_release = false;
    for line in raw.lines() {
        let t = line.trim();
        if let Some(v) = t
            .strip_prefix("path = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            let name = v.rsplit('/').next().unwrap_or("artifact").to_string();
            let dest = dir.join(&name);
            if !dest.exists() {
                if v.ends_with(".toml") {
                    fs::write(&dest, "fixture = true\n").unwrap();
                } else {
                    fs::write(
                        &dest,
                        format!("abtest robot fixture {name} deterministic bytes"),
                    )
                    .unwrap();
                }
            }
            last = Some(dest.clone());
            lines.push(format!("path = \"{}\"", dest.display()));
        } else if t.starts_with("version = \"") && rewriting_release {
            continue;
        } else if let Some(v) = t
            .strip_prefix("release = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            let name = format!("{v}.wasm");
            let dest = dir.join(&name);
            if !dest.exists() {
                fs::write(
                    &dest,
                    format!("abtest robot fixture {name} deterministic bytes"),
                )
                .unwrap();
            }
            last = Some(dest.clone());
            rewriting_release = true;
            lines.push(format!("path = \"{}\"", dest.display()));
            let bytes = fs::read(&dest).expect("fixture artifact");
            lines.push(format!("sha256 = \"{}\"", sha256_hex(&bytes)));
        } else if t.starts_with("sha256 = \"") {
            rewriting_release = false;
            let bytes = fs::read(last.as_ref().expect("path line precedes every sha256"))
                .expect("fixture artifact");
            lines.push(format!("sha256 = \"{}\"", sha256_hex(&bytes)));
        } else if line.contains("GAYX6R3NPJWKORXQMCZ3RJZ4O4YB4B64I2BZK6F3W6F3OJBSQKLRBUTD") {
            lines.push(line.replace(
                "GAYX6R3NPJWKORXQMCZ3RJZ4O4YB4B64I2BZK6F3W6F3OJBSQKLRBUTD",
                ADMIN_G,
            ));
        } else {
            lines.push(line.to_string());
        }
    }
    let cfg = dir.join("market.toml");
    fs::write(&cfg, lines.join("\n") + "\n").unwrap();
    cfg
}

fn identities(dir: &Path) {
    fs::write(
        dir.join("admin-key.txt"),
        format!("{ADMIN_G}\nSBQCJ7HNRWAC73KQKBMZT5N4Q4XVK64I2BZK6F3W6F3OJBSQKLRQI6NM\n"),
    )
    .unwrap();
    fs::write(
        dir.join("contracts.txt"),
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM\n",
    )
    .unwrap();
}

/// The injected `stellar`: deterministic canned responses, a record of every
/// call, and a `send` booking around every call that can mutate. The send
/// booking is the sequence the tests compare: it is the answer to "what did
/// this run actually ask the network to do", independent of read traffic.
///
/// `ABTEST_TOUCH_PLAN` names the run's frozen plan file; when it is set, each
/// send appends a line to it, which is how a test makes the plan change
/// between one confirmation and the next.
const SCRIPT: &str = r##"#!/bin/sh
LOG="$ABTEST_ARGV_LOG"
ALL="$*"
[ -n "$LOG" ] && echo "call $ALL" >> "$LOG"
booked() {
    [ -n "$LOG" ] && echo "send $ALL" >> "$LOG"
    [ -n "$ABTEST_TOUCH_PLAN" ] && echo "# operator edit" >> "$ABTEST_TOUCH_PLAN"
}
if [ "$1" = network ] && [ "$2" = info ]; then
    printf '%s\n' '{"passphrase":"Test SDF Network ; September 2015"}'
    exit 0
fi
case "$*" in
    *--version*) echo 'stellar 26.5.0' ;;
    *--help*|*help*|*ledger*entries*) echo '{}' ;;
    *keys*public-key*|*keys*show*|*keys*address*|*keys*public*)
        if [ "$3" = admin ]; then grep '^G' "$ABTEST_ADMIN" | head -n 1
        elif [ -s "$ABTEST_ADMIN.$3" ]; then cat "$ABTEST_ADMIN.$3"
        else exit 1; fi
        ;;
    *keys*generate*)
        booked
        case "$3" in
            abtest-admin) echo GASFUXQFGKJFVWUCRAHL4WOWE5344SUESN7U7RDLTTJEA5Q4OQWXOWPM ;;
            abtest-frodo) echo GCGUYVGMPHCU6W4372SJ5BY3E75VL4L2U65ON4BQKVSYL4RTVTDCRKUD ;;
            abtest-sam) echo GDEFDFE4DRJFQ6RTY43XQ4GMFPKPDKE3HQZL3HK5LMWFYVH3BNLRFZ2W ;;
            abtest-user) echo GBDQWUISJ2WJP67TV7FXA3J44R7UQRAR477NKTX3A3ICQAIOOEPNN6LB ;;
            *) exit 1 ;;
        esac > "$ABTEST_ADMIN.$3"
        ;;
    *get_owner*) echo '"CC64WBDGS6QQP22QTTIACYIXT3WF7BBQEYOQPLTP7GTKYY7PZ74QYGSL"' ;;
    *has_role*) echo 'true' ;;
    *contract*upload*|*contract*deploy*|*contract*install*|*tx*submit*|*transaction*submit*|*contract*invoke*)
        booked
        echo '{"return":"0"}'
        ;;
    *contract*fetch*|*inspect*|*simulate*|*decode*|*explain*) head -n 1 "$ABTEST_CONTRACTS" ;;
    *contract*id*predict*) head -n 1 "$ABTEST_CONTRACTS" ;;
    *network*passphrase*|*passphrase*) echo 'Test SDF Network ; September 2015' ;;
    *) echo 'ok' ;;
esac
exit 0
"##;

fn stellar(dir: &Path) -> PathBuf {
    let p = dir.join("stellar");
    fs::write(&p, SCRIPT).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn argv_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("argv.log")).unwrap_or_default()
}

/// Every send the injected CLI was asked to make, in order.
fn sends(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| line.strip_prefix("send "))
        .map(str::to_string)
        .collect()
}

struct Target {
    dir: PathBuf,
    stellar: PathBuf,
    admin: PathBuf,
    contracts: PathBuf,
}

/// One scratch fixture: a rewritten market config, the identity material, the
/// injected CLI, and an argv log of its own.
fn target(tag: &str) -> (Target, PathBuf) {
    let dir = scratch(tag);
    identities(&dir);
    let cfg = market_config(&dir);
    let t = Target {
        dir: dir.clone(),
        stellar: stellar(&dir),
        admin: dir.join("admin-key.txt"),
        contracts: dir.join("contracts.txt"),
    };
    (t, cfg)
}

/// Run the tool with stdin closed: a robot run that ever reads a
/// confirmation would see EOF, so a passing run proves it read nothing.
fn run(t: &Target, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("STELLAR_BIN", &t.stellar)
        .env("ABTEST_ARGV_LOG", t.dir.join("argv.log"))
        .env("ABTEST_ADMIN", &t.admin)
        .env("ABTEST_CONTRACTS", &t.contracts)
        .stdin(Stdio::null())
        .output()
        .expect("the tool runs")
}

fn deploy_args<'a>(cfg: &'a str, out: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "--out",
        out,
        "market",
        "deploy",
        "--market",
        cfg,
        "--admin-identity",
        "admin",
    ];
    args.extend(extra);
    args
}

fn resume_args<'a>(cfg: &'a str, out: &'a str, dir: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "--out",
        out,
        "market",
        "resume",
        "--market",
        cfg,
        "--admin-identity",
        "admin",
        "--admin-config-dir",
        dir,
    ];
    args.extend(extra);
    args
}

/// Every non-empty stdout line of a robot run, which must all be JSON
/// objects: a consumer parses the stream, so one human line in it is a bug.
fn json_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("robot stdout must be JSON only ({e}): {line}"))
        })
        .collect()
}

/// The events a robot run printed, in order. Every line is one; a line that
/// is not JSON is a defect in the run, not a line to skip.
fn events(stdout: &str) -> Vec<serde_json::Value> {
    json_lines(stdout)
}

fn event<'a>(events: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    events
        .iter()
        .find(|e| e["event"] == name)
        .unwrap_or_else(|| panic!("no {name} event in {events:#?}"))
}

fn plan_digest(out: &Path) -> String {
    let bytes = fs::read(out.join("deployment-plan.toml")).expect("the run froze a plan");
    sha256_hex(&bytes)
}

/// `--robot` without an approval: the run publishes the frozen plan and its
/// digest, states the confirmation it stopped at, and sends nothing.
#[test]
fn robot_without_approval_prints_the_plan_and_sends_nothing() {
    let (t, cfg) = target("plan");
    let out = t.dir.join("run");
    let output = run(
        &t,
        &deploy_args(
            cfg.to_str().unwrap(),
            out.to_str().unwrap(),
            &["--admin-config-dir", t.dir.to_str().unwrap(), "--robot"],
        ),
    );
    assert_eq!(
        output.status.code(),
        Some(PLAN_EXIT),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        sends(&argv_log(&t.dir)).is_empty(),
        "a plan-only robot run sent something: {}",
        argv_log(&t.dir)
    );
    let events = events(&String::from_utf8_lossy(&output.stdout));
    let plan = event(&events, "plan");
    // The digest the operator approves is the file's own, byte for byte.
    assert_eq!(
        plan["plan_sha256"].as_str().expect("digest"),
        plan_digest(&out),
        "the printed digest must be the sha256 of deployment-plan.toml"
    );
    assert_eq!(
        plan["run_dir"].as_str(),
        Some(out.to_str().unwrap()),
        "the plan names the run it belongs to"
    );
    let steps = plan["steps"].as_array().expect("the plan lists its steps");
    assert!(
        !steps.is_empty(),
        "a deploy plan lists the graph it will run"
    );
    for step in steps {
        for key in ["phase", "slot", "action", "mutation_kind", "settled"] {
            assert!(!step[key].is_null(), "step without {key}: {step}");
        }
    }
    // The first pending confirmation is part of the plan a robot approves.
    let confirm = event(&events, "confirm");
    assert!(
        confirm["summary"].as_str().is_some_and(|s| !s.is_empty()),
        "the plan must state the confirmation it stopped at: {confirm}"
    );
    // No token ever reaches stdout: the approval is the plan, not a token,
    // so no event carries one as a field.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().filter(|line| line.starts_with('{')) {
        let value: serde_json::Value =
            serde_json::from_str(line).expect("one JSON object per line");
        assert!(
            value.get("token").is_none(),
            "the confirmation token must never be printed: {line}"
        );
    }
    assert_eq!(
        confirm.as_object().expect("confirm object").len(),
        2,
        "a confirm event carries the summary and nothing else: {confirm}"
    );
    let _ = fs::remove_dir_all(&t.dir);
}

/// An approval that is not the file's digest refuses before the first send:
/// the operator approved a plan this run does not have.
#[test]
fn robot_with_a_wrong_digest_refuses_before_any_send() {
    let (t, cfg) = target("wrong-digest");
    let out = t.dir.join("run");
    let wrong = "0".repeat(64);
    let output = run(
        &t,
        &deploy_args(
            cfg.to_str().unwrap(),
            out.to_str().unwrap(),
            &[
                "--admin-config-dir",
                t.dir.to_str().unwrap(),
                "--robot",
                "--approve-plan",
                &wrong,
            ],
        ),
    );
    assert_eq!(
        output.status.code(),
        Some(REFUSED_EXIT),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        sends(&argv_log(&t.dir)).is_empty(),
        "a refused approval sent something: {}",
        argv_log(&t.dir)
    );
    let events = events(&String::from_utf8_lossy(&output.stdout));
    let refused = event(&events, "approval_refused");
    assert_eq!(refused["approved_sha256"].as_str(), Some(wrong.as_str()));
    assert_eq!(
        refused["plan_sha256"].as_str(),
        Some(plan_digest(&out).as_str()),
        "the refusal states the plan it actually found"
    );
    let _ = fs::remove_dir_all(&t.dir);
}

/// `--approve-plan` without `--robot` is a usage error: the approval has no
/// meaning on the interactive path, which confirms on stdin.
#[test]
fn approve_plan_without_robot_is_a_usage_error() {
    let (t, cfg) = target("usage");
    let out = t.dir.join("run");
    let digest = "a".repeat(64);
    let output = run(
        &t,
        &deploy_args(
            cfg.to_str().unwrap(),
            out.to_str().unwrap(),
            &[
                "--admin-config-dir",
                t.dir.to_str().unwrap(),
                "--approve-plan",
                &digest,
            ],
        ),
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--robot"),
        "the usage error must name the flag it requires: {stderr}"
    );
    assert!(
        !out.join("deployment-plan.toml").exists(),
        "a usage error must not touch a run directory"
    );
    let _ = fs::remove_dir_all(&t.dir);
}

/// An approval binds to one file: a plan edited after the digest was read
/// refuses before the next send, and nothing of the edited plan runs.
#[test]
fn robot_refuses_a_plan_edited_after_approval() {
    let (t, cfg) = target("edited");
    let out = t.dir.join("run");
    let out_s = out.to_str().unwrap().to_string();
    let dir_s = t.dir.to_str().unwrap().to_string();
    // Phase one: publish the plan and read its digest.
    let planned = run(
        &t,
        &deploy_args(
            cfg.to_str().unwrap(),
            &out_s,
            &["--admin-config-dir", &dir_s, "--robot"],
        ),
    );
    assert_eq!(planned.status.code(), Some(PLAN_EXIT));
    let approved = plan_digest(&out);
    // The plan changes before the approved run starts.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(out.join("deployment-plan.toml"))
        .expect("the frozen plan");
    file.write_all(b"# operator edit after approval\n")
        .expect("append to the plan");
    drop(file);
    let output = run(
        &t,
        &resume_args(
            cfg.to_str().unwrap(),
            &out_s,
            &dir_s,
            &["--robot", "--approve-plan", &approved],
        ),
    );
    assert_eq!(
        output.status.code(),
        Some(REFUSED_EXIT),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        sends(&argv_log(&t.dir)).is_empty(),
        "an edited plan released a send: {}",
        argv_log(&t.dir)
    );
    let events = events(&String::from_utf8_lossy(&output.stdout));
    let refused = event(&events, "approval_refused");
    assert_eq!(refused["approved_sha256"].as_str(), Some(approved.as_str()));
    assert_ne!(
        refused["plan_sha256"].as_str(),
        Some(approved.as_str()),
        "the refusal must report the edited file, not the approved one"
    );
    let _ = fs::remove_dir_all(&t.dir);
}

/// The digest is re-derived at every confirmation, not once at startup: a
/// plan edited while the run is in flight refuses before the next send, so a
/// run cannot continue on approvals that no longer describe the file.
#[test]
fn robot_re_checks_the_plan_at_every_confirmation() {
    let (t, cfg) = target("mid-run");
    let out = t.dir.join("run");
    let out_s = out.to_str().unwrap().to_string();
    let dir_s = t.dir.to_str().unwrap().to_string();
    let planned = run(
        &t,
        &deploy_args(
            cfg.to_str().unwrap(),
            &out_s,
            &["--admin-config-dir", &dir_s, "--robot"],
        ),
    );
    assert_eq!(planned.status.code(), Some(PLAN_EXIT));
    let approved = plan_digest(&out);
    // The injected CLI edits the plan while it serves a send, so the next
    // confirmation re-reads a file that is no longer the approved one.
    let output = Command::new(BIN)
        .args(resume_args(
            cfg.to_str().unwrap(),
            &out_s,
            &dir_s,
            &["--robot", "--approve-plan", &approved],
        ))
        .env("STELLAR_BIN", &t.stellar)
        .env("ABTEST_ARGV_LOG", t.dir.join("argv.log"))
        .env("ABTEST_ADMIN", &t.admin)
        .env("ABTEST_CONTRACTS", &t.contracts)
        .env("ABTEST_TOUCH_PLAN", out.join("deployment-plan.toml"))
        .stdin(Stdio::null())
        .output()
        .expect("the tool runs");
    assert_eq!(
        output.status.code(),
        Some(REFUSED_EXIT),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let sent = sends(&argv_log(&t.dir));
    assert_eq!(
        sent.len(),
        1,
        "the run must refuse at the confirmation after the edited send: {sent:?}"
    );
    let events = events(&String::from_utf8_lossy(&output.stdout));
    event(&events, "approval_refused");
    assert_ne!(
        plan_digest(&out),
        approved,
        "the plan on disk is no longer the approved one"
    );
    let _ = fs::remove_dir_all(&t.dir);
}

/// The approved digest releases the same coordinates the interactive path
/// reaches, in the same order. Both runs drive the same graph over the same
/// injected CLI, so the sequence of sends the CLI booked must be identical;
/// the robot run simply has no operator.
#[test]
fn robot_with_the_approved_plan_matches_the_interactive_send_sequence() {
    if !script_available() {
        eprintln!(
            "SKIP robot_with_the_approved_plan_matches_the_interactive_send_sequence: \
             `script(1)` (util-linux) is unavailable, and the interactive half needs a real terminal"
        );
        return;
    }
    // Interactive half: a real terminal, with `y` lines typed at every gate.
    let (tty, tty_cfg) = target("interactive");
    let tty_out = tty.dir.join("run");
    let typed = interactive(&tty, &tty_cfg, &tty_out);
    let interactive_sends = sends(&argv_log(&tty.dir));
    assert!(
        !interactive_sends.is_empty(),
        "the interactive run confirmed nothing, so there is no sequence to \
         compare; typescript:\n{}",
        capture(&tty.dir)
    );
    // Robot half: publish the plan, then resume it under the printed digest.
    let (robot, robot_cfg) = target("approved");
    let robot_out = robot.dir.join("run");
    let planned = run(
        &robot,
        &deploy_args(
            robot_cfg.to_str().unwrap(),
            robot_out.to_str().unwrap(),
            &["--admin-config-dir", robot.dir.to_str().unwrap(), "--robot"],
        ),
    );
    assert_eq!(planned.status.code(), Some(PLAN_EXIT));
    assert!(
        sends(&argv_log(&robot.dir)).is_empty(),
        "the plan phase sent something: {}",
        argv_log(&robot.dir)
    );
    let approved = plan_digest(&robot_out);
    let output = run(
        &robot,
        &resume_args(
            robot_cfg.to_str().unwrap(),
            robot_out.to_str().unwrap(),
            robot.dir.to_str().unwrap(),
            &["--robot", "--approve-plan", &approved],
        ),
    );
    // Both surfaces stop at the same place: same exits, same sends.
    assert_eq!(
        output.status.code(),
        Some(typed),
        "stdout:\n{}\nstderr:\n{}\ninteractive typescript:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        capture(&tty.dir)
    );
    assert_eq!(
        scrub(&sends(&argv_log(&robot.dir)), &robot.dir),
        scrub(&interactive_sends, &tty.dir),
        "the robot path must confirm the same coordinates in the same order as \
         an operator would"
    );
    assert!(
        interactive_sends.len() > 1,
        "one send is not a sequence; both runs must confirm the graph's \
         coordinates: {interactive_sends:?}"
    );
    let _ = fs::remove_dir_all(&tty.dir);
    let _ = fs::remove_dir_all(&robot.dir);
}

/// Send lines with the scratch root folded to one token: the two fixtures
/// live in different directories, and the argv that matters is the command,
/// not where the run was parked.
fn scrub(sends: &[String], dir: &Path) -> Vec<String> {
    let root = dir.to_string_lossy().into_owned();
    sends
        .iter()
        .map(|s| s.replace(&root, "<scratch>"))
        .collect()
}

/// A custody stop is a failure, not a success: robot exit 1, and the event
/// says what it is. Nothing later is attempted, and every line on stdout is
/// still one JSON object.
#[test]
fn robot_exits_1_on_a_custody_stop_and_prints_only_json() {
    let (t, cfg) = target("custody");
    let out = t.dir.join("run");
    let out_s = out.to_str().unwrap().to_string();
    let dir_s = t.dir.to_str().unwrap().to_string();
    let planned = run(
        &t,
        &deploy_args(
            cfg.to_str().unwrap(),
            &out_s,
            &["--admin-config-dir", &dir_s, "--robot", "--no-registry"],
        ),
    );
    assert_eq!(planned.status.code(), Some(PLAN_EXIT));
    let approved = plan_digest(&out);
    let output = run(
        &t,
        &resume_args(
            cfg.to_str().unwrap(),
            &out_s,
            &dir_s,
            &["--robot", "--approve-plan", &approved, "--no-registry"],
        ),
    );
    // A stop before publication exits 1 without ever asking the registry
    // anything: the deployer's own code is the whole answer, and the registry
    // step (which would also answer 1 over the missing manifest) is not
    // reached at all.
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("registry:"),
        "a stopped run must not reach the registry step: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unresolved submission is a failed run: stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = json_lines(&stdout);
    let custody = event(&events, "custody");
    assert_eq!(
        custody["unresolved"],
        serde_json::json!(true),
        "the stop is reported as unresolved custody: {custody}"
    );
    assert!(
        !sends(&argv_log(&t.dir)).is_empty(),
        "the fixture reaches custody after its confirms, so something was sent"
    );
    let _ = fs::remove_dir_all(&t.dir);
}

/// The real-market surface prints a human account of the run before its first
/// gate. In robot mode that account belongs on stderr: stdout is the event
/// stream a consumer parses, and one human line in it is a defect.
#[test]
fn robot_real_deploy_keeps_its_human_progress_on_stderr() {
    let (t, _lab) = target("real-progress");
    let pool = real_pool_config(&t.dir);
    let out = t.dir.join("run");
    let output = run(
        &t,
        &[
            "--out",
            out.to_str().unwrap(),
            "market",
            "deploy",
            "--config",
            pool.to_str().unwrap(),
            "--network",
            "testnet",
            "--admin-identity",
            "admin",
            "--admin-config-dir",
            t.dir.to_str().unwrap(),
            "--no-registry",
            "--robot",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Whatever the run reaches, its stdout parses as events only.
    let _ = json_lines(&stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in [
        "real market deploy: robot mode",
        "network: testnet",
        "reserves: 2",
    ] {
        assert!(
            stderr.contains(line),
            "the human preamble must be on stderr ({line}): stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    for line in ["network: testnet", "reserves: 2", "planned coordinates"] {
        assert!(
            !stdout.contains(line),
            "a human line leaked onto robot stdout ({line}): {stdout}"
        );
    }
    let _ = fs::remove_dir_all(&t.dir);
}

/// A pool config whose admin is a contract is a market this run owes a
/// handover for, and the plan a robot approves is where that becomes
/// visible before anything is sent: the handoff coordinates are in it, with
/// the roles they act on. A config owned by the deploying key — the shape
/// every config had before an owner could be named — plans no such step, so
/// the two runs together are what makes the difference the owner and not an
/// unrelated fault.
#[test]
fn robot_real_deploy_plans_the_handoff_a_curated_pool_owes() {
    let planned = |tag: &str, owner: &str| -> String {
        let (t, _lab) = target(tag);
        let pool = real_pool_config_owned(&t.dir, owner);
        let out = t.dir.join("run");
        let output = run(
            &t,
            &[
                "--out",
                out.to_str().unwrap(),
                "market",
                "deploy",
                "--config",
                pool.to_str().unwrap(),
                "--network",
                "testnet",
                "--admin-identity",
                "admin",
                "--admin-config-dir",
                t.dir.to_str().unwrap(),
                "--no-registry",
                "--robot",
            ],
        );
        let _ = fs::remove_dir_all(&t.dir);
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    let curated = planned("real-handoff-curator", CURATOR_C);
    let plan_steps = |stdout: &str| -> Vec<serde_json::Value> {
        let lines = json_lines(stdout);
        let plan = lines
            .iter()
            .find(|line| line["event"] == "plan")
            .unwrap_or_else(|| panic!("the robot prints the plan it asks to approve: {stdout}"));
        plan["steps"]
            .as_array()
            .expect("the plan carries its steps")
            .clone()
    };
    let steps = plan_steps(&curated);
    let hand = |action: &str| {
        steps
            .iter()
            .any(|step| step["phase"] == 11 && step["action"] == action)
    };
    assert!(
        hand("hand off the pool"),
        "a pool owned by another key must plan its handover: {curated}"
    );
    assert!(
        hand("hand off a price source"),
        "a source this run deployed goes with the pool it prices: {curated}"
    );
    assert!(
        hand("hand off the backing pool"),
        "the backing pool's controller follows the pool's admin: {curated}"
    );
    let self_owned = planned("real-handoff-self", ADMIN_G);
    let owned = plan_steps(&self_owned);
    assert!(
        !owned.iter().any(|step| step["phase"] == 11),
        "a run that hands nothing over plans no handoff: {self_owned}"
    );
}

/// A completed real market run directory: the published manifest and the
/// frozen plan a finished run leaves behind, with one Lazer source this run
/// deployed and one third-party SEP-40 oracle the pool config quotes. The
/// registry step reads exactly these two files.
fn write_completed_real_run(out: &Path) {
    let digest = |byte: &str| byte.repeat(32);
    let runtime = "CBR3K46443VAX3ACC24GOSPF7BYHWUJT2RUHIZE5TFJ5PAJEZ3O2ZMM7";
    let adapter = "CBJY4SAB2W6N2E2CKMEOGOTAZ5GFOBNCKVJYELEABHR5VICYHUCOYV5B";
    let external = "CA4RB3CG4MEWLMQIA2XUJEVDTB5566AFUYBTQCZIGPAGFLZ7WHLEDTGO";
    let asset = "CDNCBF42U52GBG443RC7O2LLKYRKDHUZGJJEMHWCXXVMIIFMWCZ3JIR2";
    let manifest = serde_json::json!({
        "network": {"label": "testnet", "passphrase": "Test SDF Network ; September 2015"},
        "admin": ADMIN_G,
        "rpc_host": "soroban-testnet.stellar.org",
        "oracle": {
            "runtime": runtime,
            "adapter": adapter,
            "sources": [runtime, external],
            "min_sources": 2,
            "max_age_secs": 300,
            "max_clock_drift_secs": 60,
            "decimals": 7,
            "resolution": 300,
            "base": "{\"Other\":\"USD\"}"
        },
        "tokens": {},
        "pool": "CCW7TXPOI3HJKA5MVLOKRFWFXOB3OFYLKKRTEVXBFJZQQRPQ6FJF7XDC",
        "backstop": "CDK4UFQSNLK3376MUVPSI267HZD2HJHE77VTZGYJVTTODDXNKJ6K7HUX",
        "factory": "CDVQGLTCU7Y23PUIS2B2AKSMO24GQPTL46WJZTC3QS5ZZ5SF4A7EHREC",
        "comet": "CBY5WU3AXYBUHFODEJBUE6PLEFR2T4WOQK7LN7GLRC52DZ2DYYX4HZWX",
        "emitter": "CB4L4NXEKTNZNZNQSRFS6A5QK4QEFJCB2H66FRPVAPSGRZYNET6WDPHO",
        "reserves": {},
        "identities": {},
        "deploy_ledger": 1234,
        "real": {
            "pool_name": "abtest-robot-registry",
            "reserves": [{"label": "usdc", "asset": asset, "index": 0, "decimals": 7}],
            "blnd": {"code": "SHIMBLND", "issuer": ADMIN_G,
                     "sac": "CA4RB3CG4MEWLMQIA2XUJEVDTB5566AFUYBTQCZIGPAGFLZ7WHLEDTGO"},
            "usdc": {"code": "SHIMUSDC", "issuer": ADMIN_G,
                     "sac": "CD6AR2BURCP545VNW6KZEN6LPPK43QQNA56AYLKNMLU6V4JIFF4XNVZ3"},
            "backstop_deposit": 1000,
            "q4w_pct": 50,
            "proxies": [{
                "label": "usdc",
                "asset": asset,
                "adapter": adapter,
                "runtime": runtime,
                "sources": ["{\"source\":\"lazer:71\"}", format!("{{\"source\":\"sep40:{external}\"}}")],
                "min_sources": 2,
                "max_age_secs": 300,
                "max_clock_drift_secs": 60,
                "decimals": 7,
                "base": "{\"Other\":\"USD\"}"
            }],
            "lazer_sources": [runtime]
        }
    });
    let plan = serde_json::json!({
        "schema": 1,
        "config_digest": digest("ab"),
        "nonce": "01",
        "network_label": "testnet",
        "passphrase": "Test SDF Network ; September 2015",
        "admin": ADMIN_G,
        "artifacts": {
            "adapter": digest("01"),
            "backstop": digest("02"),
            "comet": digest("03"),
            "emitter": digest("04"),
            "factory": digest("05"),
            "lazer-source": digest("06"),
            "pool": digest("07")
        },
        "salt_roles": [],
        "slots": []
    });
    fs::write(
        out.join("market-manifest.toml"),
        toml::to_string(&manifest).expect("manifest as toml"),
    )
    .expect("write the manifest");
    fs::write(
        out.join("deployment-plan.toml"),
        toml::to_string(&plan).expect("plan as toml"),
    )
    .expect("write the plan");
}

/// A precept checkout whose registry script reports like the real one and
/// writes nothing: enough to make the record step print its report, its
/// notice and its pull-request instructions.
fn fake_registry_checkout(dir: &Path) -> PathBuf {
    let checkout = dir.join("precept");
    let registry = checkout.join("registry");
    fs::create_dir_all(&registry).expect("registry directory");
    let script = registry.join("registry.py");
    // The step runs it with `python3 <script> add ...`, exactly as the real
    // one is run.
    fs::write(
        &script,
        "import sys\nprint(f\"{sys.argv[1]}: 2 added, 0 updated, 0 retired\")\n",
    )
    .expect("registry script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
    // One unrelated row, so the table reads as a registry table.
    fs::write(
        registry.join("testnet.jsonl"),
        format!(
            "{{\"kind\":\"wasm\",\"network\":\"testnet\",\"hash\":\"{}\",\"artifact\":\"pool\",\"version\":\"v0.0.1\",\"source\":{{}}}}\n",
            "cd".repeat(32)
        ),
    )
    .expect("registry table");
    checkout
}

/// The registry step's report, its notice and the pull-request instructions
/// are human lines. A robot run keeps stdout for its JSON event stream, and a
/// consumer that had to filter them out would be parsing prose.
#[test]
fn robot_stdout_stays_json_through_the_registry_step() {
    let (t, cfg) = target("registry-notices");
    let out = t.dir.join("run");
    fs::create_dir_all(&out).expect("run directory");
    write_completed_real_run(&out);
    let registry = fake_registry_checkout(&t.dir);
    let output = run(
        &t,
        &[
            "--out",
            out.to_str().unwrap(),
            "market",
            "deploy",
            "--config",
            cfg.to_str().unwrap(),
            "--network",
            "testnet",
            "--resume",
            "--admin-identity",
            "admin",
            "--admin-config-dir",
            t.dir.to_str().unwrap(),
            "--registry",
            registry.to_str().unwrap(),
            "--robot",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Every line on stdout is one JSON object; this is the assertion, and the
    // registry step is the last thing a finished run prints.
    let _ = json_lines(&stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in [
        "add: 2 added, 0 updated, 0 retired",
        "registry: recording into",
        "registry updated, not committed. To PR it:",
        "git -C",
    ] {
        assert!(
            stderr.contains(line),
            "the registry's human line belongs on stderr ({line}): stdout {stdout}\nstderr {stderr}"
        );
    }
    for line in ["added", "To PR it", "git -C"] {
        assert!(
            !stdout.contains(line),
            "a human line leaked onto robot stdout ({line}): {stdout}"
        );
    }

    // The same step without --robot prints its report exactly where it always
    // has: stdout.
    let out = t.dir.join("run-human");
    fs::create_dir_all(&out).expect("run directory");
    write_completed_real_run(&out);
    let human = run(
        &t,
        &[
            "--out",
            out.to_str().unwrap(),
            "market",
            "deploy",
            "--config",
            cfg.to_str().unwrap(),
            "--network",
            "testnet",
            "--resume",
            "--admin-identity",
            "admin",
            "--admin-config-dir",
            t.dir.to_str().unwrap(),
            "--registry",
            registry.to_str().unwrap(),
        ],
    );
    let human_out = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_out.contains("add: 2 added, 0 updated, 0 retired"),
        "non-robot output is unchanged: {human_out}"
    );
    assert!(
        human_out.contains("registry updated, not committed. To PR it:"),
        "non-robot output is unchanged: {human_out}"
    );
    let _ = fs::remove_dir_all(&t.dir);
}

/// Drive one deployment under a real terminal, typing exactly `y` at every
/// confirmation until the run ends. Returns the run's exit code.
fn interactive(t: &Target, cfg: &Path, out: &Path) -> i32 {
    let mut child = Command::new("script")
        .args([
            "-q",
            "-e",
            "-c",
            &format!(
                "'{BIN}' --out '{}' market deploy --market '{}' --admin-identity admin \
                 --admin-config-dir '{}'",
                out.display(),
                cfg.display(),
                t.dir.display()
            ),
        ])
        .arg(t.dir.join("typescript"))
        .env("STELLAR_BIN", &t.stellar)
        .env("ABTEST_ARGV_LOG", t.dir.join("argv.log"))
        .env("ABTEST_ADMIN", &t.admin)
        .env("ABTEST_CONTRACTS", &t.contracts)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("script(1) available");
    let until = Instant::now() + Duration::from_secs(240);
    loop {
        if let Some(tx) = child.stdin.as_mut() {
            let _ = tx.write_all(b"y\n");
            let _ = tx.flush();
        }
        if let Ok(Some(status)) = child.try_wait() {
            return status.code().unwrap_or(-1);
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the interactive run never finished; typescript:\n{}",
                capture(&t.dir)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn capture(dir: &Path) -> String {
    fs::read_to_string(dir.join("typescript")).unwrap_or_default()
}

/// A pool config in the shape `precept/registry/configs/<pool>.json` uses,
/// with both proxy roles reused: such a run deploys no proxy code, so it
/// resolves no release and needs no network to start. What it exercises here
/// is everything the command prints before its first gate.
fn real_pool_config(dir: &Path) -> PathBuf {
    real_pool_config_owned(dir, ADMIN_G)
}

/// The same pool config owned by `pool_admin`, which is the only thing a
/// run asks about before it decides whether it owes anyone a handoff.
fn real_pool_config_owned(dir: &Path, pool_admin: &str) -> PathBuf {
    let reserve = |label: &str, asset: &str, index: u32| {
        serde_json::json!({
            "asset": asset,
            "label": label,
            "config": {
                "index": index, "decimals": 7,
                "c_factor": 9_000_000, "l_factor": 9_500_000,
                "util": 8_500_000, "max_util": 9_500_000,
                "r_base": 100_000, "r_one": 500_000,
                "r_two": 5_000_000, "r_three": 15_000_000,
                "reactivity": 20,
                "supply_cap": "1000000000000000000",
                "min_position": "10000000", "enabled": true
            }
        })
    };
    let block = |feed: u32| {
        serde_json::json!({
            "min_sources": 1,
            "max_age_secs": 300,
            "sources": [{"kind": "lazer", "feed_id": feed}]
        })
    };
    let shared = "CC64WBDGS6QQP22QTTIACYIXT3WF7BBQEYOQPLTP7GTKYY7PZ74QYGSL";
    let spec = serde_json::json!({
        "name": "abtest-robot-real",
        "network": "testnet",
        "pool": {
            "admin": pool_admin,
            "name": "Abtest Robot Real",
            // An existing adapter and an existing runtime: this run deploys
            // no proxy code and resolves no release, so it starts offline.
            "oracle": shared,
            "backstop_take_rate": 0,
            "max_positions": 4,
            "min_collateral": "10000000000"
        },
        "reserves": [
            reserve("usdc", "CBW5332J2RUTSOIUESSNBNLESPEIEUG6G4LOQOAM6IP3ANORCMN5CPQB", 0),
            reserve("xlm", "CCL3WITWFFXIHV2I52ECV5DPIEOFSTU3PBPR53ILPLF2IP5KHECXRUTY", 1)
        ],
        "target_status": 0,
        "oracle": {
            "lazer_verifier": "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC",
            "runtime": shared,
            "assets": {"usdc": block(100), "xlm": block(200)}
        },
        "notes": []
    });
    let path = dir.join("pool.json");
    fs::write(&path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();
    path
}
