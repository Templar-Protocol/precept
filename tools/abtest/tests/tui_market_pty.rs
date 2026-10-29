//! PTY seam required by the protected-market plan (§5): the real binary
//! runs under a real terminal (`script(1)`), a scripted `stellar`
//! executable is injected through the existing `--stellar-bin`
//! machinery, and every ledger-mutating call is artificially delayed and
//! booked. The tests then prove, from observed process behavior and the
//! injected CLI's own record of what it was asked to execute:
//!
//! * a market deployment reaches a confirmation gate and holds there with
//!   zero mutating calls — no auto-continue; an Enter keystroke changes
//!   nothing and is answered with guidance, never with a mutation;
//! * a normal quit during a mutation waits: the injected CLI books
//!   `slow-completed` before the tool is ever allowed to exit, so a quit
//!   can never abandon or skip the joining of a child or the durable
//!   recording of its classified outcome;
//! * silent headless input never authorizes a mutation.
//!
//! No new dependencies: `script(1)` from util-linux is checked at runtime
//! and each test skips with a printed notice when it is unavailable.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_precept-abtest");

/// Identity material the injected CLI replays. This G-address is the
/// administrator for the fixture run, resolved through the normal
/// `stellar keys` readback so the alias path runs exactly as it does
/// against the real CLI.
const ADMIN_G: &str = "GBDV7KHJMC36EEXW5V75FJCGPIDRPJNEWL7Z7LID3CHY5OAE2NRREWZX";

/// Delay applied to mutating invocations, making quit-during-mutation
/// observable. Bookkeeping lines surround every such call.
const SLOW_SECONDS: u64 = 8;

fn script_available() -> bool {
    Command::new("script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

macro_rules! require_script {
    ($name:expr) => {
        if !script_available() {
            eprintln!(
                "SKIP {}: `script(1)` (util-linux) is unavailable in this environment; \
                 the PTY seam needs a real terminal, and no Rust dependency may be \
                 substituted for it",
                $name
            );
            return;
        }
    };
}

/// Scratch root. The PTY harness quotes commands for `script -c`, which
/// cannot survive whitespace in paths, so the suite fails loudly here
/// instead of silently mis-quoting later.
fn scratch(tag: &str) -> PathBuf {
    // Per-process name: `cargo test` runs these in sibling threads, and a
    // shared root would have one test's cleanup pull the tree out from
    // under another's still-running PTY.
    let root = std::env::temp_dir().join(format!("abtest-pty-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    assert!(
        !root.to_string_lossy().contains(char::is_whitespace),
        "TMPDIR must not contain whitespace for the PTY seam"
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

/// The frozen market configuration shipped with the tool, rewritten only
/// as far as offline determinism requires: artifact paths move into the
/// scratch tree with deterministic bytes and matching sha256 values so
/// the deployment preflight can be satisfied without network access, and
/// the administrator pin is set to the identity this test resolves. The
/// testnet label and passphrase pins, the freshness budgets, and every
/// other safety-bearing field are exactly what a real deployment reads.
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
                        format!("abtest pty fixture {name} deterministic bytes"),
                    )
                    .unwrap();
                }
            }
            last = Some(dest.clone());
            lines.push(format!("path = \"{}\"", dest.display()));
        } else if t.starts_with("version = \"") && rewriting_release {
            // A pinned version would make the offline fixture depend on the
            // live catalog; the path rewrite below ignores it.
            continue;
        } else if let Some(v) = t
            .strip_prefix("release = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            // A release-form table resolves through the live catalog, which
            // the offline contract forbids: rewrite it to the same
            // deterministic path fixture the path form uses.
            let name = format!("{v}.wasm");
            let dest = dir.join(&name);
            if !dest.exists() {
                fs::write(
                    &dest,
                    format!("abtest pty fixture {name} deterministic bytes"),
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
            // The shipped placeholder administrator is replaced with the
            // identity the injected CLI resolves for this run.
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

/// Identity material replayed by the injected CLI: one valid
/// administrator G-address (and an accompanying key shape) so alias
/// resolution proceeds exactly as it does against the real CLI.
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

/// The injected `stellar`: deterministic canned responses, a record of
/// every call it receives, and start/complete booking around a delay on
/// every call that can mutate the ledger.
const SCRIPT: &str = r#"#!/bin/sh
# PTY seam stand-in for the stellar CLI. Deterministic only.
LOG="$ABTEST_ARGV_LOG"
[ -n "$LOG" ] && echo "call $*" >> "$LOG"
mutate() {
    [ -n "$LOG" ] && echo "slow-started" >> "$LOG"
    sleep __SLOW__
    [ -n "$LOG" ] && echo "slow-completed" >> "$LOG"
    echo '{"return":"0"}'
}
if [ "$1" = network ] && [ "$2" = info ]; then
    printf '%s\n' '{"passphrase":"Test SDF Network ; September 2015"}'
    exit 0
fi
case "$*" in
    *--version*) echo 'stellar 26.5.0' ;;
    *--help*|*help*|*ledger*entries*) echo '{}' ;;
    # The operator alias resolves to the administrator. A run-local actor
    # resolves only after `keys generate` created it, as in a fresh config
    # directory, so actor provisioning is a real, booked mutation.
    *keys*public-key*|*keys*show*|*keys*address*|*keys*public*)
        if [ "$3" = admin ]; then grep '^G' "$ABTEST_ADMIN" | head -n 1
        elif [ -s "$ABTEST_ADMIN.$3" ]; then cat "$ABTEST_ADMIN.$3"
        else exit 1; fi
        ;;
    *keys*generate*)
        [ -n "$LOG" ] && echo "slow-started" >> "$LOG"
        sleep __SLOW__
        case "$3" in
            abtest-admin) echo GASFUXQFGKJFVWUCRAHL4WOWE5344SUESN7U7RDLTTJEA5Q4OQWXOWPM ;;
            abtest-frodo) echo GCGUYVGMPHCU6W4372SJ5BY3E75VL4L2U65ON4BQKVSYL4RTVTDCRKUD ;;
            abtest-sam) echo GDEFDFE4DRJFQ6RTY43XQ4GMFPKPDKE3HQZL3HK5LMWFYVH3BNLRFZ2W ;;
            abtest-user) echo GBDQWUISJ2WJP67TV7FXA3J44R7UQRAR477NKTX3A3ICQAIOOEPNN6LB ;;
            *) exit 1 ;;
        esac > "$ABTEST_ADMIN.$3"
        [ -n "$LOG" ] && echo "slow-completed" >> "$LOG"
        ;;
    # Mutations first: the read-only arms below match bare substrings
    # (`*inspect*`, `*explain*`, …), so a mutating argv must never reach
    # them. This arm is the tool's own mutation taxonomy
    # (`deploy.rs` MUTATIONS) plus the install/submit spellings, so
    # `contract upload` and `contract deploy` are booked like every other
    # ledger-changing call instead of falling through to the `ok` default.
    *contract*upload*|*contract*deploy*|*contract*install*|*tx*submit*|*transaction*submit*|*contract*invoke*)
        mutate
        ;;
    *contract*fetch*|*inspect*|*simulate*|*decode*|*explain*) head -n 1 "$ABTEST_CONTRACTS" ;;
    *contract*id*predict*) head -n 1 "$ABTEST_CONTRACTS" ;;
    *network*passphrase*|*passphrase*) echo 'Test SDF Network ; September 2015' ;;
    *) echo 'ok' ;;
esac
exit 0
"#;

fn stellar(dir: &Path) -> PathBuf {
    let p = dir.join("stellar");
    fs::write(&p, SCRIPT.replace("__SLOW__", &SLOW_SECONDS.to_string())).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn argv_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("argv.log")).unwrap_or_default()
}

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

/// Starts the real binary under a real PTY, with the injected `stellar`
/// as the only executable the market machinery can reach.
fn pty<'a, I: IntoIterator<Item = &'a str>>(dir: &Path, args: I) -> Child {
    let quoted = args
        .into_iter()
        .map(|a| format!("'{a}'"))
        .collect::<Vec<_>>()
        .join(" ");
    Command::new("script")
        .args(["-q", "-e", "-c", &format!("'{BIN}' {quoted}")])
        .arg(dir.join("typescript"))
        .env("ABTEST_ARGV_LOG", dir.join("argv.log"))
        .env("ABTEST_ADMIN", dir.join("admin-key.txt"))
        .env("ABTEST_CONTRACTS", dir.join("contracts.txt"))
        .env("STELLAR_BIN", stellar(dir))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("script(1) available")
}

fn capture(dir: &Path) -> String {
    fs::read_to_string(dir.join("typescript")).unwrap_or_default()
}

/// Polls `probe` until it holds; false on timeout.
fn wait_until(secs: u64, probe: impl Fn() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if probe() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Polls the PTY capture until `needle` appears; false on timeout.
fn wait_for(dir: &Path, needle: &str, secs: u64) -> bool {
    wait_until(secs, || capture(dir).contains(needle))
}

fn send(child: &mut Child, bytes: &[u8]) {
    if let Some(tx) = child.stdin.as_mut() {
        let _ = tx.write_all(bytes);
        let _ = tx.flush();
    }
}

fn exited(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(Some(_)))
}

fn wait_exit(child: &mut Child, secs: u64) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if exited(child) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn kill(mut child: Child) {
    if !exited(&mut child) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn deploy_args(cfg: &Path, out: &Path) -> Vec<String> {
    vec![
        "--out".to_string(),
        out.to_string_lossy().into_owned(),
        "market".to_string(),
        "tui".to_string(),
        "--mode".to_string(),
        "deploy".to_string(),
        "--market".to_string(),
        cfg.to_string_lossy().into_owned(),
        "--admin-identity".to_string(),
        "admin".to_string(),
        "--admin-config-dir".to_string(),
        cfg.parent().unwrap().to_string_lossy().into_owned(),
    ]
}

/// Deploy under a real terminal. The market state machine must halt at a
/// confirmation gate and hold there: zero mutating calls while the gate
/// is unconfirmed, Enter alone changes nothing and is answered with
/// guidance, and the operator can always leave.
#[test]
fn market_gate_holds_with_zero_mutations_and_enter_never_fires() {
    require_script!("market_gate_holds_with_zero_mutations_and_enter_never_fires");
    let dir = scratch("gate");
    identities(&dir);
    let cfg = market_config(&dir);
    let out = dir.join("run");
    let args = deploy_args(&cfg, &out);
    let mut child = pty(&dir, args.iter().map(String::as_str));
    assert!(
        wait_for(&dir, "MUTATION AWAITING CONFIRMATION", 240),
        "the deployment must reach a confirmation gate and hold there; capture:\n{}",
        capture(&dir)
    );
    // No auto-continue: while the gate is open the injected CLI must
    // never even be asked for a mutating call.
    assert_eq!(
        count(&argv_log(&dir), "slow-started"),
        0,
        "a mutation was attempted while the gate was unconfirmed: {}",
        argv_log(&dir)
    );
    // Enter alone never fires. Send only an Enter rhythm: it must not
    // release the gate, and it must not queue into any later action.
    send(&mut child, b"\r\n");
    thread::sleep(Duration::from_secs(5));
    assert_eq!(
        count(&argv_log(&dir), "slow-started"),
        0,
        "Enter released a mutation: {}",
        argv_log(&dir)
    );
    assert!(
        capture(&dir).contains("never fires"),
        "Enter must be answered with guidance, not with progress; capture:\n{}",
        capture(&dir)
    );
    // The gate holds indefinitely and the operator can always leave.
    send(&mut child, b"q");
    assert!(
        wait_exit(&mut child, 60),
        "quit from an unconfirmed gate must be prompt; capture:\n{}",
        capture(&dir)
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Quitting during a mutation must not abandon the child. The injected
/// CLI always books `slow-completed`; the tool may only exit after that
/// booking exists. An exit while a mutation is outstanding is the crash
/// path, not a quit, and must fail this test.
#[test]
fn market_quit_waits_for_the_joined_child_and_records_the_outcome() {
    require_script!("market_quit_waits_for_the_joined_child_and_records_the_outcome");
    let dir = scratch("quit");
    identities(&dir);
    let cfg = market_config(&dir);
    let out = dir.join("run");
    let args = deploy_args(&cfg, &out);
    let mut child = pty(&dir, args.iter().map(String::as_str));
    // The only way a mutation is ever released is an operator `y` at this
    // gate, so the run must be seen waiting there first and must not have
    // touched the ledger before the keystroke. Quitting at an open gate is
    // the gate test's property; here the property under test is the join,
    // which needs a child that actually exists.
    assert!(
        wait_for(&dir, "MUTATION AWAITING CONFIRMATION", 240),
        "the deployment must reach a confirmation gate; capture:\n{}",
        capture(&dir)
    );
    assert_eq!(
        count(&argv_log(&dir), "slow-started"),
        0,
        "a mutating call was made before any confirmation: {}",
        argv_log(&dir)
    );
    send(&mut child, b"y");
    assert!(
        wait_until(240, || argv_log(&dir).contains("slow-started")),
        "the confirmed mutation was never booked by the injected CLI; capture:\n{}",
        capture(&dir)
    );
    send(&mut child, b"q");
    // Still no exit: the mutation must run to completion and be joined.
    thread::sleep(Duration::from_secs(2));
    assert!(
        !exited(&mut child),
        "quit abandoned a mutating child; argv log:\n{}",
        argv_log(&dir)
    );
    assert!(
        wait_until(3 * (SLOW_SECONDS + 10), || argv_log(&dir)
            .contains("slow-completed")),
        "the child's completion was never observed before exit; argv log:\n{}",
        argv_log(&dir)
    );
    assert!(
        wait_exit(&mut child, 120),
        "after the mutation settled the tool must exit; capture:\n{}",
        capture(&dir)
    );
    let log = argv_log(&dir);
    assert_eq!(
        count(&log, "slow-started"),
        count(&log, "slow-completed"),
        "bookkeeping mismatch: {}",
        log
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Headless surface: a deployment may never mutate on its own when its
/// operator input is silent. Repeated Enter keystrokes are not a
/// confirmation: the run must end with zero mutating calls and must
/// never claim success over an unsettled run.
#[test]
fn headless_deployment_never_mutates_on_empty_input() {
    require_script!("headless_deployment_never_mutates_on_empty_input");
    let dir = scratch("headless");
    identities(&dir);
    let cfg = market_config(&dir);
    let out = dir.join("run");
    let mut child = pty(
        &dir,
        [
            "--out",
            out.to_str().unwrap(),
            "market",
            "deploy",
            "--market",
            cfg.to_str().unwrap(),
            "--admin-identity",
            "admin",
            "--admin-config-dir",
            cfg.parent().unwrap().to_str().unwrap(),
        ],
    );
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until {
        send(&mut child, b"\r\n");
        thread::sleep(Duration::from_millis(500));
        if exited(&mut child) {
            break;
        }
    }
    let done = wait_exit(&mut child, 120);
    kill(child);
    let log = argv_log(&dir);
    assert!(done, "silent headless input must terminate the run");
    assert_eq!(
        count(&log, "slow-started"),
        0,
        "empty input produced a mutating call: {log}"
    );
    // An empty terminal is never proof of deployment: if a manifest was
    // published it belongs to a reconciled deployment, and nothing above
    // may have touched the ledger without a confirmation.
    let manifest = out.join("market-manifest.toml");
    if manifest.exists() {
        let text = fs::read_to_string(&manifest).unwrap();
        assert!(
            text.contains(ADMIN_G),
            "a published manifest must name the reconciled administrator"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}
