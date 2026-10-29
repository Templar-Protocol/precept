//! Consumer-surface checks for market explorer commands and pair-run watch
//! (`market contracts`, `market inspect <role> [function]`,
//! `market attach <role> <contract-id>`, `tui --watch`).
//!
//! Every assertion drives the shipped binary in a unique per-test scratch
//! fixture. Explorer/action cases use a fake `stellar` executable and a
//! synthetic completed market manifest; the pair-run watcher uses synthetic
//! pair manifest/state/evidence and a deny-all fake. Fakes log argv; discovery
//! uses only version and live testnet identity probes followed by read-only
//! `contract info interface`, never a submit, deploy, upload or invoke. Separate
//! action tests prove that a preview is
//! unsigned and neither a missing terminal nor a mainnet RPC can send. The
//! live function schemas are rendered from the CLI interface, and attachments
//! persist as separate records while the frozen manifest stays
//! byte-identical, that role collisions or an ABI change on a bound
//! contract fail closed, that a forged attachment contract id carrying
//! terminal controls never reaches error output, and that a forged
//! manifest pool contract id is refused at the same checksum boundary
//! without echoing its controls or sentinels. Version gates
//! are exercised on both sides: majors 22 and 25, the known-defect 26.0.0
//! build, and an unparseable version line are refused before
//! any interface fetch, and 26.1.0 is accepted.
//!
//! A real-terminal regression proves an early buffered `y` cannot authorize
//! a market send past the cleared confirmation gate, while a fresh `y` typed
//! after the preview reaches the one guarded send attempt that the fake
//! boundary refuses.
//! A pair-run watch PTY regression checks that B-only evidence changes and
//! removed records are reflected in the live read-only terminal.
//! A status-screen PTY regression counts the native reads behind it: the
//! first `s` pays them, an `Esc` + `s` reopen repaints the retained snapshot
//! with zero further reads even after the fake's answers change underneath,
//! and only an explicit `s` inside the screen re-reads and shows the new price.
//! A parallel status regression drives the shipped collector through the
//! same fake: independent reads genuinely overlap within the eight-job
//! ceiling, one injected HTTP 429 is retried on its own leg only while
//! every sibling leg runs exactly once, and no send or journal intent
//! survives the run.
//! A guided `pool/set-emissions-config` PTY regression proves the empty typed
//! `res_emission_metadata` vector reaches native invocation as `[]`. A fake
//! BadRequest (1200) response leaves no confirmation or journal intent and
//! permits no send after `r`, `p`, or `y`.
//! Explorer/action checks exercise `market` subcommands; the watch regression
//! drives `tui --watch`. All fixtures are synthetic and touch no real chain.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_precept-abtest");

const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
/// Version the fake reports on the accepted market path: 26.1.0.
const V261: &str = "stellar 26.1.0 (1228cff)";
const V22: &str = "stellar 22.6.0 (aaa1111)";
const V25: &str = "stellar 25.0.0 (ccc3333)";
const V260: &str = "stellar 26.0.0 (60f7458)";
const VBAD: &str = "stellar (version unknown)";

/// What the fake answers the read-only `network info --output json`
/// identity probe by default: a testnet endpoint's own reported
/// passphrase. `FAKE_STELLAR_NETWORK_INFO` points the fake at a
/// different file, which lets a negative control arm a mainnet-reporting
/// endpoint through the same production seam.
const NETWORK_INFO_TESTNET: &str = r#"{"passphrase":"Test SDF Network ; September 2015","friendbot_url":"FAKE-FRIENDBOT-SENTINEL-UNUSED"}"#;

/// The negative control: an endpoint that truthfully reports the
/// mainnet passphrase from its own `getNetwork`, with a sentinel field
/// that must never reach any operator-visible output.
const NETWORK_INFO_MAINNET: &str = r#"{"passphrase":"Public Global Stellar Network ; September 2015","history":"FAKE-MAINNET-SENTINEL-DO-NOT-LEAK"}"#;

/// The live ABI the fake serves for any
/// `contract info interface --contract-id <id> --output json` request.
/// CLI 26 emits a top-level JSON array whose every entry is a
/// single-key `SCSpecEntry` object. Function names and schemas live
/// inside their `function_v0` arm, parameters use `name` plus `type_`,
/// and structured types use tagged encodings. This fixture records the
/// price source's real published and SEP-40 read functions, including
/// `set_data(asset, price, timestamp)` and `lastprice(asset)`, plus
/// the `PriceData` and `Asset` UDT arms referenced by those functions.
const SPEC_LIVE: &str = r#"[{"function_v0":{"doc":"","name":"set_data","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"price","type_":"i128"},{"doc":"","name":"timestamp","type_":"u64"}],"outputs":[{"result":{"ok_type":"void","error_type":{"udt":{"name":"ContractError"}}}}]}},{"function_v0":{"doc":"","name":"base","inputs":[],"outputs":[{"udt":{"name":"Asset"}}]}},{"function_v0":{"doc":"","name":"assets","inputs":[],"outputs":[{"vec":{"element_type":{"udt":{"name":"Asset"}}}}]}},{"function_v0":{"doc":"","name":"decimals","inputs":[],"outputs":["u32"]}},{"function_v0":{"doc":"","name":"resolution","inputs":[],"outputs":["u32"]}},{"function_v0":{"doc":"","name":"price","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"timestamp","type_":"u64"}],"outputs":[{"option":{"value_type":{"udt":{"name":"PriceData"}}}}]}},{"function_v0":{"doc":"","name":"prices","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"records","type_":"u32"}],"outputs":[{"option":{"value_type":{"vec":{"element_type":{"udt":{"name":"PriceData"}}}}}}]}},{"function_v0":{"doc":"","name":"lastprice","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}}],"outputs":[{"option":{"value_type":{"udt":{"name":"PriceData"}}}}]}},{"function_v0":{"doc":"","name":"extend_ttl","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}}],"outputs":[]}},{"udt_struct_v0":{"doc":"","lib":"","fields":[{"doc":"","name":"price","type_":"i128"},{"doc":"","name":"timestamp","type_":"u64"}],"name":"PriceData"}},{"udt_union_v0":{"doc":"","lib":"","name":"Asset","cases":[{"tuple_v0":{"doc":"","name":"Stellar","type_":["address"]}},{"tuple_v0":{"doc":"","name":"Other","type_":["symbol"]}}]}}]"#;

/// Changes only the `set_data` price input from `i128` to `i64`.
fn spec_tampered() -> String {
    let mut spec: Value = serde_json::from_str(SPEC_LIVE).expect("live interface fixture");
    spec[0]["function_v0"]["inputs"][1]["type_"] = Value::String("i64".into());
    serde_json::to_string(&spec).expect("drifted interface fixture")
}

/// Returns the name carried inside a tagged `function_v0` arm.
fn tagged_function_name(entry: &Value) -> Option<&str> {
    entry
        .get("function_v0")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
}

/// Decodes a rendered inspection payload and verifies it is the single
/// tagged `function_v0` `set_data` entry the live CLI fixture carries.
/// A named-function inspection renders exactly one `SCSpecEntry`, so an
/// array, a flat entry, or any other shape is a wrong render. Decoded
/// values are compared, which pins the native tag, the function name,
/// and every input name and type without pinning serialization key
/// order. A renamed, retyped, absent, or untagged entry still fails.
fn assert_rendered_live_set_data_entry(rendered: &str) {
    let live: Vec<Value> =
        serde_json::from_str(SPEC_LIVE).expect("live fixture must be valid tagged JSON");
    let live_set_data = live
        .iter()
        .find(|entry| tagged_function_name(entry) == Some("set_data"))
        .expect("live fixture must carry the tagged set_data entry");
    let rendered: Value =
        serde_json::from_str(rendered.trim()).expect("a named inspection must render valid JSON");
    assert!(
        rendered.is_object(),
        "a named inspection renders one tagged SCSpecEntry object, not: {rendered}"
    );
    assert_eq!(
        tagged_function_name(&rendered),
        Some("set_data"),
        "the render must carry the requested native function_v0 tag: {rendered}"
    );
    let function = rendered
        .get("function_v0")
        .expect("a tagged entry must carry a function_v0 arm");
    for (name, type_) in [("price", "i128"), ("timestamp", "u64")] {
        assert!(
            function
                .get("inputs")
                .and_then(Value::as_array)
                .is_some_and(|inputs| inputs.iter().any(|input| {
                    input.get("name").and_then(Value::as_str) == Some(name)
                        && input.get("type_").and_then(Value::as_str) == Some(type_)
                })),
            "the render must bind {name} to its live {type_} type: {rendered}"
        );
    }
    assert_eq!(
        &rendered, live_set_data,
        "the render must reproduce the live tagged set_data schema exactly"
    );
}

/// Asserts that an inspection payload which must render nothing carries
/// no tagged function entry once decoded. Serialization key order makes
/// any text pin unreliable as a leak probe, so this inspects values:
/// every payload shape, a single entry or an array, is decoded and every
/// `function_v0` arm it holds is rejected.
fn assert_no_rendered_tagged_functions(rendered: &str) {
    let Ok(payload) = serde_json::from_str::<Value>(rendered.trim()) else {
        return;
    };
    let entries: &[Value] = match &payload {
        Value::Array(entries) => entries,
        value => std::slice::from_ref(value),
    };
    assert!(
        !entries
            .iter()
            .any(|entry| tagged_function_name(entry).is_some()),
        "a failed inspection must render no tagged function entry"
    );
}

/// Deterministic fake `stellar` CLI for this suite only: logs every
/// argv, answers `--version` from an environment-controlled line, and
/// answers interface reads from an environment-controlled file. It
/// mutates no chain state and writes nothing outside its log.
///
/// The market read paths add narrowly scoped, environment-gated responses:
/// a simulate-only `contract invoke ... --send no ...` (with a separate live
/// asset-registry reply), an `events ... --output json` page, a read-only
/// transaction fetch, and a `keys public-key <alias> --config-dir` answer.
/// Every one of those decisions is made by comparing one individual argv
/// word at a time, so a caller-supplied `--arg` value can never
/// impersonate the `--send`, `--output`, or subcommand word that gates
/// it, and any invoke, submit, deploy, upload, or funded key generation
/// that is not an explicit `--send no` simulation is refused outright.
///
/// Two further channels answer an invoke only after that gate has passed:
/// `FAKE_STELLAR_VIEW_DIR` serves one file per native function name, so each
/// leg of a dashboard can carry the value its own ABI declares, and
/// `FAKE_STELLAR_INVOKE_STDERR` adds CLI stderr for invokes alone, so a
/// contract guard's rejection such as `Error(Contract, #1200)` can be
/// reproduced without coloring the version, identity, or interface probes.
/// Two further channels exist only for the parallel status regression:
/// `FAKE_STELLAR_SYNC_LOG` makes every ABI or simulation child record a
/// start and end event around a bounded hold, so peak in-flight count is
/// read from recorded order rather than measured speed, and
/// `FAKE_STELLAR_TRANSIENT_ON` together with `FAKE_STELLAR_TRANSIENT_ONCE`
/// fails one exact invoke argv once with an HTTP 429 after claiming an
/// atomic marker, driving the transport retry. Both branch strictly
/// after the no-send gate, so a mutation can never reach either.
/// With no response armed, discovery replies remain available; every
/// unsupported command fails closed.
const FAKE_TEMPLATE: &str = r#"#!/bin/sh
LOG="$FAKE_STELLAR_LOG"
emit() { [ -n "$LOG" ] && printf '%s\n' "$1" >> "$LOG"; }
emit "$*"
[ -z "$FAKE_STELLAR_CWD_LOG" ] || printf '%s|%s\n' "$(pwd -P)" "$XDG_CONFIG_HOME" >> "$FAKE_STELLAR_CWD_LOG"
respond() {
    _err="$FAKE_STELLAR_STDERR"
    [ -n "$_err" ] && [ -f "$_err" ] && cat "$_err" >&2
    [ -n "$1" ] && [ -f "$1" ] && cat "$1"
    case "$2" in
        ''|*[!0-9]*) exit 0 ;;
    esac
    exit "$2"
}
refuse_mutation() {
    emit FAKE-REFUSED-MUTATION
    printf '%s\n' 'fake stellar: refusing a chain-mutating command' >&2
    exit 71
}
sync_hold() {
    [ -n "$FAKE_STELLAR_SYNC_LOG" ] || return 0
    printf 'SYNC-START %s %s\n' "$$" "$1" >> "$FAKE_STELLAR_SYNC_LOG"
    sleep "${FAKE_STELLAR_SYNC_HOLD:-0.3}"
    printf 'SYNC-END %s %s\n' "$$" "$1" >> "$FAKE_STELLAR_SYNC_LOG"
}
send_no=0
_prev=
for _word in "$@"; do
    if [ "$_prev" = --send ] && [ "$_word" = no ]; then
        send_no=1
    fi
    _prev=$_word
done
if [ "$1" = contract ] && [ "$2" = invoke ]; then
    [ "$send_no" = 1 ] || refuse_mutation
    _fn=
    _after_separator=0
    for _word in "$@"; do
        if [ "$_after_separator" = 1 ]; then
            _fn=$_word
            break
        fi
        [ "$_word" != -- ] || _after_separator=1
    done
    sync_hold invoke
    if [ -n "$FAKE_STELLAR_TRANSIENT_ON" ] && [ -n "$FAKE_STELLAR_TRANSIENT_ONCE" ]; then
        case "$*" in
            *"$FAKE_STELLAR_TRANSIENT_ON"*)
                if mkdir "$FAKE_STELLAR_TRANSIENT_ONCE" 2>/dev/null; then
                    printf '%s\n' 'error: HTTP 429 Too Many Requests (injected transient)' >&2
                    exit 1
                fi
                ;;
        esac
    fi
    if [ "$_fn" = registered_assets ] && [ -n "$FAKE_STELLAR_REGISTERED_ASSETS" ]; then
        respond "$FAKE_STELLAR_REGISTERED_ASSETS" ""
    fi
    if [ -n "$FAKE_STELLAR_INVOKE_STDERR" ] && [ -f "$FAKE_STELLAR_INVOKE_STDERR" ]; then
        cat "$FAKE_STELLAR_INVOKE_STDERR" >&2
    fi
    if [ -n "$FAKE_STELLAR_VIEW_DIR" ] && [ -f "$FAKE_STELLAR_VIEW_DIR/$_fn" ]; then
        respond "$FAKE_STELLAR_VIEW_DIR/$_fn" "$FAKE_STELLAR_VIEW_STATUS"
    fi
    respond "$FAKE_STELLAR_VIEW" "$FAKE_STELLAR_VIEW_STATUS"
fi
_sub=0
for _word in "$@"; do
    if [ "$_word" = fetch ]; then
        _sub=1
    fi
done
case "$1" in
    deploy|upload|submit|restore|snapshot|generate) refuse_mutation ;;
esac
case "$2" in
    deploy|upload|submit|restore|snapshot|generate) refuse_mutation ;;
esac
if [ "$1" = keys ]; then
    _fund=0
    for _word in "$@"; do
        if [ "$_word" = --fund ]; then
            _fund=1
        fi
    done
    [ "$_fund" = 1 ] && refuse_mutation
fi
if [ "$1" = keys ] && [ "$2" = public-key ]; then
    if [ -n "$FAKE_STELLAR_PUBLIC_KEY" ] && [ -f "$FAKE_STELLAR_PUBLIC_KEY" ]; then
        cat "$FAKE_STELLAR_PUBLIC_KEY"
        exit 0
    fi
fi
if [ "$1" = events ]; then
    _json=0
    _prev=
    for _word in "$@"; do
        if [ "$_prev" = --output ] && [ "$_word" = json ]; then
            _json=1
        fi
        _prev=$_word
    done
    [ "$_json" = 1 ] && respond "$FAKE_STELLAR_EVENTS" "$FAKE_STELLAR_EVENTS_STATUS"
fi
if [ "$1" = tx ] && [ "$_sub" = 1 ]; then
    respond "$FAKE_STELLAR_TX" "$FAKE_STELLAR_TX_STATUS"
fi
case "$*" in
    *--version*)
        printf '%s\n' "$FAKE_STELLAR_VERSION"
        ;;
    network\ info*)
        if [ -n "$FAKE_STELLAR_NETWORK_INFO" ] && [ -f "$FAKE_STELLAR_NETWORK_INFO" ]; then
            cat "$FAKE_STELLAR_NETWORK_INFO"
        fi
        respond "" "$FAKE_STELLAR_NETWORK_INFO_STATUS"
        ;;
    *contract*info*interface*)
        sync_hold interface
        [ -n "$FAKE_STELLAR_SPEC" ] && [ -f "$FAKE_STELLAR_SPEC" ] && cat "$FAKE_STELLAR_SPEC"
        ;;
    *)
        printf '%s\n' "fake stellar: unexpected command: $*" >&2
        exit 72
        ;;
esac
exit 0
"#;

// StrKey minting (CRC16-XMODEM over version byte + payload, matching the
// codec the manifest loader validates). Ids here are synthetic test-only
// values derived from labels, never hand-invented.
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

fn payload32(label: &str) -> [u8; 32] {
    let mut p = [0u8; 32];
    let bytes = label.as_bytes();
    assert!(bytes.len() <= 32, "label too long");
    p[..bytes.len()].copy_from_slice(bytes);
    p
}

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

fn contract_id(label: &str) -> String {
    strkey(0x10, &payload32(label))
}

fn account_id(label: &str) -> String {
    strkey(0x30, &payload32(label))
}

/// Every role the frozen registry must enumerate, paired with the exact
/// contract id the synthetic manifest binds to it.
fn fixture_roles() -> Vec<(&'static str, String)> {
    vec![
        ("pool", contract_id("explorer-cli-pool")),
        ("backstop", contract_id("explorer-cli-backstop")),
        ("factory", contract_id("explorer-cli-factory")),
        ("comet", contract_id("explorer-cli-comet")),
        ("emitter", contract_id("explorer-cli-emitter")),
        ("proxy-runtime", contract_id("explorer-cli-runtime")),
        ("proxy-governance", contract_id("explorer-cli-governance")),
        ("adapter", contract_id("explorer-cli-adapter")),
        ("source.0", contract_id("explorer-cli-src-0")),
        ("source.1", contract_id("explorer-cli-src-1")),
        ("source.2", contract_id("explorer-cli-src-2")),
        ("token.weth", contract_id("explorer-cli-tok-weth")),
        ("token.usdc", contract_id("explorer-cli-tok-usdc")),
        ("token.xlm", contract_id("explorer-cli-tok-xlm")),
        ("token.stable", contract_id("explorer-cli-tok-stable")),
    ]
}

/// A contract id outside every registry namespace: the only legitimate
/// `market attach` subject in this suite.
fn watcher_id() -> String {
    contract_id("explorer-cli-watcher")
}

/// A second outside id used for collision and re-binding attempts.
fn alt_id() -> String {
    contract_id("explorer-cli-alt")
}

/// Synthetic completed-run `market-manifest.toml` in the layout the
/// loader accepts (same stable shape as `tests/market_sentinel.rs`),
/// bound to the exact testnet and to the ids `fixture_roles()` maps.
fn manifest_toml() -> String {
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
        admin = account_id("explorer-cli-admin"),
        pool = contract_id("explorer-cli-pool"),
        backstop = contract_id("explorer-cli-backstop"),
        factory = contract_id("explorer-cli-factory"),
        comet = contract_id("explorer-cli-comet"),
        emitter = contract_id("explorer-cli-emitter"),
        runtime = contract_id("explorer-cli-runtime"),
        governance = contract_id("explorer-cli-governance"),
        adapter = contract_id("explorer-cli-adapter"),
        s0 = contract_id("explorer-cli-src-0"),
        s1 = contract_id("explorer-cli-src-1"),
        s2 = contract_id("explorer-cli-src-2"),
        weth = contract_id("explorer-cli-tok-weth"),
        usdc = contract_id("explorer-cli-tok-usdc"),
        xlm = contract_id("explorer-cli-tok-xlm"),
        stable = contract_id("explorer-cli-tok-stable"),
        frodo = account_id("explorer-cli-frodo"),
        sam = account_id("explorer-cli-sam"),
        user = account_id("explorer-cli-user"),
    )
}

struct Harness {
    run: PathBuf,
    fake: PathBuf,
    log: PathBuf,
    manifest: PathBuf,
    scratch: PathBuf,
    network_info: PathBuf,
}

struct WatchChild(std::process::Child);

impl std::ops::Deref for WatchChild {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for WatchChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for WatchChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// One fully isolated fixture tree: fresh scratch (pid-scoped tag), an
/// executable fake CLI, and a completed-run directory holding the
/// synthetic manifest. Nothing is shared between tests.
fn harness(tag: &str) -> Harness {
    let scratch = std::env::temp_dir().join(format!(
        "abtest-market-explorer-{}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).expect("scratch dir");
    let fake = scratch.join("stellar");
    fs::write(&fake, FAKE_TEMPLATE).expect("write fake cli");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod fake cli");
    let syntax = Command::new("/bin/sh")
        .arg("-n")
        .arg(&fake)
        .output()
        .expect("sh -n on explorer fake");
    if !syntax.status.success() {
        panic!(
            "explorer fake failed sh -n: {}",
            String::from_utf8_lossy(&syntax.stderr)
        );
    }
    let run = scratch.join("run");
    fs::create_dir_all(&run).expect("run dir");
    let manifest = run.join("market-manifest.toml");
    fs::write(&manifest, manifest_toml()).expect("write fixture manifest");
    let network_info = scratch.join("fake.network-info.json");
    fs::write(&network_info, NETWORK_INFO_TESTNET).expect("write fake network info");
    let spec_file = scratch.join("fake.spec.json");
    fs::write(&spec_file, SPEC_LIVE).expect("write live interface fixture");
    Harness {
        run,
        fake,
        log: scratch.join("fake.log"),
        manifest,
        scratch,
        network_info,
    }
}

/// Runs the shipped binary against this fixture with an explicit
/// completed-manifest `--out`, the fake CLI pinned via `--stellar-bin`,
/// and the testnet RPC endpoint. `spec` controls what the fake serves
/// for interface reads; `version` controls what it reports.
fn run_cli(h: &Harness, version: &str, spec: &str, args: &[&str]) -> std::process::Output {
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(&spec_file, spec).expect("write fake spec");
    Command::new(BIN)
        .arg("--out")
        .arg(&h.run)
        .arg("--stellar-bin")
        .arg(&h.fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .args(args)
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", version)
        .env("FAKE_STELLAR_SPEC", &spec_file)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run precept-abtest")
}

fn fake_log(h: &Harness) -> String {
    fs::read_to_string(&h.log).unwrap_or_default()
}

/// Counts the read-only `network info --output json` RPC-identity probes
/// the fake was asked to run. Every guarded market command must reach
/// this seam exactly once before any other executor call.
fn network_probe_count(h: &Harness) -> usize {
    fake_log(h)
        .lines()
        .filter(|line| line.starts_with("network info --output json"))
        .count()
}

/// Fails unless every logged fake call is the RPC-identity probe or the
/// local version probe, each carrying the explicit network arguments:
/// the single read-only probe the production guard composes, and nothing
/// else. Any interface fetch, invoke, send, or mutating verb is a leak
/// past the failed identity check.
fn assert_only_guard_probes_logged(h: &Harness) {
    for line in fake_log(h).lines() {
        assert!(
            (line.starts_with("network info --output json")
                || line.contains("--version")
                || line.contains("--rpc-url")
                || line.contains("--config-dir"))
                && !line.contains("contract info interface")
                && !line.contains("contract invoke")
                && !line.contains("--send")
                && !line.contains("tx submit")
                && !line.contains("contract deploy")
                && !line.contains("contract upload"),
            "a failed market command may only reach the read-only identity probes: {line}"
        );
    }
}

/// Every logged fake-CLI invocation must be a read. No argv ever seen
/// by the fake may carry a chain-mutating verb or the send flag.
fn assert_chain_untouched(h: &Harness) {
    let log = fake_log(h);
    for verb in [
        "submit", "deploy", "upload", "invoke", "--send", "asset", "snapshot", "restore",
    ] {
        assert!(
            !log.contains(verb),
            "explorer read paths must never reach a mutating CLI verb ({verb}):\n{log}"
        );
    }
}

fn output_text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `market contracts` must enumerate every frozen role — pool, the
/// oracle/source stack, and every token — each paired with the exact
/// contract id the completed manifest binds, without ever invoking the
/// CLI.
#[test]
fn market_contracts_lists_pool_source_and_token_roles() {
    let h = harness("catalog");
    let out = run_cli(&h, V261, SPEC_LIVE, &["market", "contracts"]);
    assert!(
        out.status.success(),
        "catalog must succeed: {}",
        output_text(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (role, id) in fixture_roles() {
        assert!(
            stdout.contains(&format!("{role} {id}")),
            "catalog must list {role} bound to {id}:\n{stdout}"
        );
    }
    assert!(
        !h.log.exists(),
        "the manifest-bound catalog may not invoke the CLI at all:\n{}",
        fake_log(&h)
    );
    assert_chain_untouched(&h);
}

/// `market inspect source.0 set_data` must return the live function
/// schema the fake CLI emits from `contract info interface --output
/// json`, proving inspection reads the deployed contract's real ABI
/// rather than a static list.
#[test]
fn market_inspect_returns_live_function_schema_for_source_role() {
    let h = harness("inspect-live-schema");
    let out = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "inspect", "source.0", "set_data"],
    );
    assert_eq!(
        network_probe_count(&h),
        1,
        "every guarded market command must reach the read-only RPC-identity probe exactly once:\n{}",
        fake_log(&h)
    );
    assert!(
        fake_log(&h).contains("--network-passphrase Test SDF Network ; September 2015"),
        "the identity probe must carry the explicit testnet network arguments:\n{}",
        fake_log(&h)
    );
    assert!(
        out.status.success(),
        "CLI 26 inspection must succeed: {}",
        output_text(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_rendered_live_set_data_entry(&stdout);
    let source0 = contract_id("explorer-cli-src-0");
    let log = fake_log(&h);
    assert!(
        log.contains(&format!(
            "contract info interface --contract-id {source0} --output json"
        )),
        "inspection must fetch the live spec for the bound source contract:\n{log}"
    );
    let fake_method = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "inspect", "source.0", "fake"],
    );
    assert!(
        !fake_method.status.success(),
        "inspection of an absent tagged function must fail closed: {}",
        output_text(&fake_method)
    );
    let absent_output = output_text(&fake_method);
    assert!(
        absent_output.contains("requested function is not present in the live contract interface"),
        "an absent function must fail the live-tagged lookup: {absent_output}"
    );
    assert!(
        !String::from_utf8_lossy(&fake_method.stdout).contains("function_v0")
            && !String::from_utf8_lossy(&fake_method.stdout).contains("lastprice"),
        "a failed absent-function inspection must not render live interface entries: {}",
        String::from_utf8_lossy(&fake_method.stdout)
    );
    assert_no_rendered_tagged_functions(&String::from_utf8_lossy(&fake_method.stdout));
    assert_chain_untouched(&h);
    assert_eq!(
        network_probe_count(&h),
        2,
        "each market command reaches the read-only identity probe exactly once:\n{}",
        fake_log(&h)
    );
}

/// `market attach watcher <id>` must persist its own attachment record,
/// and a second attach to another id must persist a separate record.
/// Re-attaching an already-bound role must fail.
#[test]
fn market_attach_persists_separate_records_and_rejects_role_reuse() {
    let h = harness("attach-persist");
    let watcher = watcher_id();
    let alt = alt_id();
    let manifest_before = fs::read(&h.manifest).expect("fixture manifest bytes");

    let ok = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "attach", "watcher", &watcher],
    );
    assert!(
        ok.status.success(),
        "first attach must succeed: {}",
        output_text(&ok)
    );
    let second = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "attach", "watcher-2", &alt],
    );
    assert!(
        second.status.success(),
        "attach to a distinct role must succeed: {}",
        output_text(&second)
    );

    let stored: Vec<String> = fs::read_dir(h.run.join("market-attachments"))
        .expect("attachment directory must exist")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        stored.iter().any(|n| n.contains("watcher")),
        "watcher attachment must be persisted, found {stored:?}"
    );
    assert!(
        stored.len() >= 2,
        "the second attach must create a separate record, found {stored:?}"
    );
    let contents: String = stored
        .iter()
        .filter_map(|n| fs::read_to_string(h.run.join("market-attachments").join(n)).ok())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        contents.contains(&watcher) && contents.contains(&alt),
        "attachments must bind the exact ids they were given"
    );
    assert!(
        contents.contains(TESTNET_PASSPHRASE),
        "attachments must carry the exact testnet binding"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("manifest must still exist"),
        manifest_before,
        "attachments persist separately; the frozen manifest must stay byte-identical"
    );
    let reuse = run_cli(&h, V261, SPEC_LIVE, &["market", "attach", "watcher", &alt]);
    assert!(
        !reuse.status.success(),
        "re-attaching an already-bound role must fail: {}",
        output_text(&reuse)
    );
    assert_chain_untouched(&h);
    assert_eq!(
        network_probe_count(&h),
        2,
        "role reuse must refuse locally without a third RPC probe"
    );
    assert_eq!(
        fake_log(&h)
            .lines()
            .filter(|line| line.starts_with("contract info interface"))
            .count(),
        2,
        "role reuse must not fetch a third contract interface"
    );
}

/// Attaching over a frozen manifest role (`pool`) is a collision and
/// must fail without ever fetching the offered contract's interface.
/// Once a watcher is bound, a change to that contract's live ABI must
/// make every later inspection of the attachment fail closed with no
/// schema rendered, while the stored record stays intact.
#[test]
fn market_attach_refuses_frozen_role_and_abi_drift_breaks_inspection() {
    let h = harness("attach-drift");
    let watcher = watcher_id();
    let alt = alt_id();

    let collide = run_cli(&h, V261, SPEC_LIVE, &["market", "attach", "pool", &alt]);
    assert!(
        !collide.status.success(),
        "attaching over a frozen manifest role must fail: {}",
        output_text(&collide)
    );
    assert!(
        fake_log(&h).is_empty(),
        "a frozen-role collision must refuse before any CLI child starts"
    );

    let ok = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "attach", "watcher", &watcher],
    );
    assert!(
        ok.status.success(),
        "legitimate attach must succeed: {}",
        output_text(&ok)
    );
    assert_eq!(
        network_probe_count(&h),
        1,
        "only the legitimate attach may reach the RPC identity probe"
    );
    let record = h.run.join("market-attachments").join("watcher.json");
    let record_before = fs::read(&record).expect("watcher attachment must be stored");

    // The bound contract's live spec changes. Re-inspection re-fetches
    // through the same fake and must fail the recorded-digest recheck.
    let drifted = run_cli(
        &h,
        V261,
        &spec_tampered(),
        &["market", "inspect", "watcher", "set_data"],
    );
    assert!(
        !drifted.status.success(),
        "inspection of an attachment whose ABI changed must fail: {}",
        output_text(&drifted)
    );
    let drifted_stdout = String::from_utf8_lossy(&drifted.stdout);
    assert!(
        !drifted_stdout.contains("function_v0") && !drifted_stdout.contains("lastprice"),
        "nothing from the changed tagged spec may render: {drifted_stdout}"
    );
    assert_no_rendered_tagged_functions(&drifted_stdout);
    assert_eq!(
        fs::read(&record).expect("attachment record"),
        record_before,
        "a failed drift recheck must leave the stored attachment untouched"
    );
    assert_chain_untouched(&h);
    assert_eq!(
        network_probe_count(&h),
        2,
        "valid attach and drift recheck each prove the endpoint once"
    );
}

/// CLI majors below 26, the known-defect 26.0.0 build, and an
/// unparseable version line are refused on the explorer paths before any
/// interface fetch reaches the network: the fake may only ever see the
/// local `--version` probe under each rejected version. CLI 26.1.0 is
/// accepted.
#[test]
fn market_explorer_rejects_old_and_bad_cli_versions_before_network_and_accepts_26_1() {
    let h = harness("version-gate");
    let watcher = watcher_id();
    for rejected in [V22, V25, V260, VBAD] {
        let inspect = run_cli(
            &h,
            rejected,
            SPEC_LIVE,
            &["market", "inspect", "source.0", "set_data"],
        );
        assert!(
            !inspect.status.success(),
            "{rejected} must be refused on inspect: {}",
            output_text(&inspect)
        );
        let attach = run_cli(
            &h,
            rejected,
            SPEC_LIVE,
            &["market", "attach", "watcher", &watcher],
        );
        assert!(
            !attach.status.success(),
            "{rejected} must be refused on attach: {}",
            output_text(&attach)
        );
        let log = fake_log(&h);
        assert!(
            log.contains("--version"),
            "the version probe must run before refusal under {rejected}:\n{log}"
        );
        assert!(
            !log.contains("interface"),
            "{rejected} must be refused before any interface fetch:\n{log}"
        );
        assert_eq!(
            network_probe_count(&h),
            0,
            "no RPC-identity probe may run before CLI 26.1.0 is accepted:\n{log}"
        );
    }
    assert!(
        !h.run.join("market-attachments").exists(),
        "a version-refused attach may not store any record"
    );

    let accepted = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "inspect", "source.0", "set_data"],
    );
    assert!(
        accepted.status.success(),
        "CLI 26.1.0 must be accepted: {}",
        output_text(&accepted)
    );
    assert_rendered_live_set_data_entry(&String::from_utf8_lossy(&accepted.stdout));
    assert_chain_untouched(&h);
    assert_eq!(
        network_probe_count(&h),
        1,
        "the accepted command reached the read-only identity probe exactly once:\n{}",
        fake_log(&h)
    );
}

/// An attachment record bearing an invalid contract id with terminal
/// controls must be rejected through the shared checksum boundary
/// before any fake CLI invocation and without rendering the injected
/// value. This exercises attachment deserialization against the
/// `ContractId` trust-boundary fix.
#[test]
fn invalid_attachment_contract_id_rejects_without_execution_or_control_leak() {
    let h = harness("invalid-id-injection");
    let attachment_dir = h.run.join("market-attachments");
    fs::create_dir_all(&attachment_dir).expect("attachment directory");
    let raw_injected_id =
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAATEST_INVALID_MANIFEST_ID_SENTINEL\u{1b}];\u{7}INVALID_MANIFEST_ID_SENTINEL\u{1b}\\\r\n";
    let json_escaped_contract = raw_injected_id
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\u{1b}', "\\u001B")
        .replace('\u{7}', "\\u0007")
        .replace('\r', "\\u000D")
        .replace('\n', "\\u000A");
    fs::write(
        attachment_dir.join("watcher.json"),
        format!(
            "{{\"role\":\"watcher\",\"contract\":\"{json_escaped_contract}\",\"spec_sha256\":\"{}\",\"network\":\"{TESTNET_PASSPHRASE}\"}}",
            "0".repeat(64)
        ),
    )
    .expect("planted malformed attachment");

    let result = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "inspect", "watcher", "set_data"],
    );
    assert!(
        !result.status.success(),
        "an invalid attachment contract id must reject before any fake CLI call: {}",
        output_text(&result)
    );
    assert!(
        fake_log(&h).is_empty(),
        "malformed attachment must refuse before any CLI child starts"
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}'),
        "invalid contract id must not place terminal controls on stdout"
    );
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "invalid contract id must not place terminal controls on stderr"
    );
    assert!(
        !stdout.contains("INVALID_MANIFEST_ID_SENTINEL") && !stdout.contains("TEST_INVALID"),
        "invalid contract id must remain absent from stdout"
    );
    assert!(
        !stderr.contains("INVALID_MANIFEST_ID_SENTINEL") && !stderr.contains("TEST_INVALID"),
        "invalid contract id must remain absent from stderr"
    );
    let log = fake_log(&h);
    assert!(
        !log.contains("contract info interface"),
        "an invalid attachment id must be rejected before any interface fetch:\n{log}"
    );
    assert_chain_untouched(&h);
    let attachment_records = fs::read_dir(&attachment_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        attachment_records, 1,
        "rejection must not replace or add attachment records"
    );
    assert_eq!(
        fs::read(attachment_dir.join("watcher.json")).expect("planted attachment"),
        format!(
            "{{\"role\":\"watcher\",\"contract\":\"{json_escaped_contract}\",\"spec_sha256\":\"{}\",\"network\":\"{TESTNET_PASSPHRASE}\"}}",
            "0".repeat(64)
        )
            .as_bytes(),
        "rejection must leave the malformed attachment intact for audit"
    );
}

/// A completed manifest whose pool ContractId is checksum-invalid and
/// decodes to terminal controls must be refused by the `ContractId`
/// deserialization boundary before any fake executable call, without
/// echoing the injected value. The forged document keeps every other
/// manifest byte and TOML delimiter intact, and the identical
/// `market contracts` command succeeds on the unforged fixture in
/// `market_contracts_lists_pool_source_and_token_roles`, so a pass here
/// is attributable to the contract-id boundary and not to a parse error.
#[test]
fn invalid_market_manifest_contract_id_rejects_before_executor_and_without_control_leak() {
    let h = harness("manifest-control-injection");
    let pristine = manifest_toml();
    let genuine_pool_line = format!("pool = \"{}\"", contract_id("explorer-cli-pool"));
    assert_eq!(
        pristine.matches(&genuine_pool_line).count(),
        1,
        "the fixture manifest must bind the pool role exactly once"
    );
    let (prefix, suffix) = pristine
        .split_once(&genuine_pool_line)
        .expect("the fixture manifest must bind the pool role");
    let forged_pool = format!(
        "{prefix}pool = \"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAATEST_INVALID_MANIFEST_ID_SENTINEL\\u001B];\\u0007INVALID_MANIFEST_ID_SENTINEL\\u001B\\u000D\\u000A\"{suffix}"
    );
    assert_ne!(
        forged_pool, pristine,
        "the fixture must actually forge its pool binding"
    );
    assert!(
        forged_pool.starts_with(prefix) && forged_pool.ends_with(suffix),
        "only the pool binding may change; every other manifest byte and TOML delimiter stays intact"
    );
    assert!(
        forged_pool.lines().any(|line| line.starts_with("pool = \"") && line.ends_with('"')),
        "the forged pool binding must stay a well-formed single-line TOML string, so a refusal cannot be a parse error"
    );
    assert!(
        !forged_pool.contains(&genuine_pool_line),
        "the genuine pool binding must be replaced, not merely annotated"
    );
    let forged_document: toml::Value = toml::from_str(&forged_pool)
        .expect("the forged manifest must stay well-formed TOML, so any refusal can only come from the contract-id boundary");
    let forged_binding = forged_document
        .get("pool")
        .and_then(toml::Value::as_str)
        .expect("the forged manifest must still carry a pool string binding");
    assert!(
        forged_binding.contains("INVALID_MANIFEST_ID_SENTINEL"),
        "the forged pool binding must decode to the planted sentinel value"
    );
    assert!(
        forged_binding.contains('\u{1b}')
            && forged_binding.contains('\u{7}')
            && forged_binding.contains('\r')
            && forged_binding.contains('\n'),
        "the forged pool binding must decode to a control-bearing value"
    );
    fs::write(&h.manifest, &forged_pool).expect("forge fixture manifest");

    let contracts = run_cli(&h, V261, SPEC_LIVE, &["market", "contracts"]);
    assert!(
        !contracts.status.success(),
        "market contracts must refuse a malformed manifest pool ContractId: {}",
        output_text(&contracts)
    );
    assert_eq!(
        fake_log(&h),
        "",
        "a malformed manifest ContractId must refuse before any fake executable call"
    );

    let inspect = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &["market", "inspect", "source.0", "set_data"],
    );
    assert!(
        !inspect.status.success(),
        "market inspect must refuse the same malformed manifest: {}",
        output_text(&inspect)
    );

    for rejected in [&contracts, &inspect] {
        let stdout = String::from_utf8_lossy(&rejected.stdout);
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        for stream in [&stdout, &stderr] {
            assert!(
                !stream.contains('\u{1b}') && !stream.contains('\u{7}'),
                "manifest refusal must not emit raw terminal controls"
            );
            assert!(
                !stream.contains("TEST_INVALID_MANIFEST_ID_SENTINEL")
                    && !stream.contains("INVALID_MANIFEST_ID_SENTINEL")
                    && !stream.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                    && !stream.contains("u001B"),
                "manifest refusal must not echo its injected contract-id value"
            );
        }
    }
    assert_eq!(
        fake_log(&h),
        "",
        "manifest deserialization must refuse before the CLI executor starts"
    );
    assert_chain_untouched(&h);
    assert_eq!(
        fs::read(&h.manifest).expect("the forged manifest must stay readable"),
        forged_pool.as_bytes(),
        "refusal must leave the forged manifest intact for audit"
    );
}

/// Runs the shipped binary with the fake's market read responses armed
/// for this one test. Every response body is written inside this
/// fixture's own scratch directory and served only in reply to the one
/// command shape it names; `statuses` arms the non-zero exits the real
/// CLI returns for a missing record or a refused page. The version probe,
/// the interface read, and the default `ok` answer stay exactly as the
/// discovery tests find them, and no armed response is ever a send.
fn run_reads(
    h: &Harness,
    args: &[&str],
    responses: &[(&str, &str)],
    statuses: &[(&str, &str)],
) -> std::process::Output {
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(&spec_file, SPEC_LIVE).expect("write live interface fixture");
    let mut cmd = Command::new(BIN);
    cmd.arg("--out")
        .arg(&h.run)
        .arg("--stellar-bin")
        .arg(&h.fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .args(args)
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_SPEC", &spec_file)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info);
    for (name, body) in responses {
        assert!(
            name.starts_with("FAKE_STELLAR_") && !name.ends_with("_STATUS"),
            "unknown fake response channel: {name}"
        );
        let path = h.scratch.join(format!("fake.read.{name}"));
        fs::write(&path, body).expect("write fake read response");
        cmd.env(name, &path);
    }
    for (name, status) in statuses {
        assert!(
            name.starts_with("FAKE_STELLAR_") && name.ends_with("_STATUS"),
            "unknown fake status channel: {name}"
        );
        cmd.env(name, status);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run precept-abtest")
}

/// Asserts that every command the fake was asked to run was a read. Any
/// `contract invoke` must carry the two separate words `--send no`; a
/// `--send yes`, a bare `--send`, or a mutating subcommand is a chain
/// write. The fake refuses a sending invoke on its own positional argv
/// words and records the refusal, so a marker in the log is direct
/// evidence that a read path tried to mutate the market.
fn assert_reads_are_send_free(h: &Harness) {
    let log = fake_log(h);
    assert!(
        !log.contains("FAKE-REFUSED-MUTATION"),
        "a market read path asked the fake to mutate the chain:\n{log}"
    );
    for line in log.lines() {
        assert!(
            !line.contains("--send yes"),
            "a market read path requested a send: {line}"
        );
        for verb in [
            "contract deploy",
            "contract upload",
            "tx submit",
            "keys generate --fund",
        ] {
            assert!(
                !line.contains(verb),
                "a market read path reached the mutating verb {verb}: {line}"
            );
        }
        if line.starts_with("contract invoke") {
            let words: Vec<&str> = line.split(' ').collect();
            let send = words.iter().position(|word| *word == "--send");
            assert!(
                send.is_some_and(|index| words.get(index + 1) == Some(&"no")),
                "every market invocation must be a --send no simulation: {line}"
            );
        }
    }
}

/// A native `Asset` value in the tagged form the live `Asset` union
/// declares. It carries no spaces so it is recorded as one argv word.
fn asset_value(symbol: &str) -> String {
    format!(r#"{{"Other":["{symbol}"]}}"#)
}

/// A `Some(PriceData)` return value in the tagged form the live
/// `PriceData` struct and Soroban's `Option` encoding declare. The price
/// is an `i128`, so it is a string, and a reader must never coerce it.
fn live_price_value(price: &str, timestamp: &str) -> String {
    format!(r#"{{"vec":[{{"map":[["price","{price}"],["timestamp","{timestamp}"]]}}]}}"#)
}

/// `market query source.0 lastprice --arg asset=…` must return the live
/// tagged value the pinned CLI decodes, unchanged, and must reach it only
/// through a `--send no` simulation sourced by the manifest admin. A
/// native UDT argument passes through as the single argv word it is, and
/// the frozen manifest stays byte-identical.
#[test]
fn market_query_returns_the_live_tagged_price_as_a_no_send_simulation() {
    let h = harness("query-live-price");
    let manifest_before = fs::read(&h.manifest).expect("fixture manifest bytes");
    let source0 = contract_id("explorer-cli-src-0");
    let admin = account_id("explorer-cli-admin");
    let asset = asset_value("BLND");
    let price = live_price_value("12500000000", "1758000000");

    let out = run_reads(
        &h,
        &[
            "market",
            "query",
            "source.0",
            "lastprice",
            "--arg",
            &format!("asset={asset}"),
        ],
        &[("FAKE_STELLAR_VIEW", &price)],
        &[],
    );
    assert!(
        out.status.success(),
        "a live read function must succeed: {}",
        output_text(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let got: Value = serde_json::from_str(stdout.trim())
        .expect("a market query must render the decoded live value as JSON");
    let want: Value = serde_json::from_str(&price).expect("live value fixture");
    assert_eq!(
        got, want,
        "the live tagged value must pass through unchanged"
    );
    assert_eq!(
        got.pointer("/vec/0/map/0/1"),
        Some(&Value::String("12500000000".to_string())),
        "an i128 price must survive as its own string, never coerced to a number"
    );

    let log = fake_log(&h);
    assert!(
        log.contains(&format!(
            "contract invoke --id {source0} --source {admin} --send no"
        )),
        "a query must simulate against the bound contract with --send no:\n{log}"
    );
    assert!(
        log.contains(&format!("-- lastprice --asset {asset}")),
        "the caller's native UDT argument must reach the CLI unchanged:\n{log}"
    );
    assert_reads_are_send_free(&h);
    assert_eq!(
        fs::read(&h.manifest).expect("fixture manifest must still exist"),
        manifest_before,
        "a market read must leave the frozen manifest byte-identical"
    );
}

/// An absent price is a fact about the network, not a zero. When the
/// simulation returns nothing, `market query` must report JSON null and
/// must never render a price-shaped value, while the underlying read
/// stays a `--send no` simulation.
#[test]
fn market_query_reports_an_absent_price_as_null_and_never_as_a_number() {
    let h = harness("query-absent-price");
    let source0 = contract_id("explorer-cli-src-0");
    let asset = asset_value("BLND");

    let out = run_reads(
        &h,
        &[
            "market",
            "query",
            "source.0",
            "lastprice",
            "--arg",
            &format!("asset={asset}"),
        ],
        &[("FAKE_STELLAR_VIEW", "null")],
        &[],
    );
    assert!(
        out.status.success(),
        "an absent price is a successful read: {}",
        output_text(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        "null",
        "an absent price must be reported as JSON null, never as a value"
    );
    assert!(
        !stdout.chars().any(|c| c.is_ascii_digit()),
        "an absent price must never be reported as a fabricated number: {stdout}"
    );
    assert!(
        !stdout.contains("1970") && !stdout.contains("0000-00-00"),
        "an absent price must never be reported with a fabricated epoch: {stdout}"
    );
    let log = fake_log(&h);
    assert!(
        log.contains(&format!("contract invoke --id {source0}")) && log.contains("--send no"),
        "the null must come from a real --send no simulation:\n{log}"
    );
    assert_reads_are_send_free(&h);
}

/// Unknown, malformed, and duplicated arguments are refused before any
/// invocation is composed, and the refusal never echoes what the caller
/// passed. A valid query is run alongside them as the control, so a pass
/// here cannot be a read path that never works at all.
#[test]
fn market_query_refuses_unknown_malformed_and_duplicated_args_before_invoking() {
    let h = harness("query-argument-boundary");
    let source0 = contract_id("explorer-cli-src-0");
    let admin = account_id("explorer-cli-admin");
    let asset = asset_value("BLND");
    let price = live_price_value("12500000000", "1758000000");
    let control = run_reads(
        &h,
        &[
            "market",
            "query",
            "source.0",
            "lastprice",
            "--arg",
            &format!("asset={asset}"),
        ],
        &[("FAKE_STELLAR_VIEW", &price)],
        &[],
    );
    assert!(
        control.status.success(),
        "the valid query must work: {}",
        output_text(&control)
    );
    let control_log = fake_log(&h);
    assert!(
        control_log.contains(&format!(
            "contract invoke --id {source0} --source {admin} --send no"
        )),
        "the control query must reach a --send no simulation:\n{control_log}"
    );

    let rejected_args: Vec<Vec<String>> = vec![
        vec![
            "--arg".to_string(),
            "asset=SENTINEL-DUP-1".into(),
            "--arg".to_string(),
            "asset=SENTINEL-DUP-2".into(),
        ],
        vec!["--arg".to_string(), "asset".into()],
        vec!["--arg".to_string(), "9bad-name=SENTINEL-MALFORMED".into()],
        vec!["--arg".to_string(), "bogus=SENTINEL-UNKNOWN".into()],
        vec![
            "--arg".to_string(),
            format!("asset=SENTINEL-CONTROL-\u{1b}"),
        ],
        vec![
            "--arg".to_string(),
            format!("asset=SENTINEL-BIG-{}", "x".repeat(4096)),
        ],
    ];
    let sentinels = [
        "SENTINEL-DUP-1",
        "SENTINEL-DUP-2",
        "SENTINEL-MALFORMED",
        "SENTINEL-UNKNOWN",
        "SENTINEL-CONTROL",
        "SENTINEL-BIG",
    ];
    for rejected in &rejected_args {
        let mut args = vec!["market", "query", "source.0", "lastprice"];
        args.extend(rejected.iter().map(String::as_str));
        let out = run_reads(&h, &args, &[("FAKE_STELLAR_VIEW", &price)], &[]);
        let text = output_text(&out);
        assert!(
            !out.status.success(),
            "a rejected argument list must fail closed: {text}"
        );
        assert!(
            text.contains("argument") || text.contains("not an input"),
            "a refusal must name the argument boundary it enforced: {text}"
        );
        for sentinel in sentinels {
            assert!(
                !text.contains(sentinel),
                "an argument refusal must not echo the caller's bytes: {text}"
            );
        }
    }

    let log = fake_log(&h);
    for sentinel in sentinels {
        assert!(
            log.lines()
                .filter(|line| line.starts_with("contract invoke"))
                .all(|line| !line.contains(sentinel)),
            "a refused argument list must never reach an invocation for {sentinel}:\n{log}"
        );
    }
    assert_reads_are_send_free(&h);
}

/// One decoded CLI26 event line. The seam rechecks the contract and the
/// ledger and requires the RPC cursor grammar, so every fixture id here
/// is grammar-valid: a contract mismatch or a pre-window ledger must be
/// the only reason a page can be rejected. The nested `params` value
/// carries a sentinel that must never survive into a rendered page.
fn decoded_event_line(contract: &str, ledger: u64, index: u32) -> String {
    format!(
        r#"{{"type":"contract","ledger":{ledger},"ledger_closed_at":"2026-09-01T00:00:00Z","contract_id":"{contract}","event_name":"transfer","prefix_topics":["AAAABgAAAABTRUNSRVQ="],"params":{{"amount":"1","note":"SECRET-EVENT-PARAM-{ledger}-{index}"}},"id":"{ledger}-{index}"}}"#
    )
}

/// One raw-fallback CLI26 event line carrying the same validated cursor
/// grammar and the same nested payload sentinel.
fn raw_event_line(contract: &str, ledger: u64, index: u32) -> String {
    format!(
        r#"{{"type":"contract","ledger":{ledger},"ledgerClosedAt":"2026-09-01T00:00:01Z","contractId":"{contract}","id":"{ledger}-{index}","txHash":"{}","topic":["AAAABgAAAABTRUNSRVQ="],"value":"AAAACgAAAAM="}}"#,
        "a".repeat(64)
    )
}

/// `market events pool --start-ledger 42 --count 3` must read exactly
/// one bounded `stellar events --id <pool> --start-ledger 42 --count 3
/// --output json` page and report the validated cursors and ledgers it
/// carried, in both the decoded and the raw CLI shape. Nested event
/// payloads must not survive the page, and no read may ever send.
#[test]
fn market_events_reports_one_bounded_page_and_holds_nested_payloads_back() {
    let h = harness("events-bounded-page");
    let pool = contract_id("explorer-cli-pool");
    let page = format!(
        "{}\n{}",
        decoded_event_line(&pool, 42, 1),
        raw_event_line(&pool, 43, 2)
    );

    let out = run_reads(
        &h,
        &[
            "market",
            "events",
            "pool",
            "--start-ledger",
            "42",
            "--count",
            "3",
        ],
        &[("FAKE_STELLAR_EVENTS", &page)],
        &[],
    );
    assert!(
        out.status.success(),
        "a bounded event page is a successful read: {}",
        output_text(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rendered: Value =
        serde_json::from_str(stdout.trim()).expect("the history dashboard must render as JSON");
    assert_eq!(
        rendered.get("contract_id").and_then(Value::as_str),
        Some(pool.as_str()),
        "the page must be reported against the bound pool contract"
    );
    assert_eq!(
        rendered.get("start_ledger").and_then(Value::as_u64),
        Some(42)
    );
    assert_eq!(
        rendered.get("requested_count").and_then(Value::as_u64),
        Some(3)
    );
    assert_eq!(rendered.get("returned").and_then(Value::as_u64), Some(2));
    let cursors: Vec<&str> = rendered
        .get("events")
        .and_then(Value::as_array)
        .map(|events| {
            events
                .iter()
                .filter_map(|event| event.get("cursor").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        cursors,
        vec!["42-1", "43-2"],
        "both CLI event shapes must be reported by their validated cursors"
    );
    assert!(
        !stdout.contains("SECRET-EVENT-PARAM") && !stdout.contains("AAAABgAAAABTRUNSRVQ="),
        "a rendered page must hold nested event payloads back: {stdout}"
    );

    let log = fake_log(&h);
    assert!(
        log.contains(&format!(
            "events --id {pool} --start-ledger 42 --count 3 --output json"
        )),
        "the request must be one bounded read of the bound contract's events:\n{log}"
    );
    assert_reads_are_send_free(&h);
}

/// A page that overruns the caller's bound is refused, never quietly
/// truncated, and an event that belongs to another contract or to a
/// ledger before the requested window fails closed without echoing the
/// untrusted line.
#[test]
fn market_events_refuse_an_overrun_window_and_a_foreign_event_without_leaking_them() {
    let h = harness("events-screening");
    let pool = contract_id("explorer-cli-pool");
    let foreign = contract_id("explorer-cli-src-0");

    let overrun = [42u64, 43, 44, 45]
        .iter()
        .enumerate()
        .map(|(index, ledger)| decoded_event_line(&pool, *ledger, index as u32 + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let overrun_run = run_reads(
        &h,
        &[
            "market",
            "events",
            "pool",
            "--start-ledger",
            "42",
            "--count",
            "3",
        ],
        &[("FAKE_STELLAR_EVENTS", &overrun)],
        &[],
    );
    assert!(
        !overrun_run.status.success(),
        "a page larger than the requested bound must be refused: {}",
        output_text(&overrun_run)
    );
    let overrun_text = output_text(&overrun_run);
    assert!(
        overrun_text.contains("failed validation and was rejected"),
        "the refusal must carry the shipped history rejection: {overrun_text}"
    );
    assert!(
        !overrun_text.contains("SECRET-EVENT-PARAM"),
        "a refused page must render no event payload: {overrun_text}"
    );

    // Every id below is cursor-grammar valid and every ledger is in or
    // below the requested window on purpose, so a contract mismatch or a
    // pre-window ledger is the only reason these pages can be rejected.
    for rejected in [
        decoded_event_line(&foreign, 42, 1),
        raw_event_line(&foreign, 42, 2),
        decoded_event_line(&pool, 41, 1),
        raw_event_line(&pool, 41, 2),
    ] {
        let out = run_reads(
            &h,
            &[
                "market",
                "events",
                "pool",
                "--start-ledger",
                "42",
                "--count",
                "3",
            ],
            &[("FAKE_STELLAR_EVENTS", &rejected)],
            &[],
        );
        let text = output_text(&out);
        assert!(
            !out.status.success(),
            "an off-target or pre-window event must fail closed: {text}"
        );
        assert!(
            text.contains("failed validation and was rejected"),
            "the refusal must carry the shipped history rejection: {text}"
        );
        assert!(
            !text.contains("SECRET-EVENT-PARAM") && !text.contains(&foreign),
            "a refused page must not echo the untrusted contract or payload: {text}"
        );
    }
    assert_reads_are_send_free(&h);
}

/// Zero events is a successful empty page, not a gap the tool is allowed
/// to describe as a complete history. The empty answer must report zero
/// events for that page and must state that the retention floor is
/// server-set and unknown, never a start ledger or an age it does not
/// know.
#[test]
fn market_events_reports_an_empty_page_truthfully_and_never_as_a_complete_history() {
    let h = harness("events-empty-page");
    let pool = contract_id("explorer-cli-pool");

    let out = run_reads(
        &h,
        &[
            "market",
            "events",
            "pool",
            "--start-ledger",
            "42",
            "--count",
            "3",
        ],
        &[
            ("FAKE_STELLAR_EVENTS", ""),
            ("FAKE_STELLAR_STDERR", "No events"),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "an empty page is a successful read: {}",
        output_text(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rendered: Value =
        serde_json::from_str(stdout.trim()).expect("the history dashboard must render as JSON");
    assert_eq!(rendered.get("returned").and_then(Value::as_u64), Some(0));
    assert_eq!(
        rendered.get("events").and_then(Value::as_array),
        Some(&Vec::new()),
        "an empty page must report no events, never a placeholder"
    );
    assert!(
        matches!(rendered.get("next_cursor"), None | Some(Value::Null)),
        "an empty page may not hand out a continuation cursor"
    );
    assert!(
        stdout.contains("this page only") && stdout.contains("server-set"),
        "an empty page must be reported as this page only with a server-set floor: {stdout}"
    );
    let log = fake_log(&h);
    assert!(
        log.contains(&format!(
            "events --id {pool} --start-ledger 42 --count 3 --output json"
        )),
        "an empty answer still requires the bounded read:\n{log}"
    );
    assert_reads_are_send_free(&h);
}

/// A receipt is read through the one read-only `tx fetch events` lookup
/// for the requested hash. An empty record and a NOT_FOUND lookup are
/// both inconclusive: neither may be reported as proof that a transaction
/// was absent, succeeded, or failed, and a malformed hash never reaches
/// the network at all.
#[test]
fn market_receipt_reads_only_and_reports_absent_records_as_inconclusive() {
    let h = harness("receipt-inconclusive");
    let hash = "dd77fe6e7554cdf3d4ab32730fc2c9fb1039f0b16c5ed99769ae62b9809801f1";
    let empty = r#"{"contract_events":[],"diagnostic_events":[],"transaction_events":[]}"#;

    let empty_run = run_reads(
        &h,
        &["market", "receipt", hash],
        &[("FAKE_STELLAR_TX", empty)],
        &[],
    );
    assert!(
        empty_run.status.success(),
        "an inconclusive lookup is still a successful read: {}",
        output_text(&empty_run)
    );
    let empty_stdout = String::from_utf8_lossy(&empty_run.stdout);
    let rendered: Value =
        serde_json::from_str(empty_stdout.trim()).expect("the receipt must render as JSON");
    assert_eq!(
        rendered.get("status").and_then(Value::as_str),
        Some("inconclusive"),
        "an absent record must be reported as inconclusive"
    );
    assert!(
        empty_stdout.contains("never prove"),
        "an absent record must be reported as proof of nothing: {empty_stdout}"
    );
    assert!(
        !empty_stdout.contains("succeeded") && !empty_stdout.contains("\"failed\""),
        "an absent record may not be reported as an outcome: {empty_stdout}"
    );

    let missing_run = run_reads(
        &h,
        &["market", "receipt", hash],
        &[(
            "FAKE_STELLAR_STDERR",
            "error: transaction not found on network",
        )],
        &[("FAKE_STELLAR_TX_STATUS", "1")],
    );
    assert!(
        !missing_run.status.success(),
        "a NOT_FOUND lookup is not a receipt: {}",
        output_text(&missing_run)
    );
    let missing_text = output_text(&missing_run);
    assert!(
        missing_text.contains("receipt inconclusive") && missing_text.contains("not proven"),
        "a NOT_FOUND lookup must be reported as inconclusive: {missing_text}"
    );

    for malformed in [&"f".repeat(63), &"F".repeat(64), "0xdeadbeef"] {
        let rejected = run_reads(&h, &["market", "receipt", malformed], &[], &[]);
        assert!(
            !rejected.status.success(),
            "a malformed transaction hash must be refused: {}",
            output_text(&rejected)
        );
        assert!(
            output_text(&rejected).contains("64 lowercase hex"),
            "the refusal must name the hash boundary: {}",
            output_text(&rejected)
        );
    }

    let log = fake_log(&h);
    assert!(
        log.contains(&format!("tx fetch events --hash {hash} --output json")),
        "a receipt must come from the read-only transaction lookup:\n{log}"
    );
    assert!(
        log.lines()
            .filter(|line| line.starts_with("tx fetch events"))
            .all(|line| line.contains(&format!("--hash {hash}"))),
        "a receipt lookup may only ever ask for the requested hash:\n{log}"
    );
    assert_reads_are_send_free(&h);
}

/// Walks a rendered report and collects every numeric value that sits under
/// a key naming `field`. A stale price that is invented as `0` arrives here
/// as a number under a price-named key, which is exactly what an operator
/// must never be shown.
fn numeric_field_paths(value: &Value, field: &str, path: String, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = format!("{path}.{key}");
                if key.contains(field) && matches!(child, Value::Number(_)) {
                    found.push(child_path.clone());
                }
                numeric_field_paths(child, field, child_path, found);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                numeric_field_paths(child, field, format!("{path}[{index}]"), found);
            }
        }
        _ => {}
    }
}

/// Collects every string in a rendered report that equals `needle`. Shipped
/// leg states are fixed literals, so a fabricated price is only ever reported
/// under the one state that means a price was observed.
fn string_occurrences(value: &Value, needle: &str, found: &mut Vec<String>) {
    match value {
        Value::String(text) if text == needle => found.push(text.clone()),
        Value::Object(map) => {
            for child in map.values() {
                string_occurrences(child, needle, found);
            }
        }
        Value::Array(items) => {
            for child in items {
                string_occurrences(child, needle, found);
            }
        }
        _ => {}
    }
}

/// A market whose price cannot be read must never be reported as playable.
/// Every status read in this run returns the void the live `lastprice` ABI
/// declares for an absent price, so the shipped report must refuse readiness,
/// must name the stale price as a reason, must keep each reserve leg visible
/// instead of dropping it, and must not invent a figure for the price. The CLI
/// must exit non-zero with the shipped refusal, and every read must stay a
/// `--send no` simulation.
#[test]
fn market_status_refuses_readiness_when_a_price_is_absent_and_fabricates_nothing() {
    let h = harness("status-absent-price");
    let manifest_before = fs::read(&h.manifest).expect("fixture manifest bytes");

    let out = run_reads(
        &h,
        &["market", "status"],
        &[("FAKE_STELLAR_VIEW", "null")],
        &[],
    );
    assert!(
        !out.status.success(),
        "a market with no readable price must not report readiness: {}",
        output_text(&out)
    );
    let text = output_text(&out);
    assert!(
        text.contains("market prices are not ready"),
        "an unreadable price must be refused by name: {text}"
    );

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let rendered: Value = serde_json::from_str(stdout.trim())
        .expect("market status must still render its partial dashboard");
    assert_eq!(
        rendered.get("ready").and_then(Value::as_bool),
        Some(false),
        "an unreadable price may never be reported as ready: {stdout}"
    );
    assert_eq!(
        rendered.get("readiness").and_then(Value::as_str),
        Some("not-ready"),
        "an unreadable price must be reported as not-ready: {stdout}"
    );
    let reasons = rendered
        .get("reasons")
        .and_then(Value::as_array)
        .expect("a refused dashboard must carry its reasons");
    assert!(
        reasons.iter().any(|reason| reason.as_str()
            == Some("a required adapter price is null, stale, or unavailable")),
        "a refused dashboard must name the stale price as a reason: {reasons:?}"
    );
    assert!(
        reasons.iter().any(|reason| reason.as_str()
            == Some("a required reserve is unavailable, disabled, or malformed")),
        "a refused dashboard must name the unreadable reserves as a reason: {reasons:?}"
    );

    let mut invented_prices: Vec<String> = Vec::new();
    numeric_field_paths(&rendered, "price", String::new(), &mut invented_prices);
    assert!(
        invented_prices.is_empty(),
        "an absent price must never be rendered as a figure: {invented_prices:?}"
    );
    let mut observed_prices: Vec<String> = Vec::new();
    string_occurrences(&rendered, "adapter-price-observed", &mut observed_prices);
    assert!(
        observed_prices.is_empty(),
        "an absent price may never be reported as observed: {stdout}"
    );

    let oracle = rendered
        .get("oracle")
        .and_then(Value::as_object)
        .expect("a refused dashboard must keep the oracle legs visible");
    assert_eq!(
        oracle.len(),
        4,
        "every registered market asset must keep its own price leg: {stdout}"
    );
    let mut explicit_absence = 0usize;
    for asset in ["weth", "usdc", "xlm", "stable"] {
        let price = oracle
            .get(asset)
            .and_then(|panel| panel.get("adapter_lastprice"))
            .unwrap_or_else(|| panic!("asset {asset} lost its price leg: {stdout}"));
        let state = price
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("the price leg for {asset} lost its state: {stdout}"));
        assert!(
            [
                "adapter-price-null",
                "adapter-price-malformed",
                "read-failed",
                "null",
                "unsupported",
                "interface-unavailable",
                "interface-malformed",
                "interface-mismatch",
                "missing",
            ]
            .contains(&state),
            "an unreadable price must be reported as an explicit absent state, not as a value: {state}"
        );
        if state == "adapter-price-null" {
            explicit_absence += 1;
            assert!(
                price.get("observed").map_or(true, Value::is_null),
                "an absent price must carry no observed data: {stdout}"
            );
        }
    }
    assert!(
        explicit_absence > 0,
        "an absent price must be reported explicitly as null somewhere: {stdout}"
    );

    let reserves = rendered
        .get("pool")
        .and_then(|pool| pool.get("reserves"))
        .and_then(Value::as_object)
        .expect("a refused dashboard must keep the reserve legs visible");
    assert_eq!(
        reserves.len(),
        3,
        "every reserve asset must keep its own leg: {stdout}"
    );
    for asset in ["stable", "xlm", "weth"] {
        let reserve = reserves
            .get(asset)
            .unwrap_or_else(|| panic!("reserve {asset} was dropped from the dashboard: {stdout}"));
        let state = reserve
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("reserve leg {asset} lost its state: {stdout}"));
        assert!(
            !state.is_empty() && state != "reserve-enabled",
            "an unreadable reserve must stay visible and may never be reported as enabled: {state}"
        );
    }

    let adapter = contract_id("explorer-cli-adapter");
    let admin = account_id("explorer-cli-admin");
    let log = fake_log(&h);
    assert!(
        log.lines().any(|line| {
            line.starts_with("contract invoke ")
                && line.contains(&format!("--id {adapter}"))
                && line.contains(&format!("--source {admin}"))
                && line.contains("--send no")
                && line.contains("-- lastprice")
        }),
        "the absent price must be sought from the manifest adapter through a simulation sourced by the manifest signer:\n{log}"
    );
    for line in log
        .lines()
        .filter(|line| line.starts_with("contract invoke "))
    {
        assert!(
            line.contains(&format!("--source {admin}")),
            "no market status read may invoke from a source other than the manifest signer: {line}"
        );
    }
    assert_eq!(
        fs::read(&h.manifest).expect("fixture manifest bytes"),
        manifest_before,
        "a status read may not touch the frozen manifest"
    );
    assert_reads_are_send_free(&h);
}

#[test]
fn guarded_market_send_refuses_noninteractive_stdin_before_any_cli_or_journal_use() {
    let h = harness("send-requires-tty");
    let signer_dir = h.scratch.join("identities");
    fs::create_dir_all(&signer_dir).expect("identity directory");
    let before = fs::read(&h.manifest).expect("frozen manifest bytes");
    let out = run_cli(
        &h,
        V261,
        SPEC_LIVE,
        &[
            "market",
            "call",
            "source.0",
            "set_data",
            "--signer",
            "admin",
            "--signer-config-dir",
            signer_dir.to_str().expect("test path is UTF-8"),
            "--execute",
            "--arg",
            "price=10000000",
        ],
    );
    assert!(!out.status.success(), "a pipe cannot authorize a send");
    assert!(
        output_text(&out).contains("requires an interactive terminal"),
        "refusal should tell the operator how to confirm safely"
    );
    assert!(
        fake_log(&h).is_empty(),
        "noninteractive confirmation must refuse before any CLI child starts"
    );
    assert!(
        !h.run.join("market-pending").exists(),
        "no intent is reserved for a noninteractive confirmation"
    );
    assert_eq!(fs::read(&h.manifest).unwrap(), before);
}

/// Mainnet responses are refused by each chain-read boundary. A headless
/// `--execute` is refused even earlier because it has no confirmation
/// terminal. Query, preview and inspect prove the endpoint separately,
/// before any interface fetch, simulation, journal reservation or send.
#[test]
fn mainnet_endpoint_refuses_every_guarded_market_command_before_any_chain_or_journal_use() {
    let h = harness("mainnet-refusal");
    // A completed deployment owns an empty journal root; without it the
    // preview correctly refuses local custody before reaching the RPC seam.
    fs::create_dir(h.run.join("market-pending")).expect("completed run journal root");
    let manifest_before = fs::read(&h.manifest).expect("fixture manifest bytes");
    let signer_dir = h.scratch.join("identities");
    fs::create_dir_all(&signer_dir).expect("identity directory");
    let mainnet = NETWORK_INFO_MAINNET.to_string();
    let asset = asset_value("BLND");

    let refused = run_reads(
        &h,
        &[
            "market",
            "query",
            "source.0",
            "lastprice",
            "--arg",
            &format!("asset={asset}"),
        ],
        &[("FAKE_STELLAR_NETWORK_INFO", &mainnet)],
        &[],
    );
    assert!(
        !refused.status.success(),
        "a mainnet endpoint must refuse a query"
    );
    let refused = output_text(&refused);
    assert!(
        refused.contains("market RPC did not prove the exact testnet passphrase"),
        "a mainnet endpoint must fail with the fixed identity refusal: {refused}"
    );
    assert!(
        !refused.contains("FAKE-MAINNET-SENTINEL") && !refused.contains("FAKE-FRIENDBOT"),
        "no byte of the endpoint's own response may echo back: {refused}"
    );

    let call = run_reads(
        &h,
        &[
            "market",
            "call",
            "source.0",
            "set_data",
            "--signer",
            "admin",
            "--signer-config-dir",
            signer_dir.to_str().expect("test path is UTF-8"),
            "--execute",
            "--arg",
            "price=10000000",
        ],
        &[("FAKE_STELLAR_NETWORK_INFO", &mainnet)],
        &[],
    );
    assert!(
        !call.status.success(),
        "headless execute cannot authorize a send"
    );
    let refused = output_text(&call);
    assert!(
        refused.contains("requires an interactive terminal")
            && !refused.contains("FAKE-MAINNET-SENTINEL"),
        "a local terminal refusal must not echo endpoint bytes"
    );
    assert_eq!(
        network_probe_count(&h),
        1,
        "headless send must not launch a second CLI child after the earlier query"
    );

    let preview = run_reads(
        &h,
        &[
            "market",
            "call",
            "source.0",
            "set_data",
            "--signer",
            "admin",
            "--signer-config-dir",
            signer_dir.to_str().expect("test path is UTF-8"),
            "--arg",
            "price=10000000",
        ],
        &[("FAKE_STELLAR_NETWORK_INFO", &mainnet)],
        &[],
    );
    let refused = output_text(&preview);
    assert!(
        !preview.status.success()
            && refused.contains("market RPC did not prove the exact testnet passphrase")
            && !refused.contains("FAKE-MAINNET-SENTINEL"),
        "a read-only preview must refuse an unproven endpoint before the ABI read"
    );

    let inspect = run_reads(
        &h,
        &["market", "inspect", "source.0", "set_data"],
        &[("FAKE_STELLAR_NETWORK_INFO", &mainnet)],
        &[],
    );
    assert!(
        !inspect.status.success(),
        "a mainnet endpoint must refuse inspection: {}",
        output_text(&inspect)
    );
    let refused = output_text(&inspect);
    assert!(
        refused.contains("market RPC did not prove the exact testnet passphrase"),
        "an inspection against a mainnet endpoint must fail with the fixed identity refusal: {refused}"
    );
    assert!(
        !refused.contains("FAKE-MAINNET-SENTINEL"),
        "an inspection refusal must carry no endpoint bytes: {refused}"
    );

    assert_eq!(
        network_probe_count(&h),
        3,
        "query, preview and inspect each require a fresh RPC identity proof"
    );
    assert_only_guard_probes_logged(&h);
    let log = fake_log(&h);
    assert!(
        !log.contains("contract info interface")
            && !log.contains("contract invoke")
            && !log.contains("--send")
            && !log.contains("tx submit"),
        "a refused mainnet endpoint may never reach a chain read or send"
    );
    assert!(
        fs::read_dir(h.run.join("market-pending"))
            .expect("completed run journal root")
            .next()
            .is_none(),
        "a refused mainnet endpoint may not reserve a journal coordinate"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("fixture manifest bytes"),
        manifest_before,
        "refusals must leave the completed manifest byte-identical"
    );
}

/// A real terminal removes the cheap headless refusal, so a confirmed
/// action must still reject a mainnet RPC before ABI, signer, or send.
#[test]
fn confirmed_market_call_in_a_pty_refuses_mainnet_before_reserving_custody() {
    if !Command::new("script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("SKIP: script(1) is required for the real-terminal identity test");
        return;
    }
    let h = harness("mainnet-confirm-pty");
    let manifest_before = fs::read(&h.manifest).expect("frozen manifest");
    fs::create_dir(h.run.join("market-pending")).expect("completed journal root");
    let signer_dir = h.scratch.join("identities");
    fs::create_dir(&signer_dir).expect("signer fixture directory");
    fs::write(&h.network_info, NETWORK_INFO_MAINNET).expect("mainnet RPC fixture");
    let spec = h.scratch.join("fake.spec.json");
    fs::write(&spec, SPEC_LIVE).expect("live ABI fixture");
    let shell_path = |path: &std::path::Path| {
        let text = path.to_string_lossy();
        assert!(!text.contains('\'') && !text.contains('\n'));
        format!("'{text}'")
    };
    let command = format!(
        "{} --out {} --stellar-bin {} --rpc-url https://rpc.testnet.stellar.org --passphrase 'Test SDF Network ; September 2015' market call source.0 set_data --signer admin --signer-config-dir {} --execute --arg price=10000000",
        shell_path(std::path::Path::new(BIN)),
        shell_path(&h.run),
        shell_path(&h.fake),
        shell_path(&signer_dir),
    );
    let output = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::null())
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_SPEC", &spec)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .output()
        .expect("drive actual PTY-backed guarded call");
    let rendered = output_text(&output);
    assert!(
        !output.status.success()
            && rendered.contains("market RPC did not prove the exact testnet passphrase")
            && !rendered.contains("FAKE-MAINNET-SENTINEL")
            && !rendered.contains("requires an interactive terminal"),
        "PTY-backed call must reach the fixed RPC refusal without echoing endpoint bytes"
    );
    assert_eq!(network_probe_count(&h), 1);
    assert_only_guard_probes_logged(&h);
    assert!(
        fs::read_dir(h.run.join("market-pending"))
            .expect("completed journal root")
            .next()
            .is_none(),
        "a mainnet endpoint cannot reserve a confirmed intent"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("frozen manifest"),
        manifest_before
    );
}

#[test]
fn live_market_reads_run_from_the_controlled_root_with_confined_signing_config() {
    let h = harness("controlled-root");
    let cwd_log = h.scratch.join("cwd.log");
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(&spec_file, SPEC_LIVE).expect("live ABI fixture");
    let out = Command::new(BIN)
        .arg("--out")
        .arg(&h.run)
        .arg("--stellar-bin")
        .arg(&h.fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .arg("--passphrase")
        .arg("Test SDF Network ; September 2015")
        .args(["market", "inspect", "source.0", "set_data"])
        .current_dir(&h.run)
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_SPEC", &spec_file)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env("FAKE_STELLAR_CWD_LOG", &cwd_log)
        .output()
        .expect("inspect through fake CLI from the market run directory");
    assert!(
        out.status.success(),
        "the controlled-root inspection must work"
    );
    assert_rendered_live_set_data_entry(&String::from_utf8_lossy(&out.stdout));
    let report = fs::read_to_string(&cwd_log).expect("fake recorded executor cwd");
    assert!(
        !report.is_empty(),
        "the command really reached the fake CLI"
    );
    for line in report.lines() {
        let (cwd, xdg) = line.split_once('|').expect("recorded cwd and config");
        assert_eq!(cwd, "/", "the fake may not execute inside the market run");
        assert_eq!(
            xdg,
            h.run.join(".xdg").to_str().expect("UTF-8 test path"),
            "XDG configuration must stay confined to this run"
        );
    }
    let logged = fake_log(&h);
    let expected = format!("--config-dir {}", h.run.join(".stellar").display());
    assert!(logged
        .lines()
        .any(|line| line.starts_with("network info") && line.contains(&expected)));
    assert!(logged
        .lines()
        .any(|line| line.starts_with("contract info") && line.contains(&expected)));
    assert!(!logged.contains("--send yes"));
    assert!(!h.run.join("market-pending").exists());
}

#[test]
fn market_binary_lookup_skips_nonexecutables_and_direct_paths_refuse_them() {
    let h = harness("binary-executable");
    let first = h.scratch.join("first");
    let second = h.scratch.join("second");
    fs::create_dir_all(&first).expect("first PATH dir");
    fs::create_dir_all(&second).expect("second PATH dir");
    let decoy = first.join("stellar");
    fs::write(&decoy, "#!/bin/sh\nexit 99\n").expect("nonexecutable decoy");
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o644)).expect("clear execute bits");
    fs::copy(&h.fake, second.join("stellar")).expect("copy executable CLI");
    fs::set_permissions(second.join("stellar"), fs::Permissions::from_mode(0o755))
        .expect("second PATH binary executable");
    let spec = h.scratch.join("fake.spec.json");
    fs::write(&spec, SPEC_LIVE).expect("live ABI fixture");
    let path = format!(
        "{}:{}:{}",
        first.display(),
        second.display(),
        std::env::var("PATH").expect("test coreutils PATH")
    );
    let command = |bin: &std::path::Path| {
        Command::new(BIN)
            .arg("--out")
            .arg(&h.run)
            .arg("--stellar-bin")
            .arg(bin)
            .arg("--rpc-url")
            .arg("https://rpc.testnet.stellar.org")
            .arg("--passphrase")
            .arg("Test SDF Network ; September 2015")
            .args(["market", "inspect", "source.0", "set_data"])
            .env("PATH", &path)
            .env("FAKE_STELLAR_LOG", &h.log)
            .env("FAKE_STELLAR_VERSION", V261)
            .env("FAKE_STELLAR_SPEC", &spec)
            .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
            .output()
            .expect("spawn guarded explorer")
    };
    let selected = command(std::path::Path::new("stellar"));
    assert!(
        selected.status.success(),
        "PATH must skip a nonexecutable decoy"
    );
    assert_rendered_live_set_data_entry(&String::from_utf8_lossy(&selected.stdout));
    let before = fake_log(&h);
    assert!(before.contains("network info --output json"));
    let refused = command(&decoy);
    assert!(
        !refused.status.success(),
        "explicit nonexecutable cannot launch"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("market stellar CLI could not be resolved safely"),
        "refusal is fixed and input-free"
    );
    assert!(
        !String::from_utf8_lossy(&refused.stderr).contains(decoy.to_string_lossy().as_ref()),
        "the refused executable path is not echoed"
    );
    assert_eq!(
        fake_log(&h),
        before,
        "nonexecutable refusal must spawn no child"
    );
    assert!(!h.run.join("market-pending").exists());
}

/// Exercise the shipped explorer in a real terminal without owning a PTY
/// driver or signing credentials. `script(1)` is the same native seam used by
/// the deployment TUI tests.
#[test]
fn explore_terminal_shows_roles_and_exits_with_only_read_only_reads() {
    let h = harness("explore-terminal");
    let manifest_before = fs::read(&h.manifest).expect("fixture manifest bytes");
    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let command = format!(
        "{} --out {} --stellar-bin {} --rpc-url https://rpc.testnet.stellar.org \
         --passphrase 'Test SDF Network ; September 2015' market tui --mode explore",
        quote(std::path::Path::new(BIN)),
        quote(&h.run),
        quote(&h.fake)
    );
    let transcript = h.scratch.join("explore.typescript");
    let mut child = Command::new("script")
        .args(["-q", "-e", "-f", "-c", &command])
        .arg(&transcript)
        .env_remove("STELLAR_RPC_URL")
        .env_remove("STELLAR_NETWORK_PASSPHRASE")
        .env("TERM", "xterm-256color")
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_SPEC", h.scratch.join("fake.spec.json"))
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch explorer under script(1)");
    let until = Instant::now() + Duration::from_secs(12);
    let mut screen = String::new();
    while Instant::now() < until {
        screen = String::from_utf8_lossy(&fs::read(&transcript).unwrap_or_default()).into_owned();
        if screen.contains("manifest roles")
            && screen.contains("source.0")
            && screen.contains("assets")
        {
            break;
        }
        if child.try_wait().expect("poll explorer").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !screen.contains("manifest roles")
        || !screen.contains("source.0")
        || !screen.contains("assets")
    {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "explorer never rendered live functions and roles; {} transcript bytes",
            screen.len()
        );
    }
    assert!(screen.contains("MARKET / testnet / unsigned / read-only"));
    child
        .stdin
        .as_mut()
        .expect("terminal input")
        .write_all(b"/set_data\r")
        .expect("search live function");
    let until = Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while Instant::now() < until {
        let bytes = fs::read(&transcript).unwrap_or_default();
        let rendered = String::from_utf8_lossy(&bytes);
        if rendered.contains("set_data")
            && rendered.contains("price")
            && rendered.contains("timestamp")
        {
            found = true;
            break;
        }
        if child.try_wait().expect("poll function search").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !found {
        let _ = child.kill();
        let _ = child.wait();
        panic!("explorer search did not expose the live function's input fields");
    }
    child
        .stdin
        .take()
        .expect("terminal input")
        .write_all(b"q")
        .expect("quit explorer");
    let until = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll explorer exit") {
            break status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("explorer did not quit after q");
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "q must cleanly exit the unsigned explorer"
    );
    let log = fake_log(&h);
    assert!(log
        .lines()
        .any(|line| line.starts_with("network info --output json")));
    assert!(
        log.lines()
            .any(|line| line.starts_with("contract info interface ")),
        "the function browser must inspect the deployed ABI"
    );
    assert!(log.lines().all(|line| {
        line.contains("--version")
            || line.starts_with("network info --output json")
            || line.starts_with("contract info interface ")
    }));
    assert!(!h.run.join("market-pending").exists());
    assert_eq!(fs::read(&h.manifest).unwrap(), manifest_before);
}

/// The real pair-run watcher must re-adopt a B-only snapshot change and retire
/// a removed selected record without touching any run authority or invoking stellar.
#[test]
fn tui_watch_reloads_b_snapshot_and_retires_removed_record() {
    let h = harness("tui-watch-currency");
    let pair_run = h.scratch.join("pair-run");
    fs::create_dir_all(&pair_run).expect("fresh pair run directory");
    let pair_manifest = pair_run.join("manifest.toml");
    let state = pair_run.join("state.json");
    let lock = pair_run.join("run.lock");
    let step = pair_run.join("evidence/lending/000-borrow");
    fs::create_dir_all(&step).expect("evidence fixture");
    fs::write(
        &pair_manifest,
        r#"profile = "stock-vs-fork"
description = "synthetic watch currency fixture"
[net]
rpc_url = "https://rpc.invalid"
passphrase = "Test SDF Network ; September 2015"
config_dir = "/c"
headers = []
[identities]
admin = "Gadmin"
frodo = "Gfrodo"
sam = "Gsam"
user = "Guser"
[a]
label = "s"
deploy_ledger = 1
wasm = {}
minted = {}
[a.contracts]
blnd = ""
usdc = ""
weth = ""
xlm = ""
stable = "CSTBL"
lp = ""
emitter = ""
backstop = ""
pool_factory = ""
oracle = "CORC"
pool = ""
[a.reserves.stable]
index = 0
decimals = 6
[b]
label = "s"
deploy_ledger = 1
wasm = {}
minted = {}
[b.contracts]
blnd = ""
usdc = ""
weth = ""
xlm = ""
stable = "CSTBL"
lp = ""
emitter = ""
backstop = ""
pool_factory = ""
oracle = "CORC"
pool = ""
[b.reserves.stable]
index = 0
decimals = 6
"#,
    )
    .expect("write synthetic pair manifest");
    fs::write(
        &state,
        serde_json::to_vec(&serde_json::json!({
            "completed": {"lending": ["borrow"]},
            "requested": {},
            "minted": {},
            "auction_blocks": {},
            "frozen": false,
            "scenario_order": ["lending"]
        }))
        .expect("encode fixture state"),
    )
    .expect("write synthetic run state");
    let stat = fs::read_to_string(format!("/proc/{}/stat", std::process::id()))
        .expect("read test process start time");
    let start_ticks = stat
        .rsplit_once(')')
        .expect("process stat comm delimiter")
        .1
        .split_whitespace()
        .nth(19)
        .expect("process stat start ticks");
    fs::write(&lock, format!("{} {start_ticks}\n", std::process::id()))
        .expect("write live run lock");

    let snapshot = serde_json::json!({
        "positions": {
            "admin": {"collateral": {}, "liabilities": {}, "supply": {}},
            "frodo": {"collateral": {}, "liabilities": {}, "supply": {}},
            "sam": {"collateral": {}, "liabilities": {}, "supply": {}},
            "user": {"collateral": {}, "liabilities": {}, "supply": {}},
            "pool": {"collateral": {}, "liabilities": {}, "supply": {}},
            "backstop": {"collateral": {}, "liabilities": {}, "supply": {}}
        },
        "reserves": {"stable": {"asset": "CSTBL", "config": {"index": 0}, "data": {
            "b_supply": "0", "d_supply": "0",
            "b_rate": "1000000000000", "d_rate": "1000000000000",
            "backstop_credit": "0"
        }}},
        "config": {"bstop_rate": 0, "oracle": "CORC"},
        "backstop_pool": {"shares": "0", "tokens": "0"},
        "backstop_users": {
            "admin": {"shares": "0", "q4w": []},
            "frodo": {"shares": "0", "q4w": []},
            "sam": {"shares": "0", "q4w": []},
            "user": {"shares": "0", "q4w": []}
        },
        "reward_zone": [],
        "balances": {
            "blnd": {}, "usdc": {}, "weth": {}, "xlm": {}, "stable": {},
            "lp": {"backstop": "0"}
        },
        "auctions": {},
        "raw": {
            "pool_emis": null,
            "res_data_stable": {
                "key": "res", "val": "0", "last_modified": 0, "live_until": null
            }
        }
    });
    let invocation = serde_json::json!({
        "ok": true, "value": null, "error": null, "events": [], "stderr": ""
    });
    for side in ["a", "b"] {
        fs::write(
            step.join(format!("{side}.json")),
            serde_json::to_vec(&serde_json::json!({
                "label": "borrow",
                "side": "s",
                "invocation": invocation.clone(),
                "before": snapshot.clone(),
                "snapshot": snapshot.clone(),
                "violations": []
            }))
            .expect("encode source-style step record"),
        )
        .expect("write source-style step record");
    }
    let a_path = step.join("a.json");
    let b_path = step.join("b.json");
    let diff_path = step.join("diff.json");
    fs::write(&diff_path, b"[]\n").expect("write unchanged diff fixture");
    let manifest_before = fs::read(&pair_manifest).expect("pair manifest bytes");
    let harness_manifest_before = fs::read(&h.manifest).expect("harness manifest bytes");
    let state_before = fs::read(&state).expect("run state bytes");
    let lock_before = fs::read(&lock).expect("run lock bytes");
    let a_before = fs::read(&a_path).expect("A evidence bytes");
    let b_before = fs::read(&b_path).expect("B evidence bytes");
    let diff_before = fs::read(&diff_path).expect("diff evidence bytes");

    fs::write(
        &h.fake,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_STELLAR_LOG\"\nexit 72\n",
    )
    .expect("write deny-all stellar fake");
    fs::set_permissions(&h.fake, fs::Permissions::from_mode(0o755)).expect("chmod deny-all fake");
    fs::write(&h.log, "").expect("initialize deny-all call log");
    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let command = format!(
        "stty rows 24 cols 80; exec {} --out {} --stellar-bin {} \
         --rpc-url https://rpc.invalid --passphrase 'Test SDF Network ; September 2015' tui --watch",
        quote(std::path::Path::new(BIN)),
        quote(&pair_run),
        quote(&h.fake)
    );
    let transcript = h.scratch.join("tui-watch-currency.typescript");
    let mut child = WatchChild(
        Command::new("script")
            .args(["-q", "-e", "-f", "-c", &command])
            .arg(&transcript)
            .env_remove("STELLAR_RPC_URL")
            .env_remove("STELLAR_NETWORK_PASSPHRASE")
            .env_remove("STELLAR_CONTRACT_ID")
            .env("TERM", "xterm-256color")
            .env("FAKE_STELLAR_LOG", &h.log)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("script(1) is required for the pair-watch PTY regression; do not skip"),
    );
    let screen = wait_for_watch_screen(
        &mut child,
        &transcript,
        "initial borrow with diffs 0",
        |screen| {
            screen.lines().any(|line| {
                line.contains("lending/borrow")
                    && line.contains("A ok")
                    && line.contains("B ok")
                    && line.contains("diffs 0")
            })
        },
    );
    assert!(
        screen.contains("WATCH"),
        "watch mode must be visible:\n{screen}"
    );

    let mut changed_b: Value = serde_json::from_slice(&b_before).expect("parse B record fixture");
    changed_b["snapshot"]["config"]["bstop_rate"] = serde_json::json!(999);
    fs::write(
        &b_path,
        serde_json::to_vec(&changed_b).expect("encode B-only mutation"),
    )
    .expect("change only B snapshot");
    assert_eq!(fs::read(&diff_path).unwrap(), diff_before);
    let changed_screen = wait_for_watch_screen(
        &mut child,
        &transcript,
        "B-only mutation with diffs 1",
        |screen| {
            screen.lines().any(|line| {
                line.contains("lending/borrow")
                    && line.contains("A ok")
                    && line.contains("B ok")
                    && line.contains("diffs 1")
            })
        },
    );
    assert!(changed_screen.contains("WATCH"));

    fs::remove_dir_all(&step).expect("remove selected evidence");
    let empty_screen = wait_for_watch_screen(
        &mut child,
        &transcript,
        "removed record and unavailable status",
        |screen| {
            screen.contains("no steps recorded yet")
                && screen.contains("no steps yet")
                && !screen.contains("lending/borrow")
        },
    );
    assert!(
        empty_screen.contains("verdict INCOMPLETE") && !empty_screen.contains("verdict PASS"),
        "removed evidence must not retain a certification:\n{empty_screen}"
    );

    fs::create_dir_all(&step).expect("restore evidence directory");
    fs::write(&a_path, &a_before).expect("restore A evidence");
    fs::write(&b_path, &b_before).expect("restore original B evidence");
    fs::write(&diff_path, &diff_before).expect("restore unchanged diff");
    let recovered_screen = wait_for_watch_screen(
        &mut child,
        &transcript,
        "recovered borrow with diffs 0",
        |screen| {
            screen.lines().any(|line| {
                line.contains("lending/borrow")
                    && line.contains("A ok")
                    && line.contains("B ok")
                    && line.contains("diffs 0")
            })
        },
    );
    assert!(recovered_screen.contains("WATCH"));

    child
        .stdin
        .as_mut()
        .expect("watch terminal input")
        .write_all(b"q")
        .expect("quit watch TUI");
    let until = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll watch TUI exit") {
            break status;
        }
        assert!(Instant::now() < until, "watch TUI did not quit after q");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "read-only watcher must exit cleanly");
    assert_eq!(fake_log(&h), "", "watcher must not invoke stellar");
    assert_eq!(fs::read(&pair_manifest).unwrap(), manifest_before);
    assert_eq!(fs::read(&h.manifest).unwrap(), harness_manifest_before);
    assert_eq!(fs::read(&state).unwrap(), state_before);
    assert_eq!(fs::read(&lock).unwrap(), lock_before);
    assert_eq!(fs::read(&a_path).unwrap(), a_before);
    assert_eq!(fs::read(&b_path).unwrap(), b_before);
    assert_eq!(fs::read(&diff_path).unwrap(), diff_before);
}

#[test]
fn guided_refresh_uses_registered_assets_instead_of_the_first_manifest_token() {
    let h = harness("guided-refresh-registry");
    let blnd = contract_id("explorer-cli-tok-blnd");
    let token_id = |token: &str| contract_id(&format!("explorer-cli-tok-{token}"));
    let weth_id = token_id("weth");
    let usdc_id = token_id("usdc");
    let xlm_id = token_id("xlm");
    let stable_id = token_id("stable");
    let manifest = manifest_toml().replace("[tokens]\n", &format!("[tokens]\nblnd = \"{blnd}\"\n"));
    fs::write(&h.manifest, &manifest).unwrap();
    let registered: Vec<Value> = ["weth", "usdc", "xlm", "stable"]
        .iter()
        .map(|token| serde_json::json!({"Stellar": token_id(token)}))
        .collect();
    let registry = h.scratch.join("registered-assets.json");
    fs::write(&registry, serde_json::to_vec(&registered).unwrap()).unwrap();
    let view = h.scratch.join("refresh-result.json");
    fs::write(&view, r#""Updated""#).unwrap();
    let mut spec: Vec<Value> = serde_json::from_str(SPEC_LIVE).unwrap();
    spec.extend([
        serde_json::json!({"function_v0": {
            "doc": "", "name": "registered_assets", "inputs": [],
            "outputs": [{"vec": {"element_type": {"udt": {"name": "Asset"}}}}]
        }}),
        serde_json::json!({"function_v0": {
            "doc": "", "name": "refresh",
            "inputs": [{"doc": "", "name": "asset", "type_": {"udt": {"name": "Asset"}}}],
            "outputs": []
        }}),
    ]);
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(&spec_file, serde_json::to_vec(&spec).unwrap()).unwrap();
    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let command = format!(
        "stty rows 32 cols 110; {} --out {} --stellar-bin {} \
         --rpc-url https://rpc.testnet.stellar.org \
         --passphrase 'Test SDF Network ; September 2015' market tui --mode explore",
        quote(std::path::Path::new(BIN)),
        quote(&h.run),
        quote(&h.fake)
    );
    let transcript = h.scratch.join("guided-refresh.typescript");
    let mut child = Command::new("script")
        .args(["-q", "-e", "-f", "-c", &command])
        .arg(&transcript)
        .env_remove("STELLAR_RPC_URL")
        .env_remove("STELLAR_NETWORK_PASSPHRASE")
        .env("TERM", "xterm-256color")
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_SPEC", &spec_file)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env("FAKE_STELLAR_REGISTERED_ASSETS", &registry)
        .env("FAKE_STELLAR_VIEW", &view)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch guided refresh in an isolated terminal");
    let wait_for = |child: &mut std::process::Child, marker: &str| {
        wait_for_terminal_marker(child, &transcript, marker, 12);
    };
    let transcript_len = || {
        fs::metadata(&transcript)
            .map(|metadata| metadata.len() as usize)
            .unwrap_or_default()
    };
    let send_keys = |child: &mut std::process::Child, keys: &[u8]| {
        child
            .stdin
            .as_mut()
            .expect("guided terminal input")
            .write_all(keys)
            .expect("send guided terminal keys");
    };
    let wait_after = |child: &mut std::process::Child, marker: &str, offset: usize| {
        wait_for_terminal_marker_after(child, &transcript, marker, 12, offset);
    };
    let screen_now = || {
        terminal_fixture_screen::<110, 32>(&fs::read(&transcript).expect("read guided terminal"))
    };
    let visible = |screen: &str| -> String { screen.split_whitespace().collect() };
    let log_lines = || -> Vec<String> {
        fs::read_to_string(&h.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let asset_reads = |lines: &[String]| -> usize {
        lines
            .iter()
            .filter(|line| line.contains("-- registered_assets"))
            .count()
    };
    wait_for(&mut child, "assets");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"a/oracle/refresh\r\r")
        .unwrap();
    wait_for(&mut child, "Manifest bindings are read-only.");
    wait_for(&mut child, "asset = weth");

    // Speed fix: a non-adjacent asset is picked with the same filterable
    // chooser the address and request-type fields use. The whole pick must
    // cost exactly one guide reload, never a walk through the eligible assets
    // that sit between weth and stable in the live registry.
    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[A\r");
    wait_after(&mut child, "Asset query:", offset);
    // Choices come from the live registry rather than the manifest token
    // table: BLND is a manifest token but was never registered.
    let offset = transcript_len();
    send_keys(&mut child, b"blnd");
    wait_after(&mut child, "No asset matches", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\x1b");
    wait_after(&mut child, "Asset unchanged.", offset);
    let reads_before_pick = log_lines();
    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[A\r");
    wait_after(&mut child, "Asset query:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"stable");
    wait_after(&mut child, "Asset query: stable", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "asset = stable", offset);
    // Wait until the reloaded form has settled, not merely been requested.
    let until = Instant::now() + Duration::from_secs(12);
    loop {
        let screen = visible(&screen_now());
        if screen.contains("asset=stable") && !screen.contains("Loadingguidedform") {
            break;
        }
        if Instant::now() >= until || child.try_wait().expect("poll guided terminal").is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "guided terminal did not show the reloaded stable form:\n{}",
                screen_now()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let pick = log_lines()[reads_before_pick.len()..].to_vec();
    assert_eq!(
        asset_reads(&pick),
        1,
        "choosing a non-adjacent asset must reload the guide exactly once:\n{pick:?}"
    );

    // The removed `[`/`]` cycling must stay inert: no re-binding and no extra
    // guide reload. The single read inside this window is the one the r
    // simulation performs while re-resolving the guide.
    let reads_before_keys = log_lines();
    let offset = transcript_len();
    send_keys(&mut child, b"[]r");
    wait_after(&mut child, "Stage: read-only simulation", offset);
    let keys = log_lines()[reads_before_keys.len()..].to_vec();
    assert_eq!(
        asset_reads(&keys),
        1,
        "the removed cycling keys must not reload the guide:\n{keys:?}"
    );
    let refreshed = keys
        .iter()
        .find(|line| line.starts_with("contract invoke ") && line.contains(" -- refresh "))
        .expect("the chosen guided call must reach the native no-send seam");
    assert!(
        refreshed.contains(stable_id.as_str()),
        "the payload must bind the asset chosen through the chooser:\n{refreshed}"
    );
    for other in [weth_id.as_str(), usdc_id.as_str(), xlm_id.as_str()] {
        assert!(
            !refreshed.contains(other),
            "the payload must bind the chosen asset alone:\n{refreshed}"
        );
    }
    child.stdin.take().unwrap().write_all(b"q").unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("guided refresh did not terminate");
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    let log = fake_log(&h);
    let refresh = log
        .lines()
        .find(|line| line.starts_with("contract invoke ") && line.contains(" -- refresh "))
        .expect("a guided refresh must reach the native no-send seam");
    assert!(
        !refresh.contains(&blnd),
        "BLND is a token but is not registered"
    );
    assert!(
        registered
            .iter()
            .any(|asset| refresh.contains(asset["Stellar"].as_str().unwrap())),
        "refresh must use an actually registered manifest asset"
    );
    assert!(log
        .lines()
        .filter(|line| line.starts_with("contract invoke "))
        .all(|line| line.contains("--send no")));
    assert!(!fake_requested_a_send(&h));
    assert!(!h.run.join("market-pending").exists());
    assert_eq!(fs::read_to_string(&h.manifest).unwrap(), manifest);
}

/// Named address completion is exercised only through the shipped explorer
/// and its existing fake Stellar CLI; each invocation is a no-send simulation.
#[test]
fn named_address_autocomplete_resolves_generic_and_guided_fields_across_restart() {
    let h = harness("named-address-autocomplete");
    let manifest_before = fs::read(&h.manifest).expect("fixture manifest bytes");
    let sam = account_id("explorer-cli-sam");
    let saved_address = account_id("explorer-cli-retry-address");
    assert!(
        !String::from_utf8_lossy(&manifest_before).contains(&saved_address),
        "the fresh saved address must not already appear in the frozen manifest"
    );
    let pool = contract_id("explorer-cli-pool");

    let mut spec: Vec<Value> = serde_json::from_str(SPEC_LIVE).expect("live ABI fixture");
    spec.extend([
        serde_json::json!({"function_v0": {
            "doc": "", "name": "probe_address",
            "inputs": [{"doc": "", "name": "target", "type_": "address"}],
            "outputs": []
        }}),
        serde_json::json!({"function_v0": {
            "doc": "", "name": "propose_admin",
            "inputs": [{"doc": "", "name": "new_admin", "type_": "address"}],
            "outputs": []
        }}),
    ]);
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(
        &spec_file,
        serde_json::to_vec(&spec).expect("serialize fake ABI"),
    )
    .expect("write fake ABI");
    let view = h.scratch.join("named-address-view.json");
    fs::write(&view, r#""Updated""#).expect("write simulate-only response");

    let launch = |transcript: &std::path::Path| {
        let quote = |path: &std::path::Path| {
            format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
        };
        let command = format!(
            "stty rows 32 cols 110; {} --out {} --stellar-bin {} \
             --rpc-url https://rpc.testnet.stellar.org \
             --passphrase 'Test SDF Network ; September 2015' market tui --mode explore",
            quote(std::path::Path::new(BIN)),
            quote(&h.run),
            quote(&h.fake)
        );
        Command::new("script")
            .args(["-q", "-e", "-f", "-c", &command])
            .arg(transcript)
            .env_remove("STELLAR_RPC_URL")
            .env_remove("STELLAR_NETWORK_PASSPHRASE")
            .env_remove("STELLAR_CONTRACT_ID")
            .env("TERM", "xterm-256color")
            .env("FAKE_STELLAR_LOG", &h.log)
            .env("FAKE_STELLAR_VERSION", V261)
            .env("FAKE_STELLAR_SPEC", &spec_file)
            .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
            .env("FAKE_STELLAR_VIEW", &view)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch explorer under script(1)")
    };
    let transcript_len = |path: &std::path::Path| {
        fs::metadata(path)
            .map(|metadata| metadata.len() as usize)
            .unwrap_or_default()
    };

    let generic_transcript = h.scratch.join("named-address-generic.typescript");
    let mut child = WatchChild(launch(&generic_transcript));
    wait_for_terminal_marker(&mut child, &generic_transcript, "assets", 12);
    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"/probe_address\r")
        .expect("filter generic live function");
    wait_for_terminal_marker_after(&mut child, &generic_transcript, "target", 12, offset);
    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"\r\r")
        .expect("enter arguments and open generic address editor");
    wait_for_terminal_marker_after(
        &mut child,
        &generic_transcript,
        "Address query:",
        12,
        offset,
    );

    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"sam")
        .expect("filter manifest address by name");
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"\t")
        .expect("select Sam autocomplete choice");
    wait_for_terminal_marker_after(
        &mut child,
        &generic_transcript,
        "Full public ID",
        12,
        offset,
    );
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"\r")
        .expect("commit Sam's concrete address");
    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"\x1b[Z")
        .expect("return to generic function selection");
    wait_for_terminal_marker_after(&mut child, &generic_transcript, "probe_address", 12, offset);
    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"r")
        .expect("simulate generic address call");
    wait_for_terminal_marker_after(
        &mut child,
        &generic_transcript,
        "Stage: read-only simulation",
        20,
        offset,
    );
    let generic_log = fake_log(&h);
    let generic_call = generic_log
        .lines()
        .find(|line| line.starts_with("contract invoke ") && line.contains("probe_address"))
        .expect("generic address call must reach the fake CLI");
    assert!(
        generic_call.contains(&sam),
        "generic autocomplete must pass Sam's concrete G-address:\n{generic_call}"
    );
    assert!(
        !generic_call.contains("sam"),
        "generic invocation must not pass the unresolved name:\n{generic_call}"
    );

    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"\x1b[Z")
        .expect("return from detail to generic arguments");
    wait_for_terminal_marker_after(&mut child, &generic_transcript, "probe_address", 12, offset);
    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"\r")
        .expect("reopen generic address editor");
    wait_for_terminal_marker_after(
        &mut child,
        &generic_transcript,
        "Address query:",
        12,
        offset,
    );
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(format!("{saved_address}\r").as_bytes())
        .expect("enter fresh public G-address");
    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"n")
        .expect("save selected address under a name");
    wait_for_terminal_marker_after(
        &mut child,
        &generic_transcript,
        "Save address as:",
        12,
        offset,
    );
    let offset = transcript_len(&generic_transcript);
    child
        .stdin
        .as_mut()
        .expect("generic terminal input")
        .write_all(b"retry-address\r")
        .expect("enter saved-address name containing r and y");
    wait_for_terminal_marker_after(
        &mut child,
        &generic_transcript,
        "Named address saved; available for autocomplete.",
        12,
        offset,
    );
    let saved_record_path = h.run.join("market-addresses/retry-address.json");
    let saved_record: Value = serde_json::from_slice(
        &fs::read(&saved_record_path).expect("saved address must persist in the run"),
    )
    .expect("saved address JSON");
    assert_eq!(
        saved_record,
        serde_json::json!({"name": "retry-address", "address": saved_address}),
        "the saved record must contain the concrete public ID and normalized name"
    );
    child
        .stdin
        .take()
        .expect("generic terminal input")
        .write_all(b"q")
        .expect("exit generic explorer");
    assert!(
        wait_for_guarded_call_exit(&h, &mut child, 8).success(),
        "generic explorer must exit cleanly"
    );

    let guided_transcript = h.scratch.join("named-address-guided.typescript");
    let mut child = WatchChild(launch(&guided_transcript));
    wait_for_terminal_marker(&mut child, &guided_transcript, "assets", 12);
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"a")
        .expect("open Actions pane");
    wait_for_terminal_marker_after(
        &mut child,
        &guided_transcript,
        "recipe help and selectors",
        12,
        offset,
    );
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"/")
        .expect("open recipe filter");
    wait_for_terminal_marker_after(&mut child, &guided_transcript, "Filter:", 12, offset);
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"pool/propose-admin")
        .expect("type guided recipe filter");
    wait_for_terminal_marker_after(
        &mut child,
        &guided_transcript,
        "Filter: pool/propose-admin",
        12,
        offset,
    );
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"\r")
        .expect("apply guided recipe filter");
    wait_for_terminal_marker_after(
        &mut child,
        &guided_transcript,
        "Method: propose_admin",
        12,
        offset,
    );
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"\r")
        .expect("select guided recipe and load its form");
    wait_for_terminal_marker_after(
        &mut child,
        &guided_transcript,
        "Typed guided form ready; r simulates one call, p prepares one call.",
        12,
        offset,
    );
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"\r")
        .expect("open guided new_admin address editor");
    wait_for_terminal_marker_after(&mut child, &guided_transcript, "Address query:", 12, offset);
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"retry")
        .expect("autocomplete saved name in guided field");
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"\t")
        .expect("select saved autocomplete choice");
    wait_for_terminal_marker_after(&mut child, &guided_transcript, "Full public ID", 12, offset);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"\r")
        .expect("commit saved address in guided field");
    let offset = transcript_len(&guided_transcript);
    child
        .stdin
        .as_mut()
        .expect("guided terminal input")
        .write_all(b"r")
        .expect("simulate guided address call");
    wait_for_terminal_marker_after(
        &mut child,
        &guided_transcript,
        "Stage: read-only simulation",
        20,
        offset,
    );
    let log = fake_log(&h);
    let guided_call = log
        .lines()
        .find(|line| line.starts_with("contract invoke ") && line.contains("propose_admin"))
        .expect("guided address call must reach the fake CLI");
    assert!(
        guided_call.contains(&pool) && guided_call.contains(&saved_address),
        "guided autocomplete must reload and invoke the saved concrete G-address:\n{guided_call}"
    );
    assert!(
        !guided_call.contains("retry"),
        "guided invocation must not pass the unresolved saved name:\n{guided_call}"
    );
    child
        .stdin
        .take()
        .expect("guided terminal input")
        .write_all(b"q")
        .expect("exit guided explorer");
    assert!(
        wait_for_guarded_call_exit(&h, &mut child, 8).success(),
        "guided explorer must exit cleanly"
    );

    assert_reads_are_send_free(&h);
    assert!(
        !fake_requested_a_send(&h),
        "no chain send may reach the fake"
    );
    assert!(
        !h.run.join("market-pending").exists(),
        "no pending intent may be created by read-only simulations"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("frozen manifest bytes"),
        manifest_before,
        "saved named addresses may not alter the completed manifest"
    );
}

#[test]
fn guided_pool_submit_request_type_choices_export_integer_payloads_in_a_pty() {
    let h = harness("guided-pool-submit-array-enter");
    let manifest_before = fs::read(&h.manifest).expect("frozen manifest bytes");
    let admin = account_id("explorer-cli-admin");
    let frodo = account_id("explorer-cli-frodo");
    let sam = account_id("explorer-cli-sam");
    let pool = contract_id("explorer-cli-pool");
    let stable = contract_id("explorer-cli-tok-stable");

    let mut spec: Vec<Value> = serde_json::from_str(SPEC_LIVE).expect("live ABI fixture");
    spec.extend([
        serde_json::json!({"function_v0": {
            "doc": "",
            "name": "submit",
            "inputs": [
                {"doc": "", "name": "from", "type_": "address"},
                {"doc": "", "name": "spender", "type_": "address"},
                {"doc": "", "name": "to", "type_": "address"},
                {"doc": "", "name": "requests", "type_": {
                    "vec": {"element_type": {"udt": {"name": "Request"}}}
                }}
            ],
            "outputs": []
        }}),
        serde_json::json!({"udt_struct_v0": {
            "doc": "",
            "lib": "",
            "name": "Request",
            "fields": [
                {"doc": "", "name": "request_type", "type_": "u32"},
                {"doc": "", "name": "address", "type_": "address"},
                {"doc": "", "name": "amount", "type_": "i128"}
            ]
        }}),
    ]);
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(
        &spec_file,
        serde_json::to_vec(&spec).expect("serialize live ABI"),
    )
    .expect("write live ABI");
    let view = h.scratch.join("pool-submit-view.json");
    fs::write(&view, "null").expect("write no-send simulation response");

    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let command = format!(
        "stty rows 32 cols 110; {} --out {} --stellar-bin {} \
         --rpc-url https://rpc.testnet.stellar.org \
         --passphrase 'Test SDF Network ; September 2015' market tui --mode explore",
        quote(std::path::Path::new(BIN)),
        quote(&h.run),
        quote(&h.fake)
    );
    let transcript = h.scratch.join("guided-pool-submit-array.typescript");
    let mut child = WatchChild(
        Command::new("script")
            .args(["-q", "-e", "-f", "-c", &command])
            .arg(&transcript)
            .env_remove("STELLAR_RPC_URL")
            .env_remove("STELLAR_NETWORK_PASSPHRASE")
            .env_remove("STELLAR_CONTRACT_ID")
            .env("TERM", "xterm-256color")
            .env("FAKE_STELLAR_LOG", &h.log)
            .env("FAKE_STELLAR_VERSION", V261)
            .env("FAKE_STELLAR_SPEC", &spec_file)
            .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
            .env("FAKE_STELLAR_VIEW", &view)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch guided pool submit under script(1)"),
    );
    let transcript_len = || {
        fs::metadata(&transcript)
            .map(|metadata| metadata.len() as usize)
            .unwrap_or_default()
    };
    let send_keys = |child: &mut std::process::Child, keys: &[u8]| {
        child
            .stdin
            .as_mut()
            .expect("guided terminal input")
            .write_all(keys)
            .expect("send guided terminal keys");
    };
    let wait_after = |child: &mut std::process::Child, marker: &str, offset: usize| {
        wait_for_terminal_marker_after(child, &transcript, marker, 12, offset);
    };

    wait_for_terminal_marker(&mut child, &transcript, "assets", 12);
    let offset = transcript_len();
    send_keys(&mut child, b"a");
    wait_after(&mut child, "recipe help and selectors", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"/");
    wait_after(&mut child, "Filter:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"pool/submit");
    wait_after(&mut child, "Filter: pool/submit", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Method: submit", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(
        &mut child,
        "Typed guided form ready; r simulates one call, p prepares one call.",
        offset,
    );

    // Keep the existing admin `from` value; choose the other caller fields
    // and request address by their built-in manifest names.
    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[B");
    wait_after(&mut child, "spender", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Address query:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"frodo\t");
    wait_after(&mut child, "Selected name: frodo", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "spender = frodo", offset);

    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[B");
    wait_after(&mut child, "to = ", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Address query:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"sam\t");
    wait_after(&mut child, "Selected name: sam", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "to = sam", offset);

    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[B");
    wait_after(&mut child, "requests = 0 rows", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "requests[0].request_type", offset);
    let screen =
        terminal_fixture_screen::<110, 32>(&fs::read(&transcript).expect("read first request row"));
    assert!(
        screen.contains("requests = 1 rows") && screen.contains("> requests[0].request_type"),
        "Enter on the empty requests header must create and focus the first typed child:\n{screen}"
    );
    assert!(
        !screen.contains("requests= [Enter save / Esc cancel]"),
        "Enter on a vector header must not open an aggregate text editor:\n{screen}"
    );

    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Request type query:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"Borrow");
    wait_after(&mut child, "Request type query: Borrow", offset);
    let screen = terminal_fixture_screen::<110, 32>(
        &fs::read(&transcript).expect("read request type choices"),
    );
    assert!(
        screen.contains("REQUEST TYPE AUTOCOMPLETE") && screen.contains("4 Borrow"),
        "searching request types must keep the numbered Borrow choice visible:\n{screen}"
    );
    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[B");
    wait_after(&mut child, "> 4 Borrow", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "requests[0].request_type = 4", offset);

    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[B");
    wait_after(&mut child, "requests[0].address", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Address query:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"token.stable\t");
    wait_after(&mut child, "Full public ID", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "requests[0].address = token.stable", offset);

    let offset = transcript_len();
    send_keys(&mut child, b"\x1b[B");
    wait_after(&mut child, "requests[0].amount", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(
        &mut child,
        "requests[0].amount= [Enter save / Esc cancel]",
        offset,
    );
    let offset = transcript_len();
    send_keys(&mut child, b"1234567\r");
    wait_after(&mut child, "requests[0].amount = 1234567", offset);

    // Re-enter the populated header: it must focus its existing first child,
    // not append an uninitialized duplicate request.
    for marker in [
        "requests[0].address = token.stable",
        "requests[0].request_type = 4",
        "requests = 1 rows",
    ] {
        let offset = transcript_len();
        send_keys(&mut child, b"\x1b[A");
        wait_after(&mut child, marker, offset);
    }
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "requests[0].request_type", offset);
    let screen = terminal_fixture_screen::<110, 32>(
        &fs::read(&transcript).expect("read re-entered request header"),
    );
    assert!(
        screen.contains("requests = 1 rows")
            && screen.contains("requests[0].request_type = 4")
            && screen.matches("requests[0].request_type").count() >= 2,
        "re-entering requests must keep one row and focus its existing Borrow child:\n{screen}"
    );
    assert!(
        !screen.contains("requests = 2 rows"),
        "re-entering a populated header must not append a row:\n{screen}"
    );

    // Esc discards an in-progress numeric draft without replacing Borrow.
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Request type query:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"5");
    wait_after(&mut child, "Request type query: 5", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\x1b");
    wait_after(&mut child, "requests[0].request_type = 4", offset);
    let screen = terminal_fixture_screen::<110, 32>(
        &fs::read(&transcript).expect("read canceled request type"),
    );
    assert!(
        screen.contains("requests[0].request_type = 4"),
        "canceling a request-type draft must preserve the committed Borrow value:\n{screen}"
    );

    let offset = transcript_len();
    send_keys(&mut child, b"r");
    wait_after(&mut child, "Stage: read-only simulation", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\x1b");
    wait_after(&mut child, "requests[0].request_type = 4", offset);
    let screen = terminal_fixture_screen::<110, 32>(
        &fs::read(&transcript).expect("read form after Borrow simulation"),
    );
    assert!(
        screen.contains("requests = 1 rows")
            && screen.contains("requests[0].request_type = 4")
            && !screen.contains("requests = 2 rows"),
        "returning from simulation must retain the single Borrow request:\n{screen}"
    );

    // A numeric value commits directly, without selecting from the menu.
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Request type query:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"5");
    wait_after(&mut child, "Request type query: 5", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "requests[0].request_type = 5", offset);
    let screen = terminal_fixture_screen::<110, 32>(
        &fs::read(&transcript).expect("read form after numeric request type"),
    );
    assert!(
        screen.contains("requests = 1 rows")
            && screen.contains("requests[0].request_type = 5")
            && !screen.contains("requests = 2 rows"),
        "direct numeric entry must update the existing request only:\n{screen}"
    );

    let offset = transcript_len();
    send_keys(&mut child, b"r");
    wait_after(&mut child, "Stage: read-only simulation", offset);
    send_keys(&mut child, b"q");
    assert!(
        wait_for_guarded_call_exit(&h, &mut child, 8).success(),
        "guided pool submit explorer must exit cleanly"
    );

    let log = fake_log(&h);
    let invocations: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("contract invoke "))
        .collect();
    assert_eq!(
        invocations.len(),
        2,
        "the guided pool submit must simulate once for Borrow and once for numeric 5:\n{log}"
    );
    for (&invocation, request_type) in invocations.iter().zip([4, 5]) {
        assert!(
            invocation.starts_with(&format!(
                "contract invoke --id {pool} --source {admin} --send no"
            )),
            "every pool submit must target the manifest pool and use the admin no-send source:\n{invocation}"
        );
        let (_, native_args) = invocation
            .split_once(" -- submit ")
            .expect("the native invoke must carry the submit method");
        let expected_request = serde_json::json!([{
            "request_type": request_type,
            "address": stable,
            "amount": "1234567"
        }]);
        let expected_args =
            format!("--from {admin} --spender {frodo} --to {sam} --requests {expected_request}");
        assert_eq!(
            native_args, expected_args,
            "the native submit argv must carry exact bound callers and one Request"
        );
        let (_, request_json) = native_args
            .split_once("--requests ")
            .expect("submit argv must contain the requests input");
        let requests: Value =
            serde_json::from_str(request_json).expect("native request argument must be JSON");
        assert_eq!(
            requests, expected_request,
            "the request type must remain an integer and the i128 amount its exact string"
        );
    }
    assert_reads_are_send_free(&h);
    assert!(
        !fake_requested_a_send(&h),
        "pool submit simulation must never request a chain send"
    );
    assert!(
        !h.run.join("market-pending").exists(),
        "simulation must not create the market pending journal"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("frozen manifest bytes"),
        manifest_before,
        "the guided simulation must leave the completed manifest byte-identical"
    );
}

/// A complete, loadable market configuration for the mutation-boundary
/// regressions: one dummy artifact file per required role (file existence
/// is what the loader checks; byte verification belongs to the deployer
/// preflight, which these fakes must never pass).
fn mutation_market_config(scratch: &std::path::Path) -> PathBuf {
    let mut text = String::from(
        "[network]\n\
         passphrase = \"Test SDF Network ; September 2015\"\n\
         label = \"testnet\"\n\n",
    );
    for role in [
        "pool",
        "backstop",
        "factory",
        "token",
        "source",
        "comet",
        "emitter",
        "proxy-runtime",
        "proxy-governance",
        "adapter",
    ] {
        let file = scratch.join(format!("{role}.wasm"));
        fs::write(&file, b"wasm").expect("write artifact fixture");
        text.push_str(&format!(
            "[artifacts.{role}]\n\
             path = \"{}\"\n\
             sha256 = \"{}\"\n\n",
            file.display(),
            "0".repeat(64)
        ));
    }
    text.push_str(
        "[oracle]\n\
         sources = 3\n\
         min_sources = 3\n\
         max_age_secs = 3600\n\
         max_clock_drift_secs = 60\n\
         base = \"USD\"\n\
         decimals = 7\n\
         resolution = 300\n\n\
         [adapter]\n\
         decimals = 7\n\
         resolution = 1\n\
         base = \"USD\"\n\n\
         [reserves.stable]\n\
         c_factor = 9000000\nl_factor = 9500000\nutil = 8500000\nmax_util = 9500000\n\
         supply_cap = \"1000000000000000000\"\n\
         r_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
         [reserves.xlm]\n\
         c_factor = 7500000\nl_factor = 7500000\nutil = 5000000\nmax_util = 9500000\n\
         supply_cap = \"1000000000000000000\"\n\
         r_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
         [reserves.weth]\n\
         c_factor = 8000000\nl_factor = 8000000\nutil = 7000000\nmax_util = 9500000\n\
         supply_cap = \"170141183460469231731687303715884105727\"\n\
         r_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
         [[balances]]\n\
         who = \"frodo\"\n\
         token = \"stable\"\n\
         amount = 100000000000\n",
    );
    let file = scratch.join("mutation-market.toml");
    fs::write(&file, text).expect("write mutation market config");
    file
}

/// Every child the fake was asked to execute must have run from the
/// controlled root with the run-confined signing configuration, and every
/// chain-mutating argv the fake ever saw must originate from a pinned
/// G-address, sign by naming only a key alias, and carry the explicit
/// `--config-dir` — never a raw seed or muxed account value.
fn assert_mutation_boundary_held(h: &Harness, cwd_log: &std::path::Path, run: &std::path::Path) {
    if let Ok(report) = fs::read_to_string(cwd_log) {
        for line in report.lines() {
            let (cwd, xdg) = line.split_once('|').expect("recorded cwd and config");
            assert_eq!(
                cwd, "/",
                "a market child may not execute outside the controlled root: {line}"
            );
            assert_eq!(
                xdg,
                run.join(".xdg").to_str().expect("UTF-8 test path"),
                "XDG configuration must stay confined to this run"
            );
        }
    }
    let log = fake_log(h);
    for line in log.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for word in &words {
            assert!(
                !(word.starts_with('S') && word.len() == 56
                    || word.starts_with('M') && word.len() == 56),
                "a raw seed or muxed account entered a market child argv: {line}"
            );
        }
        let mutating = ["contract invoke", "contract deploy", "contract upload"]
            .iter()
            .any(|verb| line.starts_with(verb));
        if !mutating {
            continue;
        }
        let pinned = words
            .windows(2)
            .any(|w| w[0] == "--source" && w[1].starts_with('G'))
            || words
                .windows(2)
                .any(|w| w[0] == "--source-account" && w[1].starts_with('G'));
        let alias = words
            .windows(2)
            .any(|w| w[0] == "--sign-with-key" && !w[1].is_empty())
            || words
                .windows(2)
                .any(|w| w[0] == "--signer" && !w[1].is_empty());
        let config = words.iter().any(|w| *w == "--config-dir");
        assert!(
            pinned && alias && config,
            "a market mutation must originate from a pinned G-address, sign by \
             alias only, and carry an explicit --config-dir: {line}"
        );
    }
}

/// SEC001/SEC003 behavior: a fresh `market deploy` attempt against the
/// fake CLI must reach its execution seam (proving the mutation path
/// really tries to run), must fail closed, and may only ever execute
/// children from the controlled root. Every mutation argv the fake sees
/// carries the pinned G source, the alias-only signing flag, and the
/// explicit signing `--config-dir`; no child may read the deployment run
/// directory.
#[test]
fn market_deploy_children_run_only_from_the_controlled_root_with_pinned_signing() {
    let h = harness("deploy-controlled-root");
    let market = mutation_market_config(&h.scratch);
    let admin_config = h.scratch.join("admin-config");
    fs::create_dir_all(&admin_config).expect("admin identity directory");
    let cwd_log = h.scratch.join("deploy-cwd.log");
    let out = Command::new(BIN)
        .arg("--out")
        .arg(h.scratch.join("deploy-run"))
        .arg("--stellar-bin")
        .arg(&h.fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .arg("--passphrase")
        .arg(TESTNET_PASSPHRASE)
        .args(["market", "deploy"])
        .args(["--market", market.to_str().expect("UTF-8 test path")])
        .args(["--admin-identity", "admin"])
        .args([
            "--admin-config-dir",
            admin_config.to_str().expect("UTF-8 test path"),
        ])
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env("FAKE_STELLAR_CWD_LOG", &cwd_log)
        .stdin(Stdio::null())
        .output()
        .expect("drive a fresh market deploy through the fake CLI");
    assert!(
        !out.status.success(),
        "a fake-refused deployment must fail closed: {}",
        output_text(&out)
    );
    let report = fs::read_to_string(&cwd_log).expect("the deploy path must reach real execution");
    assert!(
        !report.is_empty(),
        "the deploy attempt must really spawn children"
    );
    assert_mutation_boundary_held(&h, &cwd_log, &h.scratch.join("deploy-run"));
    assert!(
        !h.run.join("market-manifest.toml.clone").exists(),
        "the completed manifest fixture must stay untouched by a fresh deploy"
    );
}

/// SEC001 behavior: post-deploy play commands against the completed-run
/// fixture must run every child from the controlled root with confined
/// signing configuration, must reach the read-only identity probe, and
/// may never reach a chain mutation or reserve custody through this fake.
#[test]
fn market_play_children_run_only_from_the_controlled_root() {
    let h = harness("play-controlled-root");
    let market = mutation_market_config(&h.scratch);
    let admin_config = h.scratch.join("admin-config");
    fs::create_dir_all(&admin_config).expect("admin identity directory");
    let cwd_log = h.scratch.join("play-cwd.log");
    let out = Command::new(BIN)
        .arg("--out")
        .arg(&h.run)
        .arg("--stellar-bin")
        .arg(&h.fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .arg("--passphrase")
        .arg(TESTNET_PASSPHRASE)
        .args(["market", "play"])
        .args(["--market", market.to_str().expect("UTF-8 test path")])
        .args(["--admin-identity", "admin"])
        .args([
            "--admin-config-dir",
            admin_config.to_str().expect("UTF-8 test path"),
        ])
        .args(["--step", "0"])
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env("FAKE_STELLAR_CWD_LOG", &cwd_log)
        .stdin(Stdio::null())
        .output()
        .expect("drive a market play step through the fake CLI");
    assert!(
        !out.status.success(),
        "a fake-refused play step must fail closed: {}",
        output_text(&out)
    );
    let text = output_text(&out);
    assert!(
        !text.contains(&h.scratch.display().to_string()),
        "a play refusal must not echo scratch paths: {text}"
    );
    assert!(
        !h.run.join("market-pending").exists(),
        "a play refusal may not reserve custody"
    );
    assert_mutation_boundary_held(&h, &cwd_log, &h.run);
    assert_chain_untouched(&h);
}

/// SEC001 fail-closed boundary: a `.env` planted inside the run directory
/// (where the CLI would auto-load it and could hijack the bound contract
/// identity) must refuse deployment and play before any child executes,
/// with input-free text that leaks no path bytes.
#[test]
fn planted_run_directory_env_refuses_mutation_commands_before_any_child() {
    let h = harness("planted-env-refusal");
    let market = mutation_market_config(&h.scratch);
    let admin_config = h.scratch.join("admin-config");
    fs::create_dir_all(&admin_config).expect("admin identity directory");
    fs::write(h.run.join(".env"), "STELLAR_CONTRACT_ID=PLANTED-SENTINEL\n").expect("plant env");
    let cwd_log = h.scratch.join("planted-cwd.log");
    let refused = Command::new(BIN)
        .arg("--out")
        .arg(&h.run)
        .arg("--stellar-bin")
        .arg(&h.fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .arg("--passphrase")
        .arg(TESTNET_PASSPHRASE)
        .args(["market", "play"])
        .args(["--market", market.to_str().expect("UTF-8 test path")])
        .args(["--admin-identity", "admin"])
        .args([
            "--admin-config-dir",
            admin_config.to_str().expect("UTF-8 test path"),
        ])
        .args(["--step", "0"])
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env("FAKE_STELLAR_CWD_LOG", &cwd_log)
        .stdin(Stdio::null())
        .output()
        .expect("drive a play step against a planted run-directory .env");
    assert!(
        !refused.status.success(),
        "a planted run-directory .env must refuse the mutation command"
    );
    let text = output_text(&refused);
    assert!(
        !text.contains("PLANTED-SENTINEL"),
        "the refusal must not echo planted bytes: {text}"
    );
    assert!(
        !text.contains(&h.scratch.display().to_string()),
        "the refusal must not echo scratch paths: {text}"
    );
    assert!(
        !cwd_log.exists()
            || fs::read_to_string(&cwd_log)
                .map(|r| r.is_empty())
                .unwrap_or(true),
        "a refused mutation command may not execute any child: {}",
        fs::read_to_string(&cwd_log).unwrap_or_default()
    );
    assert!(
        fake_log(&h).is_empty(),
        "a refused command may not reach the CLI at all"
    );
    assert!(
        !h.run.join("market-pending").exists(),
        "refusal may not reserve custody"
    );
}

/// SEC001 executable boundary: a `--stellar-bin` path that is resolvable
/// only relative to the run directory (never relative to the invocation
/// context) must be refused before any child runs, with fixed input-free
/// text. Pair behavior is untouched; this is the market boundary only.
#[test]
fn stellar_bin_resolvable_only_from_the_run_directory_refuses_before_any_child() {
    let h = harness("relative-bin-refusal");
    let market = mutation_market_config(&h.scratch);
    let admin_config = h.scratch.join("admin-config");
    fs::create_dir_all(&admin_config).expect("admin identity directory");
    let planted_dir = h.run.join("planted-bin");
    fs::create_dir_all(&planted_dir).expect("planted bin directory");
    let planted = planted_dir.join("stellar");
    fs::write(&planted, FAKE_TEMPLATE).expect("write planted fake cli");
    fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).expect("chmod planted fake");
    let cwd_log = h.scratch.join("relative-cwd.log");
    let out = Command::new(BIN)
        .arg("--out")
        .arg(&h.run)
        .arg("--stellar-bin")
        .arg("planted-bin/stellar")
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .arg("--passphrase")
        .arg(TESTNET_PASSPHRASE)
        .args(["market", "play"])
        .args(["--market", market.to_str().expect("UTF-8 test path")])
        .args(["--admin-identity", "admin"])
        .args([
            "--admin-config-dir",
            admin_config.to_str().expect("UTF-8 test path"),
        ])
        .args(["--step", "0"])
        .current_dir(&h.run)
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env("FAKE_STELLAR_CWD_LOG", &cwd_log)
        .stdin(Stdio::null())
        .output()
        .expect("drive a market command with a run-relative binary");
    assert!(
        !out.status.success(),
        "a market command may not execute a binary resolvable only from the run directory"
    );
    let text = output_text(&out);
    assert!(
        !text.contains("PLANTED") && !text.contains(&h.run.display().to_string()),
        "the executable-refusal must be input-free: {text}"
    );
    assert!(
        fake_log(&h).is_empty(),
        "the planted executable may never run"
    );
    assert!(
        !cwd_log.exists(),
        "no child may run for an unsafe executable"
    );
}
/// Builds one guarded explorer send under `script(1)`: a completed-run
/// fixture with an empty (clear) journal root, live ABI, valid signer
/// lookup, and a void simulation answer, so preparation genuinely reaches
/// the confirmation gate while the fake refuses any actual chain mutation.
fn guarded_call_command(h: &Harness) -> String {
    fs::create_dir(h.run.join("market-pending")).expect("clear journal root");
    let identities = h.scratch.join("identities");
    fs::create_dir(&identities).expect("signer fixture directory");
    fs::write(
        h.scratch.join("fake.signer-public-key"),
        format!("{}\n", account_id("explorer-cli-confirm-signer")),
    )
    .expect("write signer lookup fixture");
    fs::write(h.scratch.join("fake.simulate-void"), "").expect("write void simulation fixture");
    let shell_path = |path: &std::path::Path| {
        let text = path.to_string_lossy();
        assert!(!text.contains('\'') && !text.contains('\n'));
        format!("'{text}'")
    };
    format!(
        "{} --out {} --stellar-bin {} --rpc-url https://rpc.testnet.stellar.org --passphrase 'Test SDF Network ; September 2015' market call source.0 set_data --signer admin --signer-config-dir {} --execute --arg asset='{{\"Other\":[\"BLND\"]}}' --arg price=10000000 --arg timestamp=1758000000",
        shell_path(std::path::Path::new(BIN)),
        shell_path(&h.run),
        shell_path(&h.fake),
        shell_path(&identities),
    )
}

fn spawn_guarded_call_under_pty(h: &Harness, transcript: &std::path::Path) -> std::process::Child {
    let spec = h.scratch.join("fake.spec.json");
    fs::write(&spec, SPEC_LIVE).expect("live ABI fixture");
    Command::new("script")
        .args(["-q", "-e", "-f", "-c"])
        .arg(guarded_call_command(h))
        .arg(transcript)
        .env("TERM", "xterm-256color")
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_SPEC", &spec)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env(
            "FAKE_STELLAR_PUBLIC_KEY",
            h.scratch.join("fake.signer-public-key"),
        )
        .env("FAKE_STELLAR_VIEW", h.scratch.join("fake.simulate-void"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch guarded call under script(1)")
}

fn transcript_contains(transcript: &std::path::Path, needle: &str) -> bool {
    String::from_utf8_lossy(&fs::read(transcript).unwrap_or_default()).contains(needle)
}

fn wait_for_terminal_marker(
    child: &mut std::process::Child,
    transcript: &std::path::Path,
    marker: &str,
    seconds: u64,
) {
    wait_for_terminal_marker_after(child, transcript, marker, seconds, 0);
}

// Replay cursor/erase operations, including DEC 1049's alternate-screen
// clear; stripping CSI loses characters Ratatui leaves unchanged.
fn terminal_fixture_screen<const WIDTH: usize, const HEIGHT: usize>(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let mut cells = [[' '; WIDTH]; HEIGHT];
    let (mut row, mut column) = (0usize, 0usize);
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.next() != Some('[') {
                continue;
            }
            let mut parameters = String::new();
            for command in chars.by_ref() {
                if !('@'..='~').contains(&command) {
                    parameters.push(command);
                    continue;
                }
                let values = parameters
                    .split(';')
                    .map(|value| value.parse::<usize>().unwrap_or(0))
                    .collect::<Vec<_>>();
                let n = values[0].max(1);
                match command {
                    'h' if parameters == "?1049" => {
                        cells.fill([' '; WIDTH]);
                        row = 0;
                        column = 0;
                    }
                    'H' | 'f' => {
                        row = (n - 1).min(HEIGHT - 1);
                        column = (values.get(1).copied().unwrap_or(1).max(1) - 1).min(WIDTH - 1);
                    }
                    'A' => row = row.saturating_sub(n),
                    'B' => row = row.saturating_add(n).min(HEIGHT - 1),
                    'C' => column = column.saturating_add(n).min(WIDTH - 1),
                    'D' => column = column.saturating_sub(n),
                    'G' => column = (n - 1).min(WIDTH - 1),
                    'd' => row = (n - 1).min(HEIGHT - 1),
                    'J' if matches!(values[0], 2 | 3) => cells.fill([' '; WIDTH]),
                    'J' if values[0] == 0 => {
                        cells[row][column.min(WIDTH)..].fill(' ');
                        cells[row + 1..].fill([' '; WIDTH]);
                    }
                    'K' => match values[0] {
                        0 => cells[row][column.min(WIDTH)..].fill(' '),
                        1 => cells[row][..column.saturating_add(1).min(WIDTH)].fill(' '),
                        2 => cells[row].fill(' '),
                        _ => {}
                    },
                    _ => {}
                }
                break;
            }
        } else {
            match ch {
                '\r' => column = 0,
                '\n' => row = (row + 1).min(HEIGHT - 1),
                ch if !ch.is_control() => {
                    if column < WIDTH {
                        cells[row][column] = ch;
                    }
                    column += 1;
                }
                _ => {}
            }
        }
    }
    cells
        .iter()
        .flat_map(|line| line.iter().copied().chain(std::iter::once('\n')))
        .collect()
}

fn wait_for_watch_screen(
    child: &mut std::process::Child,
    transcript: &std::path::Path,
    expected: &str,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let until = Instant::now() + Duration::from_secs(10);
    let mut screen = String::new();
    while Instant::now() < until {
        screen = terminal_fixture_screen::<80, 24>(&fs::read(transcript).unwrap_or_default());
        if predicate(&screen) {
            return screen;
        }
        if child.try_wait().expect("poll watch terminal").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(40));
    }
    panic!("watch terminal did not show {expected}:\n{screen}");
}

fn wait_for_terminal_marker_after(
    child: &mut std::process::Child,
    transcript: &std::path::Path,
    marker: &str,
    seconds: u64,
    offset: usize,
) {
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        let bytes = fs::read(transcript).unwrap_or_default();
        let projected = terminal_fixture_screen::<110, 32>(&bytes);
        if bytes.len() > offset
            && projected
                .split_whitespace()
                .collect::<String>()
                .contains(&marker.split_whitespace().collect::<String>())
        {
            return;
        }
        if child.try_wait().expect("poll terminal").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("terminal did not reach {marker}");
}

fn fake_requested_a_send(h: &Harness) -> bool {
    let log = fake_log(h);
    log.contains("FAKE-REFUSED-MUTATION") || log.contains("--send yes")
}

fn wait_for_guarded_call_exit(
    h: &Harness,
    child: &mut std::process::Child,
    seconds: u64,
) -> std::process::ExitStatus {
    let until = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait().expect("poll guarded call") {
            return status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("guarded call did not exit; fake log:\n{}", fake_log(h));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// A `y` buffered during live ABI/simulation must never authorize the send
/// gate. Under a real terminal the shipped binary clears queued input before
/// the preview appears, so the early `y` is discarded, the gate stays open
/// and waiting, the fresh refusal is honored, and zero sends or custody
/// reservations reach the fake.
#[test]
fn early_buffered_y_cannot_authorize_a_market_send_in_a_pty() {
    let h = harness("stale-y-send");
    let manifest_before = fs::read(&h.manifest).expect("frozen manifest bytes");
    let transcript = h.scratch.join("stale-y.typescript");
    let mut child = spawn_guarded_call_under_pty(&h, &transcript);
    child
        .stdin
        .as_mut()
        .expect("terminal input")
        .write_all(b"y\n")
        .expect("queue consent during preparation");
    child
        .stdin
        .as_mut()
        .expect("terminal input")
        .flush()
        .expect("flush queued consent");
    let until = Instant::now() + Duration::from_secs(60);
    while !transcript_contains(&transcript, "type exactly y then Enter") {
        if Instant::now() >= until || child.try_wait().expect("poll guarded call").is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "early input bypassed the cleared send gate; fake log:\n{}",
                fake_log(&h)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let watch = Instant::now() + Duration::from_secs(5);
    while Instant::now() < watch {
        if child.try_wait().expect("poll guarded call").is_some() {
            panic!(
                "the send gate closed without fresh input; fake log:\n{}",
                fake_log(&h)
            );
        }
        if fake_requested_a_send(&h)
            || fs::read_dir(h.run.join("market-pending"))
                .expect("journal root")
                .next()
                .is_some()
        {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "early buffered y authorized a market send; fake log:\n{}",
                fake_log(&h)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    child
        .stdin
        .as_mut()
        .expect("terminal input")
        .write_all(b"n\n")
        .expect("send fresh refusal");
    let status = wait_for_guarded_call_exit(&h, &mut child, 30);
    assert!(
        !status.success() && transcript_contains(&transcript, "market action not sent"),
        "the fresh refusal must be honored without a send"
    );
    assert!(!fake_requested_a_send(&h), "zero sends may reach the fake");
    assert!(
        fs::read_dir(h.run.join("market-pending"))
            .expect("journal root")
            .next()
            .is_none(),
        "a refused action may never reserve custody"
    );
    assert_reads_are_send_free(&h);
    assert_eq!(
        fs::read(&h.manifest).expect("frozen manifest bytes"),
        manifest_before,
        "a refused action may never touch the frozen manifest"
    );
}

/// Consent matters only after the exact preview is on screen: once the gate
/// is presented, a fresh `y` proceeds to the one bounded send attempt bound
/// to the frozen signer. The fake refuses that attempt (exit 71, no chain
/// mutation) and logs the argv, so the advance is provable with no network
/// send and the frozen manifest stays byte-identical.
#[test]
fn fresh_confirmation_after_the_preview_advances_the_guarded_send_path() {
    let h = harness("fresh-y-send");
    let manifest_before = fs::read(&h.manifest).expect("frozen manifest bytes");
    let transcript = h.scratch.join("fresh-y.typescript");
    let mut child = spawn_guarded_call_under_pty(&h, &transcript);
    let until = Instant::now() + Duration::from_secs(60);
    while !transcript_contains(&transcript, "type exactly y then Enter") {
        if Instant::now() >= until || child.try_wait().expect("poll guarded call").is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the guarded send gate never presented for fresh confirmation; fake log:\n{}",
                fake_log(&h)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    child
        .stdin
        .take()
        .expect("terminal input")
        .write_all(b"y\n")
        .expect("confirm after the preview");
    let status = wait_for_guarded_call_exit(&h, &mut child, 60);
    assert!(
        !status.success(),
        "the fake send refusal must fail the call"
    );
    let log = fake_log(&h);
    assert!(
        log.contains("--send yes") && log.contains("FAKE-REFUSED-MUTATION"),
        "a fresh y after the preview must advance to the single guarded send attempt:\n{log}"
    );
    assert!(
        fs::read_dir(h.run.join("market-pending"))
            .expect("journal root")
            .next()
            .is_some(),
        "the confirmed attempt must be reserved in the journal"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("frozen manifest bytes"),
        manifest_before,
        "the send attempt may never touch the frozen manifest"
    );
}
/// Reproduce the typed guided-integer rejection in a real explorer terminal:
/// an invalid edit must keep its editor open, let the operator correct it
/// without dispatching hotkeys, and let Esc discard a later draft without
/// replacing the last committed value. Every call stays on the fake's
/// read-only simulation path.
#[test]
fn guided_invalid_integer_draft_recovers_and_cancels_in_a_pty() {
    let h = harness("guided-invalid-integer-draft");
    let manifest_before = fs::read(&h.manifest).expect("frozen manifest bytes");
    let admin = account_id("explorer-cli-admin");
    let governance = contract_id("explorer-cli-governance");
    let mut spec: Vec<Value> = serde_json::from_str(SPEC_LIVE).expect("live ABI fixture");
    spec.push(serde_json::json!({"function_v0": {
        "doc": "",
        "name": "execute_proposal",
        "inputs": [
            {"doc": "", "name": "caller", "type_": "address"},
            {"doc": "", "name": "id", "type_": "u32"}
        ],
        "outputs": []
    }}));
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(
        &spec_file,
        serde_json::to_vec(&spec).expect("serialize live ABI"),
    )
    .expect("write live ABI");
    let view = h.scratch.join("execute-proposal-view.json");
    fs::write(&view, "null").expect("write null simulation response");

    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let command = format!(
        "stty rows 18 cols 60; {} --out {} --stellar-bin {} \
         --rpc-url https://rpc.testnet.stellar.org \
         --passphrase 'Test SDF Network ; September 2015' market tui --mode explore",
        quote(std::path::Path::new(BIN)),
        quote(&h.run),
        quote(&h.fake)
    );
    let transcript = h.scratch.join("guided-invalid-integer.typescript");
    let mut child = WatchChild(
        Command::new("script")
            .args(["-q", "-e", "-f", "-c", &command])
            .arg(&transcript)
            .env_remove("STELLAR_RPC_URL")
            .env_remove("STELLAR_NETWORK_PASSPHRASE")
            .env_remove("STELLAR_CONTRACT_ID")
            .env("TERM", "xterm-256color")
            .env("FAKE_STELLAR_LOG", &h.log)
            .env("FAKE_STELLAR_VERSION", V261)
            .env("FAKE_STELLAR_SPEC", &spec_file)
            .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
            .env("FAKE_STELLAR_VIEW", &view)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch guided explorer in an isolated terminal"),
    );
    let transcript_len = |path: &std::path::Path| {
        fs::metadata(path)
            .map(|metadata| metadata.len() as usize)
            .unwrap_or_default()
    };
    let send_keys = |child: &mut std::process::Child, keys: &[u8]| {
        child
            .stdin
            .as_mut()
            .expect("guided terminal input")
            .write_all(keys)
            .expect("send guided terminal keys");
    };
    let wait_for_terminal_marker_after = |child: &mut std::process::Child,
                                          transcript: &std::path::Path,
                                          marker: &str,
                                          seconds: u64,
                                          offset: usize|
     -> String {
        let until = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < until {
            let bytes = fs::read(transcript).unwrap_or_default();
            let projected = terminal_fixture_screen::<60, 18>(&bytes);
            if bytes.len() > offset
                && projected
                    .split_whitespace()
                    .collect::<String>()
                    .contains(&marker.split_whitespace().collect::<String>())
            {
                return projected;
            }
            if child.try_wait().expect("poll guided terminal").is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("terminal did not reach {marker} at 60x18");
    };
    let wait_for_terminal_marker = |child: &mut std::process::Child,
                                    transcript: &std::path::Path,
                                    marker: &str,
                                    seconds: u64| {
        wait_for_terminal_marker_after(child, transcript, marker, seconds, 0);
    };
    let wait_for_form_returned = |child: &mut std::process::Child| {
        let until = Instant::now() + Duration::from_secs(12);
        while Instant::now() < until {
            if let Ok(bytes) = fs::read(&transcript) {
                let screen = terminal_fixture_screen::<60, 18>(&bytes);
                if !screen.contains("[Enter save / Esc cancel]") && screen.contains("id = 9") {
                    return screen;
                }
            }
            if child.try_wait().expect("poll guided terminal").is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("guided terminal did not return to the committed id 9 form");
    };
    let assert_modal_guidance = |screen: &str| {
        let visible = screen
            .split_whitespace()
            .collect::<String>()
            .to_ascii_lowercase();
        assert!(
            visible.contains("r/p/ytypetext")
                && visible.contains("actionshortcutsinactive")
                && visible.contains("entercompletesinput")
                && visible.contains("esccancels")
                && visible.contains("ctrl-cquitssafely"),
            "active input must explain text entry, inactive shortcuts, completion, cancel, and quit:\n{screen}"
        );
        let has_actions_label = screen.split_whitespace().any(|word| {
            word.trim_matches(|character: char| !character.is_ascii_alphanumeric())
                .eq_ignore_ascii_case("actions")
        });
        assert!(
            !has_actions_label
                && !visible.contains("aactions")
                && !visible.contains("pprepare")
                && !visible.contains("yconfirms")
                && !visible.contains("enteropen/edit")
                && !visible.contains("+/-rows")
                && !visible.contains("[]cycleeligibleasset")
                && !visible.contains("typedguidedformready"),
            "active input must not advertise pane/global actions or stale form-ready state:\n{screen}"
        );
    };
    let assert_form_action_guidance = |screen: &str| {
        let visible = screen
            .split_whitespace()
            .collect::<String>()
            .to_ascii_lowercase();
        assert!(
            visible.contains("aactions")
                && !visible.contains("r/p/ytypetext")
                && !visible.contains("actionshortcutsinactive")
                && !visible.contains("entercompletesinput")
                && !visible.contains("ctrl-cquitssafely"),
            "committed form must restore action navigation without editing-only guidance:\n{screen}"
        );
    };

    wait_for_terminal_marker(&mut child, &transcript, "assets", 12);
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"a/governance/execute-proposal\r\r");
    wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "Typed guided form ready",
        20,
        offset,
    );
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"\r");
    wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "id= [Enter save / Esc cancel]",
        12,
        offset,
    );
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"7\r");
    wait_for_terminal_marker_after(&mut child, &transcript, "id = 7", 12, offset);

    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"\r");
    wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "id=7 [Enter save / Esc cancel]",
        12,
        offset,
    );
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"\x7fbad\r");
    let rejected_screen = wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "id: invalid unsigned integer",
        12,
        offset,
    );
    let visible = rejected_screen.split_whitespace().collect::<String>();
    assert!(
        visible.contains("id:invalidunsignedinteger"),
        "the complete rejected-field diagnostic must remain visible at 60x18:\n{rejected_screen}"
    );
    assert!(
        visible.contains("id=bad[Entersave/Esccancel]"),
        "the invalid draft and Enter/Esc recovery guidance must remain visible with its diagnostic:\n{rejected_screen}"
    );
    assert_modal_guidance(&rejected_screen);

    let log_before_hotkeys = fake_log(&h);
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"pry");
    let pry_screen = wait_for_terminal_marker_after(&mut child, &transcript, "badpry", 12, offset);
    assert_modal_guidance(&pry_screen);
    assert_eq!(
        fake_log(&h),
        log_before_hotkeys,
        "p/r/y must remain draft text after validation rejects the integer"
    );

    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"\x7f\x7f\x7f\x7f\x7f\x7f9");
    wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "9 [Enter save / Esc cancel]",
        12,
        offset,
    );
    send_keys(&mut child, b"\r");
    let committed_screen = wait_for_form_returned(&mut child);
    assert_form_action_guidance(&committed_screen);
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"r");
    wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "Stage: read-only simulation",
        20,
        offset,
    );

    send_keys(&mut child, b"\x1b");
    wait_for_form_returned(&mut child);
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"\r");
    wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "id=9 [Enter save / Esc cancel]",
        12,
        offset,
    );
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"\x7fbad\r");
    let canceled_rejected_screen =
        wait_for_terminal_marker_after(&mut child, &transcript, "invalid unsigned", 12, offset);
    assert_modal_guidance(&canceled_rejected_screen);

    let log_before_cancel = fake_log(&h);
    send_keys(&mut child, b"\x1b");
    let canceled_screen = wait_for_form_returned(&mut child);
    assert_form_action_guidance(&canceled_screen);
    assert_eq!(
        fake_log(&h),
        log_before_cancel,
        "Esc must cancel the invalid draft without starting a CLI job"
    );
    let offset = transcript_len(&transcript);
    send_keys(&mut child, b"r");
    wait_for_terminal_marker_after(
        &mut child,
        &transcript,
        "Stage: read-only simulation",
        20,
        offset,
    );

    send_keys(&mut child, b"q");
    assert!(
        wait_for_guarded_call_exit(&h, &mut child, 8).success(),
        "the unsigned explorer must exit cleanly"
    );
    let log = fake_log(&h);
    let calls: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("contract invoke "))
        .collect();
    assert_eq!(
        calls.len(),
        2,
        "only the two explicit simulations may invoke"
    );
    let invocation_prefix = format!("contract invoke --id {governance} --source {admin} --send no");
    let executed_arguments = format!(" -- execute_proposal --caller {admin} --id 9");
    for call in calls {
        assert!(
            call.starts_with(&invocation_prefix) && call.contains(&executed_arguments),
            "simulation must preserve the bound caller and corrected id 9 with --send no:\n{call}"
        );
        assert!(
            !call.contains(&format!(" -- execute_proposal --caller {admin} --id 7")),
            "the stale committed id 7 must never be simulated:\n{call}"
        );
    }
    assert_reads_are_send_free(&h);
    assert!(!fake_requested_a_send(&h), "no send may reach the fake");
    assert!(
        !log.contains("keys public-key"),
        "this unsigned simulation must not resolve a signer"
    );
    assert!(
        !h.run.join("market-pending").exists(),
        "an unsigned simulation must not reserve a pending intent"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("frozen manifest bytes"),
        manifest_before,
        "the guided simulations may not alter the frozen manifest"
    );
}

/// Reopening retains the displayed price without reads; only in-page `s`
/// observes a changed native response. The snapshot remains read-only.
#[test]
fn status_snapshot_reopens_from_cache_and_only_explicit_refresh_rereads_the_market() {
    let h = harness("status-snapshot-cache");
    let manifest_before = fs::read(&h.manifest).expect("fixture manifest bytes");
    let admin = account_id("explorer-cli-admin");
    let view_dir = h.scratch.join("status-views");
    fs::create_dir(&view_dir).expect("status view directory");
    let unsupported = h.scratch.join("status-null.json");
    fs::write(&unsupported, "null").expect("write unsupported-leg answer");
    let write_price = |price: &str| {
        fs::write(
            view_dir.join("lastprice"),
            format!(r#"{{"price":{price},"timestamp":1759000001}}"#),
        )
        .expect("write adapter price");
    };
    const OLD_PRICE: &str = "1111000000000111";
    const NEW_PRICE: &str = "8888000000000888";
    write_price(OLD_PRICE);

    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let command = format!(
        "stty rows 32 cols 110; {} --out {} --stellar-bin {} \
         --rpc-url https://rpc.testnet.stellar.org \
         --passphrase 'Test SDF Network ; September 2015' market tui --mode explore",
        quote(std::path::Path::new(BIN)),
        quote(&h.run),
        quote(&h.fake)
    );
    let transcript = h.scratch.join("status-cache.typescript");
    let mut child = WatchChild(
        Command::new("script")
            .args(["-q", "-e", "-f", "-c", &command])
            .arg(&transcript)
            .env_remove("STELLAR_RPC_URL")
            .env_remove("STELLAR_NETWORK_PASSPHRASE")
            .env_remove("STELLAR_CONTRACT_ID")
            .env("TERM", "xterm-256color")
            .env("FAKE_STELLAR_LOG", &h.log)
            .env("FAKE_STELLAR_VERSION", V261)
            .env("FAKE_STELLAR_SPEC", h.scratch.join("fake.spec.json"))
            .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
            .env("FAKE_STELLAR_VIEW", &unsupported)
            .env("FAKE_STELLAR_VIEW_DIR", &view_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch the status explorer under script(1)"),
    );
    let press = |child: &mut std::process::Child, keys: &[u8], marker: &str| {
        let offset = fs::metadata(&transcript)
            .expect("terminal transcript")
            .len() as usize;
        child
            .stdin
            .as_mut()
            .expect("status terminal input")
            .write_all(keys)
            .expect("send status terminal keys");
        wait_for_terminal_marker_after(child, &transcript, marker, 20, offset);
    };
    let current_screen =
        || terminal_fixture_screen::<110, 32>(&fs::read(&transcript).expect("status transcript"));
    let status_reads = || {
        fake_log(&h)
            .lines()
            .filter(|line| line.starts_with("contract invoke "))
            .count()
    };

    wait_for_terminal_marker(&mut child, &transcript, "Live ABI loaded", 12);
    press(&mut child, b"s", "[\"readiness\"]");
    press(&mut child, b"\x1b[B", OLD_PRICE); // Overview -> Oracle.
    let first_reads = status_reads();
    let log = fake_log(&h);
    assert!(
        log.lines()
            .any(|line| line.starts_with("contract invoke ") && line.contains(" -- lastprice ")),
        "the displayed price must come from a native read:\n{log}"
    );
    for line in log
        .lines()
        .filter(|line| line.starts_with("contract invoke "))
    {
        assert!(
            line.contains("--send no") && line.contains(&format!("--source {admin}")),
            "a status read must simulate as the manifest signer: {line}"
        );
    }

    write_price(NEW_PRICE);
    thread::sleep(Duration::from_millis(400));
    let screen = current_screen();
    assert!(
        screen.contains(OLD_PRICE) && !screen.contains(NEW_PRICE),
        "{screen}"
    );
    assert_eq!(status_reads(), first_reads, "the open page must not poll");

    press(&mut child, b"\x1b", "MARKET / testnet /");
    press(&mut child, b"s", OLD_PRICE);
    let screen = current_screen();
    assert!(!screen.contains(NEW_PRICE), "{screen}");
    assert!(
        !screen.contains("LOADING") && !screen.contains("REFRESHING"),
        "{screen}"
    );
    assert_eq!(
        status_reads(),
        first_reads,
        "reopening must reuse the snapshot"
    );

    press(&mut child, b"s", NEW_PRICE);
    assert!(
        !current_screen().contains(OLD_PRICE),
        "refresh must replace the old price"
    );
    assert_eq!(
        status_reads(),
        2 * first_reads,
        "only explicit refresh rereads"
    );

    press(&mut child, b"\x1b", "MARKET / testnet /");
    child
        .stdin
        .as_mut()
        .expect("status terminal input")
        .write_all(b"q")
        .expect("quit");
    assert!(wait_for_guarded_call_exit(&h, &mut child, 8).success());
    assert_reads_are_send_free(&h);
    assert!(!h.run.join("market-pending").exists(), "no status intent");
    assert_eq!(
        fs::read(&h.manifest).expect("fixture manifest bytes"),
        manifest_before
    );
}

/// The guided `pool/set-emissions-config` recipe runs the real simulate and
/// prepare path: the empty typed `res_emission_metadata` vector is passed
/// through to the native seam as `[]`, the fork's own guard refuses it with
/// BadRequest (1200), and neither the refused simulation nor the refused
/// prepare can leave a confirmation, a journal intent, or a chain send behind.
#[test]
fn guided_pool_set_emissions_config_empty_metadata_meets_the_fork_guard_without_any_send() {
    let h = harness("guided-pool-set-emissions-config");
    let manifest_before = fs::read(&h.manifest).expect("frozen manifest bytes");
    let pool = contract_id("explorer-cli-pool");
    let admin = account_id("explorer-cli-admin");

    let mut spec: Vec<Value> = serde_json::from_str(SPEC_LIVE).expect("live ABI fixture");
    spec.extend([
        serde_json::json!({"function_v0": {
            "doc": "",
            "name": "set_emissions_config",
            "inputs": [{"doc": "", "name": "res_emission_metadata", "type_": {
                "vec": {"element_type": {"udt": {"name": "ReserveEmissionMetadata"}}}
            }}],
            "outputs": []
        }}),
        serde_json::json!({"udt_struct_v0": {
            "doc": "", "lib": "", "name": "ReserveEmissionMetadata",
            "fields": [
                {"doc": "", "name": "res_index", "type_": "u32"},
                {"doc": "", "name": "res_type", "type_": "u32"},
                {"doc": "", "name": "share", "type_": "u64"}
            ]
        }}),
    ]);
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(
        &spec_file,
        serde_json::to_vec(&spec).expect("serialize emissions ABI"),
    )
    .expect("write emissions ABI");
    // The fork traps `set_emissions_config` outright
    // (`blend-contracts-v2/pool/src/contract.rs:522`), before any admin check.
    // The fake only speaks this answer for an invoke, after its send gate.
    fs::write(
        h.scratch.join("fake.contract-1200"),
        "error: transaction simulation failed: HostError: Error(Contract, #1200)\n",
    )
    .expect("write fork guard stderr fixture");
    fs::write(h.scratch.join("fake.emissions-view"), "null").expect("write invoke stdout fixture");
    // An empty (clear) journal root so `p` genuinely reaches the native
    // simulation instead of being refused by the custody gate.
    fs::create_dir(h.run.join("market-pending")).expect("clear journal root");
    let identities = h.scratch.join("identities");
    fs::create_dir(&identities).expect("signer fixture directory");
    fs::write(
        h.scratch.join("fake.signer-public-key"),
        format!("{}\n", account_id("explorer-cli-emissions-signer")),
    )
    .expect("write signer lookup fixture");

    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let command = format!(
        "stty rows 32 cols 110; {} --out {} --stellar-bin {} \
         --rpc-url https://rpc.testnet.stellar.org \
         --passphrase 'Test SDF Network ; September 2015' \
         market tui --mode explore --signer admin --signer-config-dir {}",
        quote(std::path::Path::new(BIN)),
        quote(&h.run),
        quote(&h.fake),
        quote(&identities)
    );
    let transcript = h.scratch.join("guided-emissions.typescript");
    let mut child = WatchChild(
        Command::new("script")
            .args(["-q", "-e", "-f", "-c", &command])
            .arg(&transcript)
            .env_remove("STELLAR_RPC_URL")
            .env_remove("STELLAR_NETWORK_PASSPHRASE")
            .env_remove("STELLAR_CONTRACT_ID")
            .env("TERM", "xterm-256color")
            .env("FAKE_STELLAR_LOG", &h.log)
            .env("FAKE_STELLAR_VERSION", V261)
            .env("FAKE_STELLAR_SPEC", &spec_file)
            .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
            .env(
                "FAKE_STELLAR_PUBLIC_KEY",
                h.scratch.join("fake.signer-public-key"),
            )
            .env("FAKE_STELLAR_VIEW", h.scratch.join("fake.emissions-view"))
            .env("FAKE_STELLAR_VIEW_STATUS", "1")
            .env(
                "FAKE_STELLAR_INVOKE_STDERR",
                h.scratch.join("fake.contract-1200"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch the guided emissions explorer under script(1)"),
    );
    let transcript_len = || {
        fs::metadata(&transcript)
            .map(|metadata| metadata.len() as usize)
            .unwrap_or_default()
    };
    let send_keys = |child: &mut std::process::Child, keys: &[u8]| {
        child
            .stdin
            .as_mut()
            .expect("emissions terminal input")
            .write_all(keys)
            .expect("send emissions terminal keys");
    };
    let wait_after = |child: &mut std::process::Child, marker: &str, offset: usize| {
        wait_for_terminal_marker_after(child, &transcript, marker, 20, offset);
    };
    let screen_now = || {
        terminal_fixture_screen::<110, 32>(&fs::read(&transcript).expect("emissions transcript"))
    };
    // Terminals wrap, so a rendered phrase is matched with whitespace collapsed.
    let shows = |screen: &str, needle: &str| -> bool {
        screen
            .split_whitespace()
            .collect::<String>()
            .contains(&needle.split_whitespace().collect::<String>())
    };

    wait_for_terminal_marker(&mut child, &transcript, "assets", 12);
    let offset = transcript_len();
    send_keys(&mut child, b"a");
    wait_after(&mut child, "recipe help and selectors", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"/");
    wait_after(&mut child, "Filter:", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"pool/set-emissions-config");
    wait_after(&mut child, "Filter: pool/set-emissions-config", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(&mut child, "Method: set_emissions_config", offset);
    let offset = transcript_len();
    send_keys(&mut child, b"\r");
    wait_after(
        &mut child,
        "Typed guided form ready; r simulates one call, p prepares one call.",
        offset,
    );
    let screen = screen_now();
    assert!(
        shows(&screen, "res_emission_metadata = 0 rows"),
        "the recipe must start with an empty typed emission list the contract guard itself must face:\n{screen}"
    );
    assert!(
        shows(&screen, "MARKET / testnet / signer configured"),
        "the run must be armed for a guarded prepare, so the refusal below is the contract's and not a local unsigned block:\n{screen}"
    );

    // `r`: the empty vector reaches the native seam as `[]` and the fork's own
    // guard refuses it, named as a contract rejection with no confirmation.
    let offset = transcript_len();
    send_keys(&mut child, b"r");
    wait_after(&mut child, "Stage: read-only simulation", offset);
    let screen = screen_now();
    assert!(
        shows(&screen, "Operation: pool/set-emissions-config")
            && shows(&screen, "Stage: read-only simulation")
            && shows(
                &screen,
                "contract rejected the simulated operation: BadRequest (1200)"
            ),
        "the refused simulation must name the recipe and the fork's contract error:\n{screen}"
    );
    assert!(
        shows(&screen, "failed / no confirmation"),
        "a refused simulation may not leave a confirmation available:\n{screen}"
    );
    assert!(
        !shows(&screen, "y sends this exact call once"),
        "a refused simulation must never present a send prompt:\n{screen}"
    );

    // `y` with nothing prepared is refused by the shipped explorer itself.
    let offset = transcript_len();
    send_keys(&mut child, b"y");
    wait_after(
        &mut child,
        "No current confirmation: y cannot send. Prepare and review first.",
        offset,
    );

    // `p`: prepare re-runs the same native simulation and refuses before any
    // intent is reserved, so the confirmation gate never opens.
    let offset = transcript_len();
    send_keys(&mut child, b"p");
    wait_after(&mut child, "Stage: prepare / simulation", offset);
    let screen = screen_now();
    assert!(
        shows(&screen, "Operation: pool/set-emissions-config")
            && shows(
                &screen,
                "contract rejected the simulated operation: BadRequest (1200)"
            ),
        "prepare must reach the fork's guard and refuse there:\n{screen}"
    );
    assert!(
        !shows(&screen, "Send disabled: a configured signer"),
        "prepare must not be refused locally while a signer is configured:\n{screen}"
    );
    let offset = transcript_len();
    send_keys(&mut child, b"y");
    wait_after(
        &mut child,
        "No current confirmation: y cannot send. Prepare and review first.",
        offset,
    );
    assert!(
        shows(&screen_now(), "failed / no confirmation"),
        "the refused prepare must leave the run without any confirmation"
    );

    send_keys(&mut child, b"q");
    let status = wait_for_guarded_call_exit(&h, &mut child, 8);
    assert_eq!(
        status.code(),
        Some(1),
        "a run whose only action the contract refused must not report success:\n{}",
        fake_log(&h)
    );

    let log = fake_log(&h);
    let invocations: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("contract invoke "))
        .collect();
    assert_eq!(
        invocations.len(),
        2,
        "exactly one simulation and one prepare may invoke:\n{log}"
    );
    assert!(
        invocations[0].starts_with(&format!(
            "contract invoke --id {pool} --source {admin} --send no"
        )),
        "the simulation must target the manifest pool as a no-send read sourced by the manifest signer:\n{}",
        invocations[0]
    );
    assert!(
        log.contains("keys public-key admin"),
        "prepare must resolve the configured signer alias through the CLI:\n{log}"
    );
    assert!(
        invocations[1].starts_with(&format!(
            "contract invoke --id {pool} --source admin --send no"
        )),
        "prepare must simulate as the configured signer alias, never as a send:\n{}",
        invocations[1]
    );
    for call in invocations {
        let (_, native_args) = call
            .split_once(" -- set_emissions_config ")
            .expect("the native invoke must carry the fork's method name");
        assert_eq!(
            native_args, "--res_emission_metadata []",
            "the empty typed vector must reach the contract as native empty JSON"
        );
    }
    assert_reads_are_send_free(&h);
    assert_eq!(
        fs::read_dir(h.run.join("market-pending"))
            .expect("journal root")
            .count(),
        0,
        "a refused prepare must reserve no pending intent"
    );
    assert_eq!(
        fs::read(&h.manifest).expect("frozen manifest bytes"),
        manifest_before,
        "the refused recipe may not alter the frozen manifest"
    );
}

/// The shipped `market status` collector must overlap independent native
/// reads without ever exceeding the eight-job ceiling, retry a transient
/// transport failure on the one affected leg (never a whole-snapshot
/// replay), and still never send or reserve intent. Each instrumented
/// fake-CLI child records its own start/end event and holds briefly
/// between them, so the peak concurrency is a property of the recorded
/// event order, not of measured wall-clock speed. The shared fixture
/// binds BLND and enumerates four manifest actors (admin, frodo, sam,
/// user), so the per-method census is exact: 4 adapter prices, 4 caches,
/// 4 breaker views, 1 config, 1 admin, 3 reserves, 7 positions
/// (admin + 4 identities + pool + backstop), 1 pool_data, 5 backstop
/// balances (admin + 4 identities), 1 reward zone, and 15 token balances.
/// Exactly one
/// `lastprice --asset <stable>` call is failed with a standalone HTTP 429
/// by an atomic first-attempt marker: that leg must appear twice in the
/// argv log and still render the expected price, while every successful
/// sibling leg appears exactly once and the frozen manifest stays
/// byte-identical with no journal intent reserved.
#[test]
fn market_status_overlaps_independent_reads_within_eight_and_retries_one_transient_leg() {
    fn count_lines(log: &str, needle: &str) -> usize {
        log.lines().filter(|line| line.contains(needle)).count()
    }

    let h = harness("status-parallel-retry");
    let blnd = contract_id("explorer-cli-tok-blnd");
    let stable = contract_id("explorer-cli-tok-stable");
    let pool = contract_id("explorer-cli-pool");
    let backstop = contract_id("explorer-cli-backstop");
    let adapter = contract_id("explorer-cli-adapter");
    let admin = account_id("explorer-cli-admin");
    let manifest = manifest_toml().replace("[tokens]\n", &format!("[tokens]\nblnd = \"{blnd}\"\n"));
    fs::write(&h.manifest, manifest).expect("write five-token manifest");
    let manifest_before = fs::read(&h.manifest).expect("manifest bytes");
    fs::create_dir(h.run.join("market-pending")).expect("clear journal root");

    // The status ABI: exactly the getters the collector reads, with the
    // parameter names `read_status` rechecks against the live interface.
    let mut abi: Vec<Value> = Vec::new();
    for (name, inputs) in [
        ("lastprice", vec!["asset"]),
        ("get_cached", vec!["asset"]),
        ("get_breaker_set_view", vec!["asset"]),
        ("get_config", vec![]),
        ("get_admin", vec![]),
        ("get_reserve", vec!["asset"]),
        ("get_positions", vec!["address"]),
        ("pool_data", vec!["pool"]),
        ("user_balance", vec!["pool", "user"]),
        ("reward_zone", vec![]),
        ("balance", vec!["id"]),
    ] {
        abi.push(serde_json::json!({"function_v0": {
            "name": name,
            "inputs": inputs.iter().map(|input| serde_json::json!({"name": input})).collect::<Vec<_>>()
        }}));
    }
    let spec_file = h.scratch.join("fake.spec.json");
    fs::write(&spec_file, serde_json::to_vec(&abi).expect("abi")).expect("write status abi");

    // One canned answer per native getter, each in the shape its own
    // classifier accepts. Only the stable reserve is armed enabled, so
    // the other two stay visibly malformed — partial truth, never a
    // fabricated clear.
    let view_dir = h.scratch.join("status-views");
    fs::create_dir(&view_dir).expect("status view directory");
    for (function, body) in [
        (
            "lastprice",
            r#"{"price":7777000000000777,"timestamp":1759000001}"#.to_string(),
        ),
        ("get_cached", "\"Accepted\"".to_string()),
        (
            "get_breaker_set_view",
            r#"{"is_manually_tripped":false,"is_blocking":false}"#.to_string(),
        ),
        (
            "get_config",
            format!(r#"{{"status":1,"oracle":"{adapter}"}}"#),
        ),
        ("get_admin", format!("\"{admin}\"")),
        (
            "get_reserve",
            format!(
                r#"{{"asset":"{stable}","config":{{"index":0,"decimals":6,"enabled":true}},"data":{{"d_rate":1,"b_rate":2,"ir_mod":3,"b_supply":4,"d_supply":5,"backstop_credit":6,"last_time":7}}}}"#
            ),
        ),
        ("get_positions", r#"[["0","123"]]"#.to_string()),
        ("pool_data", r#"[["credit","1"]]"#.to_string()),
        ("user_balance", r#"[["credit","9"]]"#.to_string()),
        ("reward_zone", "[]".to_string()),
        ("balance", "4242".to_string()),
    ] {
        fs::write(view_dir.join(function), body).expect("write status view response");
    }

    let sync_log = h.scratch.join("status-sync.log");
    let transient_once = h.scratch.join("status-transient-claimed");
    let transient_marker = format!(r#"-- lastprice --asset {{"Stellar":"{stable}"}}"#);
    let out = Command::new(BIN)
        .arg("--out")
        .arg(&h.run)
        .arg("--stellar-bin")
        .arg(&h.fake)
        .arg("--rpc-url")
        .arg("https://rpc.testnet.stellar.org")
        .args(["market", "status"])
        .env("FAKE_STELLAR_LOG", &h.log)
        .env("FAKE_STELLAR_VERSION", V261)
        .env("FAKE_STELLAR_SPEC", &spec_file)
        .env("FAKE_STELLAR_NETWORK_INFO", &h.network_info)
        .env("FAKE_STELLAR_VIEW_DIR", &view_dir)
        .env("FAKE_STELLAR_SYNC_LOG", &sync_log)
        .env("FAKE_STELLAR_SYNC_HOLD", "0.35")
        .env("FAKE_STELLAR_TRANSIENT_ON", &transient_marker)
        .env("FAKE_STELLAR_TRANSIENT_ONCE", &transient_once)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("collect a fresh market status");

    // Match child lifetimes and prove both stages overlap without exceeding
    // the shared ceiling, even if a future change overlaps the stages.
    let events = fs::read_to_string(&sync_log).expect("status event log");
    let mut active = std::collections::BTreeMap::new();
    let mut max_in_flight = 0usize;
    let mut stage_peaks = [0usize; 2];
    for line in events.lines() {
        let (event, rest) = line.split_once(' ').expect("event and child");
        let (pid, kind) = rest.split_once(' ').expect("child and stage");
        assert!(matches!(kind, "interface" | "invoke"), "{line}");
        match event {
            "SYNC-START" => assert!(active.insert(pid, kind).is_none(), "{line}"),
            "SYNC-END" => assert_eq!(active.remove(pid), Some(kind), "{line}"),
            _ => panic!("unexpected status event: {line}"),
        }
        max_in_flight = max_in_flight.max(active.len());
        for (index, stage) in ["interface", "invoke"].into_iter().enumerate() {
            stage_peaks[index] = stage_peaks[index].max(
                active
                    .values()
                    .copied()
                    .filter(|kind| *kind == stage)
                    .count(),
            );
        }
    }
    assert!(active.is_empty(), "unjoined status reads: {active:?}");
    assert!(
        max_in_flight <= 8,
        "status collection exceeded the eight-job ceiling: {max_in_flight}\n{events}"
    );
    for (stage, peak) in ["interface", "invoke"].into_iter().zip(stage_peaks) {
        assert!(peak > 1, "{stage} reads never overlapped:\n{events}");
    }

    // ABI and invoke admission: nine distinct manifest-bound targets plus the
    // display-only governance contract, one identity probe, and 46 successful
    // observation legs plus exactly one injected retry, matched per method
    // below. This fixture ABI has no governance getters, so they never invoke.
    let log = fake_log(&h);
    assert_eq!(network_probe_count(&h), 1, "one fail-closed identity probe");
    assert_eq!(
        count_lines(&log, "contract info interface "),
        10,
        "one deduplicated ABI read per bound target:\n{log}"
    );
    let invokes: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("contract invoke "))
        .collect();
    // The exact per-method census this fixture's four manifest actors
    // produce. The `[identities]` table enumerates admin, frodo, sam, and
    // user, and production extends both account lists with every
    // identity, so positions read admin + 4 identities + pool + backstop
    // (7) and backstop balances read admin + 4 identities (5): 46 legs,
    // and the injected 429 therefore shows up as exactly one extra
    // attempt. A whole-snapshot replay would double every row below.
    fn invoked(line: &str) -> Option<&str> {
        line.split_once(" -- ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
    }
    let expected_legs: [(&str, usize); 11] = [
        ("lastprice", 5),
        ("get_cached", 4),
        ("get_breaker_set_view", 4),
        ("get_config", 1),
        ("get_admin", 1),
        ("get_reserve", 3),
        ("get_positions", 7),
        ("pool_data", 1),
        ("user_balance", 5),
        ("reward_zone", 1),
        ("balance", 15),
    ];
    assert_eq!(
        invokes.len(),
        expected_legs.iter().map(|(_, count)| count).sum::<usize>(),
        "46 observation legs plus exactly one transient retry:\n{log}"
    );
    for (function, expected) in expected_legs {
        assert_eq!(
            invokes
                .iter()
                .filter(|line| invoked(line) == Some(function))
                .count(),
            expected,
            "sibling conservation for {function}; a whole-snapshot replay doubles this row:\n{log}"
        );
    }
    for target in [
        pool.as_str(),
        backstop.as_str(),
        adapter.as_str(),
        blnd.as_str(),
    ] {
        assert!(
            invokes
                .iter()
                .any(|line| line.contains(&format!("--id {target}"))),
            "a bound target was never read: {target}\n{log}"
        );
    }
    assert_eq!(
        count_lines(&log, r#"-- lastprice --asset {"Stellar":"#),
        5,
        "four price legs plus the single retried attempt:\n{log}"
    );
    assert_eq!(
        count_lines(&log, &transient_marker),
        2,
        "the injected HTTP 429 leg must run its first attempt and its retry only:\n{log}"
    );
    assert!(
        transient_once.is_dir(),
        "the first-attempt marker must have been claimed atomically"
    );
    for line in &invokes {
        assert!(
            line.contains(&format!("--source {admin}")),
            "no status read may invoke from a source other than the manifest signer: {line}"
        );
    }
    assert_reads_are_send_free(&h);
    assert_eq!(
        fs::read(&h.manifest).expect("manifest bytes"),
        manifest_before,
        "status collection may not touch the frozen manifest"
    );
    assert_eq!(
        fs::read_dir(h.run.join("market-pending"))
            .expect("journal root")
            .count(),
        0,
        "a status read reserves no intent"
    );

    // The retried leg and its successful siblings survive with their
    // exact expected values; the two unarmed reserves stay visibly
    // malformed instead of being dropped or fabricated.
    assert!(
        !out.status.success(),
        "a market with malformed reserves must not report readiness: {}",
        output_text(&out)
    );
    let rendered: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("market status renders its dashboard");
    assert_eq!(rendered["schema_version"], serde_json::json!(1));
    for asset in ["weth", "usdc", "xlm", "stable"] {
        let price = &rendered["oracle"][asset]["adapter_lastprice"];
        assert_eq!(
            price["state"].as_str(),
            Some("adapter-price-observed"),
            "every price leg (including the retried stable one) must observe its value"
        );
        assert!(
            price["observed"]
                .as_str()
                .is_some_and(|observed| observed.contains("7777000000000777")),
            "the exact i128 price must survive the {asset} read: {price}"
        );
        assert_eq!(
            rendered["oracle"][asset]["proxy_cache"]["state"].as_str(),
            Some("oracle-cache-accepted")
        );
        assert_eq!(
            rendered["oracle"][asset]["proxy_breaker"]["state"].as_str(),
            Some("breaker-clear")
        );
    }
    assert_eq!(
        rendered["pool"]["config"]["state"].as_str(),
        Some("pool-active")
    );
    assert_eq!(
        rendered["pool"]["admin"]["state"].as_str(),
        Some("admin-match")
    );
    assert_eq!(
        rendered["pool"]["reserves"]["stable"]["state"].as_str(),
        Some("reserve-enabled")
    );
    for asset in ["xlm", "weth"] {
        let reserve = &rendered["pool"]["reserves"][asset];
        assert!(
            !reserve["state"]
                .as_str()
                .is_some_and(|state| state == "reserve-enabled"),
            "an unarmed reserve may never be reported enabled: {reserve}"
        );
    }
    assert_eq!(
        rendered["backstop"]["reward_zone"]["state"].as_str(),
        Some("observed")
    );
    assert_eq!(
        rendered["backstop"]["pool_data"]["state"].as_str(),
        Some("observed")
    );
    let balances = rendered["token_balances"]
        .as_object()
        .expect("token balances");
    assert_eq!(balances.len(), 5, "BLND must keep its own balance stage");
    for token in ["blnd", "usdc", "weth", "xlm", "stable"] {
        for holder in ["pool", "backstop", "admin"] {
            assert_eq!(
                rendered["token_balances"][token][holder]["state"].as_str(),
                Some("observed"),
                "the fifteen-leg balance stage lost {token}/{holder}"
            );
        }
    }
    let reasons = rendered["reasons"]
        .as_array()
        .expect("readiness reasons")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        reasons.contains(&"a required reserve is unavailable, disabled, or malformed"),
        "the malformed reserves must be named: {reasons:?}"
    );
    assert!(
        !reasons.contains(&"a required adapter price is null, stale, or unavailable"),
        "the retried price leg may not close readiness: {reasons:?}"
    );
    assert_eq!(rendered["journal"]["state"].as_str(), Some("journal-clear"));
}
