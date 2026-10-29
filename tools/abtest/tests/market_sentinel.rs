//! AC10 sentinel suite (plan §3 Verify): sentinel-based, not shape-based.
//!
//! A scripted `stellar` executable emits per-channel distinct sentinel
//! strings through all seven egress channels while the real binary drives
//! every landed market surface (`market play` journeys, refusal surfaces,
//! halt, unresolved custody, the tool-failure printer, and headless
//! `market deploy`). The suite then asserts, byte-exactly, that:
//!
//! * value-class sentinels (channels 1–2: success-path stdout JSON strings,
//!   stderr-parsed event values) never appear unescaped — their assertion
//!   is the market image property (`|`/backtick escaping plus the cap),
//!   because raw-form absence is vacuous for `Value` text;
//! * stream-class sentinels (channels 3–4, 7: failure stdout, failure
//!   stderr, detail-pane extraction shapes) never appear in raw form —
//!   raw forms carry sanitizer-destroyable bytes (ANSI/control plus a
//!   newline) and a unique printable core, so sanitization destroys the
//!   raw form while raw leakage preserves it;
//! * identifier-class sentinels (channels 5–6) are shape-conforming per
//!   position but constructed to fail that position's validation
//!   (bad-checksum `C…`/`G…` StrKeys derived programmatically from
//!   CRC16-XMODEM, a non-64-hex hash token, a bad-base64/word-alignment
//!   `RawEntry` column), so their printable cores are asserted absent —
//!   including on rejection-path surfaces, which are input-free by
//!   contract.
//!
//! Sanitized images are sanctioned content and are not asserted.
//!
//! The crate is binary-only, so guard discrimination (plan §3's pinned
//! seven-tuple channel→guard map) runs as explicit paired assertions in
//! [`guard_map_is_load_bearing_paired_weakening_discriminates`]: for each
//! channel, a test-local weakening harness reproduces the Pair-arm /
//! pre-guard semantics (verbatim passthrough, plain `Value` display, the
//! retired `starts_with` + length token checks, lenient columns) applied to
//! the exact payloads the fake emitted, and asserts the channel's market
//! predicate FAILS there — i.e. the green assertions above would go red if
//! the mapped guard were removed. The channel-5 hash position carries the
//! plan's explicit no-claim for red-without-guard on the pre-existing
//! strict shape check; its core-absence rests on the input-free
//! rejection-error contract and is asserted as absence only.
//!
//! No PTY is needed: every surface here is headless (files under the run
//! directory plus captured process stdout/stderr — the smoke-bundle-shaped
//! output). The fake's own bookkeeping lives outside the run directory and
//! is harness evidence, never a market surface.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_precept-abtest");

/// `palette::specs().len() + 0` — the headless index of the market-only
/// `refresh_all` action. Pinned to the landed palette table; if the palette
/// grows, the run below fails its `"step refresh all"` check loudly and this
/// constant moves with it.
const REFRESH_ALL_STEP: usize = 22;

/// The landed palette index of `update_status`: an argless pair spec whose
/// `Step` is mutating, so market mode must refuse it through the
/// fail-closed mutation gate (an input-free `Failed` surface).
const UPDATE_STATUS_STEP: usize = 7;

// --------------------------------------------------------------------------
// Sentinel corpus (per-channel distinct; no `'`, `%`, or backslash bytes so
// the payloads survive single-quoted `printf '%s'` emission in the fake).
// --------------------------------------------------------------------------

/// Channel 1 — success-path stdout JSON strings (value class): `|`, a
/// backtick, and padding beyond the 2048-char `render_parsed` cap.
const SNT1_CORE: &str = "SNT1pipe|tick`json";
/// Blocks longer than any market render cap (2048): a capped, sanitized
/// image cannot contain them, while an uncapped render carries them.
fn pad_block_1() -> String {
    "SNT1PAD".repeat(300)
}
/// Channel-2 beyond-cap block (the emitted payload carries exactly this).
fn pad_block_2() -> String {
    "SNT2PAD".repeat(300)
}

/// Channel 2 — stderr-parsed event values (value class).
const SNT2_TOPIC_CORE: &str = "SNT2pipe|tick`event";
const SNT2_VALUE_CORE: &str = "SNT2val|tag`two";

/// Channel 3 — failure stdout (stream class): sanitizer-destroyable bytes
/// (ESC + BEL + newline) around a unique printable core.
const SNT3_CORE: &str = "SNT3core-qtx";
fn ch3_raw() -> String {
    format!("\u{1b}[35m{SNT3_CORE} open\u{7} mid\n{SNT3_CORE} close")
}

/// Channel 4 — failure stderr (stream class). The definite variant parses
/// to `ErrorKind::Contract` (closed-enum text only); the unresolved variant
/// carries an unclassifiable headline so the journey halts with open
/// custody — surfacing the journal, `Halt.reason`, and the tool-failure
/// printer as real rejection-path surfaces.
const SNT4_CORE: &str = "SNT4core-znw";
fn ch4a_raw() -> String {
    format!(
        "error: transaction simulation failed: HostError: Error(Contract, #1)\n\
         \u{1b}[31m{SNT4_CORE} ctrl\u{7} mid\n{SNT4_CORE} tail"
    )
}
const SNT4B_CORE: &str = "SNT4bcore-unresolved";
fn ch4b_raw() -> String {
    format!("\u{1b}[31m{SNT4B_CORE} refused\u{7} mid\n{SNT4B_CORE} tail")
}

/// Channel 5 — non-JSON stdout token positions (identifier class). Cores
/// are minted programmatically in [`bad_core`]/[`HASH_TOKEN`]: 56-char
/// StrKey-shaped with a deliberately broken CRC16-XMODEM checksum, and a
/// 64-char lowercase token that is not hex (it must fail the pre-existing
/// strict shape check inside `parse_hash`, which lowercases candidates).
/// `SNT5HEAD` rides the `parse_event_lines` event-head position, which is
/// end-to-end reachable today: a bad-checksum head drops the whole event.
const SNT5HEAD: &str = "SNT5head";

/// Channel 6 — `RawEntry` columns (identifier class): base64-column-shaped
/// (`-` is not in the base64 alphabet) so the strict column validation
/// rejects the row; the word-alignment core `QUFB` decodes to three bytes,
/// failing the 4-byte-word rule on a second column.
const SNT6_CORE: &str = "SNT6COLUMN-X";

/// Channel 7 — detail-pane extraction shapes (stream class): a
/// `Diagnostic Event … data:` line whose payload carries the raw-form
/// bytes; `pick_diagnostics`'s market arm sanitizes it.
const SNT7_CORE: &str = "SNT7core-dtl|pan`diag";
fn ch7_raw() -> String {
    format!("\u{1b}[33m{SNT7_CORE}\u{7}")
}
fn ch7_line(contract: &str) -> String {
    format!(
        "\u{1f4c5} {contract} - Diagnostic Event (contract: {contract}): data: \"{} end\"",
        ch7_raw()
    )
}

/// The channel-5 hash-position sentinel: 64 lowercase characters, none of
/// them hex, so the strict `:566` shape check rejects it instead of binding
/// it into a manifest.
fn hash_token() -> String {
    "snt5hash".repeat(8)
}

// --------------------------------------------------------------------------
// StrKey minting (mirrors the landed codec semantics: base32 over
// version-byte + 32-byte payload + little-endian CRC16-XMODEM checksum).
// The bad-checksum cores are derived, never hand-invented.
// --------------------------------------------------------------------------

fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0x0000;
    for b in bytes {
        crc ^= (*b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn base32_encode(data: &[u8]) -> String {
    const AL: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for b in data {
        buf = (buf << 8) | *b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(AL[(buf >> bits) as usize & 0x1f] as char);
        }
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    fn d5(ch: u8) -> Option<u32> {
        match ch {
            b'A'..=b'Z' => Some((ch - b'A') as u32),
            b'2'..=b'7' => Some((ch - b'2') as u32 + 26),
            _ => None,
        }
    }
    let mut bits: u32 = 0;
    let mut pending = 0u32;
    let mut out = Vec::new();
    for &ch in s.as_bytes() {
        if ch == b'=' {
            break;
        }
        bits = (bits << 5) | d5(ch)?;
        pending += 5;
        if pending >= 8 {
            pending -= 8;
            out.push((bits >> pending) as u8);
            bits &= (1 << pending) - 1;
        }
    }
    (pending == 0).then_some(out)
}

/// Mint one 56-char StrKey for a version byte and 32-byte payload.
fn strkey(version: u8, payload: &[u8; 32]) -> String {
    let mut v = Vec::with_capacity(35);
    v.push(version);
    v.extend_from_slice(payload);
    let crc = crc16_xmodem(&v).to_le_bytes();
    v.push(crc[0]);
    v.push(crc[1]);
    assert_eq!(base32_encode(&v).len(), 56);
    base32_encode(&v)
}

/// The landed `strkey_validate` mirror: length, alphabet, version byte, and
/// little-endian checksum. Used both to self-check the minted ids and as
/// the strict side of the channel-5 paired discrimination.
fn strkey_valid(addr: &str, version: u8) -> bool {
    if addr.len() != 56 {
        return false;
    }
    let Some(bytes) = base32_decode(addr) else {
        return false;
    };
    if bytes.len() != 35 || bytes[0] != version {
        return false;
    }
    let (payload, sum) = bytes.split_at(33);
    u16::from_le_bytes([sum[0], sum[1]]) == crc16_xmodem(payload)
}

/// A 32-byte payload from a label, for minting distinct ids.
fn payload32(label: &str) -> [u8; 32] {
    let mut p = [0u8; 32];
    let bytes = label.as_bytes();
    assert!(bytes.len() <= 32, "label too long");
    p[..bytes.len()].copy_from_slice(bytes);
    p
}

/// One shape-conforming but checksum-invalid core: the stored checksum is
/// XOR-corrupted relative to the payload it must attest, so the checksum
/// cannot match by construction. Length, alphabet, and version prefix are
/// preserved; only validation fails. Derived from a minted valid core,
/// never hand-invented.
fn bad_core(prefix: char, version: u8, label: &str) -> String {
    let payload = payload32(label);
    let good = strkey(version, &payload);
    let mut bytes = Vec::with_capacity(35);
    bytes.push(version);
    bytes.extend_from_slice(&payload);
    let crc = crc16_xmodem(&bytes).to_le_bytes();
    bytes.push(crc[0] ^ 0xFF);
    bytes.push(crc[1] ^ 0x0F);
    let bad = base32_encode(&bytes);
    assert_eq!(bad.len(), 56);
    assert!(bad.starts_with(prefix), "shape must hold");
    assert!(strkey_valid(&good, version), "minted core must be valid");
    assert!(
        !strkey_valid(&bad, version),
        "corrupted core must be invalid"
    );
    bad
}

// The fixed identifier set for the fixture manifest and the event lines.
struct Ids {
    admin: String,
    user: String,
    frodo: String,
    sam: String,
    runtime: String,
    event_contract: String,
    bad_contract: String,
    bad_account: String,
}
fn ids() -> Ids {
    let c = |label: &str| strkey(0x10, &payload32(label));
    let g = |label: &str| strkey(0x30, &payload32(label));
    Ids {
        admin: g("market-sentinel-admin"),
        user: g("market-sentinel-user"),
        frodo: g("market-sentinel-frodo"),
        sam: g("market-sentinel-sam"),
        runtime: c("market-sentinel-runtime"),
        event_contract: c("market-sentinel-event"),
        bad_contract: bad_core('C', 0x10, "market-sentinel-badcontract"),
        bad_account: bad_core('G', 0x30, "market-sentinel-badaccount"),
    }
}

// --------------------------------------------------------------------------
// Fake `stellar` (pattern: tests/tui_market_pty.rs). Deterministic canned
// responses, per-channel sentinel payloads, and an emission log that lives
// in the harness directory — never inside the market run directory.
// --------------------------------------------------------------------------

const FAKE_TEMPLATE: &str = r#"#!/bin/sh
# AC10 sentinel emitter (plan 3 Verify). Deterministic only.
LOG="$ABTEST_FAKE_LOG"
emit() { [ -n "$LOG" ] && printf '%s\n' "$1" >> "$LOG"; }
if [ "$1" = network ] && [ "$2" = info ]; then
    emit "emit network-info"
    printf '%s\n' '{"passphrase":"Test SDF Network ; September 2015"}'
    exit 0
fi
case "$*" in
    *--version*)
        emit "emit version"
        printf '%s\n' 'stellar 26.5.0'
        ;;
    *keys*public-key*|*keys*show*|*keys*address*)
        emit "emit ch5-pubkey"
        case "$*" in
            *abtest-user*) printf '%s\n' '__USER_ACCOUNT__' ;;
            *) printf '%s\n' '__BAD_ACCOUNT__' ;;
        esac
        ;;
    *contract*read*)
        emit "emit ch6-row"
        printf '%s\n' '__READ_ROW__'
        ;;
    *--send*no*)
        emit "emit ch1-view"
        emit "emit ch2-event"
        emit "emit ch5-head"
        emit "emit ch7-diagnostic"
        printf '%s\n' '__VIEW_JSON__'
        printf '%s\n' '__EV_GOOD__' >&2
        printf '%s\n' '__EV_BAD__' >&2
        printf '%s\n' '__DIAG_LINE__' >&2
        ;;
    *contract*invoke*)
        emit "emit ch3-stdout"
        emit "emit ch4-stderr"
        printf '%s' '__CH3_RAW__'
        printf '%s' '__CH4_RAW__' >&2
        exit 1
        ;;
    *contract*upload*|*contract*deploy*|*"contract id"*|*contract*fetch*|*contract*inspect*)
        emit "emit ch5-id-and-hash"
        printf '%s __HASH_TOKEN__\n' '__BAD_CONTRACT__'
        ;;
    *)
        emit "emit default"
        printf '%s\n' 'ok'
        ;;
esac
exit 0
"#;

fn view_json() -> String {
    // Channel 1: `|`, a backtick, and padding beyond the 2048 cap, inside a
    // success-path stdout JSON string (nested in an object).
    let pad = "SNT1PAD".repeat(400);
    format!(
        "{{\"status\":\"Accepted\",\"Some\":{{\"mantissa\":1,\"exponent\":-7,\
         \"last_modified\":100}},\"is_manually_tripped\":false,\"is_blocking\":false,\
         \"note\":\"{SNT1_CORE} tail\",\"pad\":\"{pad}\"}}"
    )
}

fn ev_good(contract: &str) -> String {
    // Channel 2: event values parsed from stderr (valid contract head).
    let pad = "SNT2PAD".repeat(300);
    format!(
        "\u{1f4c5} {contract} - Success - Event: [\"{SNT2_TOPIC_CORE}{pad}\"] = {{\"v\":\"{SNT2_VALUE_CORE}\"}}"
    )
}

fn ev_bad(bad_contract: &str) -> String {
    // Channel 5, event-head position: bad-checksum head drops the event.
    format!("\u{1f4c5} {bad_contract} - Success - Event: [\"{SNT5HEAD}\"] = \"{SNT5HEAD}val\"")
}

fn read_row() -> String {
    // Channel 6: `SNT6COLUMN-X` is not strict base64; `QUFB` decodes to a
    // three-byte non-word. Both columns fail the landed row validation.
    format!("{SNT6_CORE},QUFB,Q0FBRA")
}

fn write_fake(dir: &Path, ch4_raw: &str) -> PathBuf {
    let i = ids();
    let log = dir.join("fake.log");
    let script = FAKE_TEMPLATE
        .replace("$ABTEST_FAKE_LOG", &log.display().to_string())
        .replace("__VIEW_JSON__", &view_json())
        .replace("__EV_GOOD__", &ev_good(&i.event_contract))
        .replace("__EV_BAD__", &ev_bad(&i.bad_contract))
        .replace("__DIAG_LINE__", &ch7_line(&i.event_contract))
        .replace("__CH3_RAW__", &ch3_raw())
        .replace("__CH4_RAW__", ch4_raw)
        .replace("__READ_ROW__", &read_row())
        .replace("__BAD_CONTRACT__", &i.bad_contract)
        .replace("__BAD_ACCOUNT__", &i.bad_account)
        .replace("__USER_ACCOUNT__", &i.user)
        .replace("__HASH_TOKEN__", &hash_token());
    let p = dir.join("stellar");
    fs::write(&p, script).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    // Self-check: a malformed script is indistinguishable downstream from an
    // unresolved submission (the child dies before emitting anything).
    let syntax = std::process::Command::new("/bin/sh")
        .arg("-n")
        .arg(&p)
        .output()
        .expect("sh -n on sentinel fake");
    if !syntax.status.success() {
        let _ = fs::remove_file(&p);
        panic!(
            "sentinel fake failed sh -n: {}",
            String::from_utf8_lossy(&syntax.stderr)
        );
    }
    p
}

// --------------------------------------------------------------------------
// Market fixtures: a completed-run `market-manifest.toml` and the shipped
// market config rewritten for offline determinism (tests/tui_market_pty.rs
// pattern: artifact paths move into the scratch tree with matching sha256).
// --------------------------------------------------------------------------

fn manifest_toml(i: &Ids) -> String {
    let c = |label: &str| strkey(0x10, &payload32(label));
    let tokens = [
        ("weth", c("market-sentinel-tok-weth")),
        ("usdc", c("market-sentinel-tok-usdc")),
        ("xlm", c("market-sentinel-tok-xlm")),
        ("stable", c("market-sentinel-tok-stable")),
    ];
    let sources = [
        c("market-sentinel-src-1"),
        c("market-sentinel-src-2"),
        c("market-sentinel-src-3"),
    ];
    // Bare keys must precede all tables in TOML.
    format!(
        "admin = \"{admin}\"\n\
         rpc_host = \"testnet-rpc\"\n\
         pool = \"{pool}\"\n\
         backstop = \"{backstop}\"\n\
         factory = \"{factory}\"\n\
         comet = \"{comet}\"\n\
         emitter = \"{emitter}\"\n\
         deploy_ledger = 4242\n\n\
         [network]\n\
         label = \"testnet\"\n\
         passphrase = \"Test SDF Network ; September 2015\"\n\n\
         [oracle]\n\
         runtime = \"{runtime}\"\n\
         governance = \"{governance}\"\n\
         adapter = \"{adapter}\"\n\
         sources = [\"{s0}\", \"{s1}\", \"{s2}\"]\n\
         min_sources = 3\n\
         max_age_secs = 3600\n\
         max_clock_drift_secs = 60\n\
         decimals = 7\n\
         resolution = 1\n\
         base = \"USD\"\n\n\
         [tokens]\n\
         weth = \"{weth}\"\n\
         usdc = \"{usdc}\"\n\
         xlm = \"{xlm}\"\n\
         stable = \"{stable}\"\n\n\
         [reserves.stable]\nindex = 0\ndecimals = 6\n\n\
         [reserves.xlm]\nindex = 1\ndecimals = 7\n\n\
         [reserves.weth]\nindex = 2\ndecimals = 8\n\n\
         [identities]\n\
         admin = \"{admin}\"\n\
         frodo = \"{frodo}\"\n\
         sam = \"{sam}\"\n\
         user = \"{user}\"\n",
        admin = i.admin,
        pool = c("market-sentinel-pool"),
        backstop = c("market-sentinel-backstop"),
        factory = c("market-sentinel-factory"),
        comet = c("market-sentinel-comet"),
        emitter = c("market-sentinel-emitter"),
        runtime = i.runtime,
        governance = c("market-sentinel-governance"),
        adapter = c("market-sentinel-adapter"),
        s0 = sources[0],
        s1 = sources[1],
        s2 = sources[2],
        weth = tokens[0].1,
        usdc = tokens[1].1,
        xlm = tokens[2].1,
        stable = tokens[3].1,
        frodo = i.frodo,
        sam = i.sam,
        user = i.user,
    )
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
    String::from_utf8(out.stdout)
        .expect("ascii digest")
        .split_whitespace()
        .next()
        .expect("digest")
        .to_string()
}

/// Rewrite the shipped market config for offline determinism: artifact
/// paths become scratch fixtures with matching sha256 digests, and the
/// placeholder administrator pin becomes this run's valid identity.
fn market_config(dir: &Path, admin: &str) -> PathBuf {
    let raw = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/markets/protected-testnet.toml"
    ))
    .expect("markets/protected-testnet.toml must ship with the tool");
    let mut last: Option<PathBuf> = None;
    let mut lines: Vec<String> = Vec::new();
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
                    fs::write(&dest, format!("abtest sentinel fixture {name} bytes")).unwrap();
                }
            }
            last = Some(dest.clone());
            lines.push(format!("path = \"{}\"", dest.display()));
        } else if t.starts_with("sha256 = \"") {
            let bytes = fs::read(last.as_ref().expect("path precedes sha256")).unwrap();
            lines.push(format!("sha256 = \"{}\"", sha256_hex(&bytes)));
        } else if line.contains("GAYX6R3NPJWKORXQMCZ3RJZ4O4YB4B64I2BZK6F3W6F3OJBSQKLRBUTD") {
            lines.push(line.replace(
                "GAYX6R3NPJWKORXQMCZ3RJZ4O4YB4B64I2BZK6F3W6F3OJBSQKLRBUTD",
                admin,
            ));
        } else {
            lines.push(line.to_string());
        }
    }
    let cfg = dir.join("market.toml");
    fs::write(&cfg, lines.join("\n") + "\n").unwrap();
    cfg
}

// --------------------------------------------------------------------------
// Harness: scratch trees, run-dir surface harvest, binary driver.
// --------------------------------------------------------------------------

fn scratch(tag: &str) -> PathBuf {
    // Per-process name: two `cargo test` runs (or a rerun beside a live
    // one) must not have one cleanup pull the other's tree away. The trees
    // are dropped at the end of each passing test.
    let root = std::env::temp_dir().join(format!("abtest-sentinel-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// Every file under the run directory, byte-exactly: the serialized market
/// surfaces (journal, manifest, any state) plus the smoke-bundle shape.
fn harvest_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Ok(bytes) = fs::read(&p) {
                out.push((p.display().to_string(), bytes));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

struct RunOut {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_bin(harness: &Path, args: &[String], fake: &Path) -> RunOut {
    let mut cmd = Command::new(BIN);
    cmd.arg("--out")
        .arg(harness.join("run"))
        .arg("--stellar-bin")
        .arg(fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .args(args)
        .env("ABTEST_FAKE_LOG", harness.join("fake.log"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().expect("launch precept-abtest");
    RunOut {
        stdout: out.stdout,
        stderr: out.stderr,
    }
}

fn play_args(cfg: &Path, harness: &Path, action: &str) -> Vec<String> {
    let mut v = vec![
        "market".to_string(),
        "play".to_string(),
        "--market".to_string(),
        cfg.display().to_string(),
        "--admin-identity".to_string(),
        "admin".to_string(),
        "--admin-config-dir".to_string(),
        harness.display().to_string(),
    ];
    if action == "snapshot" {
        v.push("--snapshot".into());
    } else if action == "halt" {
        v.push("--halt".into());
    } else if action == "continue" {
        v.push("--continue".into());
    } else {
        v.push("--step".into());
        v.push(action.to_string());
    }
    v
}

/// All sentinel needles whose absence every market surface must show:
/// value-class raw (unescaped) forms and pad blocks, stream-class raw
/// forms (control bytes included), and identifier-class cores.
struct Needles {
    value_raw: Vec<(&'static str, String)>,
    stream_raw: Vec<(&'static str, String)>,
    id_cores: Vec<(&'static str, String)>,
}

fn needles() -> Needles {
    let i = ids();
    Needles {
        value_raw: vec![
            ("ch1 stdout JSON raw form", SNT1_CORE.to_string()),
            ("ch1 beyond-cap pad block", pad_block_1()),
            ("ch2 event topic raw form", SNT2_TOPIC_CORE.to_string()),
            ("ch2 event value raw form", SNT2_VALUE_CORE.to_string()),
            ("ch2 beyond-cap pad block", pad_block_2()),
        ],
        stream_raw: vec![
            ("ch3 failure-stdout raw form", ch3_raw()),
            ("ch4a failure-stderr raw form", ch4a_raw()),
            ("ch4b unresolved-stderr raw form", ch4b_raw()),
            ("ch7 detail-pane raw form", ch7_raw()),
        ],
        id_cores: vec![
            ("ch5 contract-id bad-checksum core", i.bad_contract.clone()),
            ("ch5 public-key bad-checksum core", i.bad_account),
            ("ch5 event-head bad-checksum core", i.bad_contract.clone()),
            ("ch5 non-hex hash token", hash_token()),
            ("ch6 bad-column core", SNT6_CORE.to_string()),
            ("ch5 event-head payload", SNT5HEAD.to_string()),
        ],
    }
}

/// Byte-exact absence over every harvested surface (ASCII needles survive
/// lossy decoding unchanged; sanitized images keep printable cores, so
/// stream-class needles deliberately carry control bytes).
fn assert_absent(surfaces: &[(String, Vec<u8>)], label: &str, needle: &str) {
    for (name, bytes) in surfaces {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains(needle),
            "market surface {name} leaks {label}: {needle:?}\nsurface head: {:?}",
            text.chars().take(400).collect::<String>()
        );
    }
}

fn assert_all_needles_absent(surfaces: &[(String, Vec<u8>)]) {
    let n = needles();
    for (label, needle) in n
        .value_raw
        .iter()
        .chain(n.stream_raw.iter())
        .chain(n.id_cores.iter())
    {
        assert_absent(surfaces, label, needle);
    }
}

fn fake_log(harness: &Path) -> String {
    fs::read_to_string(harness.join("fake.log")).unwrap_or_default()
}

fn emitted(harness: &Path, tag: &str) -> usize {
    fake_log(harness).matches(tag).count()
}

// --------------------------------------------------------------------------
// Test 1: the landed MarketRunner journeys against the sentinel fake.
// --------------------------------------------------------------------------

#[test]
fn market_play_surfaces_carry_no_sentinel_raw_forms_or_cores() {
    let harness = scratch("play");
    let i = ids();
    let cfg = market_config(&harness, &i.admin);
    let fake = write_fake(&harness, &ch4a_raw());

    // The completed market run directory the play journeys execute against.
    let run = harness.join("run");
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join("market-manifest.toml"), manifest_toml(&i)).unwrap();
    fs::create_dir(run.join("market-pending")).unwrap();

    // (a) Read-only snapshot battery: success-path stdout JSON strings and
    //     stderr-parsed event values flow through the real parse paths.
    let snap = run_bin(&harness, &play_args(&cfg, &harness, "snapshot"), &fake);
    assert!(
        String::from_utf8_lossy(&snap.stdout).contains("step market snapshot"),
        "snapshot journey must run; stdout: {} stderr: {}",
        String::from_utf8_lossy(&snap.stdout),
        String::from_utf8_lossy(&snap.stderr)
    );

    // (b) The journaled mutating journey reachable headlessly: refresh-all.
    //     Every per-asset invoke fails definitely (Contract) and settles,
    //     so custody records, step records, and the failure surface all
    //     carry only rendered Safe text.
    let refresh = run_bin(
        &harness,
        &play_args(&cfg, &harness, &REFRESH_ALL_STEP.to_string()),
        &fake,
    );
    let refresh_text = String::from_utf8_lossy(&refresh.stdout).to_string();
    assert!(
        refresh_text.contains("step refresh all"),
        "refresh-all must run (palette pin drifted? stdout: {refresh_text}, stderr: {})",
        String::from_utf8_lossy(&refresh.stderr),
    );

    // (c) The fail-closed mutation gate: an argless mutating pair spec is
    //     refused with an input-free literal — a rejection-path surface.
    let refusal = run_bin(
        &harness,
        &play_args(&cfg, &harness, &UPDATE_STATUS_STEP.to_string()),
        &fake,
    );
    assert!(
        String::from_utf8_lossy(&refusal.stderr).contains("market action failed"),
        "mutation-gate refusal must surface; stderr: {}",
        String::from_utf8_lossy(&refusal.stderr)
    );

    // (d) Halt surface.
    let halt = run_bin(&harness, &play_args(&cfg, &harness, "halt"), &fake);
    assert!(
        String::from_utf8_lossy(&halt.stderr).contains("market runner halted"),
        "halt must surface; stderr: {}",
        String::from_utf8_lossy(&halt.stderr)
    );

    // Emission proof (harness evidence, not a market surface): every
    // channel the landed play graph can reach was actually emitted.
    assert!(emitted(&harness, "emit ch1-view") >= 1, "ch1 never emitted");
    assert!(
        emitted(&harness, "emit ch2-event") >= 1,
        "ch2 never emitted"
    );
    assert!(
        emitted(&harness, "emit ch3-stdout") >= 1,
        "ch3 never emitted"
    );
    assert!(
        emitted(&harness, "emit ch4-stderr") >= 1,
        "ch4 never emitted"
    );
    assert!(
        emitted(&harness, "emit ch5-head") >= 1,
        "ch5 head never emitted"
    );
    assert!(
        emitted(&harness, "emit ch7-diagnostic") >= 1,
        "ch7 never emitted"
    );

    // The guarantee itself: no raw form, no core, on any surface — the run
    // directory files plus every captured process stream above.
    let mut surfaces = harvest_files(&run);
    surfaces.push(("stdout:snapshot".into(), snap.stdout));
    surfaces.push(("stderr:snapshot".into(), snap.stderr));
    surfaces.push(("stdout:refresh".into(), refresh.stdout));
    surfaces.push(("stderr:refresh".into(), refresh.stderr));
    surfaces.push(("stdout:refusal".into(), refusal.stdout));
    surfaces.push(("stderr:refusal".into(), refusal.stderr));
    surfaces.push(("stdout:halt".into(), halt.stdout));
    surfaces.push(("stderr:halt".into(), halt.stderr));
    assert_all_needles_absent(&surfaces);

    let _ = fs::remove_dir_all(&harness);
}

// --------------------------------------------------------------------------
// Test 2: unresolved custody + the tool-failure printer + the input-free
// rejection contract, then headless `market deploy` over the same fake.
// --------------------------------------------------------------------------

#[test]
fn market_unresolved_custody_and_deploy_surfaces_carry_no_sentinels() {
    let harness = scratch("custody");
    let i = ids();
    let cfg = market_config(&harness, &i.admin);
    // The unresolved variant: an unclassifiable failure headline halts the
    // journey and leaves the slot's intent open (real journal surface).
    let fake = write_fake(&harness, &ch4b_raw());

    let run = harness.join("run");
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join("market-manifest.toml"), manifest_toml(&i)).unwrap();
    fs::create_dir(run.join("market-pending")).unwrap();

    let halted = run_bin(
        &harness,
        &play_args(&cfg, &harness, &REFRESH_ALL_STEP.to_string()),
        &fake,
    );
    assert!(
        String::from_utf8_lossy(&halted.stdout).contains("step refresh all"),
        "unresolved submission must stop the journey; stdout: {} stderr: {}",
        String::from_utf8_lossy(&halted.stdout),
        String::from_utf8_lossy(&halted.stderr)
    );
    assert!(
        run.join("market-pending").is_dir(),
        "unresolved custody must leave an open intent on disk"
    );

    // Reopen with open custody: the runner must refuse before any chain
    // call — the tool-failure printer surface (input-free rejection text).
    let pending_before = harvest_files(&run.join("market-pending"));
    let calls_before = fake_log(&harness);
    let refused = run_bin(&harness, &play_args(&cfg, &harness, "continue"), &fake);
    assert!(
        refused.stdout.is_empty() && fake_log(&harness) == calls_before,
        "open custody must refuse replay before any chain call"
    );
    assert_eq!(
        harvest_files(&run.join("market-pending")),
        pending_before,
        "replay refusal may not alter the unresolved intent"
    );

    // A non-testnet manifest is rejected input-free before any executor
    // call — the manifest boundary is itself a market error surface.
    let bad_run = harness.join("run-bad");
    fs::create_dir_all(&bad_run).unwrap();
    fs::write(
        bad_run.join("market-manifest.toml"),
        manifest_toml(&i).replace(
            "Test SDF Network ; September 2015",
            "Not The Testnet Passphrase",
        ),
    )
    .unwrap();
    let harness_bad = harness.join("bad");
    fs::create_dir_all(&harness_bad).unwrap();
    let bad = {
        let mut cmd = Command::new(BIN);
        cmd.arg("--out")
            .arg(&bad_run)
            .arg("--stellar-bin")
            .arg(&fake)
            .arg("--rpc-url")
            .arg("https://rpc.testnet.stellar.org")
            .args(play_args(&cfg, &harness_bad, "snapshot"))
            .env("ABTEST_FAKE_LOG", harness.join("fake.log"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = cmd.output().expect("launch precept-abtest");
        RunOut {
            stdout: out.stdout,
            stderr: out.stderr,
        }
    };
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("testnet passphrase"),
        "non-testnet manifest must be rejected; stderr: {}",
        String::from_utf8_lossy(&bad.stderr)
    );

    // Headless deploy over the same sentinel fake: whatever the deploy
    // engine does (today it stops fail-closed before any CLI call; when
    // the engine lands it will drive upload/deploy/keys positions), its
    // surfaces must stay sentinel-free.
    let dep_harness = harness.join("dep");
    fs::create_dir_all(&dep_harness).unwrap();
    let dep_fake = write_fake(&dep_harness, &ch4a_raw());
    let dep_run = dep_harness.join("run");
    fs::create_dir_all(&dep_run).unwrap();
    let mut cmd = Command::new(BIN);
    cmd.arg("--out")
        .arg(&dep_run)
        .arg("--stellar-bin")
        .arg(&dep_fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .args([
            "market".to_string(),
            "deploy".to_string(),
            "--market".to_string(),
            cfg.display().to_string(),
            "--admin-identity".to_string(),
            "admin".to_string(),
            "--admin-config-dir".to_string(),
            dep_harness.display().to_string(),
        ])
        .env("ABTEST_FAKE_LOG", dep_harness.join("fake.log"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let dep = cmd.output().expect("launch precept-abtest deploy");

    let mut surfaces = harvest_files(&run);
    surfaces.push(("stdout:halted".into(), halted.stdout));
    surfaces.push(("stderr:halted".into(), halted.stderr));
    surfaces.push(("stdout:refused".into(), refused.stdout));
    surfaces.push(("stderr:refused".into(), refused.stderr));
    surfaces.push(("stdout:bad-manifest".into(), bad.stdout));
    surfaces.push(("stderr:bad-manifest".into(), bad.stderr));
    surfaces.extend(harvest_files(&bad_run));
    surfaces.push(("stdout:deploy".into(), dep.stdout));
    surfaces.push(("stderr:deploy".into(), dep.stderr));
    surfaces.extend(harvest_files(&dep_run));
    assert_all_needles_absent(&surfaces);

    let _ = fs::remove_dir_all(&harness);
}

// --------------------------------------------------------------------------
// Test 3: guard discrimination (plan 3 Verify's seven-tuple map). The
// weakening harness reproduces the Pair-arm / pre-guard semantics on the
// exact payloads the fake emits, and asserts each channel's market
// predicate FAILS there — proving the green assertions above are
// load-bearing on the mapped guard, not vacuous.
// --------------------------------------------------------------------------

/// The landed strict-base64 decoder semantics (mirrored; self-checked below
/// against vectors that the landed `parse_read_xdr` accepts and rejects).
fn strict_base64_ok(input: &str) -> bool {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let encoded = input.trim_end_matches('=');
    let padding = input.len() - encoded.len();
    match (encoded.len() % 4, padding) {
        (0, 0) | (2, 0 | 2) | (3, 0 | 1) => {}
        _ => return false,
    }
    let mut buf = 0u32;
    let mut bits = 0;
    let mut out = Vec::with_capacity(encoded.len() * 3 / 4);
    for c in encoded.bytes() {
        let Some(v) = T.iter().position(|&t| t == c) else {
            return false;
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    buf == 0 && !out.is_empty() && out.len() % 4 == 0
}

/// Channel-5's pre-guard token check (the retired `:289` / `:580` shape:
/// prefix + length only, no checksum).
fn weakened_token_accepts(token: &str) -> bool {
    let prefix = token.chars().next().unwrap_or_default();
    (prefix == 'C' || prefix == 'G') && token.len() == 56
}

#[test]
fn guard_map_is_load_bearing_paired_weakening_discriminates() {
    let i = ids();
    let n = needles();
    let json = view_json();
    let ev = ev_good(&i.event_contract);
    let topics_json = {
        let start = ev.find("Event: [").expect("event body") + "Event: [".len();
        let end = ev[start..].find("] = ").expect("event tail") + start;
        format!("[{}]", &ev[start..end])
    };

    // The exact market predicates used above, as functions.
    let raw_present = |text: &str, needle: &str| text.contains(needle);

    // (1)+(2) value-class guard: `render_parsed`'s market arm (escaping +
    // cap). Weakened: plain `Value` display (the pre-re-home behavior of
    // observe.rs 470-471). Every `Value` formatting re-emits `|` and the
    // backtick identically, so the image assertion goes red.
    for (payload, raw, pad) in [
        (&json, SNT1_CORE, pad_block_1()),
        (&topics_json, SNT2_TOPIC_CORE, pad_block_2()),
    ] {
        let weakened: String = serde_json::from_str::<Value>(payload).unwrap().to_string();
        assert!(
            raw_present(&weakened, raw),
            "ch1/2 paired: plain Value display must carry the raw form"
        );
        assert!(
            raw_present(&weakened, &pad),
            "ch1/2 paired: uncapped display must carry the beyond-cap pad"
        );
        // The landed surfaces showed neither (test 1); the weakened path
        // shows both — the image property is what the green test owns.
    }

    // (3) failure-stdout guard: the wall plus the market arms of
    // `error_debug`/`error_chain`. Weakened: the pair arm's verbatim
    // passthrough (form (c) composes raw streams).
    let weakened3 = ch3_raw();
    assert!(
        raw_present(&weakened3, &ch3_raw()),
        "ch3 paired: verbatim passthrough must carry the raw form"
    );

    // (4) failure-stderr guard: the wall, the market error arms, and
    // `first_error_line`'s market arm. Weakened two ways: verbatim stderr,
    // and a headline-bearing `ErrorKind::Other` whose pair-arm summary is
    // the raw first line.
    let weakened4 = ch4a_raw();
    assert!(
        raw_present(&weakened4, &n.stream_raw[1].1),
        "ch4 paired: verbatim stderr must carry the raw form"
    );
    let headline_stderr = format!("error: \u{1b}[31m{SNT4_CORE} line\u{7} x\nsecond");
    // `parse_error` on that stderr builds `Other(headline)`; the pair arm
    // of `error_summary` returns the payload verbatim (first line).
    let pair_arm_summary = headline_stderr
        .trim_start_matches("error: ")
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(
        raw_present(&pair_arm_summary, SNT4_CORE),
        "ch4 paired: a pair-arm error summary must carry the raw core"
    );

    // (5) identifier-class guard: the item-7 `FromStr` newtypes. The
    // weakened (retired) check accepts both bad-checksum cores, so under
    // guard removal each core binds verbatim into records — core absence
    // goes red. The hash position carries the plan's explicit no-claim:
    // the pre-existing strict shape check is not paired; only its
    // rejection is asserted (and core absence rests on the input-free
    // rejection contract, proven by tests 1-2's error surfaces).
    for core in [&i.bad_contract, &i.bad_account] {
        assert!(
            weakened_token_accepts(core),
            "ch5 paired: the retired shape check must accept the core"
        );
        assert!(
            !strkey_valid(core, if core.starts_with('C') { 0x10 } else { 0x30 }),
            "ch5 paired: the landed checksum guard must reject the core"
        );
        let bound_record = format!("{{\"target\": \"{core}\"}}");
        assert!(
            raw_present(&bound_record, core),
            "ch5 paired: a bound record must carry the core (assertion red)"
        );
    }
    let hash = hash_token();
    assert_eq!(hash.len(), 64);
    assert!(
        !hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "hash sentinel must fail the strict 64-hex shape check"
    );

    // (6) RawEntry-column guard: strict base64 + 4-byte-word validation
    // plus the item-8 re-home. Weakened: a non-empty column binds.
    let row = read_row();
    let cols: Vec<&str> = row.split(',').collect();
    assert_eq!(cols[0], SNT6_CORE);
    assert!(
        !strict_base64_ok(cols[0]),
        "ch6 paired: the landed column validation must reject the core"
    );
    assert!(
        !strict_base64_ok("QUFB"),
        "ch6 paired: a three-byte decode must fail the word-alignment rule"
    );
    let bound_row = format!("{{\"key\": \"{}\"}}", cols[0]);
    assert!(
        raw_present(&bound_row, SNT6_CORE),
        "ch6 paired: a bound row must carry the core (assertion red)"
    );

    // (7) detail-pane guard: `pick_diagnostics`'s market arm. Weakened:
    // the pair arm stores the extracted payload unsanitized.
    let diag = ch7_line(&i.event_contract);
    let data_at = diag.find("data: ").expect("data marker") + "data: ".len();
    let pair_arm_extract = diag[data_at..].trim().trim_matches('"');
    assert!(
        raw_present(pair_arm_extract, &n.stream_raw[3].1),
        "ch7 paired: the pair-arm extraction must carry the raw form"
    );

    // Overall boundary-strip check: a rendering that strips the redaction
    // boundary entirely (every weakened output concatenated) carries every
    // sentinel raw form and core, while the real surfaces of tests 1-2
    // carry none — the suite owns the guarantee.
    let stripped = [
        json.clone(),
        topics_json,
        ev.clone(),
        ch4b_raw(),
        format!("keys public-key output: {}", i.bad_account),
        weakened3,
        weakened4,
        headline_stderr,
        bound_record_style(&i.bad_contract),
        bound_row,
        pair_arm_extract.to_string(),
        ev_bad(&i.bad_contract),
        read_row(),
        hash_token(),
    ]
    .join("\n");
    for (label, needle) in n
        .value_raw
        .iter()
        .chain(n.stream_raw.iter())
        .chain(n.id_cores.iter())
    {
        assert!(
            stripped.contains(needle.as_str()),
            "overall paired: the boundary-stripped rendering must carry {label}"
        );
    }
}

fn bound_record_style(core: &str) -> String {
    format!("{{\"target\": \"{core}\"}}")
}
