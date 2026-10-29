//! Thin wrapper over the `stellar` CLI. Every chain action shells out; nothing here
//! signs, encodes XDR, or talks RPC directly. Trimmed from
//! `contracts/tools/soroban-vault-cli/src/stellar.rs` (executor trait, parsers).

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{MARKET_MAINNET_PASSPHRASE, MARKET_TESTNET_PASSPHRASE};

/// Minimum CLI major version able to decode current testnet XDR (protocol 28).
pub const MIN_CLI_MAJOR: u32 = 23;

/// Minimum stable CLI version accepted by the market explorer seam. The
/// historical pair gate above stays at >=23; the two gates are separate and
/// neither one relaxes the other.
const MARKET_MIN_CLI_VERSION: (u32, u32, u32) = (26, 1, 0);

/// Attempts per CLI call, including the first.
pub(crate) const RETRY_ATTEMPTS: u32 = 5;

/// 1, 2, 4, 8, 16 s, then capped at 30 s.
const fn retry_delay_secs(attempt: u32) -> u64 {
    match attempt {
        0..=4 => 1u64 << attempt,
        _ => 30,
    }
}

pub(crate) const fn retry_backoff_secs() -> u64 {
    let mut total = 0;
    let mut attempt = 0;
    while attempt + 1 < RETRY_ATTEMPTS {
        total += retry_delay_secs(attempt);
        attempt += 1;
    }
    total
}

/// Latest the single confirmed send may start after journal reservation:
/// the unpinned invoke_confirmed_bound lookup and market_pinned_signing's
/// lookup each may retry RETRY_ATTEMPTS times with run's backoff. Checked
/// against the real-time clock (the clock that stamps the intent), so a
/// stalled journal sync or a sleeping machine cannot start a late send.
pub(crate) const MARKET_SEND_START_DEADLINE_SECS: u64 =
    2 * (RETRY_ATTEMPTS as u64 * MARKET_MUTATION_WALL.as_secs() + retry_backoff_secs());

/// Latest the confirmed send can read its account sequence after
/// reservation: the start deadline plus the send's own wall (it never
/// retries).
pub(crate) const MARKET_CONFIRMED_SEND_BOUND_SECS: u64 =
    MARKET_SEND_START_DEADLINE_SECS + MARKET_MUTATION_WALL.as_secs();

/// Latest a real market coordinate's send can read its account sequence
/// after its intent was reserved. A coordinate follows the
/// read-submit-readback law (`run_real_slot`): any number of bounded
/// ledger/contract reads may complete before the one send, each through
/// the retry path. `REAL_SENDS_BEFORE_SEND_READS` is the number of such
/// reads the graph performs ahead of its send, counted from the graph
/// itself; the bound is that count times one retry-bounded read, plus the
/// send's own wall. It is a bound on the tool's own code, not a guess at
/// network latency.
pub(crate) const REAL_SENDS_BEFORE_SEND_READS: u64 = 12;
pub(crate) const REAL_COORDINATE_SEND_BOUND_SECS: u64 = REAL_SENDS_BEFORE_SEND_READS
    * (RETRY_ATTEMPTS as u64 * MARKET_MUTATION_WALL.as_secs() + retry_backoff_secs())
    + MARKET_MUTATION_WALL.as_secs();

/// Which surfaces the raw-output construction boundary renders for (boundary
/// item 2). Selected once at context construction and never reassigned: pair
/// mode reproduces the historical bytes byte-exactly (AC9); market mode emits
/// input-free literals or sanitized text only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Pair,
    Market,
}

/// Default length cap for market arms without an explicit cap parameter.
pub(crate) const MARKET_TEXT_CAP: usize = 2000;

/// Hard ceiling on market ABI-spec bytes: hostile or malformed CLI output
/// is rejected by size before it reaches any parser or operator surface.
const MARKET_CLI_SPEC_CAP: usize = 1_048_576;

/// Maximum event page size the market history seam will request or accept.
const MARKET_EVENTS_MAX_COUNT: u32 = 100;

/// Hard ceiling on accepted `stellar events` NDJSON bytes: hostile,
/// malformed, or runaway output is rejected by size before any parser or
/// operator surface sees it.
const MARKET_EVENTS_PAGE_CAP: usize = 1_048_576;

/// Hard ceiling on market command stdout: bytes beyond this are refused
/// before they enter any process-owned buffer.
pub const MARKET_CAPTURE_STDOUT_CAP: usize = 1_048_576;

/// Hard ceiling on market command stderr: bytes beyond this are refused
/// before they enter any process-owned buffer.
pub const MARKET_CAPTURE_STDERR_CAP: usize = 65_536;

/// Wall limit for bounded market reads.
pub const MARKET_READ_WALL: Duration = Duration::from_secs(90);

/// Wall limit for market deployment and play, which may wait for inclusion.
pub const MARKET_MUTATION_WALL: Duration = Duration::from_secs(300);

/// The inclusion bid every market command that can submit a transaction
/// carries: `--fee 10000` stroops, above mainnet's current Soroban inclusion
/// price (p99 200), so a market send is never left bidding the CLI default.
/// It is the inclusion bid, not a cap on the resource fee. Pair-policy argv
/// carries no fee word at all.
pub(crate) const MARKET_INCLUSION_FEE: &str = "10000";

/// Fixed input-free refusal when market process output exceeds its cap.
const MARKET_CAPTURE_OVERFLOW: &str = "market command output exceeded the bounded capture cap";

/// Fixed input-free refusal when market process capture suffers an I/O
/// failure. Transport details stay out of operator-visible text.
const MARKET_CAPTURE_FAILED: &str = "market command capture failed";

/// Fixed input-free refusal when a market process is about to be spawned
/// outside the controlled boundary (SEC001): the CLI auto-loads `.env`
/// from its working directory and resolves relative executable paths
/// against it, so every market child must run from the controlled
/// dotenv-free root with an executable already resolved from the
/// invocation context. This is the last process-boundary chokepoint: a
/// manually constructed or drifted market executor fails closed before
/// the child starts.
pub const MARKET_EXECUTOR_UNSAFE: &str =
    "market process boundary refused: unsafe execution context before spawn";

/// Which process-capture policy a `RealExecutor` enforces. Construction
/// requires an explicit choice, so no market call site can accidentally
/// select unbounded capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    /// Preserve the historical pair path exactly: `Command::output()` and
    /// its existing argv-bearing error context remain byte-for-byte unchanged.
    Pair,
    /// Spawn the market command with both output streams bounded. Bytes
    /// beyond either cap are refused before storage growth, and the child
    /// is killed and reaped on overflow or wall expiry.
    Market {
        stdout_cap: usize,
        stderr_cap: usize,
        wall: Option<Duration>,
    },
}

#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureFailure {
    Overflow,
    Expired,
    Failed,
}

#[cfg(target_os = "linux")]
const MARKET_CAPTURE_CHUNK: usize = 8 * 1024;

#[cfg(target_os = "linux")]
struct CaptureStream {
    received: usize,
    overflow: bool,
    closed: bool,
    error: bool,
}

/// Read one bounded capture stream. Overflow is detected at cap + 1 before
/// the corresponding bytes are retained. An I/O error is an error closure,
/// distinct from clean end-of-file, so a truncated stream can never be
/// mistaken for complete output.
#[cfg(target_os = "linux")]
fn read_market_stream(
    stream: &mut UnixStream,
    cap: usize,
    captured: &mut Vec<u8>,
    state: &mut CaptureStream,
) {
    if state.closed || state.received > cap {
        state.overflow = state.received > cap;
        return;
    }
    let mut chunk = [0_u8; MARKET_CAPTURE_CHUNK];
    match stream.read(&mut chunk) {
        Ok(0) => state.closed = true,
        Ok(read) => {
            state.received = state.received.saturating_add(read);
            if state.received <= cap {
                captured.extend_from_slice(&chunk[..read]);
            } else {
                state.overflow = true;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(_) => {
            state.closed = true;
            state.error = true;
        }
    }
}

/// Alternately drain both capture streams without blocking either one. The
/// deadline is checked on every iteration regardless of child state, and the
/// direct child is always killed and reaped before this function returns.
///
/// ponytail: A descendant that inherits a capture endpoint can keep it open
/// after the direct child is reaped, retaining a socket until it exits. Its
/// writes fail with EPIPE once our endpoint is dropped, while the monotonic
/// market deadline always returns the bounded call first. Upgrade path: kernel
/// support for reliably terminating every descendant, or process-group /
/// namespace ownership if market children are ever permitted to spawn helpers.
#[cfg(target_os = "linux")]
fn capture_market_streams(
    child: &mut std::process::Child,
    stdout: &mut UnixStream,
    stderr: &mut UnixStream,
    stdout_cap: usize,
    stderr_cap: usize,
    wall: Duration,
) -> Result<(bool, Vec<u8>, Vec<u8>), CaptureFailure> {
    let started = Instant::now();
    let mut stdout_buffer = Vec::new();
    let mut stderr_buffer = Vec::new();
    let mut stdout_state = CaptureStream {
        received: 0,
        overflow: false,
        closed: false,
        error: false,
    };
    let mut stderr_state = CaptureStream {
        received: 0,
        overflow: false,
        closed: false,
        error: false,
    };
    let mut exited = false;
    let mut succeeded = false;
    loop {
        if started.elapsed() >= wall {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CaptureFailure::Expired);
        }
        read_market_stream(stdout, stdout_cap, &mut stdout_buffer, &mut stdout_state);
        read_market_stream(stderr, stderr_cap, &mut stderr_buffer, &mut stderr_state);
        if stdout_state.overflow || stderr_state.overflow {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CaptureFailure::Overflow);
        }
        if stdout_state.error || stderr_state.error {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CaptureFailure::Failed);
        }
        if !exited {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exited = true;
                    succeeded = status.success();
                }
                Ok(None) => {}
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CaptureFailure::Failed);
                }
            }
        }
        if exited && stdout_state.closed && stderr_state.closed {
            return Ok((succeeded, stdout_buffer, stderr_buffer));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
fn prepare_market_endpoint(
    stream: std::io::Result<(UnixStream, UnixStream)>,
) -> anyhow::Result<(UnixStream, std::process::Stdio)> {
    let (parent, child) =
        stream.map_err(|_| anyhow!("{}", Safe::literal(MARKET_CAPTURE_FAILED)))?;
    parent
        .set_nonblocking(true)
        .map_err(|_| anyhow!("{}", Safe::literal(MARKET_CAPTURE_FAILED)))?;
    let child: std::os::fd::OwnedFd = child.into();
    Ok((parent, std::process::Stdio::from(child)))
}

/// Bounded market capture. The command receives one endpoint from each of two
/// independent Unix socketpairs as its stdout and stderr, keeping the streams
/// separately capped while preserving byte-for-byte output capture for normal
/// commands.
#[cfg(target_os = "linux")]
fn market_capture(
    mut cmd: Command,
    stdout_cap: usize,
    stderr_cap: usize,
    wall: Duration,
    start_by: Option<std::time::SystemTime>,
) -> anyhow::Result<CmdOutput> {
    let (mut stdout_parent, stdout_child) = prepare_market_endpoint(UnixStream::pair())?;
    let (mut stderr_parent, stderr_child) = prepare_market_endpoint(UnixStream::pair())?;
    // The last check before the child exists: nothing but the spawn follows.
    if let Some(start_by) = start_by {
        ensure_started_by(start_by)?;
    }
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(stdout_child)
        .stderr(stderr_child)
        .spawn()
        .map_err(|_| anyhow!("{}", Safe::literal(MARKET_CAPTURE_FAILED)))?;
    drop(cmd);
    let capture = capture_market_streams(
        &mut child,
        &mut stdout_parent,
        &mut stderr_parent,
        stdout_cap,
        stderr_cap,
        wall,
    );
    let _ = stdout_parent.shutdown(std::net::Shutdown::Both);
    let _ = stderr_parent.shutdown(std::net::Shutdown::Both);
    let (succeeded, stdout_buffer, stderr_buffer) = match capture {
        Ok(output) => output,
        Err(CaptureFailure::Overflow) => {
            return Err(anyhow!("{}", Safe::literal(MARKET_CAPTURE_OVERFLOW)))
        }
        Err(CaptureFailure::Expired | CaptureFailure::Failed) => {
            return Err(anyhow!("{}", Safe::literal(MARKET_CAPTURE_FAILED)))
        }
    };
    Ok(CmdOutput::new(
        RawText(Some(
            String::from_utf8_lossy(&stdout_buffer).trim().to_string(),
        )),
        RawText(Some(
            String::from_utf8_lossy(&stderr_buffer).trim().to_string(),
        )),
        succeeded,
    ))
}

/// Bounded market capture is Linux-only; other targets refuse without
/// spawning a child while keeping the ordinary pair executor available.
#[cfg(not(target_os = "linux"))]
fn market_capture(
    _cmd: Command,
    _stdout_cap: usize,
    _stderr_cap: usize,
    _wall: Duration,
    _start_by: Option<std::time::SystemTime>,
) -> anyhow::Result<CmdOutput> {
    Err(anyhow!("{}", Safe::literal(MARKET_CAPTURE_FAILED)))
}

/// Market-arm renderer: strip control/ANSI bytes, collapse newlines to single
/// spaces, escape `|` and backtick, truncate to `cap` chars.
fn sanitize(text: &str, cap: usize) -> String {
    let mut out = String::new();
    let mut len = 0usize;
    for c in text.chars() {
        match c {
            '\n' | '\r' | '\t' => {
                if !out.ends_with(' ') {
                    out.push(' ');
                    len += 1;
                }
            }
            '|' => {
                out.push_str("\\|");
                len += 2;
            }
            '`' => {
                out.push_str("\\`");
                len += 2;
            }
            // ponytail: ESC alone is dropped; printable CSI payload bytes can
            // survive. Ceiling accepted by the plan ("strips control/ANSI
            // bytes"); full CSI parsing is the upgrade path.
            c if c.is_control() => {}
            c => {
                out.push(c);
                len += 1;
            }
        }
        if len >= cap {
            break;
        }
    }
    out
}

/// Untouchable CLI stream text (boundary item 1). Structurally unrenderable
/// outside this module: no `Display`, content-free `Debug`, module-private
/// field. Retained bytes are byte-authoritative pair evidence (boundary item
/// 4): sanitizing happens at render time, never write time.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct RawText(Option<String>);
impl fmt::Debug for RawText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(s) => write!(f, "Raw({} bytes)", s.len()),
            None => write!(f, "Raw(none)"),
        }
    }
}

impl Serialize for RawText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            Some(s) => serializer.serialize_str(s),
            None => serializer.serialize_none(),
        }
    }
}

impl RawText {
    /// The `parse_*` family's read of retained stdout (boundary item 1).
    fn pick(&self) -> &str {
        self.0.as_deref().unwrap_or("")
    }
}

/// The only renderable text type (boundary item 2). Constructible solely
/// through the constructors below plus one derived `Deserialize` (the single
/// deserialization route, required by the historical-evidence round-trip).
/// `Display`/`Serialize` emit stored bytes verbatim; `Debug` forwards the
/// inner `String`'s `Debug` so `ErrorKind::Other(Safe)` keeps pair bytes.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Safe(String);

impl fmt::Debug for Safe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Safe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Safe {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// The six choke-point context messages (form (m)) plus the item-7
/// rejection sites. Pair arms reproduce today's bytes verbatim, including the
/// `UnparseableVersion`/`UnparseableReadRow` input echo; market arms are
/// input-free.
pub(crate) enum PairCtx<'a> {
    VersionTooOld {
        stdout: &'a RawText,
        min: u32,
    },
    SubmissionUnresolved(&'a RawText),
    ViewFailed {
        f: &'a str,
        id: &'a str,
        error: Option<&'a ErrorKind>,
        stderr: &'a RawText,
    },
    ReadFailed(&'a RawText),
    UnparseableVersion(&'a str),
    UnparseableReadRow(&'a str),
}

/// Debug-form text for an optional error. The pair arm is byte-identical to
/// the legacy `{:?}` of `Option<ErrorKind>` (forms (d)/(i)); the market arm
/// is the policy-sanitized one-line summary. Returns `Safe`, never raw text.
fn opt_error_debug(policy: Policy, error: Option<&ErrorKind>) -> Safe {
    match policy {
        Policy::Pair => Safe(format!("{error:?}")),
        Policy::Market => match error {
            None => Safe::literal("none"),
            Some(kind) => Safe::error_summary(policy, kind),
        },
    }
}

impl Safe {
    fn market_invocation_error(kind: Option<&ErrorKind>) -> Safe {
        match kind {
            Some(ErrorKind::Contract(code)) => {
                let diagnostic = match (error_name(*code), error_help(*code)) {
                    (Some(name), Some(help)) => {
                        format!(
                            "contract rejected the simulated operation: {name} ({code}) — {help}"
                        )
                    }
                    (Some(name), None) => {
                        format!("contract rejected the simulated operation: {name} ({code})")
                    }
                    (None, _) => {
                        format!("contract rejected the simulated operation: unknown contract error ({code})")
                    }
                };
                Safe(sanitize(&diagnostic, MARKET_TEXT_CAP))
            }
            Some(ErrorKind::Auth) => Safe::literal(
                "market simulation authorization was rejected; this is not a contract error",
            ),
            Some(ErrorKind::Other(_)) | None => Safe::literal(MARKET_UNCLASSIFIED_FAILURE),
        }
    }

    /// Recovers only a typed, trusted market diagnostic from an anyhow error.
    /// Untyped CLI output, transport errors, and unknown local failures never
    /// become contract-rejection claims.
    pub fn market_error(error: &anyhow::Error) -> Safe {
        if let Some(safe) = error.downcast_ref::<Safe>() {
            return safe.clone();
        }
        Safe::literal(
            "market operation failed; the failure was not identified as a contract rejection",
        )
    }

    /// Compile-time literal: no runtime CLI bytes can enter through it.
    pub fn literal(text: &'static str) -> Safe {
        Safe(text.to_string())
    }

    /// Classification-only error payload read. Module-private.
    fn payload(&self) -> &str {
        &self.0
    }

    /// Raw text under a policy: pair arm verbatim, market arm sanitized.
    pub fn render(policy: Policy, raw: &RawText, cap: usize) -> Safe {
        match policy {
            Policy::Pair => Safe(raw.pick().to_string()),
            Policy::Market => Safe(sanitize(raw.pick(), cap)),
        }
    }

    /// Parsed JSON under a policy: the pair arm reproduces the historical
    /// bytes verbatim; the market arm uses the checked renderer and, for
    /// legacy callsites that need a `Safe`, emits the fixed parseable
    /// error JSON when the complete document would exceed `cap`.
    pub fn render_parsed(policy: Policy, v: &Value, cap: usize) -> Safe {
        match policy {
            Policy::Pair => Safe(v.to_string()),
            Policy::Market => Self::render_market_json(v, cap)
                .unwrap_or_else(|_| Safe::literal(MARKET_RENDER_OVERSIZE_JSON)),
        }
    }

    /// Checked market renderer for parsed values: emits the compact
    /// serde_json serialization with `|`, backtick, and all control characters
    /// represented by JSON escapes, so the
    /// result is always parseable JSON carrying no raw control or ANSI
    /// bytes and decoding to the same value. An over-cap document is
    /// rejected whole with a fixed input-free error, never truncated.
    pub fn render_market_json(v: &Value, cap: usize) -> anyhow::Result<Safe> {
        let mut out = String::new();
        for ch in v.to_string().chars() {
            match ch {
                '|' => out.push_str("\\u007c"),
                '`' => out.push_str("\\u0060"),
                c if c.is_control() => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        anyhow::ensure!(
            out.len() <= cap,
            "{}",
            Safe::literal(MARKET_RENDER_OVERSIZE)
        );
        Ok(Safe(out))
    }

    /// Form (f) under a policy: the pair arm reproduces the historical
    /// summary verbatim; the market arm never reflects raw stderr. An
    /// unclassified failure maps to the fixed input-free sentence, and
    /// contract/authentication summaries carry only validated code names.
    pub fn error_summary(policy: Policy, kind: &ErrorKind) -> Safe {
        match policy {
            Policy::Pair => Safe(error_summary(kind)),
            Policy::Market => match kind {
                ErrorKind::Other(_) => Safe::literal(MARKET_UNCLASSIFIED_FAILURE),
                _ => Safe(sanitize(&error_summary(kind), MARKET_TEXT_CAP)),
            },
        }
    }

    /// Form (f), room-to-show variant, under a policy: the pair arm is
    /// byte-identical to the legacy explainer; the market arm keeps the
    /// static contract-code help and maps every unclassified failure to
    /// the fixed input-free sentence, never raw stderr.
    pub fn error_explained(policy: Policy, kind: &ErrorKind) -> Safe {
        match policy {
            Policy::Pair => Safe(error_explained(kind)),
            Policy::Market => match kind {
                ErrorKind::Other(_) => Safe::literal(MARKET_UNCLASSIFIED_FAILURE),
                _ => Safe(sanitize(&error_explained(kind), MARKET_TEXT_CAP)),
            },
        }
    }

    /// Forms (d)/(i) under a policy: pair arm Debug-forwards, market arm
    /// `error_summary`-sanitized.
    pub fn error_debug(policy: Policy, kind: &ErrorKind) -> Safe {
        match policy {
            Policy::Pair => Safe(format!("{kind:?}")),
            Policy::Market => Self::error_summary(policy, kind),
        }
    }

    /// Error-chain text under a policy: pair arm the verbatim `{e:#}`
    /// composition, market arm sanitized.
    pub fn error_chain(policy: Policy, e: &anyhow::Error) -> Safe {
        match policy {
            Policy::Pair => Safe(format!("{e:#}")),
            Policy::Market => Safe(sanitize(&e.to_string(), MARKET_TEXT_CAP)),
        }
    }

    /// Form (l) component: first line containing "error", trimmed.
    pub fn first_error_line(policy: Policy, raw: &RawText) -> Safe {
        let line = raw
            .pick()
            .lines()
            .find(|l| l.contains("error"))
            .unwrap_or("")
            .trim();
        match policy {
            Policy::Pair => Safe(line.to_string()),
            Policy::Market => Safe(sanitize(line, MARKET_TEXT_CAP)),
        }
    }

    /// Terminal-only text projection: retain printable text and, when requested,
    /// paragraph breaks; expose other controls as escapes.
    /// Reject an over-cap display whole rather than hiding a suffix.
    pub fn render_terminal(text: &str, cap: usize, multiline: bool) -> Safe {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(text.len().min(cap));
        for ch in text.chars() {
            if ch.is_control() && !(multiline && ch == '\n') {
                write!(&mut out, "\\u{:04x}", ch as u32)
                    .expect("writing to a String is infallible");
            } else {
                out.push(ch);
            }
            if out.len() > cap {
                return Safe::literal("terminal text exceeds the display cap");
            }
        }
        Safe(out)
    }

    /// The six choke-point context messages (form (m)) and the item-7
    /// rejection sites; see `PairCtx`.
    pub(crate) fn pair_ctx(policy: Policy, ctx: PairCtx<'_>) -> Safe {
        match (policy, ctx) {
            (Policy::Pair, PairCtx::VersionTooOld { stdout, min }) => Safe(format!(
                "stellar CLI {} is too old for current testnet; need >= {}",
                stdout.pick(),
                min
            )),
            (Policy::Market, PairCtx::VersionTooOld { min, .. }) => Safe(format!(
                "stellar CLI is too old for current testnet; need >= {min}"
            )),
            (Policy::Pair, PairCtx::SubmissionUnresolved(stderr)) => {
                Safe(format!("submission outcome unresolved: {}", stderr.pick()))
            }
            (Policy::Market, PairCtx::SubmissionUnresolved(_)) => {
                Safe::literal("submission outcome unresolved")
            }
            (
                Policy::Pair,
                PairCtx::ViewFailed {
                    f,
                    id,
                    error,
                    stderr,
                },
            ) => Safe(format!(
                "view {f} on {id} failed: {}\n{}",
                opt_error_debug(policy, error),
                stderr.pick()
            )),
            (Policy::Market, PairCtx::ViewFailed { error, .. }) => {
                Safe(format!("view failed: {}", opt_error_debug(policy, error)))
            }
            (Policy::Pair, PairCtx::ReadFailed(stderr)) => {
                Safe(format!("contract read failed: {}", stderr.pick()))
            }
            (Policy::Market, PairCtx::ReadFailed(_)) => Safe::literal("contract read failed"),
            (Policy::Pair, PairCtx::UnparseableVersion(line)) => {
                Safe(format!("unparseable stellar version: {line}"))
            }
            (Policy::Market, PairCtx::UnparseableVersion(_)) => {
                Safe::literal("unparseable stellar version")
            }
            (Policy::Pair, PairCtx::UnparseableReadRow(line)) => {
                Safe(format!("unparseable read row: {line}"))
            }
            (Policy::Market, PairCtx::UnparseableReadRow(_)) => {
                Safe::literal("unparseable read row")
            }
        }
    }

    /// Form (a): the Tap argv join (up to `--rpc-url`) plus the 2000-char
    /// raw-stderr head, pinned to `Ev::Cli { argv, stderr, transient }`.
    /// Composed inside `stellar.rs` from the module-private stream fields.
    pub(crate) fn pair_tap(out: &CmdOutput, args: &[String]) -> (Safe, Safe) {
        let argv = args
            .iter()
            .take_while(|a| *a != "--rpc-url")
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let head = out.stderr.pick().chars().take(2000).collect();
        (Safe(argv), Safe(head))
    }

    /// Form (b): `run {bin} {argv}`, including the bin path.
    pub(crate) fn pair_run(bin: &Path, args: &[String]) -> Safe {
        Safe(format!("run {} {}", bin.display(), args.join(" ")))
    }

    /// Form (c): full argv join plus raw stdout/stderr, composed inside
    /// `stellar.rs` from the module-private stream fields.
    pub(crate) fn pair_failed(args: &[String], out: &CmdOutput) -> Safe {
        Safe(format!(
            "stellar {} failed\nstdout: {}\nstderr: {}",
            args.join(" "),
            out.stdout.pick(),
            out.stderr.pick()
        ))
    }

    /// Form (j): `must_ok`'s `{what} failed: {error:?}\n{stderr}`, composed
    /// inside `stellar.rs` from the invocation's retained fields.
    pub(crate) fn pair_must_ok(inv: &Invocation, what: &str) -> Safe {
        Safe(format!(
            "{what} failed: {}\n{}",
            opt_error_debug(Policy::Pair, inv.error.as_ref()),
            inv.stderr.pick()
        ))
    }
}

/// Errors that are the network's fault rather than the contract's. Policy-
/// independent classification over retained raw bytes; returns no bytes.
pub fn is_transient(out: &CmdOutput) -> bool {
    let e = out.stderr.pick().to_ascii_lowercase();
    [
        "bad gateway",
        "gateway time",
        "too many requests",
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "error sending request",
        "tcp connect error",
        "dns error",
        "rpc transport",
        "eof while parsing",
        "resourcelimitexceeded",
    ]
    .iter()
    .any(|needle| e.contains(needle))
        // Status codes match only as standalone tokens: a contract error such as
        // `Error(Contract, #502)` must not read as a gateway hiccup.
        || e.split_whitespace().any(|t| matches!(t, "429" | "502" | "503" | "504"))
}

/// Fixed, input-free failure sentences for the bounded history and
/// receipt seams. Classification reads the retained bytes; the returned
/// text is always one of these constants, never a slice or copy of what
/// the CLI or server said.
pub const HISTORY_EXPIRED: &str = "history unavailable: requested start ledger may predate the server-side retention window (the floor is server-set and not reported by stellar events; query stellar ledger fetch for the current oldest ledger)";
pub const HISTORY_UNREACHABLE: &str =
    "history unavailable: the RPC server could not be reached for this event fetch";
pub const HISTORY_FAILED: &str =
    "history unavailable: the event fetch failed for an unclassified reason";
pub const HISTORY_MALFORMED: &str =
    "history unavailable: the event page failed validation and was rejected";
pub const RECEIPT_INCONCLUSIVE: &str = "receipt inconclusive: the transaction record could not be retrieved; absence and outcome are not proven by getTransaction";
pub const HISTORY_PAGE_OVERSIZE: &str =
    "history unavailable: the event page exceeded the market size cap";
pub const HISTORY_LINE_UNPARSEABLE: &str =
    "history unavailable: the event page carried an unparseable line";
pub const RECEIPT_UNREACHABLE: &str =
    "receipt unavailable: the RPC server could not be reached for this transaction event fetch";
pub const RECEIPT_FAILED: &str =
    "receipt unavailable: the transaction event fetch failed for an unclassified reason";
pub const RECEIPT_MALFORMED: &str =
    "receipt unavailable: the transaction events response failed validation and was rejected";
pub const TX_READ_MALFORMED: &str =
    "transaction read unavailable: the transaction response failed validation and was rejected";
/// Fixed, input-free rejection for a successful market invoke whose
/// stdout is non-empty but not valid JSON: absence is only ever reported
/// from truly empty void output, never from erased parse errors.
pub const MARKET_MALFORMED_RESPONSE: &str = "malformed contract response";
/// Fixed, input-free summary for unclassified market failures: raw stderr,
/// argv values, and RPC URLs stay in retained custody bytes and never
/// reach a market-rendered cause.
pub const MARKET_UNCLASSIFIED_FAILURE: &str = "the contract call failed for an unclassified reason";
/// Fixed, input-free refusal when a complete market render would exceed
/// the caller's cap: the document is rejected whole, never truncated.
pub const MARKET_RENDER_OVERSIZE: &str = "market response exceeds the render size cap";
/// The parseable fixed error JSON legacy market render callsites emit
/// instead of a truncated or invalid document.
const MARKET_RENDER_OVERSIZE_JSON: &str =
    "{\"error\":\"market response exceeds the render size cap\"}";
/// Fixed, input-free refusal for a continuation cursor that does not
/// carry the RPC event-ID grammar.
pub const MARKET_INVALID_CURSOR: &str = "invalid event cursor";

/// Classification only: whether the retained failure bytes match a
/// too-old-start pattern. Returns a bit, never bytes.
fn is_expired_retention(out: &CmdOutput) -> bool {
    let e = out.stderr.pick().to_ascii_lowercase();
    ["too_old", "tooldate", "startledger", "start ledger"]
        .iter()
        .any(|needle| e.contains(needle))
}

/// Classification only: whether the retained failure bytes match the
/// transaction-record-not-found shape. Returns a bit, never bytes.
fn is_record_not_found(out: &CmdOutput) -> bool {
    let e = out.stderr.pick().to_ascii_lowercase();
    ["not found", "notfound"]
        .iter()
        .any(|needle| e.contains(needle))
}

/// A transaction that may already have been submitted must not be replayed blindly:
/// a timeout after submission can mean the tx landed, so retrying would double-apply it.
/// `ResourceLimitExceeded` is the documented exception: the tx was included but had no
/// effect, because the CLI's simulated budget was undersized for the inclusion ledger.
fn retryable(args: &[String], out: &CmdOutput) -> bool {
    if !is_transient(out) {
        return false;
    }
    // Every tx-submission shape is mutating, not just `invoke`: `upload`, the factory
    // `deploy` and friendbot-funded `keys generate --fund` can all land on-chain, so an
    // ambiguous error on any of them must not be replayed either. `--send` marks the
    // simulate-only view shape, which is always safe to retry.
    if !mutating(args) {
        return true;
    }
    if matches!(
        parse_error(out.stderr.pick()),
        Some(ErrorKind::Other(code)) if code.payload() == "tx failed: ResourceLimitExceeded"
    ) {
        return true;
    }
    let e = terminal_error(out.stderr.pick())
        .unwrap_or(out.stderr.pick().trim())
        .lines()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // The replay law: once the envelope may have reached the network, a retry
    // builds a fresh transaction with a new sequence and duplicate detection
    // cannot save it — only failures that are provably pre-send (name
    // resolution, refused or failed connect: the request never left) are
    // replayable on a mutating call. A pre-send phrase outranks an ambiguous
    // one: reqwest wraps connect failures as "error sending request for
    // url (...): error trying to connect: ...", and that chain is unsent.
    const PRE_SEND: [&str; 4] = [
        "dns error",
        "connection refused",
        "tcp connect error",
        "error trying to connect",
    ];
    PRE_SEND.iter().any(|needle| e.contains(needle))
}

/// Every argv shape that can land a transaction on-chain. Only `--send no`
/// makes an invoke simulation-only; `--send yes` and the CLI default may send.
/// Shared by retry and custody so they cannot disagree about submission.
fn mutating(args: &[String]) -> bool {
    let has = |w: &str| args.iter().any(|a| a == w);
    let send = args
        .split(|arg| arg == "--")
        .next()
        .unwrap_or(args)
        .windows(2)
        .find(|pair| pair[0] == "--send")
        .map(|pair| pair[1].as_str());
    (has("invoke") && send != Some("no"))
        || has("upload")
        || has("deploy")
        // `tx new …` (create-account, change-trust, payment, set-options)
        // builds, signs and submits unless `--build-only` is passed, so
        // every shape of it is treated as a send: an ambiguous post-submit
        // failure is never replayed.
        || (has("tx") && has("new"))
        || (has("generate") && has("--fund"))
}

fn terminal_error(stderr: &str) -> Option<&str> {
    stderr
        .rmatch_indices("error: ")
        .find_map(|(index, prefix)| {
            let line_prefix = stderr[..index].rsplit('\n').next()?;
            (!line_prefix.chars().any(char::is_alphanumeric))
                .then_some(&stderr[index + prefix.len()..])
        })
}

/// Shared live/replay custody rule. Diagnostic events are not submission
/// outcomes: an unclassified failure needs the CLI's terminal rejection header,
/// not an error-shaped substring somewhere in its event log.
pub(crate) fn definite_outcome(inv: &Invocation) -> bool {
    if inv.ok {
        return true;
    }
    if inv.stderr.pick().is_empty() || parse_error(inv.stderr.pick()) != inv.error {
        return false;
    }
    match &inv.error {
        Some(ErrorKind::Contract(_)) | Some(ErrorKind::Auth) => true,
        Some(ErrorKind::Other(error)) => terminal_error(inv.stderr.pick()).is_some_and(|message| {
            message.starts_with("transaction simulation failed: HostError: Error(")
                || ((message.starts_with("transaction submission failed:")
                    || message.starts_with("transaction simulation failed:"))
                    && matches!(
                        error.payload(),
                        "tx failed: Malformed"
                            | "tx failed: Trapped"
                            | "tx failed: ResourceLimitExceeded"
                            | "tx failed: EntryArchived"
                            | "tx failed: InsufficientRefundableFee"
                    ))
        }),
        None => false,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CmdOutput {
    stdout: RawText,
    stderr: RawText,
    ok: bool,
}

impl CmdOutput {
    /// The frozen construction point: raw stream text enters only here.
    pub(crate) fn new(stdout: RawText, stderr: RawText, ok: bool) -> Self {
        Self { stdout, stderr, ok }
    }

    /// The frozen success accessor for call sites outside this module.
    pub(crate) fn ok(&self) -> bool {
        self.ok
    }

    /// The frozen transient-classification accessor `Ev::Cli` producers call.
    pub(crate) fn transient(&self) -> bool {
        is_transient(self)
    }
}

pub trait Executor {
    fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput>;

    /// `run`, refused once `start_by` (real time) has passed. Executors that
    /// set up before launching override this to recheck at the launch itself.
    fn run_by(
        &self,
        args: &[String],
        start_by: std::time::SystemTime,
    ) -> anyhow::Result<CmdOutput> {
        ensure_started_by(start_by)?;
        self.run(args)
    }
}

fn ensure_started_by(start_by: std::time::SystemTime) -> anyhow::Result<()> {
    anyhow::ensure!(
        std::time::SystemTime::now() <= start_by,
        "{}",
        Safe::literal("confirmed market send refused: its start deadline after journal reservation has passed")
    );
    Ok(())
}

/// Runs the real binary. Clears `STELLAR_CONTRACT_ID` (exported in this workspace's
/// shell, it hijacks `--id`) and pins `XDG_CONFIG_HOME` so the CLI never touches a
/// global config dir.
pub struct RealExecutor {
    pub bin: PathBuf,
    pub xdg: PathBuf,
    /// Working directory: the CLI auto-loads `.env` from cwd, so never the repo root.
    pub cwd: PathBuf,
    /// Process-capture policy. Explicit selection prevents a market call site
    /// from silently inheriting the unbounded pair path.
    pub capture: Capture,
    /// Child-environment variables carrying the market RPC endpoint and its
    /// headers (`STELLAR_RPC_URL`, `STELLAR_RPC_HEADERS`). A mainnet URL may
    /// embed a provider key; argv is world-readable (`/proc/*/cmdline`,
    /// `ps`, audit logs), the child environment is uid-private, so market
    /// transport settings reach the CLI only here — never as argv.
    pub rpc_env: Vec<(String, String)>,
}

impl Executor for RealExecutor {
    fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput> {
        self.launch(args, None)
    }

    fn run_by(
        &self,
        args: &[String],
        start_by: std::time::SystemTime,
    ) -> anyhow::Result<CmdOutput> {
        self.launch(args, Some(start_by))
    }
}

/// The child-environment transport for a market net's endpoint: the CLI's
/// own `STELLAR_RPC_URL` / `STELLAR_RPC_HEADERS` variables, replacing the
/// `--rpc-url` / `--rpc-header` argv form whose value would be world-
/// readable. Headers are the documented comma-separated `Name: value` list.
/// An empty endpoint contributes nothing: a pair net's transport stays argv.
#[must_use]
pub fn rpc_env_for(net: &Net) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if !net.rpc_url.is_empty() {
        env.push(("STELLAR_RPC_URL".to_string(), net.rpc_url.clone()));
    }
    if !net.headers.is_empty() {
        let joined = net
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect::<Vec<_>>()
            .join(",");
        env.push(("STELLAR_RPC_HEADERS".to_string(), joined));
    }
    env
}

impl RealExecutor {
    /// The deadline is rechecked after every filesystem step, immediately
    /// before the child launches, so a stalled setup cannot start it late.
    fn launch(
        &self,
        args: &[String],
        start_by: Option<std::time::SystemTime>,
    ) -> anyhow::Result<CmdOutput> {
        std::fs::create_dir_all(self.xdg.join("stellar"))?;
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .env_remove("STELLAR_CONTRACT_ID")
            .env("XDG_CONFIG_HOME", &self.xdg)
            .env(
                "HOME",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
            )
            .current_dir(&self.cwd);
        // The market endpoint never comes from the ambient environment:
        // whatever this executor was given is what the child sees, and an
        // inherited value can neither add a key nor replace a pinned one.
        for name in ["STELLAR_RPC_URL", "STELLAR_RPC_HEADERS"] {
            cmd.env_remove(name);
        }
        for (name, value) in &self.rpc_env {
            cmd.env(name, value);
        }
        match self.capture {
            Capture::Pair => {
                if let Some(start_by) = start_by {
                    ensure_started_by(start_by)?;
                }
                let output = cmd
                    .output()
                    .with_context(|| Safe::pair_run(&self.bin, args))?;
                Ok(CmdOutput::new(
                    RawText(Some(
                        String::from_utf8_lossy(&output.stdout).trim().to_string(),
                    )),
                    RawText(Some(
                        String::from_utf8_lossy(&output.stderr).trim().to_string(),
                    )),
                    output.status.success(),
                ))
            }
            Capture::Market {
                stdout_cap,
                stderr_cap,
                wall: Some(wall),
            } => {
                // SEC001 process boundary: every market child runs from the
                // controlled dotenv-free root with an absolute executable
                // resolved before changing cwd.
                anyhow::ensure!(
                    self.cwd == std::path::Path::new("/")
                        && std::fs::symlink_metadata(std::path::Path::new("/.env")).is_err()
                        && market_launchable_bin(&self.bin),
                    "{}",
                    Safe::literal(MARKET_EXECUTOR_UNSAFE)
                );
                market_capture(cmd, stdout_cap, stderr_cap, wall, start_by)
            }
            Capture::Market { .. } => Err(anyhow!("{}", Safe::literal(MARKET_CAPTURE_FAILED))),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Net {
    pub rpc_url: String,
    pub passphrase: String,
    pub config_dir: PathBuf,
    /// Optional RPC request headers. Market execution contexts plumb these
    /// onto every market CLI call through the child environment; pair
    /// contexts never populate them, and the skip-serializing default keeps
    /// historical pair manifests and evidence byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

/// A market `Net`'s `rpc_url` may embed a provider key and `headers` carry
/// it outright: `Debug` redacts both, like `CliNetwork`'s hand-written impl.
impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Net")
            .field("rpc_url", &"<redacted>")
            .field("passphrase", &self.passphrase)
            .field("config_dir", &self.config_dir)
            .field("headers", &"<redacted>")
            .finish()
    }
}

impl Default for Net {
    fn default() -> Self {
        Self {
            rpc_url: String::new(),
            passphrase: String::new(),
            config_dir: PathBuf::new(),
            headers: Vec::new(),
        }
    }
}

pub struct Stellar<'a, E: Executor> {
    pub net: &'a Net,
    exec: &'a E,
    /// Injected so tests do not actually sleep between retries.
    pub sleep: fn(std::time::Duration),
    policy: Policy,
    /// The frozen market signer `(alias, pinned G-address)`, set only via
    /// [`Stellar::with_market_signer`]. Every market-policy mutation
    /// (contract upload/deploy/invoke, including confirmed sends) must
    /// originate from this pinned G address and sign by naming only this
    /// alias; the pin is frozen from externally persisted state and a fresh
    /// CLI resolution may only ever refuse it, never replace it (SEC003).
    market_signer: Option<(String, AccountId)>,
    /// The network a market run is frozen to, set only via
    /// [`Stellar::with_market_network`]. A bound context may prove only
    /// that network; an unbound one proves its own `net` passphrase.
    market_network: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub contract: String,
    pub topics: Vec<Value>,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ErrorKind {
    Contract(u32),
    Auth,
    Other(Safe),
}

impl ErrorKind {
    /// Classification-only payload read; returns no renderable text.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            ErrorKind::Other(o) => o.payload(),
            ErrorKind::Contract(_) => "contract error",
            ErrorKind::Auth => "authorization rejected",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invocation {
    pub ok: bool,
    pub value: Option<Value>,
    pub error: Option<ErrorKind>,
    pub events: Vec<Event>,
    pub stderr: RawText,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawEntry {
    pub key: String,
    pub val: String,
    pub last_modified: u32,
    pub live_until: Option<u32>,
}

impl<'a, E: Executor> Stellar<'a, E> {
    /// The mode-selected execution constructor: `policy` is fixed here and
    /// never reassigned, so no market context can recover pair-verbatim raw
    /// bytes through the render family.
    pub fn new(net: &'a Net, exec: &'a E, policy: Policy) -> Self {
        Self {
            net,
            exec,
            sleep: std::thread::sleep,
            policy,
            market_signer: None,
            market_network: None,
        }
    }

    /// Attach one frozen market signer `(alias, pinned G-address)` to a
    /// copy of this context. Pure: no CLI I/O and no resolution happens
    /// here — enforcement lives exclusively at the market mutation seams
    /// below, so identity provisioning and read paths are unaffected. The
    /// pin never replaces the frozen binding: fresh resolution may only
    /// refuse it (SEC003).
    pub fn with_market_signer(self, alias: &str, pinned: &AccountId) -> Self {
        Stellar {
            net: self.net,
            exec: self.exec,
            sleep: self.sleep,
            policy: self.policy,
            market_signer: Some((alias.to_string(), pinned.clone())),
            market_network: self.market_network,
        }
    }

    /// Bind a copy of this context to the network a market run is frozen
    /// to. Read surfaces take it from the run's own frozen manifest, so a
    /// mainnet run is read on mainnet and a testnet run on testnet; the
    /// caller's argv never decides. Pure: no CLI I/O happens here.
    #[must_use]
    pub fn with_market_network(self, network: &'a str) -> Self {
        Stellar {
            market_network: Some(network),
            ..self
        }
    }

    /// The single network every read of this context may prove: the run's
    /// frozen network when one is bound, the context's own passphrase
    /// otherwise.
    #[must_use]
    pub fn market_network(&self) -> &'a str {
        self.market_network.unwrap_or(self.net.passphrase.as_str())
    }


    /// The market-policy signing law for every chain mutation (SEC003):
    /// a frozen pin must be configured; the caller's requested source may
    /// only be that pin (alias or its G-address) — an ad-hoc substitution
    /// refuses; the configured alias is re-resolved through the CLI key
    /// storage immediately before the mutation, and a vanished or rebound
    /// alias refuses before any send. Returns the forced CLI arguments:
    /// the transaction source is always the pinned G-address and signing
    /// names only the alias — never a seed. Rejections are fixed and
    /// input-free.
    fn market_pinned_signing(&self, source: &str) -> anyhow::Result<Vec<String>> {
        let (alias, pinned) = self.market_signer.as_ref().ok_or_else(|| {
            anyhow!(
                "{}",
                Safe::literal("market mutation refused: no pinned signer is configured")
            )
        })?;
        anyhow::ensure!(
            source == alias.as_str() || source == pinned.as_str(),
            "{}",
            Safe::literal("market mutation refused: the requested source is not the pinned signer")
        );
        let live = self.keys_public_key(alias)?.ok_or_else(|| {
            anyhow!(
                "{}",
                Safe::literal(
                    "market mutation refused: the pinned signer alias no longer resolves"
                )
            )
        })?;
        anyhow::ensure!(
            live == *pinned,
            "{}",
            Safe::literal(
                "market mutation refused: the pinned signer alias resolved to a different identity"
            )
        );
        Ok(vec![
            "--source".to_string(),
            pinned.as_str().to_string(),
            "--sign-with-key".to_string(),
            alias.clone(),
        ])
    }

    fn net_args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        // The market endpoint and its headers never reach argv: a mainnet
        // URL can embed a provider key, and every market child's argv is
        // world-readable. They reach the CLI through the child environment
        // instead (`RealExecutor::rpc_env`), which the CLI consumes via its
        // documented `STELLAR_RPC_URL` / `STELLAR_RPC_HEADERS` variables.
        // Pair contexts keep their historical argv transport unchanged.
        if self.policy != Policy::Market {
            args.push("--rpc-url".into());
            args.push(self.net.rpc_url.clone());
        }
        args.push("--network-passphrase".into());
        args.push(self.net.passphrase.clone());
        args.push("--config-dir".into());
        args.push(self.net.config_dir.display().to_string());
        args
    }

    /// Read-only public-chain probe for reconciliation: runs an arbitrary
    /// CLI command with the network args and reports only exit success.
    /// Never used for mutating calls (mutations go through the journaled
    /// choke points).
    pub fn probe_ok(&self, args: &[&str]) -> bool {
        let mut argv = s(args);
        argv.extend(self.net_args());
        self.run(argv).map(|o| o.ok()).unwrap_or(false)
    }

    /// Single choke point for every CLI call, so transport retry lives here only.
    /// A transient RPC failure used to kill a 99-step run outright.
    fn run(&self, args: Vec<String>) -> anyhow::Result<CmdOutput> {
        for attempt in 0..RETRY_ATTEMPTS {
            // Market boundary: an executor start/IO failure carries the
            // pair-arm context (bin path, full argv, RPC URL) until it is
            // replaced here. Market callers only ever see the fixed
            // input-free sentence; the pair arm propagates unchanged.
            let out = match self.exec.run(&args) {
                Ok(out) => out,
                Err(_) if self.policy == Policy::Market => {
                    return Err(anyhow::Error::msg(Safe::literal("stellar command failed")));
                }
                Err(e) => return Err(e),
            };
            if out.ok || !retryable(&args, &out) || attempt + 1 == RETRY_ATTEMPTS {
                return Ok(out);
            }
            (self.sleep)(Duration::from_secs(retry_delay_secs(attempt)));
        }
        unreachable!("loop returns on the final attempt")
    }

    fn run_ok(&self, args: Vec<String>) -> anyhow::Result<CmdOutput> {
        let out = self.run(args.clone())?;
        if !out.ok() {
            let failure = Safe::run_failure(self.policy, &args, &out);
            return if self.policy == Policy::Market {
                Err(anyhow::Error::msg(failure))
            } else {
                Err(anyhow!("{}", failure))
            };
        }
        Ok(out)
    }

    pub fn assert_version(&self) -> anyhow::Result<u32> {
        let out = self.run_ok(vec!["--version".into()])?;
        let major = parse_major(self.policy, out.stdout.pick())?;
        anyhow::ensure!(
            major >= MIN_CLI_MAJOR,
            "{}",
            Safe::pair_ctx(
                self.policy,
                PairCtx::VersionTooOld {
                    stdout: &out.stdout,
                    min: MIN_CLI_MAJOR
                }
            )
        );
        Ok(major)
    }

    /// Market-only CLI version gate: accepted only on a `Policy::Market`
    /// context bound to one of the two known networks (the exact testnet or
    /// the exact mainnet passphrase), and only for CLI stable versions
    /// >=26.1.0. Every other market executor
    /// > command must sit behind it, and its own sole CLI call is the local
    /// > `--version` probe: a rejected CLI never reaches the network. The
    /// > pair gate (`assert_version`, >=23) is untouched, and every rejection
    /// > here is input-free: no raw CLI bytes are echoed.
    pub fn assert_market_cli_version(&self) -> anyhow::Result<u32> {
        if self.policy != Policy::Market {
            return Err(anyhow::Error::msg(Safe::literal(
                "market CLI version gate requires market policy",
            )));
        }
        if self.net.passphrase != MARKET_TESTNET_PASSPHRASE
            && self.net.passphrase != MARKET_MAINNET_PASSPHRASE
        {
            return Err(anyhow::Error::msg(Safe::literal(
                "market CLI version gate requires a known network",
            )));
        }
        let out = self.run_ok(vec!["--version".into()])?;
        let version = out.stdout.pick().split_whitespace().nth(1).and_then(|v| {
            let mut parts = v.split('.');
            let mut component = || {
                let part = parts.next()?;
                if part.is_empty()
                    || !part.bytes().all(|b| b.is_ascii_digit())
                    || (part.len() > 1 && part.starts_with('0'))
                {
                    return None;
                }
                part.parse::<u32>().ok()
            };
            let version = (component()?, component()?, component()?);
            parts.next().is_none().then_some(version)
        });
        match version {
            Some(version) if version >= MARKET_MIN_CLI_VERSION => Ok(version.0),
            _ => Err(anyhow::Error::msg(Safe::literal(
                "market requires stable stellar CLI >=26.1.0",
            ))),
        }
    }

    /// Bind market execution to the RPC's own network identity, not its
    /// URL, host name, or the passphrase supplied in argv. `expected` must
    /// be one of the two known market passphrases and must be the very
    /// passphrase this context is bound to; the assertion is refused locally
    /// otherwise. After the local market policy and CLI version gate, one
    /// read-only `network info --output json` probe runs with the explicit
    /// network arguments and is checked without retries. A failed,
    /// malformed, unavailable, missing, or wrong-network response is refused
    /// with one fixed input-free sentence; raw CLI bytes never leave this
    /// seam.
    pub fn assert_rpc_network(&self, expected: &str) -> anyhow::Result<()> {
        // A context bound to a run's frozen network proves that network and
        // no other; an unbound context proves the one it was built with.
        let bound = self.market_network.unwrap_or(self.net.passphrase.as_str());
        let known = expected == MARKET_TESTNET_PASSPHRASE
            || expected == MARKET_MAINNET_PASSPHRASE;
        anyhow::ensure!(
            known && bound == expected,
            "{}",
            Safe::literal("market network assertion does not name the bound known network")
        );
        self.assert_market_cli_version()?;
        let refused = || {
            let text = if expected == MARKET_MAINNET_PASSPHRASE {
                "market RPC did not prove the exact mainnet passphrase"
            } else {
                "market RPC did not prove the exact testnet passphrase"
            };
            anyhow::Error::msg(Safe::literal(text))
        };
        let mut args = s(&["network", "info", "--output", "json"]);
        args.extend(self.net_args());
        let out = self.exec.run(&args).map_err(|_| refused())?;
        if !out.ok() || out.stdout.pick().len() > MARKET_CLI_SPEC_CAP {
            return Err(refused());
        }
        let Ok(info) = serde_json::from_str::<Value>(out.stdout.pick()) else {
            return Err(refused());
        };
        if info.get("passphrase").and_then(Value::as_str) != Some(expected) {
            return Err(refused());
        }
        Ok(())
    }

    /// The testnet binding of [`Stellar::assert_rpc_network`], kept as the
    /// historical entry point: identical behaviour, pinned to the exact
    /// testnet passphrase.
    pub fn assert_rpc_testnet(&self) -> anyhow::Result<()> {
        self.assert_rpc_network(MARKET_TESTNET_PASSPHRASE)
    }

    /// Reads the live ABI of `id` through the pinned native CLI
    /// (`contract info interface --contract-id <id> --output json`) and
    /// returns the parsed entry array the CLI emitted, so no Rust-side XDR
    /// decoding exists. The market version gate and the exact-testnet
    /// binding are rechecked on every use. Malformed, non-entry, or
    /// over-cap output is rejected here with input-free errors: raw CLI
    /// stdout never leaves this module, and no chain state is touched.
    pub fn contract_interface(&self, id: &ContractId) -> anyhow::Result<Value> {
        self.assert_market_cli_version()?;
        let mut args = s(&[
            "contract",
            "info",
            "interface",
            "--contract-id",
            id.as_str(),
            "--output",
            "json",
        ]);
        args.extend(self.net_args());
        let out = self.run_ok(args)?;
        anyhow::ensure!(
            out.stdout.pick().len() <= MARKET_CLI_SPEC_CAP,
            "{}",
            Safe::literal("contract interface spec exceeds the market cap")
        );
        let spec: Value = serde_json::from_str(out.stdout.pick()).map_err(|_| {
            anyhow!(
                "{}",
                Safe::literal("contract interface returned malformed spec")
            )
        })?;
        anyhow::ensure!(
            spec.is_array()
                && spec
                    .as_array()
                    .is_some_and(|entries| entries.iter().all(Value::is_object)),
            "{}",
            Safe::literal("contract interface spec is not an entry array")
        );
        Ok(spec)
    }

    /// Read-only, single-attempt bounded event page for a
    /// checksum-validated contract target via exactly one
    /// `stellar events --id <target> --start-ledger <n> --count <k>
    /// --output json` fetch. CLI 26 performs one `getEvents` RPC and
    /// returns immediately; zero events is success with an empty page.
    /// This seam makes no retry attempts and no sleeps, never signs, and
    /// never submits. `count` is bounded to 1..=MARKET_EVENTS_MAX_COUNT
    /// before anything runs; the CLI has no end-ledger flag and the
    /// retention floor is server-set and never printed by `stellar
    /// events`, so both are reported as unknown rather than implied by
    /// the requested window. Every NDJSON line is parsed and rechecked:
    /// the contract ID must equal the requested target, the ledger must
    /// be a ledger sequence in `start_ledger..=u32::MAX`, the cursor ID
    /// must carry the RPC event-ID grammar exactly, and either the
    /// decoded (`contract_id`/`event_name`) or raw-fallback
    /// (`contractId`/`topic`/`value`) CLI shape is accepted,
    /// never a mix and never anything else. The page is rebuilt from the
    /// validated fields only, so no parsed CLI payload escapes this
    /// seam, and pages are bounded by `count` and a hard 1 MiB stdout
    /// cap. A failed fetch (retention too old, unreachable server,
    /// configuration) becomes an explicit history-unavailable failure
    /// whose message is one fixed, input-free sentence selected by
    /// classification only and never copied from CLI bytes.
    pub fn read_events(
        &self,
        id: &ContractId,
        start_ledger: u32,
        count: u32,
    ) -> anyhow::Result<Vec<Value>> {
        anyhow::ensure!(
            (1..=MARKET_EVENTS_MAX_COUNT).contains(&count),
            "{}",
            Safe::literal("event page count must be 1..=100")
        );
        self.assert_market_cli_version()?;
        let mut args = s(&[
            "events",
            "--id",
            id.as_str(),
            "--start-ledger",
            &start_ledger.to_string(),
            "--count",
            &count.to_string(),
            "--output",
            "json",
        ]);
        args.extend(self.net_args());
        // An executor start/IO failure would otherwise carry the pair-arm
        // context (bin path, full argv with the RPC URL) into a market
        // error; classify it as the fixed history-unavailable sentence.
        let out = self
            .exec
            .run(&args)
            .map_err(|_| anyhow!("{}", Safe::literal(HISTORY_FAILED)))?;
        self.decode_event_page(&out, id, start_ledger, count, None)
    }

    /// Cursor continuation of `read_events`: one validated handle, one
    /// bounded `stellar events --id <target> --cursor <cursor> --count
    /// <count> --output json` page under the pinned testnet, one RPC.
    /// The cursor is checked against the RPC event-ID grammar before any
    /// executor call, so a hostile handle is refused locally and never
    /// echoed. Every line goes through the same validation as
    /// `read_events`, plus a strict-progress rule: an event whose ID is
    /// not greater than the supplied cursor fails the whole page with
    /// the fixed malformed-history sentence, so continuation cannot
    /// replay or skip. Same-ledger continuation is supported because the
    /// ID carries the intra-ledger index and progress is compared on the
    /// validated `(toid, index)` pair. Failure classification, size cap,
    /// and the page bound are identical to `read_events`.
    pub fn read_events_after(
        &self,
        id: &ContractId,
        cursor: &str,
        count: u32,
    ) -> anyhow::Result<Vec<Value>> {
        anyhow::ensure!(
            (1..=MARKET_EVENTS_MAX_COUNT).contains(&count),
            "{}",
            Safe::literal("event page count must be 1..=100")
        );
        validate_event_cursor(cursor)?;
        self.assert_market_cli_version()?;
        let mut args = s(&[
            "events",
            "--id",
            id.as_str(),
            "--cursor",
            cursor,
            "--count",
            &count.to_string(),
            "--output",
            "json",
        ]);
        args.extend(self.net_args());
        let out = self
            .exec
            .run(&args)
            .map_err(|_| anyhow!("{}", Safe::literal(HISTORY_FAILED)))?;
        self.decode_event_page(&out, id, 0, count, Some(cursor))
    }

    /// The single event-page decoder behind both seams: failed fetches
    /// become fixed input-free history-unavailable sentences selected by
    /// classification only; pages are size-capped, line-parsed, and
    /// rebuilt from validated fields only; when `after_cursor` is given,
    /// every page event must carry an ID strictly after it. The page may
    /// never exceed `count`.
    fn decode_event_page(
        &self,
        out: &CmdOutput,
        id: &ContractId,
        start_ledger: u32,
        count: u32,
        after_cursor: Option<&str>,
    ) -> anyhow::Result<Vec<Value>> {
        if !out.ok() {
            return Err(anyhow::anyhow!(
                "{}",
                Safe::literal(if is_transient(out) {
                    HISTORY_UNREACHABLE
                } else if is_expired_retention(out) {
                    HISTORY_EXPIRED
                } else {
                    HISTORY_FAILED
                })
            ));
        }
        anyhow::ensure!(
            out.stdout.pick().len() <= MARKET_EVENTS_PAGE_CAP,
            "{}",
            Safe::literal(HISTORY_PAGE_OVERSIZE)
        );
        let text = out.stdout.pick().trim();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let mut page = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: Value = serde_json::from_str(line)
                .map_err(|_| anyhow!("{}", Safe::literal(HISTORY_LINE_UNPARSEABLE)))?;
            let entry = validated_event_cursor(&event, id, start_ledger)?;
            if let Some(cursor) = after_cursor {
                let next = entry
                    .get("cursor")
                    .and_then(Value::as_str)
                    .expect("validated cursor");
                anyhow::ensure!(
                    cursor_is_after(next, cursor),
                    "{}",
                    Safe::literal(HISTORY_MALFORMED)
                );
            }
            page.push(entry);
        }
        anyhow::ensure!(
            (page.len() as u64) <= u64::from(count),
            "{}",
            Safe::literal(HISTORY_MALFORMED)
        );
        Ok(page)
    }

    /// Read-only, single-attempt transaction receipt read: exactly one
    /// `stellar tx fetch events --hash <hash> --output json` against the
    /// pinned testnet, never a send and never a retry loop. `hash` is a
    /// strict 64-character lowercase hex transaction hash parsed into a
    /// `TxHash` at the trust boundary; a malformed value never reaches
    /// the CLI and a parsed hash is never echoed into any error or
    /// rendered text. Success output is size-capped, single-JSON, and
    /// must carry exactly the three CLI26 event arrays
    /// (`contract_events`, `diagnostic_events`, `transaction_events`) of
    /// transaction events, each an array of objects. All three empty is
    /// returned as an explicit inconclusive status, not a receipt: under
    /// this network binding a NOT_FOUND response is indistinguishable
    /// from a transaction that genuinely carries no events, and the
    /// network proves nothing from a missing record. Other failures
    /// become explicit fixed-sentence receipt-unavailable errors built
    /// only from constants.
    pub fn read_tx_events(&self, hash: &TxHash) -> anyhow::Result<Value> {
        self.assert_market_cli_version()?;
        let mut args = s(&[
            "tx",
            "fetch",
            "events",
            "--hash",
            hash.as_str(),
            "--output",
            "json",
        ]);
        args.extend(self.net_args());
        // An executor start/IO failure would otherwise carry the pair-arm
        // context (bin path, full argv with the RPC URL) into a market
        // error; classify it as the fixed receipt-unavailable sentence.
        let out = self
            .exec
            .run(&args)
            .map_err(|_| anyhow!("{}", Safe::literal(RECEIPT_FAILED)))?;
        if !out.ok() {
            return Err(anyhow::anyhow!(
                "{}",
                Safe::literal(if is_transient(&out) {
                    RECEIPT_UNREACHABLE
                } else if is_record_not_found(&out) {
                    RECEIPT_INCONCLUSIVE
                } else {
                    RECEIPT_FAILED
                })
            ));
        }
        anyhow::ensure!(
            out.stdout.pick().len() <= MARKET_EVENTS_PAGE_CAP,
            "{}",
            Safe::literal(RECEIPT_MALFORMED)
        );
        let body: Value = serde_json::from_str(out.stdout.pick())
            .map_err(|_| anyhow!("{}", Safe::literal(RECEIPT_MALFORMED)))?;
        let arrays = [
            body.get("contract_events").and_then(Value::as_array),
            body.get("diagnostic_events").and_then(Value::as_array),
            body.get("transaction_events").and_then(Value::as_array),
        ];
        let wellformed = body.is_object()
            && arrays.iter().all(|a| a.is_some())
            && arrays[0].is_some_and(|contract_events| contract_events.iter().all(Value::is_array))
            && arrays[1..]
                .iter()
                .all(|events| events.is_some_and(|events| events.iter().all(Value::is_object)));
        anyhow::ensure!(wellformed, "{}", Safe::literal(RECEIPT_MALFORMED));
        let empty = arrays
            .iter()
            .all(|events| events.as_ref().is_some_and(|events| events.is_empty()));
        if empty {
            return Ok(serde_json::json!({
                "status": "inconclusive",
                "reason": "no transaction events returned: the record may be absent (NOT_FOUND), outside the server's retention, or genuinely carry no events; getTransaction proves nothing about outcome",
            }));
        }
        Ok(serde_json::json!({
            "status": "observed",
            "contract_events": body.get("contract_events"),
            "diagnostic_events": body.get("diagnostic_events"),
            "transaction_events": body.get("transaction_events"),
            "note": "events observed on the pinned testnet for the requested transaction hash; presence of events is not confirmation of any market outcome",
        }))
    }
    /// Read-only, single-attempt transaction envelope read: exactly one
    /// `stellar tx fetch --hash <hash> --output json`. A v1 envelope is
    /// strict-parsed (source account, sequence, well-formed operations);
    /// fee-bump envelopes, record absence, and any unexpected shape are
    /// refused with fixed input-free sentences; only validated fields
    /// are returned, never raw CLI bytes.
    pub fn read_tx_envelope(
        &self,
        hash: &TxHash,
        target: Option<&ContractId>,
    ) -> anyhow::Result<EnvelopedTx> {
        self.assert_market_cli_version()?;
        let mut args = s(&["tx", "fetch", "--hash", hash.as_str(), "--output", "json"]);
        args.extend(self.net_args());
        let out = self
            .exec
            .run(&args)
            .map_err(|_| anyhow!("{}", Safe::literal(TX_READ_MALFORMED)))?;
        anyhow::ensure!(
            out.stdout.pick().len() <= MARKET_EVENTS_PAGE_CAP,
            "{}",
            Safe::literal(TX_READ_MALFORMED)
        );
        let malformed = tx_read_malformed;
        anyhow::ensure!(out.ok(), "{}", Safe::literal(TX_READ_MALFORMED));
        let body: Value = serde_json::from_str(out.stdout.pick())
            .map_err(|_| anyhow!("{}", Safe::literal(TX_READ_MALFORMED)))?;
        anyhow::ensure!(
            body.is_object() && body.get("tx").is_some() && body.get("tx_fee_bump").is_none(),
            "{}",
            Safe::literal(TX_READ_MALFORMED)
        );
        // The v1 envelope is doubly wrapped: `tx` holds the envelope,
        // whose `tx` field holds the transaction.
        let tx = body
            .pointer("/tx/tx")
            .and_then(Value::as_object)
            .ok_or_else(malformed)?;
        let source_account = tx
            .get("source_account")
            .and_then(Value::as_str)
            .and_then(|value| AccountId::from_str(value).ok())
            .ok_or_else(malformed)?;
        let seq_num = tx
            .get("seq_num")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .ok_or_else(malformed)?;
        let operations = tx
            .get("operations")
            .and_then(Value::as_array)
            .ok_or_else(malformed)?;
        let mut invokes_target = false;
        let mut creates = Vec::new();
        for operation in operations {
            let body = operation.get("body").ok_or_else(malformed)?;
            // Unit arms serialize as bare strings; neither can invoke or
            // create anything.
            if body
                .as_str()
                .is_some_and(|arm| KNOWN_UNIT_OPERATION_VARIANTS.contains(&arm))
            {
                continue;
            }
            let (variant, value) = body
                .as_object()
                .and_then(|body| sole_variant(body, KNOWN_OPERATION_VARIANTS))
                .ok_or_else(malformed)?;
            // A classic operation invokes and creates no contract.
            if variant != "invoke_host_function" {
                continue;
            }
            let host_function = value
                .get("host_function")
                .and_then(Value::as_object)
                .ok_or_else(malformed)?;
            let (host_variant, host_body) =
                sole_variant(host_function, KNOWN_HOST_FUNCTION_VARIANTS).ok_or_else(malformed)?;
            match host_variant {
                "invoke_contract" => {
                    let contract_address = host_body
                        .get("contract_address")
                        .and_then(Value::as_str)
                        .and_then(|value| ContractId::from_str(value).ok())
                        .ok_or_else(malformed)?;
                    invokes_target |= target == Some(&contract_address);
                }
                // A creation names the code it installs. It never errors
                // here: a payload this reader cannot classify is recorded as
                // such, and the law that asked decides what that means.
                "create_contract" | "create_contract_v2" => {
                    creates.push(created_code(host_body.get("executable")));
                }
                // An upload installs bytes, never a contract.
                _ => {}
            }
        }
        Ok(EnvelopedTx {
            source_account,
            seq_num,
            invokes_target,
            creates,
        })
    }

    /// Read-only, single-attempt transaction metadata read: exactly one
    /// `stellar tx fetch meta --hash <hash> --output json`. Returns the
    /// (sequence, ledger close time) pairs recorded for account entries
    /// in `updated` changes; every unexpected field or shape is refused
    /// with a fixed input-free sentence.
    pub fn read_tx_meta(&self, hash: &TxHash) -> anyhow::Result<Vec<(AccountId, u64, u64)>> {
        self.assert_market_cli_version()?;
        let mut args = s(&[
            "tx",
            "fetch",
            "meta",
            "--hash",
            hash.as_str(),
            "--output",
            "json",
        ]);
        args.extend(self.net_args());
        let out = self
            .exec
            .run(&args)
            .map_err(|_| anyhow!("{}", Safe::literal(TX_READ_MALFORMED)))?;
        anyhow::ensure!(
            out.stdout.pick().len() <= MARKET_EVENTS_PAGE_CAP,
            "{}",
            Safe::literal(TX_READ_MALFORMED)
        );
        anyhow::ensure!(out.ok(), "{}", Safe::literal(TX_READ_MALFORMED));
        let body: Value = serde_json::from_str(out.stdout.pick())
            .map_err(|_| anyhow!("{}", Safe::literal(TX_READ_MALFORMED)))?;
        let v4 = body
            .get("v4")
            .and_then(Value::as_object)
            .ok_or_else(tx_read_malformed)?;
        let malformed = tx_read_malformed;
        let changes = v4
            .get("tx_changes_before")
            .and_then(Value::as_array)
            .ok_or_else(malformed)?;
        let mut account_sequences = Vec::new();
        for change in changes {
            // A LedgerEntryChange is a union: exactly one known variant.
            let (variant, entry) = change
                .as_object()
                .and_then(|change| sole_variant(change, KNOWN_LEDGER_CHANGE_VARIANTS))
                .ok_or_else(malformed)?;
            let entry = entry.as_object().ok_or_else(malformed)?;
            if variant != "updated" {
                continue;
            }
            let account = entry
                .get("data")
                .and_then(Value::as_object)
                .and_then(|data| data.get("account"))
                .and_then(Value::as_object)
                .ok_or_else(malformed)?;
            let account_id = account
                .get("account_id")
                .and_then(Value::as_str)
                .and_then(|value| AccountId::from_str(value).ok())
                .ok_or_else(malformed)?;
            let seq_num = account
                .get("seq_num")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .ok_or_else(malformed)?;
            let seq_time = account
                .get("ext")
                .and_then(|ext| ext.pointer("/v1/ext/v2/ext/v3/seq_time"))
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .ok_or_else(malformed)?;
            account_sequences.push((account_id, seq_num, seq_time));
        }
        Ok(account_sequences)
    }

    /// Generates a funded identity (friendbot) unless it already exists.
    /// Returns the checksum-validated G-address. A successful lookup whose
    /// whole stdout is not a checksum-valid `G…` StrKey is a hard,
    /// input-free rejection; a definite failed lookup (identity absent)
    /// still routes to `keys generate <name> --fund` in both modes.
    /// `keys public-key` takes only `--config-dir`, not network flags.
    pub fn keys_ensure_funded(&self, name: &str) -> anyhow::Result<AccountId> {
        let config_dir = self.net.config_dir.display().to_string();
        let public_key = || s(&["keys", "public-key", name, "--config-dir", &config_dir]);
        let existing = self.run(public_key())?;
        if existing.ok() {
            return AccountId::from_str(existing.stdout.pick()).map_err(|_| {
                anyhow!(
                    "{}",
                    Safe::literal("keys lookup returned no valid funded identity")
                )
            });
        }
        let mut args = s(&["keys", "generate", name, "--fund"]);
        args.extend(self.net_args());
        self.run_ok(args)?;
        let funded = self.run_ok(public_key())?;
        AccountId::from_str(funded.stdout.pick()).map_err(|_| {
            anyhow!(
                "{}",
                Safe::literal("keys lookup returned no valid funded identity")
            )
        })
    }

    /// Read-only admin resolver: `keys public-key <alias> --config-dir` only —
    /// no network args, no generate fallback. `Ok(None)` on a definite lookup
    /// failure (identity absent); a successful lookup whose stdout is not a
    /// checksum-valid `G…` StrKey is a hard, input-free rejection.
    pub fn keys_public_key(&self, alias: &str) -> anyhow::Result<Option<AccountId>> {
        let config_dir = self.net.config_dir.display().to_string();
        let out = self.run(s(&[
            "keys",
            "public-key",
            alias,
            "--config-dir",
            &config_dir,
        ]))?;
        if !out.ok() {
            return Ok(None);
        }
        AccountId::from_str(out.stdout.pick())
            .map(Some)
            .map_err(|_| {
                anyhow!(
                    "{}",
                    Safe::literal("admin lookup returned no valid identity")
                )
            })
    }

    /// The shared shape of every market mutation whose source is the pinned
    /// signer itself: the CLI subcommand, then the forced
    /// `--source <pinned G> --sign-with-key <alias>` pair from the pinned
    /// signer law, then the network args, through the single choke point.
    /// Because every `tx new …` and `contract asset deploy` argv is
    /// classified as `mutating`, an ambiguous post-send failure is never
    /// replayed.
    fn market_send(&self, head: Vec<String>) -> anyhow::Result<CmdOutput> {
        let mut args = head;
        let pinned = self.market_signer.as_ref().ok_or_else(|| {
            anyhow!(
                "{}",
                Safe::literal("market mutation refused: no pinned signer is configured")
            )
        })?;
        args.extend(self.market_pinned_signing(pinned.1.as_str())?);
        // Every one of these argv shapes submits a transaction, so each bids
        // the inclusion fee.
        args.extend(s(&["--fee", MARKET_INCLUSION_FEE]));
        args.extend(self.net_args());
        self.run_ok(args)
    }

    /// `tx new create-account`: funds `destination` from the pinned signer
    /// with `starting_balance_stroops`, the unit the CLI's
    /// `--starting-balance` takes.
    pub fn tx_create_account(
        &self,
        destination: &AccountId,
        starting_balance_stroops: i64,
    ) -> anyhow::Result<()> {
        positive(starting_balance_stroops)?;
        self.market_send(s(&[
            "tx",
            "new",
            "create-account",
            "--destination",
            destination.as_str(),
            "--starting-balance",
            &starting_balance_stroops.to_string(),
        ]))?;
        Ok(())
    }

    /// `tx new change-trust` on a classic `CODE:ISSUER` asset. `None`
    /// leaves the CLI's unlimited default; `Some(0)` removes the trustline.
    pub fn tx_change_trust(
        &self,
        asset: &str,
        limit_stroops: Option<i64>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            valid_asset(asset) && asset != "native",
            "{}",
            Safe::literal("trustline asset must be a classic CODE:ISSUER asset")
        );
        let mut head = s(&["tx", "new", "change-trust", "--line", asset]);
        if let Some(limit) = limit_stroops {
            anyhow::ensure!(
                limit >= 0,
                "{}",
                Safe::literal("trustline limit must not be negative")
            );
            head.extend(s(&["--limit", &limit.to_string()]));
        }
        self.market_send(head)?;
        Ok(())
    }

    /// `tx new payment` of `amount_stroops` of `asset` (native or
    /// `CODE:ISSUER`, the units the CLI's `--amount` takes).
    pub fn tx_payment(
        &self,
        destination: &AccountId,
        asset: &str,
        amount_stroops: i64,
    ) -> anyhow::Result<()> {
        positive(amount_stroops)?;
        anyhow::ensure!(
            valid_asset(asset),
            "{}",
            Safe::literal("payment asset must be native or a classic CODE:ISSUER asset")
        );
        self.market_send(s(&[
            "tx",
            "new",
            "payment",
            "--destination",
            destination.as_str(),
            "--asset",
            asset,
            "--amount",
            &amount_stroops.to_string(),
        ]))?;
        Ok(())
    }

    /// Locks the pinned signer's own account the way the runbook does: one
    /// `set-options` with master weight zero and all three thresholds at
    /// one, so no key can move or remove the master.
    pub fn tx_lock_account(&self) -> anyhow::Result<()> {
        self.market_send(s(&[
            "tx",
            "new",
            "set-options",
            "--master-weight",
            "0",
            "--low-threshold",
            "1",
            "--med-threshold",
            "1",
            "--high-threshold",
            "1",
        ]))?;
        Ok(())
    }

    /// `contract asset deploy`: wraps `asset` and returns the contract id
    /// the CLI printed, validated as a `ContractId`.
    pub fn asset_deploy(&self, asset: &str) -> anyhow::Result<ContractId> {
        anyhow::ensure!(
            valid_asset(asset),
            "{}",
            Safe::literal("asset must be native or a classic CODE:ISSUER asset")
        );
        let unresolved = || {
            anyhow!(
                "{}",
                Safe::literal("asset deployment returned no contract id")
            )
        };
        let out = self.market_send(s(&["contract", "asset", "deploy", "--asset", asset]))?;
        let id = parse_contract_id(out.stdout.pick()).map_err(|_| unresolved())?;
        ContractId::from_str(&id).map_err(|_| unresolved())
    }

    /// The deterministic wrapper id of a classic asset:
    /// `contract id asset --asset A`, a pure derivation with no send.
    pub fn asset_contract_id(&self, asset: &str) -> anyhow::Result<ContractId> {
        anyhow::ensure!(
            valid_asset(asset),
            "{}",
            Safe::literal("asset must be native or a classic CODE:ISSUER asset")
        );
        let unresolved = || {
            anyhow!(
                "{}",
                Safe::literal("asset contract id was not returned")
            )
        };
        let mut args = s(&["contract", "id", "asset", "--asset", asset]);
        args.extend(self.net_args());
        let out = self.run_ok(args)?;
        let id = parse_contract_id(out.stdout.pick()).map_err(|_| unresolved())?;
        ContractId::from_str(&id).map_err(|_| unresolved())
    }

    /// Reads an account's ledger entry through the RPC (instant, with no
    /// Horizon lag). `Ok(None)` is the documented absence shape (an empty
    /// entry list); a malformed record is refused input-free.
    pub fn ledger_account(&self, account: &str) -> anyhow::Result<Option<LedgerAccount>> {
        let account =
            AccountId::from_str(account).map_err(|_| ledger_read_failed())?;
        let doc = match self.ledger_entry("account", &["--account", account.as_str()])? {
            Some(doc) => doc,
            None => return Ok(None),
        };
        let val = doc
            .get("val")
            .and_then(|v| v.get("account"))
            .ok_or_else(ledger_read_failed)?;
        let balance_stroops = stroops(field(val, "balance")?).ok_or_else(ledger_read_failed)?;
        let thresholds = field(val, "thresholds")?
            .as_str()
            .and_then(parse_thresholds)
            .ok_or_else(ledger_read_failed)?;
        let signers = parse_signers(field(val, "signers")?).ok_or_else(ledger_read_failed)?;
        let num_sub_entries = count32(field(val, "num_sub_entries")?).ok_or_else(ledger_read_failed)?;
        let flags = count32(field(val, "flags")?).ok_or_else(ledger_read_failed)?;
        let (master_weight, low, med, high) = thresholds;
        Ok(Some(LedgerAccount {
            balance_stroops,
            master_weight,
            low,
            med,
            high,
            signers,
            num_sub_entries,
            flags,
        }))
    }

    /// Reads one trustline through the RPC. `Ok(None)` is absence; a
    /// malformed record is refused input-free.
    pub fn ledger_trustline(&self, account: &str, asset: &str) -> anyhow::Result<Option<Trustline>> {
        let account = AccountId::from_str(account).map_err(|_| ledger_read_failed())?;
        anyhow::ensure!(
            valid_asset(asset) && asset != "native",
            "{}",
            Safe::literal("trustline asset must be a classic CODE:ISSUER asset")
        );
        let doc = match self.ledger_entry(
            "trustline",
            &["--account", account.as_str(), "--asset", asset],
        )? {
            Some(doc) => doc,
            None => return Ok(None),
        };
        let val = doc
            .get("val")
            .and_then(|v| v.get("trustline"))
            .ok_or_else(ledger_read_failed)?;
        Ok(Some(Trustline {
            balance_stroops: stroops(field(val, "balance")?).ok_or_else(ledger_read_failed)?,
            limit_stroops: stroops(field(val, "limit")?).ok_or_else(ledger_read_failed)?,
        }))
    }

    /// Shared read: `ledger entry fetch <kind> … --output json` under the
    /// network args. The output is bounded and parsed here only; raw CLI
    /// bytes never leave this module.
    fn ledger_entry(&self, kind: &str, ident: &[&str]) -> anyhow::Result<Option<Value>> {
        let mut args = s(&["ledger", "entry", "fetch", kind]);
        args.extend(s(ident));
        args.extend(s(&["--output", "json"]));
        args.extend(self.net_args());
        let out = self.run(args)?;
        if !out.ok() {
            return Err(ledger_read_failed());
        }
        let text = out.stdout.pick();
        if text.len() > MARKET_CLI_SPEC_CAP {
            return Err(ledger_read_failed());
        }
        let doc: Value = serde_json::from_str(text).map_err(|_| ledger_read_failed())?;
        sole_entry(&doc).map(|entry| entry.cloned())
    }

    /// Whether the chain holds `hash`, observed exactly the way market
    /// custody reconciliation does it: `contract fetch --wasm-hash H
    /// --out-file <unique temp>`, true iff the exit is clean. The scratch
    /// file is unique per probe and removed afterwards; only the exit
    /// status is read, never the bytes.
    pub fn wasm_hash_live(&self, hash: &WasmHash) -> bool {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let scratch = std::env::temp_dir().join(format!(
            ".abtest-wasm-probe-{}-{n}.wasm",
            std::process::id()
        ));
        let live = self.probe_ok(&[
            "contract",
            "fetch",
            "--wasm-hash",
            hash.as_str(),
            "--out-file",
            &scratch.display().to_string(),
        ]);
        let _ = std::fs::remove_file(&scratch);
        live
    }

    /// Whether a contract exists on this network: `contract fetch --id <id>
    /// --out-file <unique temp>`, true iff the exit is clean. The bytes
    /// are written to a unique scratch file and removed; only the exit
    /// status is read. Used before a paid send, where "the contract the
    /// configuration references is not on this network" must be answered
    /// locally rather than by a failed transaction.
    pub fn contract_exists(&self, id: &ContractId) -> bool {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let scratch = std::env::temp_dir().join(format!(
            ".abtest-contract-probe-{}-{n}.wasm",
            std::process::id()
        ));
        let live = self.probe_ok(&[
            "contract",
            "fetch",
            "--id",
            id.as_str(),
            "--out-file",
            &scratch.display().to_string(),
        ]);
        let _ = std::fs::remove_file(&scratch);
        live
    }

    /// Whether a contract instance exists at `id`, answered by the ledger
    /// itself: `ledger entry fetch contract-data --contract <id> --instance`.
    /// Unlike `contract_exists`, which folds a failed fetch into `false`,
    /// this tells absence from failure: `Ok(false)` only for the documented
    /// empty entry list, and every failed, malformed or ambiguous read is an
    /// error. Settling a deployment as never applied needs exactly that
    /// difference, because a read that merely failed proves nothing.
    ///
    /// stellar CLI 26 spells absence two ways and both are answers, verified
    /// on live networks: against the public testnet RPC a missing contract is
    /// **exit 1** with stderr `error: contract not found: <id>` and nothing on
    /// stdout, while against mainnet the same CLI returns exit 0 with
    /// `{"entries":[]}`. A present contract is exit 0 with one entry on both.
    /// So absence is either an empty entry list or that exact sentence naming
    /// exactly this id; every other failure — a network error, a rate limit, a
    /// malformed body, a "not found" naming some other id — is an error.
    pub fn contract_instance_present(&self, id: &ContractId) -> anyhow::Result<bool> {
        let mut args = s(&[
            "ledger",
            "entry",
            "fetch",
            "contract-data",
            "--contract",
            id.as_str(),
            "--instance",
            "--output",
            "json",
        ]);
        args.extend(self.net_args());
        let out = self.run(args)?;
        if !out.ok() {
            return if is_contract_not_found(out.stderr.pick(), id) {
                Ok(false)
            } else {
                Err(ledger_read_failed())
            };
        }
        let text = out.stdout.pick();
        if text.len() > MARKET_CLI_SPEC_CAP {
            return Err(ledger_read_failed());
        }
        let doc: Value = serde_json::from_str(text).map_err(|_| ledger_read_failed())?;
        Ok(sole_entry(&doc)?.is_some())
    }

    /// Whether the contract at `id` exists and its installed executable
    /// code re-hashes to `sha256`: `contract fetch --id <id>
    /// --out-file <unique temp>`, the bytes hashed, the scratch removed.
    /// The absence of the contract, a failed fetch, and bytes that hash to
    /// anything else are the same answer — false — because none of them
    /// is evidence that this coordinate's code was installed there.
    pub fn wasm_matches(&self, id: &ContractId, sha256: &str) -> bool {
        if self.assert_market_cli_version().is_err() {
            return false;
        }
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let scratch = std::env::temp_dir().join(format!(
            ".abtest-wasm-readback-{}-{n}.wasm",
            std::process::id()
        ));
        let mut argv = s(&[
            "contract",
            "fetch",
            "--id",
            id.as_str(),
            "--out-file",
            &scratch.display().to_string(),
        ]);
        argv.extend(self.net_args());
        let outcome = self.run(argv);
        let installed = outcome.is_ok_and(|out| {
            out.ok()
                && std::fs::read(&scratch).is_ok_and(|bytes| {
                    crate::market::sha256_hex(&bytes) == sha256
                })
        });
        let _ = std::fs::remove_file(&scratch);
        installed
    }

    /// Run-local key custody: `keys generate` in the net's own config dir,
    /// with no `--fund` and no network flags, so nothing is sent and no
    /// friendbot is involved. Idempotent: an existing name returns its
    /// address untouched and never overwrites. Secrets are never printed.
    pub fn keys_generate_local(&self, name: &str) -> anyhow::Result<AccountId> {
        anyhow::ensure!(
            plain_identifier(name),
            "{}",
            Safe::literal("key name is not a plain identifier")
        );
        if let Some(existing) = self.keys_public_key(name)? {
            return Ok(existing);
        }
        let config_dir = self.net.config_dir.display().to_string();
        self.run_ok(s(&["keys", "generate", name, "--config-dir", &config_dir]))?;
        self.keys_public_key(name)?.ok_or_else(|| {
            anyhow!(
                "{}",
                Safe::literal("generated key did not resolve to an identity")
            )
        })
    }

    /// `keys rm <name> --force` in the net's own config dir. An already
    /// absent key is the desired end state, so it is `Ok`.
    pub fn keys_remove(&self, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            plain_identifier(name),
            "{}",
            Safe::literal("key name is not a plain identifier")
        );
        if self.keys_public_key(name)?.is_none() {
            return Ok(());
        }
        let config_dir = self.net.config_dir.display().to_string();
        self.run_ok(s(&["keys", "rm", name, "--force", "--config-dir", &config_dir]))?;
        Ok(())
    }

    pub fn upload(&self, wasm: &Path, source: &str) -> anyhow::Result<WasmHash> {
        let mut args = s(&["contract", "upload", "--wasm", &wasm.display().to_string()]);
        // Market policy may only upload from the pinned G address, signing
        // by alias alone (SEC003); pair argv bytes are unchanged.
        if self.policy == Policy::Market {
            args.extend(self.market_pinned_signing(source)?);
            // An upload submits a transaction: it bids the inclusion fee.
            args.extend(s(&["--fee", MARKET_INCLUSION_FEE]));
        } else {
            args.extend(s(&["--source", source]));
        }
        args.extend(self.net_args());
        let out = self.run_ok(args)?;
        let hash = parse_hash(out.stdout.pick())?;
        WasmHash::from_str(&hash).map_err(|_| anyhow!("no wasm hash in stellar output"))
    }

    pub fn predict_id(&self, source: &str, salt: &str) -> anyhow::Result<ContractId> {
        let mut args = s(&["contract", "id", "wasm", "--salt", salt, "--source", source]);
        args.extend(self.net_args());
        let out = self.run_ok(args)?;
        let id = parse_contract_id(out.stdout.pick())?;
        ContractId::from_str(&id).map_err(|_| anyhow!("no contract id in stellar output"))
    }

    /// Deploys `wasm_hash`; `ctor` are `--name value` pairs for `__constructor`.
    pub fn deploy(
        &self,
        wasm_hash: &WasmHash,
        source: &str,
        salt: Option<&str>,
        ctor: &[(&str, String)],
    ) -> anyhow::Result<ContractId> {
        let mut args = s(&["contract", "deploy", "--wasm-hash", wasm_hash.as_str()]);
        // Market policy may only deploy from the pinned G address, signing
        // by alias alone (SEC003); pair argv bytes are unchanged.
        if self.policy == Policy::Market {
            args.extend(self.market_pinned_signing(source)?);
            // A deploy submits a transaction: it bids the inclusion fee.
            args.extend(s(&["--fee", MARKET_INCLUSION_FEE]));
        } else {
            args.extend(s(&["--source", source]));
        }
        if let Some(salt) = salt {
            args.extend(s(&["--salt", salt]));
        }
        args.extend(self.net_args());
        if !ctor.is_empty() {
            args.push("--".into());
            args.extend(flag_args(ctor));
        }
        let out = self.run_ok(args)?;
        let id = parse_contract_id(out.stdout.pick())?;
        ContractId::from_str(&id).map_err(|_| anyhow!("no contract id in stellar output"))
    }

    /// Sends a transaction. Definite contract/auth/transaction rejections
    /// are outcomes; unclassified failures leave submission custody
    /// unresolved. Historical shape: no explicit send or fee words, and the
    /// shared transport law applies, so transient pre-send failures can be
    /// retried while an ambiguous post-send failure is never replayed.
    pub fn invoke(
        &self,
        id: &str,
        source: &str,
        f: &str,
        args_kv: &[(&str, String)],
    ) -> anyhow::Result<Invocation> {
        self.invoke_inner(id, source, f, args_kv, None)
    }

    /// One confirmed market send bound to a frozen signer (SEC003):
    /// market policy only; the configured pin must equal exactly
    /// `(alias, pinned)`; the alias is re-resolved through the CLI key
    /// storage immediately before the single attempt and a vanished or
    /// rebound alias refuses before sending; the transaction source is the
    /// pinned G-address and signing names only the alias, never a seed.
    /// The send is explicit `--send yes --fee 10000` with exactly one
    /// executor attempt: an ambiguous outcome is never replayed. It refuses
    /// to start once `send_by` (real time) has passed.
    pub fn invoke_confirmed_bound(
        &self,
        id: &ContractId,
        pinned: &AccountId,
        alias: &str,
        f: &str,
        args_kv: &[(&str, String)],
        send_by: std::time::SystemTime,
    ) -> anyhow::Result<Invocation> {
        anyhow::ensure!(
            self.policy == Policy::Market,
            "{}",
            Safe::literal("confirmed market send requires market policy")
        );
        let signer = match self.market_signer.as_ref() {
            Some((stored_alias, stored_pinned)) => {
                anyhow::ensure!(
                    stored_alias == alias && stored_pinned == pinned,
                    "{}",
                    Safe::literal("confirmed market send refused: the requested signer is not the configured pinned signer")
                );
                (stored_alias.clone(), stored_pinned.clone())
            }
            None => {
                // No long-lived pin is configured on this context. The
                // caller may still bind the exact signer frozen in its
                // own one-use intent by value: the identity is proven by
                // a live key-store resolution right here, the alias is
                // re-resolved again immediately before the single
                // submission, and any mismatch refuses before a send.
                let live = self.keys_public_key(alias)?;
                anyhow::ensure!(
                    live.as_ref() == Some(pinned),
                    "{}",
                    Safe::literal("confirmed market send refused: the signer alias does not resolve to the pinned identity")
                );
                (alias.to_string(), pinned.clone())
            }
        };
        let pinned_context = Stellar {
            net: self.net,
            exec: self.exec,
            sleep: self.sleep,
            policy: self.policy,
            market_signer: Some(signer),
            market_network: None,
        };
        pinned_context.invoke_inner(id.as_str(), alias, f, args_kv, Some(send_by))
    }

    fn invoke_inner(
        &self,
        id: &str,
        source: &str,
        f: &str,
        args_kv: &[(&str, String)],
        confirmed: Option<std::time::SystemTime>,
    ) -> anyhow::Result<Invocation> {
        let mut args = s(&["contract", "invoke", "--id", id]);
        // Market policy may only send from the pinned G address, signing by
        // alias alone (SEC003); pair argv bytes are unchanged.
        if self.policy == Policy::Market {
            args.extend(self.market_pinned_signing(source)?);
        } else {
            args.extend(s(&["--source", source]));
        }
        args.extend(self.net_args());
        if confirmed.is_some() {
            // A confirmed generic market send always sends; the displayed
            // fee word is the inclusion bid, not a cap on the resource fee.
            args.extend(s(&["--send", "yes", "--fee", MARKET_INCLUSION_FEE]));
        } else if self.policy == Policy::Market {
            // An unconfirmed market invoke still submits (it is classified
            // mutating and never blindly replayed). Only the explicit send
            // word distinguishes the two paths, so the bid is the same.
            args.extend(s(&["--fee", MARKET_INCLUSION_FEE]));
        }
        args.push("--".into());
        args.push(f.into());
        args.extend(flag_args(args_kv));
        let may_land = mutating(&args);
        let out = if let Some(send_by) = confirmed {
            // One attempt only: a replay after an ambiguous failure would
            // bypass the single-use operator confirmation. The executor
            // refuses to launch it once `send_by` has passed, rechecked at
            // the launch itself, so a late start cannot void the settle bound.
            self.exec
                .run_by(&args, send_by)
                .map_err(|_| anyhow!("{}", Safe::literal("stellar command failed")))?
        } else {
            self.run(args)?
        };
        let inv = invocation_from(out);
        // Custody law: only a call that could have been submitted can be
        // ambiguously applied, and only an unexplained failure of one is
        // unresolved. The pending journal — not evidence — holds custody there.
        anyhow::ensure!(
            !may_land || definite_outcome(&inv),
            "{}",
            Safe::pair_ctx(self.policy, PairCtx::SubmissionUnresolved(&inv.stderr))
        );
        Ok(inv)
    }

    /// Simulation only (`--send no`). Returns the decoded return value;
    /// market failures retain a trusted `Safe` anyhow payload, while pair
    /// failure text preserves its legacy policy rendering.
    pub fn view(
        &self,
        id: &str,
        source: &str,
        f: &str,
        args_kv: &[(&str, String)],
    ) -> anyhow::Result<Value> {
        let mut args = s(&[
            "contract", "invoke", "--id", id, "--source", source, "--send", "no",
        ]);
        args.extend(self.net_args());
        args.push("--".into());
        args.push(f.into());
        args.extend(flag_args(args_kv));
        let out = self.run(args)?;
        // Market law: absence is proven only by truly empty void output.
        // A successful invoke whose stdout is non-empty and not valid
        // JSON is a malformed response, never a laundered null; the pair
        // branch keeps its historical invocation semantics unchanged.
        if self.policy == Policy::Market && out.ok() {
            anyhow::ensure!(
                out.stdout.pick().is_empty()
                    || serde_json::from_str::<Value>(out.stdout.pick()).is_ok(),
                "{}",
                Safe::literal(MARKET_MALFORMED_RESPONSE)
            );
        }
        let inv = invocation_from(out);
        match (inv.ok, inv.value) {
            (true, Some(v)) => Ok(v),
            (true, None) => Ok(Value::Null),
            (false, _) if self.policy == Policy::Market => Err(anyhow::Error::msg(
                Safe::market_invocation_error(inv.error.as_ref()),
            )),
            (false, _) => Err(anyhow!(
                "{}",
                Safe::pair_ctx(
                    self.policy,
                    PairCtx::ViewFailed {
                        f,
                        id,
                        error: inv.error.as_ref(),
                        stderr: &inv.stderr
                    }
                )
            )),
        }
    }

    /// Like `view` but keeps failures as data (for expected-error probes).
    pub fn try_view(
        &self,
        id: &str,
        source: &str,
        f: &str,
        args_kv: &[(&str, String)],
    ) -> anyhow::Result<Invocation> {
        let mut args = s(&[
            "contract", "invoke", "--id", id, "--source", source, "--send", "no",
        ]);
        args.extend(self.net_args());
        args.push("--".into());
        args.push(f.into());
        args.extend(flag_args(args_kv));
        let out = self.run(args)?;
        // Market law (shared with `view`): non-empty successful stdout
        // that is not valid JSON is a malformed response, never data.
        if self.policy == Policy::Market && out.ok() {
            anyhow::ensure!(
                out.stdout.pick().is_empty()
                    || serde_json::from_str::<Value>(out.stdout.pick()).is_ok(),
                "{}",
                Safe::literal(MARKET_MALFORMED_RESPONSE)
            );
        }
        Ok(invocation_from(out))
    }

    pub fn latest_ledger(&self) -> anyhow::Result<u32> {
        let mut args = s(&["ledger", "latest", "--output", "json"]);
        args.extend(self.net_args());
        let out = self.run_ok(args)?;
        let v: Value = serde_json::from_str(out.stdout.pick())?;
        v["sequence"]
            .as_u64()
            .map(|n| n as u32)
            .context("ledger latest: no sequence")
    }

    /// Reads one persistent entry as XDR rows. `Ok(None)` when the key is absent.
    pub fn read_raw(&self, id: &str, key: RawKey) -> anyhow::Result<Option<RawEntry>> {
        self.read_entry(id, key, "persistent")
    }

    /// Reads one temporary entry without invoking a potentially trapping getter.
    pub fn read_temp(&self, contract: &str, key: RawKey) -> anyhow::Result<Option<RawEntry>> {
        self.read_entry(contract, key, "temporary")
    }

    fn read_entry(
        &self,
        id: &str,
        key: RawKey,
        durability: &str,
    ) -> anyhow::Result<Option<RawEntry>> {
        let mut args = s(&[
            "contract",
            "read",
            "--id",
            id,
            "--durability",
            durability,
            "--output",
            "xdr",
        ]);
        match &key {
            RawKey::Symbol(sym) => args.extend(s(&["--key", sym])),
            RawKey::Xdr(b64) => args.extend(s(&["--key-xdr", b64])),
        }
        args.extend(self.net_args());
        let out = self.run(args)?;
        if !out.ok() {
            if out.stdout.pick().trim().is_empty() && is_missing_entry(out.stderr.pick()) {
                return Ok(None);
            }
            return Err(anyhow!(
                "{}",
                Safe::pair_ctx(self.policy, PairCtx::ReadFailed(&out.stderr))
            ));
        }
        let mut rows = parse_read_xdr(self.policy, out.stdout.pick())?;
        anyhow::ensure!(
            rows.len() == 1,
            "contract read expected one row, got {}",
            rows.len()
        );
        let row = rows.pop().expect("one row");
        let matches = match key {
            // Two CLI encoders produce the compared strings (`xdr encode` vs
            // `contract read --output xdr`), so compare decoded bytes; string
            // equality is the fallback for values the in-tree decoder
            // rejects (it deliberately has no dependency on a base64 crate).
            RawKey::Xdr(b64) => match (base64_decode(&row.key), base64_decode(&b64)) {
                (Some(a), Some(b)) => a == b,
                _ => row.key == b64,
            },
            RawKey::Symbol(sym) => base64_decode(&row.key).is_some_and(|bytes| {
                // ScVal::Symbol = discriminant 15 (stellar-xdr 26.0.0
                // ScValType), then an XDR string padded with zeroes to a
                // multiple of four.
                let end = 8 + sym.len();
                bytes.get(..4) == Some(15_u32.to_be_bytes().as_slice())
                    && bytes.get(4..8) == Some((sym.len() as u32).to_be_bytes().as_slice())
                    && bytes.get(8..end) == Some(sym.as_bytes())
                    && bytes.len() == end + (4 - sym.len() % 4) % 4
                    && bytes[end..].iter().all(|byte| *byte == 0)
            }),
        };
        anyhow::ensure!(
            matches,
            "contract read returned a different key than requested"
        );
        Ok(Some(row))
    }

    pub fn xdr_encode_scval(&self, json: &Value) -> anyhow::Result<ScvalB64> {
        // `xdr encode` reads a file argument; use a unique temp file per call (parallel callers).
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("abtest-scval-{}-{n}.json", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(json)?)?;
        let args = s(&[
            "xdr",
            "encode",
            "--type",
            "ScVal",
            "--input",
            "json",
            "--output",
            "single-base64",
            &tmp.display().to_string(),
        ]);
        let out = self.run_ok(args);
        let _ = std::fs::remove_file(&tmp);
        let last = out?
            .stdout
            .pick()
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_string();
        ScvalB64::from_str(&last).map_err(|_| anyhow!("xdr encode returned invalid base64"))
    }
}

/// The validated facts of a v1 transaction envelope: checksum-validated
/// source account, sequence, and whether any Soroban invocation targets
/// the contract the caller asked about. Unknown or fee-bump envelopes
/// never construct one.
pub struct EnvelopedTx {
    pub source_account: AccountId,
    pub seq_num: u64,
    pub invokes_target: bool,
    /// The code installed by every contract-creating host function in the
    /// transaction, in operation order. Empty for a transaction that creates
    /// no contract; classic operations, uploads and invocations add nothing.
    pub creates: Vec<CreatedCode>,
}

/// What one contract-creating host function installs, read from its
/// `executable` as stellar CLI 26 serializes it: `{"wasm":"<64 hex>"}` for
/// uploaded code, the bare string `"stellar_asset"` for a Stellar Asset
/// Contract. The id preimage (`{"address":{..}}` or `{"asset":..}`) is
/// deliberately not read: what ties a creation to a coordinate is the code,
/// and a preimage-blind rule can only refuse more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreatedCode {
    /// Uploaded Wasm, by its lower-case hex digest.
    Wasm(String),
    /// The network's own Stellar Asset Contract code.
    StellarAsset,
    /// An executable this reader does not recognize.
    Unclassified,
}

fn created_code(executable: Option<&Value>) -> CreatedCode {
    match executable {
        Some(Value::String(arm)) if arm == "stellar_asset" => CreatedCode::StellarAsset,
        Some(Value::Object(arms)) if arms.len() == 1 => match arms.get("wasm") {
            Some(Value::String(hash))
                if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                CreatedCode::Wasm(hash.to_ascii_lowercase())
            }
            _ => CreatedCode::Unclassified,
        },
        _ => CreatedCode::Unclassified,
    }
}

/// OperationBody union variant names as serialized by stellar CLI 26
/// (stellar-xdr serde `snake_case` of the XDR union arms).
const KNOWN_OPERATION_VARIANTS: &[&str] = &[
    "create_account",
    "payment",
    "path_payment_strict_receive",
    "manage_sell_offer",
    "create_passive_sell_offer",
    "set_options",
    "change_trust",
    "allow_trust",
    "account_merge",
    "claim_claimable_balance",
    "begin_sponsoring_future_reserves",
    "revoke_sponsorship",
    "manage_data",
    "bump_sequence",
    "manage_buy_offer",
    "path_payment_strict_send",
    "create_claimable_balance",
    "invoke_host_function",
    "liquidity_pool_deposit",
    "liquidity_pool_withdraw",
    "clawback",
    "clawback_claimable_balance",
    "set_trust_line_flags",
    "restore_footprint",
    "extend_footprint_ttl",
];

/// OperationBody unit arms, which stellar-xdr serializes as bare strings.
const KNOWN_UNIT_OPERATION_VARIANTS: &[&str] = &["inflation", "end_sponsoring_future_reserves"];

/// HostFunction union variant names as serialized by stellar CLI 26.
const KNOWN_HOST_FUNCTION_VARIANTS: &[&str] = &[
    "invoke_contract",
    "create_contract",
    "create_contract_v2",
    "upload_contract_wasm",
];

/// LedgerEntryChange union variant names as serialized by stellar CLI 26.
const KNOWN_LEDGER_CHANGE_VARIANTS: &[&str] =
    &["created", "updated", "state", "removed", "restored"];

fn tx_read_malformed() -> anyhow::Error {
    anyhow!("{}", Safe::literal(TX_READ_MALFORMED))
}

/// The sole `(variant, value)` of a stellar-xdr tagged union in CLI JSON.
/// The object must hold exactly one key and it must be a known variant;
/// zero, several or unknown keys are refused rather than filtered.
fn sole_variant<'a>(
    union: &'a serde_json::Map<String, Value>,
    known: &[&str],
) -> Option<(&'a str, &'a Value)> {
    let mut entries = union.iter();
    let (key, value) = entries.next()?;
    (entries.next().is_none() && known.contains(&key.as_str())).then_some((key.as_str(), value))
}

pub enum RawKey {
    Symbol(String),
    Xdr(String),
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| x.to_string()).collect()
}

/// Market execution only accepts the absolute, executable regular file
/// resolved before entering the controlled child working directory.
fn market_launchable_bin(bin: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    bin.is_absolute()
        && bin
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn flag_args(kv: &[(&str, String)]) -> Vec<String> {
    kv.iter()
        .flat_map(|(k, v)| [format!("--{k}"), v.clone()])
        .collect()
}

/// Version major from `stellar --version`. The failure context is a
/// policy-selected pair_ctx member: the pair arm echoes the unparseable line
/// byte-identically to today's message, the market arm is input-free.
pub fn parse_major(policy: Policy, version_line: &str) -> anyhow::Result<u32> {
    version_line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.split('.').next())
        .and_then(|m| m.parse().ok())
        .with_context(|| Safe::pair_ctx(policy, PairCtx::UnparseableVersion(version_line)))
}

/// StrKey CRC16-XMODEM checksum (poly 0x1021, init 0, MSB-first), verified as
/// a little-endian trailing checksum over the version byte and payload.
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

/// Decode one RFC4648 base32 element (strict uppercase alphabet).
fn base32_decode5(ch: u8) -> Option<u32> {
    match ch {
        b'A'..=b'Z' => Some((ch - b'A') as u32),
        b'2'..=b'7' => Some((ch - b'2') as u32 + 26),
        _ => None,
    }
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut pending = 0u32;
    let mut out = Vec::new();
    for &ch in s.as_bytes() {
        if ch == b'=' {
            break;
        }
        bits = (bits << 5) | base32_decode5(ch)?;
        pending += 5;
        if pending >= 8 {
            pending -= 8;
            out.push((bits >> pending) as u8);
            bits &= (1 << pending) - 1;
        }
    }
    (pending == 0).then_some(out)
}

/// Validate a StrKey address: correct length, alphabet, version byte, and
/// little-endian CRC16-XMODEM checksum. Pinned by the in-file checksum
/// fixtures; returns the verified address's decoded hash payload.
fn strkey_validate(addr: &str, version: u8) -> bool {
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

/// A checksum-validated StrKey contract address (`C…`, version byte 0x10).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContractId(String);

impl ContractId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for ContractId {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        (s.starts_with('C') && strkey_validate(s, 0x10))
            .then(|| Self(s.to_string()))
            .ok_or("invalid contract id")
    }
}

impl<'de> Deserialize<'de> for ContractId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for ContractId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ContractId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// A checksum-validated StrKey account address (`G…`, version byte 0x30).
/// Deserialization parses through [`std::str::FromStr`] exactly once, so
/// forged, control-bearing, or oversized identities are rejected at load
/// with the fixed message and can never reach a simulation or render.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AccountId(String);

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for AccountId {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        (s.starts_with('G') && strkey_validate(s, 0x30))
            .then(|| Self(s.to_string()))
            .ok_or("invalid account id")
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for AccountId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// A strict 64-character lowercase hex SHA-256 hash.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
pub struct WasmHash(String);

impl WasmHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for WasmHash {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self(s.to_lowercase()))
            .ok_or("invalid wasm hash")
    }
}

impl std::fmt::Display for WasmHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for WasmHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// A strict 64-character lowercase hex SHA-256 transaction hash. Parsed at
/// the trust boundary (never validated after the fact): CLI `tx fetch`
/// arguments must carry one, and a parsed hash is never echoed into error
/// or render text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TxHash(String);

impl TxHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for TxHash {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        (s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        .then(|| Self(s.to_string()))
        .ok_or("invalid transaction hash")
    }
}

impl<'de> Deserialize<'de> for TxHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for TxHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for TxHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Contract ids are parsed with, not validated after: a 56-character `C…`
/// token only counts when it carries a valid StrKey checksum (item 7).
pub fn parse_contract_id(stdout: &str) -> anyhow::Result<String> {
    stdout
        .split_whitespace()
        .rev()
        .find(|t| t.starts_with('C') && t.len() == 56 && ContractId::from_str(t).is_ok())
        .map(ToString::to_string)
        .context("no contract id in stellar output")
}

/// Strict 64-character lowercase hex hashes only (SHA-256/XDR hashes are
/// lowercase); the candidate must be a valid `WasmHash` (item 7).
pub fn parse_hash(stdout: &str) -> anyhow::Result<String> {
    stdout
        .split_whitespace()
        .rev()
        .find(|t| {
            t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit()) && t == &t.to_lowercase()
        })
        .map(ToString::to_string)
        .context("no wasm hash in stellar output")
}

/// Classic asset argument as the CLI spells it: `CODE:ISSUER`.
pub fn classic_asset(code: &str, issuer: &AccountId) -> String {
    format!("{code}:{}", issuer.as_str())
}

/// The CLI's asset grammar, verified against `--help`: the literal
/// `native`, or `CODE:ISSUER` with a 1-12 character alphanumeric code and
/// a checksum-valid issuer. Refusing here keeps a malformed asset from
/// ever reaching argv.
fn valid_asset(asset: &str) -> bool {
    if asset == "native" {
        return true;
    }
    let Some((code, issuer)) = asset.split_once(':') else {
        return false;
    };
    !code.is_empty()
        && code.len() <= 12
        && code.bytes().all(|b| b.is_ascii_alphanumeric())
        && AccountId::from_str(issuer).is_ok()
}

/// An identity or network name as the CLI accepts it: a plain identifier,
/// so it can never smuggle a flag or a path separator into argv.
fn plain_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A positive amount of stroops, the unit every `tx new` money flag takes.
fn positive(amount_stroops: i64) -> anyhow::Result<()> {
    anyhow::ensure!(
        amount_stroops > 0,
        "{}",
        Safe::literal("transaction amount must be a positive number of stroops")
    );
    Ok(())
}

/// One account ledger record, reduced to the fields the deploy flow acts
/// on. Parsed from `ledger entry fetch account --output json`, whose
/// `thresholds` field is the XDR `Thresholds` byte string printed as hex
/// in master, low, medium, high order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerAccount {
    pub balance_stroops: i64,
    pub master_weight: u8,
    pub low: u8,
    pub med: u8,
    pub high: u8,
    pub signers: Vec<(String, u32)>,
    pub num_sub_entries: u32,
    pub flags: u32,
}

/// One trustline's money fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trustline {
    pub balance_stroops: i64,
    pub limit_stroops: i64,
}

/// Fixed, input-free refusal for a bounded ledger-entry read: neither argv
/// values nor CLI stderr ever enter it.
fn ledger_read_failed() -> anyhow::Error {
    anyhow::Error::msg(Safe::literal("ledger entry read was rejected"))
}

/// Money fields are i64/u64 XDR, which the CLI's serde layer prints as
/// decimal strings; JSON numbers are accepted too, nothing else.
fn stroops(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// u32 XDR counters (`num_sub_entries`, `flags`, signer weights).
fn count32(v: &Value) -> Option<u32> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        _ => None,
    }
}

/// A required field of a ledger record; absence is malformed.
fn field<'a>(v: &'a Value, key: &str) -> anyhow::Result<&'a Value> {
    v.get(key).ok_or_else(ledger_read_failed)
}

/// The XDR `Thresholds` byte string: master, low, medium, high.
fn parse_thresholds(hex: &str) -> Option<(u8, u8, u8, u8)> {
    let bytes = hex.as_bytes();
    if bytes.len() != 8 || !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok();
    Some((byte(0)?, byte(1)?, byte(2)?, byte(3)?))
}

/// `[{key, weight}, …]`; a signer whose shape is unknown is malformed.
fn parse_signers(v: &Value) -> Option<Vec<(String, u32)>> {
    let items = v.as_array()?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let key = item.get("key")?.as_str()?;
        let weight = count32(item.get("weight")?)?;
        out.push((key.to_string(), weight));
    }
    Some(out)
}

/// Whether a failed `ledger entry fetch contract-data` stderr is the CLI's
/// own "no such contract" answer for exactly `id`: one line, once the CLI's
/// `error:` decoration is stripped, reading `contract not found: <id>`. Any
/// other text, or the same sentence for another id, is not absence.
fn is_contract_not_found(stderr: &str, id: &ContractId) -> bool {
    let want = format!("contract not found: {}", id.as_str());
    stderr.lines().any(|line| {
        let line = line.trim();
        let line = line.strip_prefix("\u{274c} ").unwrap_or(line);
        let line = line.strip_prefix("error: ").unwrap_or(line);
        line == want
    })
}

/// One entry, or none: an empty list is the documented shape for a missing
/// account or trustline, while two entries or a non-list is malformed.
fn sole_entry(out: &Value) -> anyhow::Result<Option<&Value>> {
    let entries = field(out, "entries")?.as_array().ok_or_else(ledger_read_failed)?;
    match entries.len() {
        0 => Ok(None),
        1 => Ok(Some(&entries[0])),
        _ => Err(ledger_read_failed()),
    }
}

/// The operator's Stellar CLI network entry, as kept in
/// `<config_dir>/network/<name>.toml` (`rpc_url`, `network_passphrase`,
/// optional `rpc_headers`). Its header values carry an API key, so the
/// derived `Debug` is hand-written down to the scheme and bare host.
pub struct CliNetwork {
    pub rpc_url: String,
    pub passphrase: String,
    pub headers: Vec<(String, String)>,
}

impl CliNetwork {
    fn split(&self) -> (&str, &str) {
        match self.rpc_url.split_once("://") {
            Some((scheme, rest)) => (scheme, rest),
            None => ("", self.rpc_url.as_str()),
        }
    }

    /// Bare authority: no userinfo, no path, no query, no header value.
    fn authority(&self) -> &str {
        self.split()
            .1
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .rsplit('@')
            .next()
            .unwrap_or_default()
    }

    /// A credential-free label for logs and operator text.
    pub fn host_label(&self) -> String {
        self.authority().to_string()
    }
}

impl fmt::Debug for CliNetwork {
    /// Only the scheme and the bare authority: the URL's path, query and
    /// userinfo, and every header value, stay unprinted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.split().0 {
            "" => write!(f, "CliNetwork({})", self.authority()),
            scheme => write!(f, "CliNetwork({scheme}::{})", self.authority()),
        }
    }
}

fn cli_network_refused() -> anyhow::Error {
    anyhow::Error::msg(Safe::literal("stellar CLI network entry is missing"))
}

fn cli_network_malformed() -> anyhow::Error {
    anyhow::Error::msg(Safe::literal("stellar CLI network entry is malformed"))
}

/// The CLI's built-in `testnet` exists in its code, not as a file, so it
/// is reproduced here; `mainnet` always needs the operator's own entry.
const BUILTIN_TESTNET_RPC: &str = "https://soroban-testnet.stellar.org";

/// `rpc_headers` as the CLI writes them: `Name=Value` strings or
/// `["Name", "Value"]` pairs. Unknown shapes are skipped, never guessed.
fn parse_cli_headers(v: Option<&toml::Value>) -> Vec<(String, String)> {
    let Some(items) = v.and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            toml::Value::Array(pair) if pair.len() == 2 => {
                Some((pair[0].as_str()?.to_string(), pair[1].as_str()?.to_string()))
            }
            toml::Value::String(kv) => kv
                .split_once('=')
                .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
                .filter(|(name, value)| !name.is_empty() && !value.is_empty()),
            _ => None,
        })
        .collect()
}

/// Reads one of the operator's own `stellar network add` entries. Names
/// are plain identifiers; a missing or malformed entry — or a non-HTTP
/// RPC URL — refuses with a fixed sentence that names neither the file's
/// contents nor the URL.
pub fn cli_network(config_dir: &Path, name: &str) -> anyhow::Result<CliNetwork> {
    anyhow::ensure!(
        plain_identifier(name),
        "{}",
        Safe::literal("network name is not a plain identifier")
    );
    let path = config_dir.join("network").join(format!("{name}.toml"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        if name == "testnet" {
            return Ok(CliNetwork {
                rpc_url: BUILTIN_TESTNET_RPC.to_string(),
                passphrase: MARKET_TESTNET_PASSPHRASE.to_string(),
                headers: Vec::new(),
            });
        }
        return Err(cli_network_refused());
    };
    let doc = toml::from_str::<toml::Value>(&text).map_err(|_| cli_network_malformed())?;
    let entry = CliNetwork {
        rpc_url: doc
            .get("rpc_url")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        passphrase: doc
            .get("network_passphrase")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        headers: parse_cli_headers(doc.get("rpc_headers")),
    };
    let scheme = entry.split().0;
    if !(scheme == "https" || scheme == "http")
        || entry.host_label().is_empty()
        || entry.passphrase.is_empty()
    {
        return Err(cli_network_malformed());
    }
    Ok(entry)
}

/// stderr line shape (26.x): `📅 <C..> - Success - Event: [topics] = value`.
pub fn parse_event_lines(stderr: &str) -> Vec<Event> {
    stderr
        .lines()
        .filter_map(|line| {
            let idx = line.find("Event: ")?;
            let head = &line[..idx];
            let contract = head
                .split_whitespace()
                .find(|t| t.starts_with('C') && t.len() == 56 && ContractId::from_str(t).is_ok())?
                .to_string();
            let body = &line[idx + "Event: ".len()..];
            let eq = body.rfind(" = ")?;
            let topics: Vec<Value> = serde_json::from_str(body[..eq].trim()).ok()?;
            let value: Value = serde_json::from_str(body[eq + 3..].trim()).ok()?;
            Some(Event {
                contract,
                topics,
                value,
            })
        })
        .collect()
}

/// Recheck one parsed NDJSON event line and rebuild a minimal safe page
/// entry from validated fields only. Both CLI26 shapes are accepted and
/// mutually exclusive: the decoded form (`contract_id` plus
/// `event_name`/`params`) and the raw-fallback form (`contractId` plus
/// `topic`/`value`); a line presenting either, neither, or both is
/// malformed here. The contract ID must equal the requested target, the
/// ledger must be a number at or above the requested start and no higher
/// than the ledger-sequence range can hold, and the cursor ID must have
/// the RPC event-ID grammar the pinned CLI consumes, so nothing echoed
/// back can steer a terminal or escape a bound. Nested payloads (params,
/// topics, base64 XDR) never leave this function.
fn validated_event_cursor(
    event: &Value,
    id: &ContractId,
    start_ledger: u32,
) -> anyhow::Result<Value> {
    let malformed = || anyhow!("{}", Safe::literal(HISTORY_MALFORMED));
    let Some(fields) = event.as_object() else {
        return Err(malformed());
    };
    let decoded = fields.contains_key("event_name") && fields.contains_key("contract_id");
    let raw_fallback = fields.contains_key("contractId") && fields.contains_key("topic");
    if decoded == raw_fallback {
        return Err(malformed());
    }
    let contract_ok = fields
        .get(if decoded { "contract_id" } else { "contractId" })
        .and_then(Value::as_str)
        .is_some_and(|contract| contract == id.as_str());
    let ledger_ok = fields
        .get("ledger")
        .and_then(Value::as_u64)
        .is_some_and(|ledger| ledger >= u64::from(start_ledger) && ledger <= u64::from(u32::MAX));
    let cursor_ok = fields
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(is_event_cursor_shape);
    if !(contract_ok && ledger_ok && cursor_ok) {
        return Err(malformed());
    }
    Ok(serde_json::json!({
        "cursor": fields.get("id").and_then(Value::as_str).expect("checked cursor"),
        "ledger": fields.get("ledger").and_then(Value::as_u64).expect("checked ledger"),
        "contract_id": id.as_str(),
    }))
}

/// Whether `cursor` has exactly the shape the RPC assigns to event IDs:
/// a decimal toid and a decimal event index separated by a single hyphen
/// (the same grammar `soroban-rpc`'s `parse_cursor` requires, so a
/// continuation handle this tool hands back is one the pinned CLI can
/// actually consume). Purely syntactic; never echoes bytes back.
fn is_event_cursor_shape(cursor: &str) -> bool {
    let Some((toid, index)) = cursor.split_once('-') else {
        return false;
    };
    !toid.is_empty()
        && !index.is_empty()
        && toid.bytes().all(|b| b.is_ascii_digit())
        && index.bytes().all(|b| b.is_ascii_digit())
        && toid.parse::<u64>().is_ok()
        && index.parse::<i32>().is_ok()
        && cursor.len() <= 64
}

/// Validate a caller-supplied event continuation handle against the RPC
/// event-ID grammar before anything reaches the executor. The rejection
/// is the fixed input-free [`MARKET_INVALID_CURSOR`] sentence: a hostile
/// handle is refused locally and never echoed.
pub fn validate_event_cursor(cursor: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        is_event_cursor_shape(cursor),
        "{}",
        Safe::literal(MARKET_INVALID_CURSOR)
    );
    Ok(())
}

/// Decode a grammar-valid event ID into its `(toid, index)` pair; the
/// same parse [`is_event_cursor_shape`] accepts is required here.
fn cursor_pair(cursor: &str) -> Option<(u64, i32)> {
    let (toid, index) = cursor.split_once('-')?;
    Some((toid.parse().ok()?, index.parse().ok()?))
}

/// Strict-progress law for cursor continuation: `candidate` must sort
/// after `previous` as numeric `(toid, index)` pairs. Same-ledger pages
/// continue (the index advances within the ledger), while a replayed or
/// earlier page fails closed. Purely numeric; never echoes bytes.
fn cursor_is_after(candidate: &str, previous: &str) -> bool {
    match (cursor_pair(candidate), cursor_pair(previous)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The `run_ok` failure context under a policy. The pair arm composes
/// form (c) verbatim (full argv plus raw stdout/stderr) through
/// `Safe::pair_failed`; the market arm emits no argv values, no bin path,
/// and no raw streams.
impl Safe {
    pub(crate) fn run_failure(policy: Policy, args: &[String], out: &CmdOutput) -> Safe {
        match policy {
            Policy::Pair => Self::pair_failed(args, out),
            Policy::Market => Self::literal("stellar command failed"),
        }
    }
}

/// Contract error codes, from `pool/src/errors.rs` and `backstop/src/errors.rs`.
/// A bare number tells you nothing while you are reading a failure.
pub fn error_name(code: u32) -> Option<&'static str> {
    Some(match code {
        // backstop
        1000 => "BadRequest",
        1001 => "NotExpired",
        1002 => "InvalidRewardZoneEntry",
        1003 => "InsufficientFunds",
        1004 => "NotPool",
        1005 => "InvalidShareMintAmount",
        1006 => "InvalidTokenWithdrawAmount",
        1007 => "TooManyQ4WEntries",
        1008 => "NotInRewardZone",
        1009 => "RewardZoneFull",
        1010 => "MaxBackfillEmissions",
        1011 => "BadDebtExists",
        // pool
        1200 => "BadRequest",
        1201 => "InvalidPoolConfigArgs",
        1202 => "InvalidReserveMetadata",
        1203 => "InitNotUnlocked",
        1204 => "StatusNotAllowed",
        1205 => "InvalidHf",
        1206 => "InvalidPoolStatus",
        1207 => "InvalidUtilRate",
        1208 => "MaxPositionsExceeded",
        1209 => "InternalReserveNotFound",
        1210 => "InvalidPrice",
        1211 => "InvalidLiquidation",
        1212 => "AuctionInProgress",
        1213 => "InvalidLiqTooLarge",
        1214 => "InvalidLiqTooSmall",
        1215 => "InterestTooSmall",
        1216 => "InvalidBTokenMintAmount",
        1217 => "InvalidBTokenBurnAmount",
        1218 => "InvalidDTokenMintAmount",
        1219 => "InvalidDTokenBurnAmount",
        1220 => "ExceededSupplyCap",
        1221 => "InvalidBid",
        1222 => "InvalidLot",
        1223 => "ReserveDisabled",
        1224 => "MinCollateralNotMet",
        _ => return None,
    })
}

/// What the code actually means, in the terms this tool cares about. Fork-specific
/// notes call out the ADR 0008/0011 traps, which is why most rejections here happen.
pub fn error_help(code: u32) -> Option<&'static str> {
    Some(match code {
        // backstop
        1000 => "the backstop refused this request in its current state; on the fork all six emissions exports always refuse with it",
        1001 => "the queued withdrawal has not reached its unlock time",
        1002 => "the pool is not a valid reward-zone entry",
        1003 => "not enough balance for the requested amount",
        1004 => "the caller is not a pool deployed by the bound factory",
        1005 => "the deposit would mint zero shares",
        1006 => "the withdrawal would return zero tokens",
        1007 => "too many queued withdrawals for this user",
        1008 => "the pool is not in the reward zone",
        1009 => "the reward zone is full",
        1010 => "backfilled emissions would exceed the cap",
        1011 => "the pool still has bad debt outstanding",
        // pool
        1200 => "the pool refused this request in its current state; on the fork update_pool, flash_loan and set_emissions_config always refuse with it",
        1201 => "pool config arguments are out of range (the fork also requires bstop_rate 0)",
        1202 => "the queued reserve change is not the exact enabled -> disabled transition the fork allows",
        1203 => "the queued reserve change is still inside its timelock",
        1204 => "that status change is not permitted from the current status; status 4 is absorbing",
        1205 => "the resulting position would be below the minimum health factor",
        1206 => "the action is not allowed while the pool is in this status",
        1207 => "the borrow or withdrawal would push the reserve past its max_util",
        1208 => "the account would hold more positions than the pool allows",
        1209 => "the reserve is missing from the pool's internal cache",
        1210 => "the oracle price is stale, future-dated, non-positive, or not 7 decimals",
        1211 => "the position is not liquidatable, or the bid/lot does not fit it",
        1212 => "an auction for this user already exists",
        1213 => "the liquidation is too large: it would leave the position healthier than 1.15",
        1214 => "the liquidation is too small: it would leave the position under 1.03, so take more",
        1215 => "the accrued interest is too small to act on",
        1216 | 1217 => "a b-token mint or burn of zero or less was attempted",
        1218 | 1219 => "a d-token mint or burn of zero or less was attempted",
        1220 => "the supply would take the reserve past its sealed supply_cap",
        1221 => "the bid names an asset the user has no liability in",
        1222 => "the lot names an asset the user has no collateral in",
        1223 => "the reserve is disabled, so supply and borrow are closed",
        1224 => "the borrow is below the pool's minimum collateral",
        _ => return None,
    })
}

/// `Name (code) — what it means`, for somewhere with room to show it.
pub fn error_explained(kind: &ErrorKind) -> String {
    match kind {
        ErrorKind::Contract(c) => match (error_name(*c), error_help(*c)) {
            (Some(n), Some(h)) => format!("{n} ({c}) — {h}"),
            (Some(n), None) => format!("{n} ({c})"),
            _ => format!("contract error {c}"),
        },
        other => error_summary(other),
    }
}

/// A one-line summary of why a call failed, with the noise removed.
pub fn error_summary(kind: &ErrorKind) -> String {
    match kind {
        ErrorKind::Contract(c) => match error_name(*c) {
            Some(n) => format!("{n} ({c})"),
            None => format!("contract error {c}"),
        },
        ErrorKind::Auth => "authorization rejected".into(),
        ErrorKind::Other(_) => {
            let o = kind.as_str();
            // The CLI prints an entire Rust debug struct for a failed submission; the
            // useful part is the inner operation result.
            if let Some(p) = o.find("InvokeHostFunction(") {
                let rest = &o[p + "InvokeHostFunction(".len()..];
                let code: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if !code.is_empty() {
                    return format!("transaction failed: {code}");
                }
            }
            let cleaned = o
                .trim_start_matches("❌")
                .trim()
                .trim_start_matches("error:")
                .trim();
            cleaned.lines().next().unwrap_or(cleaned).to_string()
        }
    }
}

pub fn parse_error(stderr: &str) -> Option<ErrorKind> {
    let message = terminal_error(stderr).unwrap_or(stderr.trim());
    let headline = message.lines().next().unwrap_or_default();
    let cause = headline
        .strip_prefix("transaction simulation failed: ")
        .or_else(|| headline.strip_prefix("transaction submission failed: "))
        .unwrap_or(headline);
    let cause = cause.strip_prefix("HostError: ").unwrap_or(cause);
    if let Some(rest) = cause.strip_prefix("Error(Contract, #") {
        return rest
            .split_once(')')?
            .0
            .parse()
            .ok()
            .map(ErrorKind::Contract);
    }
    if cause.starts_with("Error(Auth,") || cause.starts_with("Missing signing key") {
        return Some(ErrorKind::Auth);
    }
    if headline.starts_with("transaction submission failed:")
        || headline.starts_with("transaction simulation failed:")
    {
        if let Some((_, rest)) = message.split_once("InvokeHostFunction(") {
            let code = rest
                .trim_start()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or_default();
            if !code.is_empty() {
                return Some(ErrorKind::Other(Safe(format!("tx failed: {code}"))));
            }
        }
    }
    Some(ErrorKind::Other(Safe(headline.trim().to_string())))
}

/// A call's terminal outcome with the CLI's local-config deprecation banner
/// filtered out of the retained bytes. Raw custody remains in the type: the
/// retained value is a `RawText` regardless of how it was produced.
fn invocation_from(out: CmdOutput) -> Invocation {
    let stderr = RawText(out.stderr.0.map(|s| {
        s.lines()
            .filter(|l| {
                !l.contains("Local config is deprecated")
                    && !l.contains("A local config was found")
                    && !l.contains("stellar config migrate")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }));
    let events = parse_event_lines(stderr.pick());
    let value = if out.ok && !out.stdout.pick().is_empty() {
        serde_json::from_str(out.stdout.pick()).ok()
    } else {
        None
    };
    Invocation {
        ok: out.ok,
        value,
        error: if out.ok {
            None
        } else {
            parse_error(stderr.pick())
        },
        events,
        stderr,
    }
}

/// The single diagnostic `stellar contract read` emits for an absent key:
/// `NoContractDataEntryFoundForContractID`. Grounded against the shipped
/// binary (`stellar 26.1.0 (1228cff)`) and the stellar-cli v26.1.0 tag's
/// cmd/soroban-cli/src/commands/contract/read.rs; treat any other stderr
/// (RPC, config, contract lookup, generic "not found") as an error —
/// reading those as absence would report a storage gap the network never
/// confirmed. Shared with the custody fake in run.rs so fake and matcher
/// cannot drift.
pub const CLI_MISSING_ENTRY: &str =
    "no matching contract data entries were found for the specified contract id";

fn is_missing_entry(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains(CLI_MISSING_ENTRY)
}

/// Parse one or more `key,val,last_modified[,live_until]` XDR rows. Every
/// column must be strictly base64-decodable, and the ledger columns must
/// decode to exactly four bytes. Malformed input is fail-closed: an absent
/// deadline is an *empty* column, never a silent zero, and a truncated
/// timestamp can no longer be laundered into `last_modified`.
pub fn parse_read_xdr(policy: Policy, stdout: &str) -> anyhow::Result<Vec<RawEntry>> {
    let bad = |line: &str| {
        anyhow!(
            "{}",
            Safe::pair_ctx(policy, PairCtx::UnparseableReadRow(line))
        )
    };
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        if cols.len() < 3 {
            return Err(bad(line));
        }
        for col in [&cols[0], &cols[1]] {
            let decoded = base64_decode(col).ok_or_else(|| bad(line))?;
            if decoded.is_empty() || decoded.len() % 4 != 0 {
                return Err(bad(line));
            }
        }
        for col in cols.iter().skip(2) {
            if base64_decode(col).is_none() {
                return Err(bad(line));
            }
        }
        let last_modified = xdr_u32(cols[2]).ok_or_else(|| bad(line))?;
        let live_until = match cols.get(3) {
            None => None,
            Some(col) if col.trim().is_empty() => None,
            Some(col) => Some(xdr_u32(col).ok_or_else(|| bad(line))?),
        };
        rows.push(RawEntry {
            key: cols[0].to_string(),
            val: cols[1].to_string(),
            last_modified,
            live_until,
        });
    }
    Ok(rows)
}

fn xdr_u32(b64: &str) -> Option<u32> {
    let bytes = base64_decode(b64)?;
    if bytes.len() != 4 {
        return None;
    }
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Whether a base64 XDR value the caller constructed is the bytes the network
/// stored. Decoded bytes are compared, never the base64 text: the two sides
/// reach the caller through different renderings of the same encoder, and
/// text equality would make a stored entry look absent for a reason that has
/// nothing to do with the ledger.
pub fn xdr_entry_matches(encoded: &str, stored: &str) -> bool {
    matches!((base64_decode(encoded), base64_decode(stored)), (Some(want), Some(found)) if want == found)
}

/// The trailing XDR `u32` of a stored value whose every earlier byte is the
/// caller's encoding: a `(.., u32)` tuple whose last field is a deadline only
/// the network knows. `None` when anything before it differs.
pub fn xdr_entry_tail_u32(encoded: &str, stored: &str) -> Option<u32> {
    let (want, found) = (base64_decode(encoded)?, base64_decode(stored)?);
    let cut = found.len().checked_sub(4)?;
    if want.len() != found.len() || want[..cut] != found[..cut] {
        return None;
    }
    Some(u32::from_be_bytes([found[cut], found[cut + 1], found[cut + 2], found[cut + 3]]))
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let encoded = input.trim_end_matches('=');
    let padding = input.len() - encoded.len();
    match (encoded.len() % 4, padding) {
        (0, 0) | (2, 0 | 2) | (3, 0 | 1) => {}
        _ => return None,
    }
    let mut out = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for c in encoded.bytes() {
        let v = T.iter().position(|&t| t == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    (buf == 0).then_some(out)
}

/// Strict base64 of a nonzero whole number of 4-byte XDR words: the encoded
/// ScVal from `stellar xdr encode`. Validated at construction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
pub struct ScvalB64(String);

impl ScvalB64 {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for ScvalB64 {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
            return Err("invalid scval");
        }
        let bytes = base64_decode(s).ok_or("invalid scval")?;
        if bytes.is_empty() || bytes.len() % 4 != 0 {
            return Err("invalid scval");
        }
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for ScvalB64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for ScvalB64 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;

    /// Scripted executor: each call pops the next output; argv is recorded.
    pub struct FakeExecutor {
        pub outputs: RefCell<Vec<CmdOutput>>,
        pub calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeExecutor {
        pub fn new(outputs: Vec<CmdOutput>) -> Self {
            Self {
                outputs: RefCell::new(outputs.into_iter().rev().collect()),
                calls: RefCell::new(vec![]),
            }
        }
        pub fn ok(stdout: &str, stderr: &str) -> CmdOutput {
            CmdOutput {
                stdout: RawText(Some(stdout.to_string())),
                stderr: RawText(Some(stderr.to_string())),
                ok: true,
            }
        }
        pub fn fail(stderr: &str) -> CmdOutput {
            CmdOutput {
                stdout: RawText(Some(String::new())),
                stderr: RawText(Some(stderr.to_string())),
                ok: false,
            }
        }
    }

    impl Executor for FakeExecutor {
        fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput> {
            self.calls.borrow_mut().push(args.to_vec());
            self.outputs
                .borrow_mut()
                .pop()
                .context("fake executor: no scripted output left")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeExecutor;
    use super::*;

    fn net() -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: "pp".into(),
            config_dir: "/cfg".into(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn parses_event_line() {
        let line = "🌎 Sending transaction…\n📅 CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT - Success - Event: [{\"symbol\":\"mint\"},{\"address\":\"GB2H\"}] = {\"i128\":\"123\"}";
        let ev = parse_event_lines(line);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].topics[0]["symbol"], "mint");
        assert_eq!(ev[0].value["i128"], "123");
    }

    #[test]
    fn market_view_errors_keep_only_trusted_bounded_diagnostics() {
        let cases = [
            (
                "error: transaction simulation failed: HostError: Error(Contract, #1000)",
                "BadRequest (1000)",
            ),
            (
                "error: transaction simulation failed: HostError: Error(Contract, #1200)",
                "BadRequest (1200)",
            ),
            (
                "error: transaction simulation failed: HostError: Error(Contract, #9999)",
                "unknown contract error (9999)",
            ),
            (
                "error: transaction simulation failed: Error(Auth, InvalidAction)",
                "authorization was rejected",
            ),
        ];
        for (stderr, expected) in cases {
            let n = net();
            let fx = FakeExecutor::new(vec![FakeExecutor::fail(stderr)]);
            let st = Stellar::new(&n, &fx, Policy::Market);
            let error = st
                .view("C1", "user", "operation", &[])
                .expect_err("native simulation rejection stays an error");
            let diagnostic = Safe::market_error(&error).to_string();
            assert!(
                diagnostic.contains(expected),
                "trusted classification missing in {diagnostic}"
            );
            assert!(!diagnostic.contains(stderr), "stderr must not be surfaced");
            assert!(fx.calls.borrow()[0]
                .windows(2)
                .any(|w| w[0] == "--send" && w[1] == "no"));
        }

        let hostile = "password=/tmp/private-key canary stderr";
        let n = net();
        let fx = FakeExecutor::new(vec![FakeExecutor::fail(hostile)]);
        let st = Stellar::new(&n, &fx, Policy::Market);
        let error = st
            .view("C1", "user", "operation", &[])
            .expect_err("unclassified CLI error is still a failure");
        let diagnostic = Safe::market_error(&error).to_string();

        assert!(!diagnostic.contains("password"));
        assert!(!diagnostic.contains("private-key"));

        let untyped = anyhow::anyhow!("Error(Contract, #1000) {hostile}");
        let diagnostic = Safe::market_error(&untyped).to_string();
        assert!(
            !diagnostic.contains("1000"),
            "untyped text cannot prove a contract code"
        );
        assert!(!diagnostic.contains("password"));
        assert!(!diagnostic.contains("private-key"));

        let metadata = Safe::render_market_json(
            &serde_json::json!("unsupported ABI type \u{009b}31m\u{0085}"),
            4096,
        )
        .unwrap();
        let metadata_error = anyhow::Error::msg(metadata).context(hostile);
        let diagnostic = Safe::market_error(&metadata_error).to_string();
        assert!(!diagnostic.chars().any(char::is_control));
        assert!(!diagnostic.contains("private-key"));
    }

    #[test]
    fn terminal_projection_keeps_json_inspectable_and_controls_inert() {
        let value = serde_json::json!({
            "operation": "backstop/distribute",
            "request": {"amount": "170141183460469231731687303715884105727"},
            "note": "a\\b\"c"
        });
        let pretty = serde_json::to_string_pretty(&value).unwrap();
        let displayed = Safe::render_terminal(&pretty, 4096, true).to_string();
        assert!(displayed.lines().count() > 1);
        assert_eq!(serde_json::from_str::<Value>(&displayed).unwrap(), value);

        let displayed =
            Safe::render_terminal("first\n\x1b[31m\u{009b}\u{0085}last", 4096, true).to_string();
        assert_eq!(displayed.lines().count(), 2);
        assert!(!displayed.chars().any(|ch| ch.is_control() && ch != '\n'));
        assert!(displayed.contains("\\u001b") && displayed.contains("\\u009b"));

        let over_cap =
            Safe::render_terminal("PRIVATE-VALUE-MUST-NOT-BE-PARTIALLY-DISPLAYED", 8, false);
        assert!(!over_cap.to_string().contains("PRIVATE"));
    }

    #[test]
    fn parses_contract_error_code() {
        assert_eq!(
            parse_error("transaction simulation failed: HostError: Error(Contract, #1200)"),
            Some(ErrorKind::Contract(1200))
        );
        assert_eq!(
            parse_error("❌ error: Missing signing key for account G..."),
            Some(ErrorKind::Auth)
        );
    }

    #[test]
    fn parses_ids_and_hashes() {
        assert_eq!(
            parse_contract_id(
                "✅ Deployed!\nCBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT"
            )
            .unwrap()
            .len(),
            56
        );
        assert_eq!(
            parse_hash(
                "ℹ️ Skipping\ndd77fe6e7554cdf3d4ab32730fc2c9fb1039f0b16c5ed99769ae62b9809801f1"
            )
            .unwrap()
            .len(),
            64
        );
        assert_eq!(
            parse_major(Policy::Pair, "stellar 26.0.0 (60f7458)").unwrap(),
            26
        );
    }

    #[test]
    fn contract_id_serde_accepts_valid_id_and_roundtrips() {
        let id: ContractId =
            serde_json::from_str("\"CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT\"")
                .expect("valid contract id must deserialize unchanged");
        assert_eq!(id, market_id());
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT\""
        );
    }

    #[test]
    fn contract_id_serde_rejects_forged_checksum() {
        let forged = "\"CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AU\"";
        assert!(serde_json::from_str::<ContractId>(forged).is_err());
        assert!(
            ContractId::from_str("CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AU")
                .is_err()
        );
    }

    #[test]
    fn contract_id_serde_rejects_terminal_controls() {
        let raw = String::from("CBEF4D\u{1b}EOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT");
        assert_eq!(raw.chars().count(), 56);
        let json = serde_json::to_string(&raw).unwrap();
        assert!(serde_json::from_str::<ContractId>(&json).is_err());
        assert!(ContractId::from_str(&raw).is_err());
    }

    #[test]
    fn invoke_returns_value_events_and_error_as_data() {
        let n = net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("\"123\"", "📅 CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT - Success - Event: [{\"symbol\":\"mint\"}] = {\"i128\":\"123\"}"),
            FakeExecutor::fail("HostError: Error(Contract, #1200)"),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Pair);
        let a = st
            .invoke(
                "C1",
                "admin",
                "mint",
                &[("to", "G".into()), ("amount", "123".into())],
            )
            .unwrap();
        assert!(a.ok && a.value == Some(Value::String("123".into())) && a.events.len() == 1);
        let b = st.invoke("C1", "user", "update_pool", &[]).unwrap();
        assert!(!b.ok && b.error == Some(ErrorKind::Contract(1200)));
        let calls = fx.calls.borrow();
        assert_eq!(&calls[0][..4], &["contract", "invoke", "--id", "C1"]);
        assert!(calls[0].iter().any(|x| x == "--amount"));
        assert!(calls[0].contains(&"--config-dir".to_string()));
    }

    fn no_sleep(_: std::time::Duration) {}

    fn st_with<'a>(n: &'a Net, fx: &'a FakeExecutor) -> Stellar<'a, FakeExecutor> {
        let mut st = Stellar::new(n, fx, Policy::Pair);
        st.sleep = no_sleep;
        st
    }

    #[test]
    fn transient_read_is_retried() {
        let n = net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::fail("error: Networking or low-level protocol error: Server returned an error status code: 502"),
            FakeExecutor::ok("\"123\"", ""),
        ]);
        let st = st_with(&n, &fx);
        let v = st.view("C1", "user", "balance", &[]).unwrap();
        assert_eq!(v, Value::String("123".into()));
        assert_eq!(fx.calls.borrow().len(), 2);
    }

    #[test]
    fn contract_error_is_never_retried() {
        let n = net();
        let fx = FakeExecutor::new(vec![FakeExecutor::fail(
            "transaction simulation failed: HostError: Error(Contract, #1200)",
        )]);
        let st = st_with(&n, &fx);
        let inv = st.invoke("C1", "user", "update_pool", &[]).unwrap();
        assert_eq!(inv.error, Some(ErrorKind::Contract(1200)));
        assert_eq!(fx.calls.borrow().len(), 1);
    }

    /// The custody seam, in both directions. Rejections the host or chain
    /// explained are recorded outcomes — including the framings the retained runs
    /// actually carry — while an unclassified failure leaves inclusion unknown and
    /// surfaces as an execution error, so the pending journal (not evidence) holds
    /// custody. Rejection fixtures are the arms of `InvokeHostFunctionResult`
    /// (stellar-xdr 26.0.0) plus the CLI's host-error display form; the negative
    /// set includes the union's `Success` arm, which is an APPLIED call and so can
    /// never settle an ambiguous failure.
    #[test]
    fn definite_rejections_are_outcomes_and_unclassified_ones_are_not() {
        let n = net();
        for stderr in [
            "error: Error(Auth, InvalidAction)",
            "error: transaction submission failed: InvokeHostFunction(Trapped)",
            "error: transaction simulation failed: InvokeHostFunction(Malformed)",
            "error: transaction submission failed: InvokeHostFunction(ResourceLimitExceeded)",
            "error: transaction submission failed: InvokeHostFunction(EntryArchived)",
            "error: transaction submission failed: InvokeHostFunction(InsufficientRefundableFee)",
            // Verbatim framing from runs/resume4/evidence/freeze/012-trap_pool_flash_loan/a.json
            "❌ error: transaction simulation failed: HostError: Error(WasmVm, InvalidAction)\n\nEvent log (newest first):\n   0: [Diagnostic Event] contract:CB3VC6RK7RAJLFXTUOSRUIZ4DPJEDQZH6GZ3OYALKLKAPQCHNLQGRRKZ, topics:[error, Error(WasmVm, InvalidAction)], data:[\"VM call trapped: UnreachableCodeReached\"]",
            "❌ error: transaction simulation failed: HostError: Error(Context, InvalidAction)\n\nEvent log (newest first):\n   0: [Diagnostic Event] contract:CB3VC6RK7RAJLFXTUOSRUIZ4DPJEDQZH6GZ3OYALKLKAPQCHNLQGRRKZ, topics:[error, Error(Context, InvalidAction)], data:\"Contract re-entry is not allowed\"",
            "❌ error: transaction submission failed: Some(\n TransactionResult {\n fee_charged: 22540,\n result: TxFailed(VecM([OpInner(InvokeHostFunction(ResourceLimitExceeded))]))",
        ] {
            let fx = FakeExecutor::new(vec![FakeExecutor::fail(stderr); RETRY_ATTEMPTS as usize]);
            let st = st_with(&n, &fx);
            let inv = st
                .invoke("C1", "user", "mint", &[])
                .unwrap_or_else(|e| panic!("{stderr:?} is a definite rejection: {e:#}"));
            assert!(!inv.ok, "{stderr:?} must be recorded as a rejection");
        }
        for stderr in [
            "error: 504 Gateway Time-out",
            "error: truncated response",
            // The host function applied; a transport failure around it says
            // nothing about whether the result was recorded.
            "error: transaction submission failed: InvokeHostFunction(Success(Hash(0000000000000000000000000000000000000000000000000000000000000000)))\nerror: request timed out",
            // Outside the closed union: may be either, so it settles nothing.
            "error: transaction submission failed: InvokeHostFunction(UnknownVariant)",
            // Not an arm of the union at all: nothing is settled by it.
            "error: transaction submission failed: InvokeHostFunction(OutOfMemory)",
            "[Diagnostic Event] topics:[error, Error(WasmVm, InvalidAction)]\nerror: request timed out",
            "error: transaction simulation failed: HostError: Error(WasmVm, InvalidAction)\nerror: request timed out",
            "InvokeHostFunction(Trapped)\nerror: request timed out",
            "[Diagnostic Event] topics:[error, Error(Contract, #1200)]\nerror: request timed out",
            "[Diagnostic Event] topics:[error, Error(Auth, InvalidAction)]\nerror: request timed out",
            "error: transaction simulation failed: InvokeHostFunction(ResourceLimitExceeded)\nerror: request timed out",
        ] {
            let fx = FakeExecutor::new(vec![FakeExecutor::fail(stderr)]);
            let st = st_with(&n, &fx);
            assert!(
                st.invoke("C1", "user", "mint", &[]).is_err(),
                "{stderr:?} leaves inclusion unknown and may not be an outcome"
            );
            assert_eq!(fx.calls.borrow().len(), 1, "{stderr:?} may not be replayed");
        }
        for stderr in [
            "error: Error(Contract, #1200)",
            "error: Error(Auth, InvalidAction)",
        ] {
            let mut retained = invocation_from(FakeExecutor::fail(stderr));
            assert!(definite_outcome(&retained));
            retained.stderr = RawText(Some(format!(
                "{}\nerror: request timed out",
                retained.stderr.pick()
            )));
            assert!(!definite_outcome(&retained));
            retained.stderr = RawText(Some(String::new()));
            assert!(!definite_outcome(&retained));
        }
        let mut retained = invocation_from(FakeExecutor::fail(
            "error: transaction submission failed: InvokeHostFunction(Trapped)",
        ));
        assert!(definite_outcome(&retained));
        retained.stderr = RawText(Some(
            "error: transaction submission failed: InvokeHostFunction(Success)".to_string(),
        ));
        assert!(!definite_outcome(&retained));
    }

    #[test]
    fn resource_limit_is_retried_on_a_mutating_call() {
        let n = net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::fail(
                "transaction submission failed: InvokeHostFunction(ResourceLimitExceeded)",
            ),
            FakeExecutor::ok("", ""),
        ]);
        let st = st_with(&n, &fx);
        assert!(st.invoke("C1", "user", "mint", &[]).unwrap().ok);
        assert_eq!(fx.calls.borrow().len(), 2);
    }

    /// The double-submit guard: a timeout may mean the tx landed, so a mutating call
    /// must not replay it, while the identical error on a simulation is safe to retry.
    #[test]
    fn ambiguous_error_retries_only_for_non_mutating_calls() {
        let n = net();
        let fx = FakeExecutor::new(vec![FakeExecutor::fail("error: request timed out")]);
        let st = st_with(&n, &fx);
        assert!(st.invoke("C1", "user", "mint", &[]).is_err());
        assert_eq!(fx.calls.borrow().len(), 1, "mutating call must not replay");

        let fx2 = FakeExecutor::new(vec![
            FakeExecutor::fail("error: request timed out"),
            FakeExecutor::ok("\"1\"", ""),
        ]);
        let st2 = st_with(&n, &fx2);
        st2.view("C1", "user", "balance", &[]).unwrap();
        assert_eq!(fx2.calls.borrow().len(), 2, "simulation is safe to retry");
    }

    /// `upload`, the factory `deploy` and friendbot funding also submit
    /// transactions; an ambiguous timeout must not replay any of them.
    #[test]
    fn ambiguous_error_never_replays_upload_or_deploy_or_funding() {
        let n = net();
        for argv in [
            vec!["contract", "upload", "--wasm", "pool.wasm"],
            vec!["contract", "deploy", "--wasm-hash", "abc", "--salt", "s"],
            vec!["keys", "generate", "abtest-rider", "--fund"],
        ] {
            let fx = FakeExecutor::new(vec![FakeExecutor::fail("error: request timed out")]);
            let st = st_with(&n, &fx);
            let out = st
                .run(argv.iter().map(|s| s.to_string()).collect())
                .unwrap();
            assert!(!out.ok);
            assert_eq!(fx.calls.borrow().len(), 1, "must not replay {argv:?}");
        }
    }

    /// A submitted invoke can land, so an ambiguous failure on it is a
    /// custody question. A simulated invoke cannot land, so it stays
    /// replayable. One word decides the difference.
    #[test]
    fn submitted_invoke_is_mutating_and_simulated_invoke_is_not() {
        let argv = |send: &str| -> Vec<String> {
            [
                "contract",
                "invoke",
                "--id",
                "C1",
                "--source",
                "frodo",
                "--send",
                send,
                "--",
                "get_last_price",
            ]
            .iter()
            .map(|arg| arg.to_string())
            .collect()
        };
        assert!(
            mutating(&argv("yes")),
            "`--send yes` can land a transaction"
        );
        assert!(
            !mutating(&argv("no")),
            "`--send no` is simulation and can never land"
        );
    }

    /// Gateway and truncated-response failures happen after the request may
    /// have been delivered; they are as ambiguous as a timeout. Pre-send
    /// failures (dns) remain replayable.
    #[test]
    fn gateway_and_truncated_errors_never_replay_mutations() {
        let n = net();
        let argv: Vec<String> = ["contract", "invoke", "--id", "C1", "--", "supply"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for err in [
            "error: HTTP status client error (429 Too Many Requests)",
            "error: 502 Bad Gateway",
            "error: 504 Gateway Time-out",
            "error: EOF while parsing a value at line 1 column 20",
            "error: rpc transport error",
        ] {
            let fx = FakeExecutor::new(vec![FakeExecutor::fail(err)]);
            let st = st_with(&n, &fx);
            let out = st.run(argv.clone()).unwrap();
            assert!(!out.ok);
            assert_eq!(
                fx.calls.borrow().len(),
                1,
                "{err:?} must not replay an invoke"
            );
        }
        // The real-world connect-phase shape: reqwest wraps the failure in an
        // "error sending request" envelope, yet it is provably unsent and must
        // stay replayable (a pre-send phrase outranks the ambiguous envelope).
        let fx = FakeExecutor::new(vec![
            FakeExecutor::fail(
                "error: error sending request for url (https://soroban-testnet.stellar.org/): \
                 error trying to connect: tcp connect error: Connection refused (os error 111)",
            );
            5
        ]);
        let st = st_with(&n, &fx);
        st.run(argv.clone()).unwrap();
        assert!(
            fx.calls.borrow().len() > 1,
            "a wrapped connect failure is pre-send and must replay"
        );
        // The request never left: replaying is safe and expected.
        let fx = FakeExecutor::new(vec![FakeExecutor::fail("error: dns error"); 5]);
        let st = st_with(&n, &fx);
        st.run(argv).unwrap();
        assert!(
            fx.calls.borrow().len() > 1,
            "pre-send failures must stay replayable"
        );
    }

    #[test]
    fn rate_limited_reads_back_off_and_stop_at_five_attempts() {
        std::thread_local! {
            static DELAYS: std::cell::RefCell<Vec<Duration>> = const {
                std::cell::RefCell::new(Vec::new())
            };
        }
        let n = net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::fail(
                "HTTP status client error (429 Too Many Requests)"
            );
            5
        ]);
        let mut st = Stellar::new(&n, &fx, Policy::Market);
        st.sleep = |delay| DELAYS.with(|delays| delays.borrow_mut().push(delay));
        assert!(st.view("C1", "user", "balance", &[]).is_err());
        assert_eq!(fx.calls.borrow().len(), 5);
        DELAYS.with(|delays| {
            assert_eq!(*delays.borrow(), [1, 2, 4, 8].map(Duration::from_secs));
        });
    }

    #[test]
    fn transient_classification() {
        for e in [
            "502 Bad Gateway",
            "Server returned an error status code: 429",
            "error sending request",
            "connection reset by peer",
            "ResourceLimitExceeded",
            "EOF while parsing a value",
        ] {
            assert!(
                is_transient(&FakeExecutor::fail(e)),
                "{e} should be transient"
            );
        }
        for e in [
            "Error(Contract, #1200)",
            "Error(Contract, #429)",
            "Missing signing key",
            "error: unexpected argument '--rpc-url' found",
        ] {
            assert!(
                !is_transient(&FakeExecutor::fail(e)),
                "{e} should not be transient"
            );
        }
    }

    #[test]
    fn every_known_code_has_a_name_and_an_explainer() {
        let codes: Vec<u32> = (1000..=1011).chain(1200..=1224).collect();
        for c in codes {
            assert!(error_name(c).is_some(), "{c} has no name");
            assert!(error_help(c).is_some(), "{c} has no explainer");
            let text = error_explained(&ErrorKind::Contract(c));
            assert!(
                text.contains(&c.to_string()) && text.contains(" — "),
                "{text}"
            );
        }
        // an unknown code degrades rather than lying
        assert_eq!(
            error_explained(&ErrorKind::Contract(4242)),
            "contract error 4242"
        );
        assert_eq!(
            error_explained(&ErrorKind::Contract(1214)),
            "InvalidLiqTooSmall (1214) — the liquidation is too small: it would leave the position under 1.03, so take more"
        );
    }

    #[test]
    fn submission_failures_lose_the_debug_dump() {
        fn other(payload: &str) -> ErrorKind {
            serde_json::from_value(serde_json::json!({ "Other": payload })).expect("fixture")
        }
        let raw = other(
            "❌ error: transaction submission failed: Some(\n TransactionResult {\n fee_charged: 22540,\n result: TxFailed(VecM([OpInner(InvokeHostFunction(ResourceLimitExceeded))]))",
        );
        assert_eq!(
            error_summary(&raw),
            "transaction failed: ResourceLimitExceeded"
        );
        assert_eq!(
            error_summary(&other("❌ error: something broke")),
            "something broke"
        );
        assert_eq!(error_summary(&ErrorKind::Auth), "authorization rejected");
    }

    /// The CLI's sole absence diagnostic maps to `None`; the older loose
    /// match on bare "not found" silently swallowed genuine lookup failures.
    #[test]
    fn read_raw_absent_is_none() {
        let n = net();
        let fx = FakeExecutor::new(vec![FakeExecutor::fail(
            // Verbatim upstream wording (stellar-cli v26.1.0 read.rs,
            // NoContractDataEntryFoundForContractID), kept as an independent
            // anchor: derived from the matcher's const, fixture and matcher
            // would drift together and pass on a wrong diagnostic.
            "error: no matching contract data entries were found for the specified contract id",
        )]);
        let st = Stellar::new(&n, &fx, Policy::Pair);
        assert!(st
            .read_raw("C1", RawKey::Symbol("PoolEmis".into()))
            .unwrap()
            .is_none());
    }

    /// A generic "not found" is NOT the CLI absence diagnostic: it must fail
    /// closed, never masquerade as a storage gap.
    #[test]
    fn generic_not_found_never_reads_as_absence() {
        for e in [
            "error: contract data entry not found",
            "error: ledger entry not found for key",
            "error: account not found",
        ] {
            let n = net();
            let fx = FakeExecutor::new(vec![FakeExecutor::fail(e)]);
            let st = st_with(&n, &fx);
            assert!(
                st.read_raw("C1", RawKey::Symbol("PoolEmis".into()))
                    .is_err(),
                "{e:?} must not read as absence"
            );
            assert_eq!(fx.calls.borrow().len(), 1, "{e:?} must not retry");
        }
    }

    /// RPC failure on a read stays an error after its retries are exhausted —
    /// never a silent `None`.
    #[test]
    fn read_raw_rpc_failure_fails_closed() {
        let n = net();
        let fx = FakeExecutor::new(vec![FakeExecutor::fail("error: rpc transport error"); 5]);
        let st = st_with(&n, &fx);
        assert!(st
            .read_raw("C1", RawKey::Symbol("PoolEmis".into()))
            .is_err());
        assert_eq!(fx.calls.borrow().len(), 5, "a read is safe to retry");
    }

    /// The temporary durability probe: exact argv, and the echoed key must be
    /// the one requested. A row for a different key is a corrupted answer.
    #[test]
    fn read_temp_requests_temporary_durability_and_checks_the_echoed_key() {
        let key = "AAAAAA==";
        let n = net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(&format!("{key},AAAAEw==,AEfhxg==,AFAORw=="), ""),
            FakeExecutor::ok("AAAARw==,AAAAEw==,AEfhxg==,AFAORw==", ""),
            FakeExecutor::ok("AAAASg==,AAAAEw==,AEfhxg==,AFAORw==", ""),
        ]);
        let st = st_with(&n, &fx);
        let row = st
            .read_temp("C1", RawKey::Xdr(key.into()))
            .unwrap()
            .expect("present");
        assert_eq!(row.key, key);
        assert_eq!(row.last_modified, 4_710_854);
        assert_eq!(row.live_until, Some(5_246_535));
        let calls = fx.calls.borrow();
        let i = calls[0].iter().position(|a| a == "--durability").unwrap();
        assert_eq!(calls[0][i + 1], "temporary");
        drop(calls);
        for wrong in ["AAAARw==", "AAAASg=="] {
            let err = st
                .read_temp("C1", RawKey::Xdr(key.into()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("different key"), "{wrong}: {err}");
        }
    }

    /// Symbol keys are echoed as ScVal XDR, not the request string: decode and
    /// require the same symbol, so a row for another key can never pass.
    #[test]
    fn read_raw_matches_echoed_symbol_key_against_request() {
        let n = net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("AAAADwAAAAhQb29sRW1pcw==,AAAAEw==,AEfhxg==,", ""),
            FakeExecutor::ok("AAAADwAAAAhQb29sRW1pdw==,AAAAEw==,AEfhxg==,", ""),
        ]);
        let st = st_with(&n, &fx);
        assert!(st
            .read_raw("C1", RawKey::Symbol("PoolEmis".into()))
            .unwrap()
            .is_some());
        assert!(st
            .read_raw("C1", RawKey::Symbol("PoolEmis".into()))
            .unwrap_err()
            .to_string()
            .contains("different key"));
    }

    /// A successful read returning no rows (or two) is not an absence; fail
    /// closed rather than guess.
    #[test]
    fn read_raw_rejects_empty_or_multiple_rows() {
        let n = net();
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("", ""),
            FakeExecutor::ok(
                "AAAAAA==,AAAAEw==,AEfhxg==,\nAAAAAA==,AAAAEw==,AEfhxg==,",
                "",
            ),
        ]);
        let st = st_with(&n, &fx);
        assert!(st.read_raw("C1", RawKey::Xdr("AAAAAA==".into())).is_err());
        assert!(st.read_raw("C1", RawKey::Xdr("AAAAAA==".into())).is_err());
    }

    #[test]
    fn parses_xdr_rows_and_ledger_numbers() {
        let rows = parse_read_xdr(Policy::Pair, "AAAAFA==,AAAAEw==,AEfhxg==,AFAORw==").unwrap();
        assert_eq!(base64_decode("AAAARw==").unwrap(), [0, 0, 0, 71]);
        assert_eq!(base64_decode("AAAAP"), None);
        assert_eq!(rows[0].last_modified, 4_710_854);
        assert_eq!(rows[0].live_until, Some(5_246_535));
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
    }

    fn market_net(passphrase: &str) -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: passphrase.to_string(),
            config_dir: "/cfg".into(),
            headers: Vec::new(),
        }
    }

    fn market_st<'a>(n: &'a Net, fx: &'a FakeExecutor) -> Stellar<'a, FakeExecutor> {
        let mut st = Stellar::new(n, fx, Policy::Market);
        st.sleep = no_sleep;
        st
    }

    fn market_id() -> ContractId {
        std::str::FromStr::from_str("CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT")
            .expect("fixture contract id")
    }

    /// Reject old, truncated and non-stable versions before any network call.
    #[test]
    fn market_version_gate_rejects_unsupported_versions_before_network() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        for line in [
            "stellar 22.6.0 (aaa1111)",
            "stellar 23.0.0 (bbb2222)",
            "stellar 24.5.1 (ccc3333)",
            "stellar 25.9.9 (ddd4444)",
            "stellar 26.0.0 (60f7458)",
            "stellar 26",
            "stellar 26.1",
            "stellar 26.1.0-rc.1",
            "stellar 26.01.0",
            "stellar 26.1.0.1",
            "stellar 4294967296.1.0",
            "stellar SECRET-INVALID-VERSION",
        ] {
            let fx = FakeExecutor::new(vec![FakeExecutor::ok(line, "")]);
            let st = market_st(&n, &fx);
            let err = st.assert_market_cli_version().unwrap_err().to_string();
            assert!(!err.contains("SECRET"), "raw CLI output leaked: {err}");
            let calls = fx.calls.borrow();
            assert_eq!(calls.len(), 1, "{line:?} may not reach the network");
            assert_eq!(calls[0], vec!["--version".to_string()]);
        }
        // The historical pair gate still accepts the rejected 26.0.0: the
        // gates are separate and neither relaxes the other.
        let pair_net = net();
        let pair_fx = FakeExecutor::new(vec![FakeExecutor::ok("stellar 26.0.0 (60f7458)", "")]);
        let pair_st = st_with(&pair_net, &pair_fx);
        assert_eq!(
            pair_st.assert_version().unwrap(),
            26,
            "the pair gate must keep accepting 26.0.0"
        );
    }

    /// The shipped 26.1.0 passes the market gate, and
    /// `contract_interface` re-runs the gate before every fetch, then
    /// retrieves the spec read-only under the explicit exact-testnet
    /// network args: no invoke, deploy, upload, or `--send` argv may ever
    /// appear, so discovery cannot mutate the chain.
    #[test]
    fn market_version_gate_accepts_26_and_interface_is_read_only() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        for (version, major) in [
            ("stellar 26.1.0 (1228cff)", 26),
            ("stellar 26.1.1", 26),
            ("stellar 26.2.0", 26),
            ("stellar 27.0.0", 27),
        ] {
            let fx = FakeExecutor::new(vec![
                FakeExecutor::ok(version, ""),
                FakeExecutor::ok(version, ""),
                FakeExecutor::ok(
                    r#"[{"function_v0":{"name":"get_data","inputs":[],"outputs":[],"doc":""}}]"#,
                    "",
                ),
            ]);
            let st = market_st(&n, &fx);
            assert_eq!(st.assert_market_cli_version().unwrap(), major, "{version}");
            let spec = st.contract_interface(&market_id()).unwrap();
            assert_eq!(spec[0]["function_v0"]["name"], "get_data");
            assert_eq!(
                spec,
                serde_json::from_str::<Value>(
                    r#"[{"function_v0":{"name":"get_data","inputs":[],"outputs":[],"doc":""}}]"#
                )
                .expect("fixture spec")
            );
            let calls = fx.calls.borrow();
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[0], vec!["--version".to_string()]);
            assert_eq!(calls[1], vec!["--version".to_string()]);
            assert_eq!(
                calls[2],
                s(&[
                    "contract",
                    "info",
                    "interface",
                    "--contract-id",
                    market_id().as_str(),
                    "--output",
                    "json",
                    "--network-passphrase",
                    crate::manifest::MARKET_TESTNET_PASSPHRASE,
                    "--config-dir",
                    "/cfg",
                ])
            );
            assert!(
                calls.iter().all(|argv| {
                    !argv
                        .iter()
                        .any(|a| a == "invoke" || a == "deploy" || a == "upload" || a == "--send")
                }),
                "spec discovery must never mutate the chain"
            );
        }
    }

    /// Malformed, non-entry-array, and over-cap spec output is rejected at
    /// this boundary with input-free messages: no raw CLI stdout and no
    /// fragment of the offending payload leaks into an error.
    #[test]
    fn contract_interface_rejects_malformed_non_array_and_oversize_specs() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        for stdout in [
            "SECRET-SPEC-FRAGMENT not json",
            "[SECRET-ENTRY,",
            "",
            "{\"kind\":\"function\",\"name\":\"SECRET-METHOD\"}",
            "[1,2,3]",
            "\"SECRET-STRING\"",
            "null",
        ] {
            let fx = FakeExecutor::new(vec![
                FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
                FakeExecutor::ok(stdout, ""),
            ]);
            let st = market_st(&n, &fx);
            let err = st.contract_interface(&market_id()).unwrap_err().to_string();
            assert!(
                err == "contract interface returned malformed spec"
                    || err == "contract interface spec is not an entry array",
                "{stdout:?} must be rejected: {err}"
            );
            assert!(!err.contains("SECRET"), "error leaked input: {err}");
            assert_eq!(fx.calls.borrow().len(), 2);
        }
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::ok(&"x".repeat(MARKET_CLI_SPEC_CAP + 1), ""),
        ]);
        let st = market_st(&n, &fx);
        assert_eq!(
            st.contract_interface(&market_id()).unwrap_err().to_string(),
            "contract interface spec exceeds the market cap"
        );
    }

    /// The seam is bound to market policy and the exact testnet and refuses
    /// locally: a pair context and a wrong-network market context produce
    /// errors with zero executor calls.
    #[test]
    fn market_seam_refuses_pair_policy_and_wrong_network_without_calls() {
        let pair = net();
        let fx = FakeExecutor::new(vec![]);
        let st = st_with(&pair, &fx);
        assert_eq!(
            st.assert_market_cli_version().unwrap_err().to_string(),
            "market CLI version gate requires market policy"
        );
        assert_eq!(
            st.contract_interface(&market_id()).unwrap_err().to_string(),
            "market CLI version gate requires market policy"
        );

        // An unknown network still refuses locally; the two known networks
        // (testnet and mainnet) are both accepted by the gate.
        let wrong = market_net("Private Unknown Network ; January 2024");
        let fx2 = FakeExecutor::new(vec![]);
        let st2 = market_st(&wrong, &fx2);
        assert_eq!(
            st2.assert_market_cli_version().unwrap_err().to_string(),
            "market CLI version gate requires a known network"
        );
        assert_eq!(
            st2.contract_interface(&market_id())
                .unwrap_err()
                .to_string(),
            "market CLI version gate requires a known network"
        );
        assert!(fx.calls.borrow().is_empty());
        assert!(fx2.calls.borrow().is_empty());
    }

    #[test]
    fn account_id_serde_accepts_valid_id_and_roundtrips() {
        const G_VALID: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
        let id: AccountId = serde_json::from_str(&format!("\"{G_VALID}\""))
            .expect("valid account id must deserialize");
        assert_eq!(id.as_str(), G_VALID);
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            format!("\"{G_VALID}\"")
        );
    }

    #[test]
    fn account_id_serde_rejects_forged_checksum() {
        let forged = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHG";
        assert!(serde_json::from_str::<AccountId>(&format!("\"{forged}\"")).is_err());
        assert!(forged.parse::<AccountId>().is_err());
    }

    #[test]
    fn account_id_serde_rejects_terminal_controls() {
        let mut raw = String::from("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF");
        raw.replace_range(1..2, "\u{1b}");
        assert_eq!(raw.chars().count(), 56);
        let json = serde_json::to_string(&raw).unwrap();
        assert!(serde_json::from_str::<AccountId>(&json).is_err());
        assert!(raw.parse::<AccountId>().is_err());
    }

    #[test]
    fn account_id_serde_rejects_oversized_identity() {
        let oversized = format!("G{}", "A".repeat(4000));
        let json = serde_json::to_string(&oversized).unwrap();
        assert!(serde_json::from_str::<AccountId>(&json).is_err());
    }

    /// A failed interface command surfaces only the policy-safe summary;
    /// the CLI's stderr never leaks through the error.
    #[test]
    fn contract_interface_failure_never_leaks_cli_output() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::fail("error: fake contract SECRET-CONTRACT was not found"),
        ]);
        let st = market_st(&n, &fx);
        let err = st.contract_interface(&market_id()).unwrap_err().to_string();
        assert_eq!(err, "stellar command failed");
        assert!(!err.contains("SECRET") && !err.contains("not found"));
    }

    /// Exit 0 with non-empty unparseable stdout is a malformed contract
    /// response for both market seams, never a laundered null, and the
    /// sentinel never reaches the error text. Truly empty void output
    /// still reports null.
    #[test]
    fn market_view_rejects_malformed_success_and_keeps_void_law() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("SECRET-STDOUT not json", ""),
            FakeExecutor::ok("SECRET-STDOUT not json", ""),
            FakeExecutor::ok("", ""),
        ]);
        let st = market_st(&n, &fx);
        let err = st
            .view(market_id().as_str(), "user", "lastprice", &[])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "malformed contract response");
        assert!(!err.contains("SECRET"), "error leaked stdout: {err}");
        let err = st
            .try_view(market_id().as_str(), "user", "lastprice", &[])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "malformed contract response");
        assert_eq!(
            st.view(market_id().as_str(), "user", "get_status", &[])
                .unwrap(),
            Value::Null,
            "empty void output must still report null"
        );
    }

    /// A failed market view never reflects stderr or argv.
    #[test]
    fn market_view_failure_is_input_free() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::fail(
                "error: failed to invoke SECRET-ARG-VALUE with https://rpc.example/?token=SECRET-URL",
            );
            RETRY_ATTEMPTS as usize
        ]);
        let st = market_st(&n, &fx);
        let err = st
            .view(market_id().as_str(), "user", "lastprice", &[])
            .unwrap_err()
            .to_string();
        assert!(!err.contains("SECRET"), "error leaked stderr: {err}");
        assert!(!err.contains("rpc.example"), "error leaked URL: {err}");
    }

    /// A market executor start failure cannot disclose the bin path or
    /// argv: the fixed sentence is all that surfaces.
    #[test]
    fn market_executor_start_failure_is_input_free() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let exec = RealExecutor {
            bin: PathBuf::from("abtest-SECRET-BIN-missing-stellar"),
            xdg: std::env::temp_dir().join("abtest-market-spawn-leak-test"),
            cwd: PathBuf::from("/"),
            capture: Capture::Market {
                stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
                stderr_cap: MARKET_CAPTURE_STDERR_CAP,
                wall: Some(MARKET_READ_WALL),
            },
            rpc_env: Vec::new(),
        };
        let st = Stellar::new(&n, &exec, Policy::Market);
        let err = st
            .view(market_id().as_str(), "user", "lastprice", &[])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "stellar command failed");
        assert!(!err.contains("SECRET"), "error leaked argv: {err}");
    }

    #[cfg(all(test, target_os = "linux"))]
    mod bounded_market_capture {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;

        struct ScriptFixture {
            scratch: std::path::PathBuf,
            script: std::path::PathBuf,
            xdg: std::path::PathBuf,
        }

        fn market_executor(
            fixture: &ScriptFixture,
            stdout_cap: usize,
            stderr_cap: usize,
            wall: Duration,
        ) -> RealExecutor {
            RealExecutor {
                bin: fixture.script.clone(),
                xdg: fixture.xdg.clone(),
                cwd: PathBuf::from("/"),
                capture: Capture::Market {
                    stdout_cap,
                    stderr_cap,
                    wall: Some(wall),
                },
                rpc_env: Vec::new(),
            }
        }

        fn script_fixture(tag: &str, script: &str) -> ScriptFixture {
            let scratch = std::env::temp_dir().join(format!(
                "abtest-bounded-market-capture-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch).expect("create bounded-capture fixture");
            std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700))
                .expect("private bounded-capture fixture");
            // Market children run only from the controlled root (SEC001),
            // so the recorded pid marker is written at an absolute path
            // inside the fixture directory, never relative to the child.
            let script = script.replace(
                "child.pid",
                &scratch.join(CHILD_PID_FILE).display().to_string(),
            );
            let script_path = scratch.join("script.sh");
            std::fs::write(&script_path, script).expect("write bounded-capture script");
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
                .expect("executable bounded-capture script");
            let syntax = std::process::Command::new("/bin/sh")
                .arg("-n")
                .arg(&script_path)
                .stdin(Stdio::null())
                .output()
                .expect("syntax check bounded-capture script");
            assert!(
                syntax.status.success(),
                "bounded-capture script syntax: {}",
                String::from_utf8_lossy(&syntax.stderr)
            );
            let xdg = scratch.join("xdg");
            ScriptFixture {
                scratch,
                script: script_path,
                xdg,
            }
        }

        /// Each test script records its own process id here before it does
        /// any work. The marker is the only proof that a script ran: a
        /// missing or malformed marker fails the test instead of passing
        /// vacuously.
        const CHILD_PID_FILE: &str = "child.pid";

        /// Remove a stale marker before a run so an old pid cannot stand in
        /// for a child that never started.
        fn clear_child_pid_marker(fixture: &ScriptFixture) {
            let _ = std::fs::remove_file(fixture.scratch.join(CHILD_PID_FILE));
        }

        /// Assert the recorded direct child ran and is gone. Bounded capture
        /// kills and reaps its direct child before returning, so a live or
        /// unreaped process is a defect and not a race to wait out. A record
        /// belonging to an unrelated process fails the same way, so pid
        /// reuse cannot mask a leak.
        fn assert_recorded_child_ceased(fixture: &ScriptFixture) {
            let marker = fixture.scratch.join(CHILD_PID_FILE);
            let recorded = std::fs::read_to_string(&marker).unwrap_or_else(|_| {
                panic!(
                    "test script never recorded a child pid: {}",
                    marker.display()
                )
            });
            let pid = recorded.trim().parse::<u32>().unwrap_or_else(|_| {
                panic!("test script recorded an unusable child pid: {recorded:?}")
            });
            assert!(
                pid > 1 && pid != std::process::id(),
                "test script recorded an implausible child pid {pid}"
            );
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return;
            }
            let state = std::fs::read(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let stat = String::from_utf8_lossy(&stat);
                    let (_, rest) = stat.rsplit_once(')')?;
                    rest.split_whitespace().next().map(str::to_string)
                });
            match state.as_deref() {
                Some("Z") => panic!("bounded-market child {pid} remained an unreaped zombie"),
                Some(state) => {
                    panic!("bounded-market child {pid} remained running in state {state}")
                }
                None => panic!("bounded-market child {pid} remained present"),
            }
        }

        /// Bounded capture returns the same bytes, stream boundaries, and
        /// exit status as an unbounded run of the same command. Each recorded
        /// child is proven to have run and to have ceased before and after
        /// the unbounded comparison.
        #[test]
        fn market_bounded_capture_matches_unbounded_output_bytes() {
            let fixture = script_fixture(
                "equivalence",
                r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
printf 'MARKET-STDOUT-A\n'
printf 'MARKET-STDERR-B\n' >&2
exit 3
"#,
            );
            let executor = market_executor(
                &fixture,
                MARKET_CAPTURE_STDOUT_CAP,
                MARKET_CAPTURE_STDERR_CAP,
                MARKET_READ_WALL,
            );
            clear_child_pid_marker(&fixture);
            let bounded = Executor::run(&executor, &["market".into(), "status".into()])
                .expect("bounded market capture must accept normal output");
            assert_recorded_child_ceased(&fixture);
            clear_child_pid_marker(&fixture);
            let baseline = std::process::Command::new(&fixture.script)
                .args(["market", "status"])
                .env_remove("STELLAR_CONTRACT_ID")
                .env("XDG_CONFIG_HOME", &fixture.xdg)
                .env(
                    "HOME",
                    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
                )
                .current_dir(&fixture.scratch)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("run unbounded comparison command");
            assert_recorded_child_ceased(&fixture);
            assert_eq!(
                bounded.stdout.pick().as_bytes(),
                String::from_utf8_lossy(&baseline.stdout).trim().as_bytes()
            );
            assert_eq!(
                bounded.stderr.pick().as_bytes(),
                String::from_utf8_lossy(&baseline.stderr).trim().as_bytes()
            );
            assert_eq!(bounded.ok(), baseline.status.success());
            assert!(!bounded.ok());
            let _ = std::fs::remove_dir_all(&fixture.scratch);
        }

        /// Output at exactly each cap is retained and reported with the
        /// child's real exit status; one byte over either cap is refused
        /// with the fixed input-free overflow sentence and never echoes the
        /// payload.
        #[test]
        fn market_bounded_capture_accepts_exact_cap_and_refunds_one_byte_over() {
            let fixture = script_fixture(
                "cap-exact",
                r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
printf '0123456789abcdef'
printf '01234567' >&2
exit 3
"#,
            );
            clear_child_pid_marker(&fixture);
            let bounded = Executor::run(
                &market_executor(&fixture, 16, 8, Duration::from_secs(10)),
                &["market".into(), "status".into()],
            )
            .expect("output at exactly each cap must be captured");
            assert_recorded_child_ceased(&fixture);
            assert_eq!(bounded.stdout.pick().as_bytes(), b"0123456789abcdef");
            assert_eq!(bounded.stderr.pick().as_bytes(), b"01234567");
            assert!(!bounded.ok(), "child exit status must be reported");
            let _ = std::fs::remove_dir_all(&fixture.scratch);

            for (tag, script) in [
                (
                    "stdout-cap-plus-one",
                    r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
printf '0123456789abcdef0'
"#,
                ),
                (
                    "stderr-cap-plus-one",
                    r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
printf '012345678' >&2
"#,
                ),
            ] {
                let fixture = script_fixture(tag, script);
                clear_child_pid_marker(&fixture);
                let refused = Executor::run(
                    &market_executor(&fixture, 16, 8, Duration::from_secs(10)),
                    &["market".into(), "status".into()],
                );
                let err = match refused {
                    Err(err) => err.to_string(),
                    Ok(_) => panic!("{tag} must be refused one byte over its cap"),
                };
                assert_eq!(
                    err, MARKET_CAPTURE_OVERFLOW,
                    "{tag} must receive the input-free overflow refusal"
                );
                assert!(
                    !err.contains("0123456789") && !err.contains("SECRET"),
                    "{tag} refusal leaked output bytes: {err}"
                );
                assert_recorded_child_ceased(&fixture);
                let _ = std::fs::remove_dir_all(&fixture.scratch);
            }
        }

        /// A flooding direct child is refused promptly with the fixed
        /// input-free overflow sentence and leaves no live or unreaped
        /// process. Every script records its own pid before flooding, so the
        /// refusal is proven against a child that demonstrably ran.
        #[test]
        fn market_bounded_capture_refunds_output_floods_promptly() {
            for (tag, script) in [
                (
                    "stdout-flood",
                    r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
while :; do printf 'FLOOD-STDOUT-SECRET'; done
"#,
                ),
                (
                    "stderr-flood",
                    r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
while :; do printf 'FLOOD-STDERR-SECRET' >&2; done
"#,
                ),
                (
                    "alternating-flood",
                    r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
while :; do
    printf 'FLOOD-STDOUT-SECRET'
    printf 'FLOOD-STDERR-SECRET' >&2
done
"#,
                ),
            ] {
                let fixture = script_fixture(tag, script);
                let executor = market_executor(&fixture, 4096, 1024, Duration::from_secs(10));
                clear_child_pid_marker(&fixture);
                let started = Instant::now();
                let result = Executor::run(&executor, &["market".into(), "status".into()]);
                let elapsed = started.elapsed();
                let err = match result {
                    Err(err) => err.to_string(),
                    Ok(_) => panic!("{tag} flood must be refused"),
                };
                assert_eq!(
                    err, MARKET_CAPTURE_OVERFLOW,
                    "{tag} flood must receive the input-free overflow refusal"
                );
                assert!(
                    !err.contains("FLOOD") && !err.contains("SECRET"),
                    "{tag} refusal leaked the flood payload: {err}"
                );
                assert!(
                    elapsed < Duration::from_secs(5),
                    "{tag} flood returned after {elapsed:?}"
                );
                assert_recorded_child_ceased(&fixture);
                let _ = std::fs::remove_dir_all(&fixture.scratch);
            }
        }

        /// A silent direct child is killed at the wall limit and the refusal
        /// stays input-free. The child records its own pid, so expiry is
        /// proven against a process that ran and then ceased.
        /// The child blocks inside the shell itself, so the recorded pid stays
        /// alive and silent until the wall kill runs.
        #[test]
        fn market_bounded_capture_kills_silent_child_at_wall() {
            let fixture = script_fixture(
                "wall-timeout",
                r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
while :; do :; done
"#,
            );
            let executor = market_executor(
                &fixture,
                MARKET_CAPTURE_STDOUT_CAP,
                MARKET_CAPTURE_STDERR_CAP,
                Duration::from_secs(1),
            );
            clear_child_pid_marker(&fixture);
            let started = Instant::now();
            let result = Executor::run(&executor, &["market".into(), "status".into()]);
            let elapsed = started.elapsed();
            assert!(result.is_err(), "silent market child must expire");
            assert!(
                elapsed < Duration::from_secs(5),
                "silent market child returned after {elapsed:?}"
            );
            assert!(
                elapsed >= Duration::from_secs(1),
                "wall expiry must be refused only after the wall limit: {elapsed:?}"
            );
            assert_eq!(
                result.unwrap_err().to_string(),
                MARKET_CAPTURE_FAILED,
                "wall expiry must be refused input-free"
            );
            assert_recorded_child_ceased(&fixture);
            let _ = std::fs::remove_dir_all(&fixture.scratch);
        }

        /// A child that closes both output streams while it is still
        /// running is not a failed process: bounded capture keeps waiting
        /// for completion under the wall limit, returns the bytes captured
        /// before closure, and reports the child's real exit status.
        #[test]
        fn market_bounded_capture_waits_for_child_after_early_eof() {
            let fixture = script_fixture(
                "early-eof-success",
                r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
printf 'MARKET-STDOUT-BEFORE-EOF\n'
printf 'MARKET-STDERR-BEFORE-EOF\n' >&2
exec 1>&- 2>&-
sleep 1
exit 0
"#,
            );
            let executor = market_executor(
                &fixture,
                MARKET_CAPTURE_STDOUT_CAP,
                MARKET_CAPTURE_STDERR_CAP,
                MARKET_READ_WALL,
            );
            clear_child_pid_marker(&fixture);
            let bounded = Executor::run(&executor, &["market".into(), "status".into()])
                .expect("early EOF while the child runs must not fail the capture");
            assert_recorded_child_ceased(&fixture);
            assert_eq!(
                bounded.stdout.pick().as_bytes(),
                b"MARKET-STDOUT-BEFORE-EOF",
                "bytes captured before early EOF must be returned"
            );
            assert_eq!(
                bounded.stderr.pick().as_bytes(),
                b"MARKET-STDERR-BEFORE-EOF",
                "bytes captured before early EOF must be returned"
            );
            assert!(
                bounded.ok(),
                "the child's successful exit status must be reported"
            );
            let _ = std::fs::remove_dir_all(&fixture.scratch);
        }

        /// Closing both output streams cannot bypass the wall limit: a
        /// child that goes silent at EOF and keeps running is killed at
        /// the wall and refused input-free, never accepted early.
        #[test]
        fn market_bounded_capture_kills_early_eof_child_at_wall() {
            let fixture = script_fixture(
                "early-eof-wall-timeout",
                r#"#!/bin/sh
printf '%s\n' "$$" > child.pid
printf 'MARKET-STDOUT-BEFORE-EOF\n'
printf 'MARKET-STDERR-BEFORE-EOF\n' >&2
exec 1>&- 2>&-
while :; do :; done
"#,
            );
            let executor = market_executor(
                &fixture,
                MARKET_CAPTURE_STDOUT_CAP,
                MARKET_CAPTURE_STDERR_CAP,
                Duration::from_secs(1),
            );
            clear_child_pid_marker(&fixture);
            let started = Instant::now();
            let result = Executor::run(&executor, &["market".into(), "status".into()]);
            let elapsed = started.elapsed();
            assert!(
                result.is_err(),
                "an early-EOF child that keeps running must expire"
            );
            assert!(
                elapsed >= Duration::from_secs(1),
                "early EOF must not shorten the wall limit: {elapsed:?}"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "early-EOF child returned after {elapsed:?}"
            );
            assert_eq!(
                result.unwrap_err().to_string(),
                MARKET_CAPTURE_FAILED,
                "wall expiry must be refused input-free"
            );
            assert_recorded_child_ceased(&fixture);
            let _ = std::fs::remove_dir_all(&fixture.scratch);
        }
    }

    /// The checked market renderer round-trips pipe, backtick, and
    /// control bytes through JSON Unicode escapes: the output stays
    /// parseable and decodes to the identical value, with no raw control
    /// or ANSI bytes. The pair arm keeps its historical bytes unchanged.
    #[test]
    fn market_json_render_roundtrips_escapes_without_control_bytes() {
        let v = serde_json::json!({"note": "a|b`c\u{1b}[31mred\u{7f}\u{009b}\u{0085}end"});
        let rendered = Safe::render_market_json(&v, 4096).expect("under cap");
        let text = rendered.to_string();
        assert!(
            !text.contains('|') && !text.contains('`'),
            "raw delimiter: {text}"
        );
        assert!(
            !text.chars().any(char::is_control),
            "raw control byte: {text:?}"
        );
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
        assert_eq!(
            Safe::render_parsed(Policy::Pair, &v, 4096).to_string(),
            v.to_string(),
            "pair render bytes must not change"
        );
    }

    /// Over-cap market documents are rejected whole: the checked renderer
    /// returns Err with the fixed sentence, and legacy market render
    /// callsites emit the fixed parseable error JSON instead of a
    /// truncated document.
    #[test]
    fn market_json_render_rejects_oversize_without_truncation() {
        let v = serde_json::json!({"note": "SECRET-OVERFLOW-PAYLOAD".repeat(64)});
        let err = Safe::render_market_json(&v, 100).unwrap_err().to_string();
        assert_eq!(err, MARKET_RENDER_OVERSIZE);
        let legacy = Safe::render_parsed(Policy::Market, &v, 100).to_string();
        assert_eq!(
            serde_json::from_str::<Value>(&legacy).unwrap(),
            serde_json::from_str::<Value>(MARKET_RENDER_OVERSIZE_JSON).unwrap()
        );
        assert!(!legacy.contains("SECRET"), "oversize leaked payload");
    }

    /// A continuation cursor that does not carry the RPC event-ID
    /// grammar is refused before any executor call and is never echoed.
    #[test]
    fn invalid_event_cursor_is_refused_before_executor() {
        for cursor in [
            "SECRET-CURSOR-LEAK",
            "abc-1",
            "1234567890123456789012345678901234567890123456789012345678901234567-1",
            "",
            "-1",
            "0-",
            "1-2-3",
        ] {
            assert!(
                validate_event_cursor(cursor).is_err(),
                "{cursor:?} must be refused"
            );
            let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
            let fx = FakeExecutor::new(vec![]);
            let st = market_st(&n, &fx);
            let err = st
                .read_events_after(&market_id(), cursor, 1)
                .unwrap_err()
                .to_string();
            assert_eq!(err, "invalid event cursor");
            assert!(!err.contains("SECRET"), "error leaked cursor: {err}");
            assert!(
                fx.calls.borrow().is_empty(),
                "{cursor:?} reached the executor"
            );
        }
    }

    /// Cursor continuation pages exactly one bounded CLI call and cannot
    /// replay the cursor's own event: a page that starts at or before
    /// the supplied handle is refused whole with the fixed history
    /// sentence.
    #[test]
    fn read_events_after_enforces_strict_progression() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let replay = serde_json::json!({
            "contract_id": market_id().as_str(),
            "event_name": "transfer",
            "id": "100-0",
            "ledger": 101,
        });
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::ok(&format!("{replay}\n"), ""),
        ]);
        let st = market_st(&n, &fx);
        let err = st
            .read_events_after(&market_id(), "100-0", 10)
            .unwrap_err()
            .to_string();
        assert_eq!(err, HISTORY_MALFORMED);
        assert!(fx
            .calls
            .borrow()
            .iter()
            .all(|c| !c.iter().any(|a| a == "--start-ledger")));
    }

    /// Same-ledger continuation works through the index: the event after
    /// the cursor's intra-ledger index is returned exactly once under
    /// one CLI call.
    #[test]
    fn read_events_after_supports_same_ledger_continuation() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let page = serde_json::json!({
            "contract_id": market_id().as_str(),
            "event_name": "transfer",
            "id": "100-1",
            "ledger": 101,
        });
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::ok(&format!("{page}\n"), ""),
        ]);
        let st = market_st(&n, &fx);
        let events = st
            .read_events_after(&market_id(), "100-0", 10)
            .expect("same-ledger continuation");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["cursor"], "100-1");
        assert_eq!(events[0]["ledger"], 101);
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 2, "one version probe plus one page fetch");
        assert_eq!(calls[1][0], "events");
        assert_eq!(
            calls[1],
            s(&[
                "events",
                "--id",
                market_id().as_str(),
                "--cursor",
                "100-0",
                "--count",
                "10",
                "--output",
                "json",
                "--network-passphrase",
                crate::manifest::MARKET_TESTNET_PASSPHRASE,
                "--config-dir",
                "/cfg",
            ]),
            "cursor continuation must compose exactly one bounded page call"
        );
        assert!(calls[1].iter().all(|a| a != "--start-ledger"));
    }

    /// Every remote response that fails to prove the exact testnet
    /// passphrase (malformed, missing, mismatched, mainnet, or a failed
    /// probe) is refused with one fixed input-free sentence, and a pair
    /// context is refused before any executor call. A self-reported testnet
    /// passphrase under a non-default URL is accepted with the explicit
    /// network arguments, so a custom testnet RPC passes on its own
    /// `getNetwork` identity while no raw CLI byte is ever echoed.
    #[test]
    fn rpc_testnet_binding_refuses_unproven_identity_without_leaking_bytes() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let probe = s(&[
            "network",
            "info",
            "--output",
            "json",
            "--network-passphrase",
            crate::manifest::MARKET_TESTNET_PASSPHRASE,
            "--config-dir",
            "/cfg",
        ]);
        let bad_responses: [Vec<CmdOutput>; 5] = [
            vec![
                FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
                FakeExecutor::ok("SECRET-RPC-FRAGMENT not json", ""),
            ],
            vec![
                FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
                FakeExecutor::ok(r#"{"friendbot":"SECRET-FROM-UNBOUND-FIELD"}"#, ""),
            ],
            vec![
                FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
                FakeExecutor::ok(
                    r#"{"passphrase":"Public Global Stellar Network ; September 2015"}"#,
                    "",
                ),
            ],
            vec![
                FakeExecutor::fail("SECRET-RPC-FAILURE-SENTINEL"),
                FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            ],
            vec![
                FakeExecutor::ok("stellar 26.0.0 (60f7458)", ""),
                FakeExecutor::ok(r#"{"passphrase":"Test SDF Network ; September 2015"}"#, ""),
            ],
        ];
        for outputs in &bad_responses {
            let fx = FakeExecutor::new(outputs.clone());
            let err = market_st(&n, &fx)
                .assert_rpc_testnet()
                .unwrap_err()
                .to_string();
            assert!(!err.contains("SECRET"), "raw CLI output leaked: {err}");
            let calls = fx.calls.borrow();
            assert!(
                !calls.iter().any(|argv| argv
                    .iter()
                    .any(|a| a.contains("SECRET") || a.starts_with("--send yes"))),
                "the identity guard may only probe read-only: {calls:?}"
            );
        }

        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok("stellar 26.1.0 (1228cff)", ""),
            FakeExecutor::ok(r#"{"passphrase":"Test SDF Network ; September 2015"}"#, ""),
        ]);
        market_st(&n, &fx)
            .assert_rpc_testnet()
            .expect("a self-reported testnet passphrase must bind the market context");
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec!["--version".to_string()]);
        assert_eq!(calls[1], probe);
    }

    /// The two market send paths are distinct by construction. `invoke`
    /// keeps the historical shape (no explicit send or fee words, the
    /// shared transport law, no blind replay of a mutating call) and
    /// `invoke_confirmed_bound` alone adds `--send yes --fee 10000` and
    /// uses exactly one executor attempt. A pair context can never reserve
    /// a confirmed market send, and a confirmed send never retries.
    #[test]
    fn unconfirmed_invoke_and_confirmed_send_have_separate_argv_and_replay_laws() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let pinned: AccountId = G_FROZEN.parse().expect("fixture pinned signer identity");
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(G_FROZEN, ""),
            FakeExecutor::fail("transient connection reset"),
        ]);
        market_st(&n, &fx)
            .with_market_signer("admin", &pinned)
            .invoke(market_id().as_str(), "admin", "set_data", &[])
            .unwrap_err();
        {
            let calls = fx.calls.borrow();
            assert_eq!(
                calls.len(),
                2,
                "the alias lookup is local and a mutating invoke with an \
                 unexplained outcome is never replayed"
            );
            assert!(
                !calls[1]
                    .iter()
                    .any(|a| a == "--send" && calls[1].iter().any(|b| b == "yes")),
                "plain invoke keeps its historical send law: it never says --send yes: {:?}",
                calls[1]
            );
            let fee = calls[1].iter().position(|a| a == "--fee");
            assert!(
                fee.is_some_and(|i| calls[1].get(i + 1) == Some(&MARKET_INCLUSION_FEE.to_string())),
                "a plain market invoke still submits, so it bids the inclusion fee: {:?}",
                calls[1]
            );
            assert_pinned_signing(&calls[1]);
        }

        // A confirmed market send that does not bind the configured pinned
        // signer refuses before the executor is ever reached (SEC003).
        let fx = FakeExecutor::new(Vec::new());
        let wrong: AccountId = G_REBOUND.parse().expect("fixture wrong signer");
        let err = market_st(&n, &fx)
            .with_market_signer("admin", &pinned)
            .invoke_confirmed_bound(&market_id(), &wrong, "admin", "set_data", &[], far())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not the configured pinned signer"),
            "refusal: {err}"
        );
        assert!(fx.calls.borrow().is_empty());

        // A pinned confirmed send gets exactly one explicit attempt from
        // the frozen G-address, signing by alias only, after the alias is
        // re-resolved against the pin immediately before submission.
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(G_FROZEN, ""),
            FakeExecutor::fail("connection reset"),
        ]);
        market_st(&n, &fx)
            .with_market_signer("admin", &pinned)
            .invoke_confirmed_bound(&market_id(), &pinned, "admin", "set_data", &[], far())
            .unwrap_err();
        {
            let calls = fx.calls.borrow();
            assert_eq!(
                calls.len(),
                2,
                "a confirmed send gets exactly one executor attempt"
            );
            let send = calls[1].iter().position(|a| a == "--send");
            let fee = calls[1].iter().position(|a| a == "--fee");
            assert!(
                send.is_some_and(|i| calls[1].get(i + 1) == Some(&"yes".to_string()))
                    && fee.is_some_and(|i| calls[1].get(i + 1) == Some(&"10000".to_string())),
                "a confirmed send must carry the explicit send and fee words: {:?}",
                calls[1]
            );
            assert_pinned_signing(&calls[1]);
        }

        // A bound send whose alias has been rebound refuses before any
        // submission; a fresh resolution never replaces the frozen pin.
        let fx = FakeExecutor::new(vec![FakeExecutor::ok(G_REBOUND, "")]);
        let err = market_st(&n, &fx)
            .invoke_confirmed_bound(&market_id(), &pinned, "admin", "set_data", &[], far())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not resolve to the pinned identity"),
            "refusal: {err}"
        );
        assert!(
            !err.contains(G_REBOUND),
            "the refusal must never echo the rebound identity"
        );
        {
            let calls = fx.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert!(
                calls[0].iter().any(|a| a == "public-key"),
                "the only executor use is the local alias lookup"
            );
            assert!(
                calls.iter().all(|c| !c.iter().any(|a| a == "invoke")),
                "a rebound alias never reached a submission"
            );
        }

        let pn = net();
        let fx = FakeExecutor::new(vec![FakeExecutor::ok("stellar 26.0.0 (60f7458)", "")]);
        let st = Stellar::new(&pn, &fx, Policy::Pair);
        let err = st
            .invoke_confirmed_bound(&market_id(), &pinned, "admin", "set_data", &[], far())
            .unwrap_err()
            .to_string();
        assert_eq!(err, "confirmed market send requires market policy");
        assert!(fx.calls.borrow().is_empty());
    }

    const G_FROZEN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const G_REBOUND: &str = "GDKOBI6KTEE7GYZCWJDZTAZPKW6Q2FW7MBY2XT6HWWZHHJCBZFXNPGAW";

    /// Every mutation argv must carry the frozen G-address as the
    /// transaction source and name only the alias for signing (SEC003).
    fn assert_pinned_signing(argv: &[String]) {
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--source" && w[1] == G_FROZEN),
            "market mutations must originate from the frozen G-address: {argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--sign-with-key" && w[1] == "admin"),
            "market mutations must sign by naming only the alias: {argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|a| (a.starts_with('S') || a.starts_with('M')) && a.len() == 56),
            "no raw signer seed or muxed account may enter the flags: {argv:?}"
        );
    }

    #[test]
    fn market_mutation_without_a_pinned_signer_refuses_before_any_executor_use() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let fx = FakeExecutor::new(Vec::new());
        let st = market_st(&n, &fx);
        let hash: WasmHash = format!("{:0>64x}", 0xaau8)
            .parse()
            .expect("fixture wasm hash");
        let err = st
            .upload(std::path::Path::new("/tmp/absent-fixture.wasm"), "admin")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no pinned signer"), "refusal: {err}");
        let err = st
            .deploy(&hash, "admin", None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no pinned signer"), "refusal: {err}");
        let err = st
            .invoke(market_id().as_str(), "admin", "set_data", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no pinned signer"), "refusal: {err}");
        let rebound: AccountId = G_REBOUND.parse().expect("fixture rebound signer");
        let frozen: AccountId = G_FROZEN.parse().expect("fixture frozen signer");
        let fx = FakeExecutor::new(Vec::new());
        let err = market_st(&n, &fx)
            .with_market_signer("admin", &rebound)
            .invoke_confirmed_bound(&market_id(), &frozen, "admin", "set_data", &[], far())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not the configured pinned signer"),
            "refusal: {err}"
        );
        assert!(
            fx.calls.borrow().is_empty(),
            "an unpinned market mutation never reaches the CLI"
        );
    }

    #[test]
    fn market_alias_rebinding_refuses_every_mutation_before_it_reaches_the_cli() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let pinned: AccountId = G_FROZEN.parse().expect("fixture pinned signer");
        let fx = FakeExecutor::new(vec![FakeExecutor::ok(G_REBOUND, ""); 8]);
        let st = market_st(&n, &fx).with_market_signer("admin", &pinned);
        let hash: WasmHash = format!("{:0>64x}", 0xaau8)
            .parse()
            .expect("fixture wasm hash");
        for err in [
            st.upload(std::path::Path::new("/tmp/absent-fixture.wasm"), "admin")
                .unwrap_err()
                .to_string(),
            st.deploy(&hash, "admin", None, &[])
                .unwrap_err()
                .to_string(),
            st.invoke(market_id().as_str(), "admin", "set_data", &[])
                .unwrap_err()
                .to_string(),
            st.invoke_confirmed_bound(&market_id(), &pinned, "admin", "set_data", &[], far())
                .unwrap_err()
                .to_string(),
        ] {
            assert!(
                err.contains("different identity") || err.contains("no longer resolves"),
                "refusal: {err}"
            );
            assert!(
                !err.contains(G_REBOUND),
                "the refusal must never echo the rebound identity"
            );
        }
        let calls = fx.calls.borrow();
        assert!(
            calls.iter().all(|c| c.first().is_some_and(|f| f == "keys")),
            "only local alias lookups may run; a rebound alias must never \
             reach a mutation: {calls:?}"
        );
    }

    #[test]
    fn pinned_market_mutations_use_the_frozen_g_source_and_alias_signing_only() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let pinned: AccountId = G_FROZEN.parse().expect("fixture pinned signer");
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(G_FROZEN, ""),
            FakeExecutor::ok(&"b".repeat(64), ""),
            FakeExecutor::ok(G_FROZEN, ""),
            FakeExecutor::ok(market_id().as_str(), ""),
            FakeExecutor::ok(G_FROZEN, ""),
            FakeExecutor::ok("\"9\"", ""),
            FakeExecutor::ok(G_FROZEN, ""),
            FakeExecutor::ok("\"9\"", ""),
        ]);
        let st = market_st(&n, &fx).with_market_signer("admin", &pinned);
        let upload = st
            .upload(std::path::Path::new("/tmp/frozen-fixture.wasm"), "admin")
            .expect("pinned upload settles");
        assert_eq!(upload.as_str(), "b".repeat(64));
        let deployed = st
            .deploy(&upload, "admin", None, &[])
            .expect("pinned deploy settles");
        assert_eq!(deployed.as_str(), market_id().as_str());
        let invoked = st
            .invoke(market_id().as_str(), "admin", "set_data", &[])
            .expect("pinned invoke settles");
        assert!(invoked.ok);
        let bound = st
            .invoke_confirmed_bound(&market_id(), &pinned, "admin", "set_data", &[], far())
            .expect("pinned confirmed send settles");
        assert!(bound.ok);
        let calls = fx.calls.borrow();
        let mut contract_calls = 0;
        let mut send_yes_calls = 0;
        for call in calls.iter() {
            if call.first().is_some_and(|c| c == "contract") {
                contract_calls += 1;
                assert_pinned_signing(call);
                let send = call.iter().position(|a| a == "--send");
                if send.is_some_and(|i| call.get(i + 1) == Some(&"yes".to_string())) {
                    send_yes_calls += 1;
                    assert!(
                        call.iter().any(|a| a == "--fee") && call.iter().any(|a| a == "10000"),
                        "the confirmed send carries the explicit fee bid: {call:?}"
                    );
                } else {
                    let fee = call.iter().position(|a| a == "--fee");
                    assert!(
                        fee.is_some_and(|i| call.get(i + 1) == Some(&MARKET_INCLUSION_FEE.to_string())),
                        "an unconfirmed market invoke still submits, so it bids the inclusion fee: {call:?}"
                    );
                }
            }
        }
        assert_eq!(contract_calls, 4, "upload, deploy, invoke, and send ran");
        assert_eq!(
            send_yes_calls, 1,
            "exactly the bound send carries the explicit send word"
        );
    }

    fn far() -> std::time::SystemTime {
        std::time::SystemTime::now() + std::time::Duration::from_secs(3600)
    }

    /// A confirmed send whose start deadline has passed (a stalled journal
    /// sync or a sleeping machine) refuses before any child that can land.
    #[test]
    fn confirmed_send_refuses_to_start_after_its_deadline() {
        let n = market_net(crate::manifest::MARKET_TESTNET_PASSPHRASE);
        let pinned: AccountId = G_FROZEN.parse().expect("fixture pinned signer");
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(G_FROZEN, ""),
            FakeExecutor::ok("\"9\"", ""),
        ]);
        let refused = market_st(&n, &fx)
            .with_market_signer("admin", &pinned)
            .invoke_confirmed_bound(
                &market_id(),
                &pinned,
                "admin",
                "set_data",
                &[],
                std::time::SystemTime::UNIX_EPOCH,
            );
        assert!(refused.is_err());
        let calls = fx.calls.borrow();
        assert!(
            calls.iter().all(|c| c.first().is_some_and(|f| f == "keys")),
            "only local alias lookups may run before the deadline check: {calls:?}"
        );
    }

    /// SEC001 process boundary: a market child spawned from any directory
    /// other than the controlled root, or with an executable that is not
    /// an already-resolved regular file, refuses before the child starts;
    /// the controlled-root market child runs, and the pair lane is
    /// untouched.
    #[cfg(target_os = "linux")]
    #[test]
    fn market_children_refuse_unsafe_contexts_before_spawn_and_pair_children_are_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("abtest-boundary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture directory");
        std::fs::write(dir.join("market.env"), "STELLAR_CONTRACT_ID=CABC\n").expect("fixture");
        let market_marker = dir.join("market.child.ran");
        let pair_marker = dir.join("pair.child.ran");
        let market_bin = dir.join("stellar");
        std::fs::write(
            &market_bin,
            format!("#!/bin/sh\nprintf x > '{}'\n", market_marker.display()),
        )
        .expect("fixture executable");
        std::fs::set_permissions(&market_bin, std::fs::Permissions::from_mode(0o755))
            .expect("fixture permissions");
        let pair_bin = dir.join("pair-cli");
        std::fs::write(
            &pair_bin,
            format!("#!/bin/sh\nprintf x > '{}'\n", pair_marker.display()),
        )
        .expect("fixture executable");
        std::fs::set_permissions(&pair_bin, std::fs::Permissions::from_mode(0o755))
            .expect("fixture permissions");
        let market_capture = || Capture::Market {
            stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
            stderr_cap: MARKET_CAPTURE_STDERR_CAP,
            wall: Some(std::time::Duration::from_secs(5)),
        };
        let unsafe_exec = RealExecutor {
            bin: market_bin.clone(),
            xdg: dir.join(".xdg"),
            cwd: dir.clone(),
            capture: market_capture(),
            rpc_env: Vec::new(),
        };
        let err = unsafe_exec
            .run(&["--version".to_string()])
            .expect_err("a market child outside the controlled root refuses");
        assert_eq!(err.to_string(), MARKET_EXECUTOR_UNSAFE);
        assert!(
            !market_marker.exists(),
            "the refused market child must never have started"
        );
        let controlled = RealExecutor {
            bin: market_bin,
            xdg: dir.join(".xdg2"),
            cwd: PathBuf::from("/"),
            capture: market_capture(),
            rpc_env: Vec::new(),
        };
        let out = controlled
            .run(&["--version".to_string()])
            .expect("the controlled-root market child runs");
        assert!(out.ok(), "the controlled-root market child succeeds");
        assert!(
            market_marker.exists(),
            "the market child ran from the controlled root"
        );
        for bad_bin in [
            PathBuf::from("relative/stellar"),
            PathBuf::from(&dir).join("missing-stellar"),
            PathBuf::from("/tmp"),
        ] {
            let bad = RealExecutor {
                bin: bad_bin,
                xdg: dir.join(".xdg3"),
                cwd: PathBuf::from("/"),
                capture: market_capture(),
                rpc_env: Vec::new(),
            };
            let err = bad
                .run(&["--version".to_string()])
                .expect_err("an unresolved market executable refuses");
            assert_eq!(err.to_string(), MARKET_EXECUTOR_UNSAFE);
        }
        let pair = RealExecutor {
            bin: pair_bin,
            xdg: dir.join(".xdg4"),
            cwd: dir.clone(),
            capture: Capture::Pair,
            rpc_env: Vec::new(),
        };
        let _ = pair.run(&["--version".to_string()]);
        assert!(
            pair_marker.exists(),
            "pair evidence children keep their historical spawn behaviour"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real executor rechecks a send deadline after its filesystem
    /// setup, at the launch itself: a deadline that has passed by then
    /// refuses without starting the child, a live one launches it.
    #[cfg(target_os = "linux")]
    #[test]
    fn real_executor_rechecks_the_start_deadline_after_setup() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("abtest-start-by-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let marker = dir.join("child.ran");
        let bin = dir.join("stellar");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\nprintf x > '{}'\n", marker.display()),
        )
        .expect("fixture executable");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("fixture permissions");
        let exec = RealExecutor {
            bin,
            xdg: dir.join(".xdg"),
            cwd: PathBuf::from("/"),
            capture: Capture::Market {
                stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
                stderr_cap: MARKET_CAPTURE_STDERR_CAP,
                wall: Some(std::time::Duration::from_secs(5)),
            },
            rpc_env: Vec::new(),
        };
        let args = ["--version".to_string()];
        assert!(exec
            .run_by(&args, std::time::SystemTime::UNIX_EPOCH)
            .is_err());
        assert!(
            dir.join(".xdg/stellar").is_dir(),
            "setup ran before the refusal, so the check sits after it"
        );
        assert!(!marker.exists(), "a late send never launches");
        assert!(exec
            .run_by(&args, far())
            .expect("a live deadline launches")
            .ok());
        assert!(marker.exists(), "the child ran under a live deadline");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B4: a market net's endpoint and headers never reach a `stellar`
    /// child's argv (world-readable via `/proc/*/cmdline`, `ps`, audit
    /// logs); they arrive only through the uid-private child environment,
    /// in the variables the CLI itself documents. An inherited ambient
    /// endpoint is stripped, and the pair lane keeps its historical argv
    /// transport byte-for-byte.
    #[cfg(target_os = "linux")]
    #[test]
    fn market_endpoint_reaches_children_through_env_never_argv() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("abtest-rpc-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let args_file = dir.join("child.argv");
        let env_file = dir.join("child.env");
        let bin = dir.join("stellar");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n\
                 printf 'URL=%s\\nHDRS=%s\\n' \"${{STELLAR_RPC_URL:-}}\" \
                 \"${{STELLAR_RPC_HEADERS:-}}\" > '{}'\n",
                args_file.display(),
                env_file.display()
            ),
        )
        .expect("fixture executable");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("fixture permissions");
        let secret_url = "https://rpc.example/v1?key=SECRET-PROVIDER-KEY";
        let net = Net {
            rpc_url: secret_url.into(),
            passphrase: crate::manifest::MARKET_TESTNET_PASSPHRASE.into(),
            config_dir: dir.join("cfg"),
            headers: vec![("x-api-key".to_string(), "SECRET-HEADER-VALUE".to_string())],
        };
        // An ambient inherited endpoint must not survive into the child.
        std::env::set_var("STELLAR_RPC_URL", "https://inherited.invalid");
        let exec = RealExecutor {
            bin: bin.clone(),
            xdg: dir.join(".xdg"),
            cwd: PathBuf::from("/"),
            capture: Capture::Market {
                stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
                stderr_cap: MARKET_CAPTURE_STDERR_CAP,
                wall: Some(std::time::Duration::from_secs(5)),
            },
            rpc_env: rpc_env_for(&net),
        };
        let st = Stellar::new(&net, &exec, Policy::Market);
        assert!(
            st.probe_ok(&["network", "info", "--output", "json"]),
            "the market child runs"
        );
        let argv = std::fs::read_to_string(&args_file).expect("child argv");
        let child_env = std::fs::read_to_string(&env_file).expect("child env");
        assert!(
            !argv.contains("--rpc-url") && !argv.contains("SECRET-PROVIDER-KEY"),
            "no endpoint flag or secret in market argv: {argv}"
        );
        assert!(
            !argv.contains("x-api-key") && !argv.contains("SECRET-HEADER-VALUE"),
            "no header flag or value in market argv: {argv}"
        );
        assert!(argv.contains("--network-passphrase") && argv.contains("--config-dir"));
        assert!(
            child_env.contains(&format!("URL={secret_url}"))
                && child_env.contains("HDRS=x-api-key: SECRET-HEADER-VALUE"),
            "the endpoint and headers reach the child environment: {child_env}"
        );
        // The pair lane keeps its historical argv transport.
        let pair_args_file = dir.join("pair.argv");
        let pair_env_file = dir.join("pair.env");
        let pair_bin = dir.join("pair-cli");
        std::fs::write(
            &pair_bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n\
                 printf 'URL=%s\\nHDRS=%s\\n' \"${{STELLAR_RPC_URL:-}}\" \
                 \"${{STELLAR_RPC_HEADERS:-}}\" > '{}'\n",
                pair_args_file.display(),
                pair_env_file.display()
            ),
        )
        .expect("fixture executable");
        std::fs::set_permissions(&pair_bin, std::fs::Permissions::from_mode(0o755))
            .expect("fixture permissions");
        let pair_net = Net {
            rpc_url: "https://pair.example".into(),
            passphrase: String::new(),
            config_dir: dir.join("cfg"),
            headers: Vec::new(),
        };
        let pair_exec = RealExecutor {
            bin: pair_bin,
            xdg: dir.join(".xdg2"),
            cwd: dir.clone(),
            capture: Capture::Pair,
            rpc_env: Vec::new(),
        };
        let pair_st = Stellar::new(&pair_net, &pair_exec, Policy::Pair);
        let _ = pair_st.probe_ok(&["network", "info"]);
        let pair_argv = std::fs::read_to_string(&pair_args_file).expect("pair child argv");
        assert!(
            pair_argv.contains("--rpc-url") && pair_argv.contains("https://pair.example"),
            "pair children keep argv transport: {pair_argv}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Debug` of a `Net` redacts the endpoint and its headers; the
    /// non-secret fields stay readable for diagnosis.
    #[test]
    fn net_debug_redacts_endpoint_and_headers() {
        let net = Net {
            rpc_url: "https://rpc.example/v1?key=SECRET-PROVIDER-KEY".into(),
            passphrase: crate::manifest::MARKET_TESTNET_PASSPHRASE.into(),
            config_dir: "/cfg".into(),
            headers: vec![("x-api-key".to_string(), "SECRET-HEADER-VALUE".to_string())],
        };
        let rendered = format!("{net:?}");
        assert!(!rendered.contains("SECRET-PROVIDER-KEY"), "{rendered}");
        assert!(!rendered.contains("SECRET-HEADER-VALUE"), "{rendered}");
        assert!(!rendered.contains("rpc.example"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(rendered.contains("config_dir"), "{rendered}");
    }

    // ---- mainnet-capable market primitives -------------------------------

    const G_PIN: &str = "GBM6UVFOTUQAJZKRPJSP4ZQWI56XBFJDL5OB72ASTPFR3AEKTFCG5F52";
    const G_DST: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    const MAIN: &str = crate::manifest::MARKET_MAINNET_PASSPHRASE;
    const TEST: &str = crate::manifest::MARKET_TESTNET_PASSPHRASE;

    fn acct(g: &str) -> AccountId {
        g.parse().expect("fixture account")
    }

    fn usdc() -> String {
        classic_asset("USDC", &acct(G_DST))
    }

    fn nt(n: &Net) -> Vec<String> {
        // Market children carry no endpoint argv: the URL and headers reach
        // the CLI through the child environment instead.
        s(&[
            "--network-passphrase",
            &n.passphrase,
            "--config-dir",
            "/cfg",
        ])
    }

    fn signed<'a>(n: &'a Net, fx: &'a FakeExecutor) -> Stellar<'a, FakeExecutor> {
        market_st(n, fx).with_market_signer("mk", &acct(G_PIN))
    }

    /// The signer re-resolution every mutation performs first.
    fn resolve_ok() -> CmdOutput {
        FakeExecutor::ok(G_PIN, "")
    }

    fn expect_send(fx: &FakeExecutor, head: &[&str], n: &Net) {
        let calls = fx.calls.borrow();
        assert_eq!(calls[0], s(&["keys", "public-key", "mk", "--config-dir", "/cfg"]));
        let mut want = s(head);
        want.extend(s(&["--source", G_PIN, "--sign-with-key", "mk"]));
        // Every market mutation this helper covers submits, so it bids the
        // inclusion fee before the network words.
        want.extend(s(&["--fee", MARKET_INCLUSION_FEE]));
        want.extend(nt(n));
        assert_eq!(calls[1], want);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn classic_asset_is_code_colon_issuer() {
        assert_eq!(usdc(), format!("USDC:{G_DST}"));
    }

    #[test]
    fn market_tx_argv_is_pinned_signed_and_exact() {
        let n = market_net(MAIN);
        let dst = acct(G_DST);
        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok("", "")]);
        signed(&n, &fx).tx_create_account(&dst, 20_000_000).unwrap();
        expect_send(
            &fx,
            &["tx", "new", "create-account", "--destination", G_DST, "--starting-balance", "20000000"],
            &n,
        );

        let a = usdc();
        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok("", "")]);
        signed(&n, &fx).tx_change_trust(&a, None).unwrap();
        expect_send(&fx, &["tx", "new", "change-trust", "--line", &a], &n);

        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok("", "")]);
        signed(&n, &fx).tx_change_trust(&a, Some(0)).unwrap();
        expect_send(&fx, &["tx", "new", "change-trust", "--line", &a, "--limit", "0"], &n);

        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok("", "")]);
        signed(&n, &fx).tx_payment(&dst, &a, 5_000_000).unwrap();
        expect_send(
            &fx,
            &["tx", "new", "payment", "--destination", G_DST, "--asset", &a, "--amount", "5000000"],
            &n,
        );

        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok("", "")]);
        signed(&n, &fx).tx_payment(&dst, "native", 1).unwrap();
        expect_send(
            &fx,
            &["tx", "new", "payment", "--destination", G_DST, "--asset", "native", "--amount", "1"],
            &n,
        );

        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok("", "")]);
        signed(&n, &fx).tx_lock_account().unwrap();
        expect_send(
            &fx,
            &[
                "tx", "new", "set-options", "--master-weight", "0", "--low-threshold", "1",
                "--med-threshold", "1", "--high-threshold", "1",
            ],
            &n,
        );
    }

    #[test]
    fn market_tx_refuses_bad_inputs_and_unsigned_context_without_sending() {
        let n = market_net(MAIN);
        let dst = acct(G_DST);
        let fx = FakeExecutor::new(vec![]);
        let unsigned = market_st(&n, &fx);
        assert_eq!(
            unsigned.tx_payment(&dst, "native", 1).unwrap_err().to_string(),
            "market mutation refused: no pinned signer is configured"
        );
        let st = signed(&n, &fx);
        assert!(st.tx_payment(&dst, "native", 0).is_err());
        assert!(st.tx_payment(&dst, "bogus", 1).is_err());
        assert!(st.tx_payment(&dst, "USDC:notanissuer", 1).is_err());
        assert!(st.tx_create_account(&dst, 0).is_err());
        assert!(st.tx_change_trust("native", None).is_err());
        assert!(st.tx_change_trust(&usdc(), Some(-1)).is_err());
        assert!(fx.calls.borrow().is_empty(), "refusals precede every CLI call");
    }

    #[test]
    fn market_asset_deploy_and_contract_id_argv() {
        let n = market_net(MAIN);
        let a = usdc();
        let id = market_id();
        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok(id.as_str(), "")]);
        let got = signed(&n, &fx).asset_deploy(&a).unwrap();
        assert_eq!(got, id);
        expect_send(&fx, &["contract", "asset", "deploy", "--asset", &a], &n);

        let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok("not an id", "")]);
        let err = signed(&n, &fx).asset_deploy(&a).unwrap_err().to_string();
        assert!(!err.contains("not an id") && !err.contains(&a));

        let fx = FakeExecutor::new(vec![FakeExecutor::ok(&format!("{id}\n"), "")]);
        let got = market_st(&n, &fx).asset_contract_id(&a).unwrap();
        assert_eq!(got, id);
        let mut want = s(&["contract", "id", "asset", "--asset", &a]);
        want.extend(nt(&n));
        assert_eq!(fx.calls.borrow()[0], want);
    }

    /// Mainnet's Soroban inclusion price (p99 200) sits above the CLI's
    /// default bid, so a market command that submits must state the same
    /// explicit inclusion fee a confirmed send does — otherwise the transaction
    /// is built, simulated clean and never included, and the CLI reports only
    /// a timeout. Every submitting shape is covered, not just `contract deploy`.
    #[test]
    fn every_market_send_bids_the_inclusion_fee() {
        let n = market_net(MAIN);
        let a = usdc();
        let upload_stdout = "ab".repeat(32);
        let hash = WasmHash::from_str(&upload_stdout).expect("fixture wasm hash");
        let id_stdout = market_id().as_str().to_string();
        let id = market_id();
        type Send = Box<dyn Fn(&Stellar<'_, FakeExecutor>) -> anyhow::Result<()>>;
        let sends: Vec<(&str, Send, &str)> = vec![
            (
                "create-account",
                Box::new(|st| st.tx_create_account(&acct(G_DST), 1)),
                "",
            ),
            (
                "change-trust",
                Box::new({
                    let a = a.clone();
                    move |st| st.tx_change_trust(&a, None)
                }),
                "",
            ),
            (
                "payment",
                Box::new({
                    let a = a.clone();
                    move |st| st.tx_payment(&acct(G_DST), &a, 1)
                }),
                "",
            ),
            ("set-options", Box::new(|st| st.tx_lock_account()), ""),
            (
                "asset-deploy",
                Box::new({
                    let a = a.clone();
                    move |st| st.asset_deploy(&a).map(|_| ())
                }),
                id_stdout.as_str(),
            ),
            (
                "upload",
                Box::new(move |st| {
                    st.upload(Path::new("/w/pool.wasm"), G_PIN).map(|_| ())
                }),
                upload_stdout.as_str(),
            ),
            (
                "deploy",
                Box::new({
                    let hash = hash.clone();
                    move |st| st.deploy(&hash, G_PIN, None, &[]).map(|_| ())
                }),
                id_stdout.as_str(),
            ),
            (
                "invoke",
                Box::new(move |st| {
                    st.invoke(id.as_str(), G_PIN, "set_data", &[]).map(|_| ())
                }),
                "",
            ),
        ];
        for (name, send, payload) in &sends {
            let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::ok(payload, "")]);
            send(&signed(&n, &fx)).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            let calls = fx.calls.borrow();
            assert_eq!(calls.len(), 2, "{name}: one alias read and one send");
            let argv = &calls[1];
            assert!(
                mutating(argv),
                "{name}: this shape must be classified as submitting: {argv:?}"
            );
            let fee = argv.iter().position(|word| word == "--fee");
            assert!(
                fee.is_some_and(|i| argv.get(i + 1) == Some(&MARKET_INCLUSION_FEE.to_string())),
                "{name}: a market send must bid the inclusion fee: {argv:?}"
            );
        }
    }

    /// Pair-policy argv bytes are unchanged: the fee bid belongs to the market
    /// surface only, and no pair command grows a word.
    #[test]
    fn pair_argv_carries_no_inclusion_fee() {
        let n = net();
        let upload_stdout = "cd".repeat(32);
        let hash = WasmHash::from_str(&upload_stdout).expect("fixture wasm hash");
        let id_stdout = market_id().as_str().to_string();
        let id = market_id();
        type Send = Box<dyn Fn(&Stellar<'_, FakeExecutor>) -> anyhow::Result<()>>;
        let sends: Vec<(&str, Send, &str)> = vec![
            (
                "upload",
                Box::new(|st| st.upload(Path::new("/w/pool.wasm"), G_DST).map(|_| ())),
                upload_stdout.as_str(),
            ),
            (
                "deploy",
                Box::new({
                    let hash = hash.clone();
                    move |st| st.deploy(&hash, G_DST, None, &[]).map(|_| ())
                }),
                id_stdout.as_str(),
            ),
            (
                "invoke",
                Box::new(move |st| {
                    st.invoke(id.as_str(), G_DST, "set_data", &[]).map(|_| ())
                }),
                "",
            ),
        ];
        for (name, send, payload) in &sends {
            let fx = FakeExecutor::new(vec![FakeExecutor::ok(payload, "")]);
            send(&st_with(&n, &fx)).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            let calls = fx.calls.borrow();
            assert_eq!(calls.len(), 1, "{name}: pair mode resolves no alias");
            assert!(
                !calls[0].iter().any(|word| word == "--fee"),
                "{name}: pair argv must not carry a fee word: {:?}",
                calls[0]
            );
        }
    }

    /// The executable shapes stellar CLI 26 prints (verified against
    /// `stellar xdr decode --type HostFunction`): uploaded code is
    /// `{"wasm":"<64 hex>"}`, a Stellar Asset Contract the bare string
    /// `"stellar_asset"`. Anything else, including a malformed hash, is
    /// recorded as unclassified rather than guessed at.
    #[test]
    fn created_code_reads_the_cli_26_executables_and_classifies_nothing_else() {
        let hash = "ab".repeat(32);
        assert_eq!(
            created_code(Some(&serde_json::json!({"wasm": hash.to_uppercase()}))),
            CreatedCode::Wasm(hash.clone()),
            "digests compare lower-case"
        );
        assert_eq!(
            created_code(Some(&serde_json::json!("stellar_asset"))),
            CreatedCode::StellarAsset
        );
        for odd in [
            None,
            Some(serde_json::json!(null)),
            Some(serde_json::json!("wasm")),
            Some(serde_json::json!({"wasm": "abab"})),
            Some(serde_json::json!({"wasm": 7})),
            Some(serde_json::json!({"wasm_hash": hash})),
            Some(serde_json::json!({"wasm": hash, "extra": 1})),
            Some(serde_json::json!({})),
        ] {
            assert_eq!(created_code(odd.as_ref()), CreatedCode::Unclassified, "{odd:?}");
        }
    }

    /// The ledger's own answers about a contract instance, in the shapes
    /// stellar CLI 26 prints (checked against the public testnet RPC): present
    /// is exit 0 and one entry; absent is exit 1 with `contract not found:
    /// <id>` on stderr. Only that sentence for this exact id is absence.
    #[test]
    fn contract_instance_presence_tells_absence_from_failure() {
        let n = market_net(MAIN);
        let id = market_id();
        let other = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526";
        let ask = |answer: CmdOutput| {
            let fx = FakeExecutor::new(vec![answer]);
            let st = market_st(&n, &fx);
            let result = st.contract_instance_present(&id);
            let calls = fx.calls.borrow().clone();
            (result, calls)
        };
        let (present, calls) = ask(FakeExecutor::ok(
            r#"{"entries":[{"key":{"contract_data":{}},"val":{"contract_data":{}}}]}"#,
            "",
        ));
        assert!(present.expect("a present contract reads back"));
        let mut want = s(&[
            "ledger", "entry", "fetch", "contract-data", "--contract", id.as_str(),
            "--instance", "--output", "json",
        ]);
        want.extend(nt(&n));
        assert_eq!(calls[0], want, "the presence read is the ledger's own");
        for (tag, answer) in [
            (
                "cli-26-not-found",
                FakeExecutor::fail(&format!("\u{274c} error: contract not found: {id}")),
            ),
            (
                "bare-not-found",
                FakeExecutor::fail(&format!("error: contract not found: {id}\n")),
            ),
            ("empty-list", FakeExecutor::ok(r#"{"entries":[]}"#, "")),
        ] {
            assert!(
                !ask(answer).0.unwrap_or_else(|e| panic!("{tag}: {e:#}")),
                "{tag}: the ledger's own absence answer"
            );
        }
        for (tag, answer) in [
            (
                "not-found-for-another-id",
                FakeExecutor::fail(&format!("error: contract not found: {other}")),
            ),
            ("rpc-failure", FakeExecutor::fail("error: rpc unreachable")),
            ("rate-limited", FakeExecutor::fail("error: 429 too many requests")),
            ("not-json", FakeExecutor::ok("not json", "")),
            ("two-entries", FakeExecutor::ok(r#"{"entries":[{"a":1},{"a":2}]}"#, "")),
            ("no-entries-field", FakeExecutor::ok("{}", "")),
        ] {
            assert!(ask(answer).0.is_err(), "{tag}: a failed read is never absence");
        }
    }

    #[test]
    fn new_sends_are_never_retried_after_an_ambiguous_failure() {
        let n = market_net(MAIN);
        let dst = acct(G_DST);
        let a = usdc();
        let id = market_id();
        type Send = Box<dyn Fn(&Stellar<'_, FakeExecutor>) -> anyhow::Result<()>>;
        let sends: Vec<(&str, Send)> = vec![
            ("create-account", Box::new(move |st| st.tx_create_account(&acct(G_DST), 1))),
            ("change-trust", Box::new({ let a = a.clone(); move |st| st.tx_change_trust(&a, None) })),
            ("payment", Box::new({ let a = a.clone(); move |st| st.tx_payment(&acct(G_DST), &a, 1) })),
            ("set-options", Box::new(|st| st.tx_lock_account())),
            ("asset-deploy", Box::new({ let a = a.clone(); move |st| st.asset_deploy(&a).map(|_| ()) })),
        ];
        let _ = (&dst, &id);
        for (name, send) in &sends {
            for stderr in ["error: bad gateway", "error: error sending request for url (http://rpc): connection reset"] {
                let fx = FakeExecutor::new(vec![resolve_ok(), FakeExecutor::fail(stderr)]);
                assert!(send(&signed(&n, &fx)).is_err(), "{name}");
                assert_eq!(fx.calls.borrow().len(), 2, "{name}: exactly one send, no replay");
            }
            // A provably pre-send failure may be replayed, exactly like deploy.
            let fx = FakeExecutor::new(vec![
                resolve_ok(),
                FakeExecutor::fail("error: dns error: failed to lookup address"),
                FakeExecutor::ok(id.as_str(), ""),
            ]);
            let _ = send(&signed(&n, &fx));
            assert_eq!(fx.calls.borrow().len(), 3, "{name}: pre-send failure retries");
        }
        for argv in [
            s(&["tx", "new", "payment", "--build-only"]),
            s(&["tx", "new", "set-options"]),
            s(&["contract", "asset", "deploy", "--asset", "native"]),
        ] {
            assert!(mutating(&argv), "{argv:?}");
        }
        for argv in [
            s(&["ledger", "entry", "fetch", "account"]),
            s(&["contract", "id", "asset", "--asset", "native"]),
            s(&["keys", "generate", "x"]),
            s(&["contract", "fetch", "--wasm-hash", "h"]),
        ] {
            assert!(!mutating(&argv), "{argv:?}");
        }
    }

    const ACCOUNT_JSON: &str = r#"{"entries":[{"key":{"account":{"account_id":"GBM6UVFOTUQAJZKRPJSP4ZQWI56XBFJDL5OB72ASTPFR3AEKTFCG5F52"}},"val":{"account":{"account_id":"GBM6UVFOTUQAJZKRPJSP4ZQWI56XBFJDL5OB72ASTPFR3AEKTFCG5F52","balance":"99850334394","seq_num":"1","num_sub_entries":3,"inflation_dest":null,"flags":2,"home_domain":"","thresholds":"00010203","signers":[{"key":"GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF","weight":7}],"ext":"v0"}},"lastModifiedLedgerSeq":5}],"latestLedger":9}"#;

    #[test]
    fn ledger_account_parses_thresholds_weights_and_missing() {
        let n = market_net(MAIN);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(ACCOUNT_JSON, ""),
            FakeExecutor::ok(r#"{"entries":[],"latestLedger":9}"#, ""),
        ]);
        let st = market_st(&n, &fx);
        let got = st.ledger_account(G_PIN).unwrap().expect("present");
        assert_eq!(got.balance_stroops, 99_850_334_394);
        assert_eq!((got.master_weight, got.low, got.med, got.high), (0, 1, 2, 3));
        assert_eq!(got.signers, vec![(G_DST.to_string(), 7)]);
        assert_eq!((got.num_sub_entries, got.flags), (3, 2));
        assert!(st.ledger_account(G_PIN).unwrap().is_none());
        let mut want = s(&["ledger", "entry", "fetch", "account", "--account", G_PIN, "--output", "json"]);
        want.extend(nt(&n));
        assert_eq!(fx.calls.borrow()[0], want);
    }

    #[test]
    fn ledger_reads_refuse_malformed_input_free() {
        let n = market_net(MAIN);
        for bad in [
            "SECRET-not-json",
            r#"{"entries":"SECRET"}"#,
            r#"{"entries":[{"val":{"account":{"balance":"1","thresholds":"0102","signers":[],"num_sub_entries":0,"flags":0}}}]}"#,
            r#"{"entries":[{"val":{"account":{"balance":"x","thresholds":"01000000","signers":[],"num_sub_entries":0,"flags":0}}}]}"#,
            r#"{"entries":[{"val":{}},{"val":{}}]}"#,
        ] {
            let fx = FakeExecutor::new(vec![FakeExecutor::ok(bad, "")]);
            let err = market_st(&n, &fx).ledger_account(G_PIN).unwrap_err().to_string();
            assert!(!err.contains("SECRET") && !err.contains(G_PIN), "{err}");
        }
        let fx = FakeExecutor::new(vec![FakeExecutor::fail("error: SECRET http://rpc")]);
        let err = market_st(&n, &fx).ledger_account(G_PIN).unwrap_err().to_string();
        assert!(!err.contains("SECRET") && !err.contains("http://rpc"), "{err}");
        let fx = FakeExecutor::new(vec![]);
        assert!(market_st(&n, &fx).ledger_account("nope").is_err());
        assert!(fx.calls.borrow().is_empty());
    }

    #[test]
    fn ledger_trustline_parses_and_builds_argv() {
        let n = market_net(MAIN);
        let a = usdc();
        let json = format!(
            r#"{{"entries":[{{"key":{{}},"val":{{"trustline":{{"account_id":"{G_PIN}","balance":"123","limit":"9223372036854775807","flags":1}}}}}}],"latestLedger":1}}"#
        );
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(&json, ""),
            FakeExecutor::ok(r#"{"entries":[],"latestLedger":1}"#, ""),
        ]);
        let st = market_st(&n, &fx);
        let t = st.ledger_trustline(G_PIN, &a).unwrap().expect("present");
        assert_eq!((t.balance_stroops, t.limit_stroops), (123, i64::MAX));
        assert!(st.ledger_trustline(G_PIN, &a).unwrap().is_none());
        let mut want = s(&["ledger", "entry", "fetch", "trustline", "--account", G_PIN, "--asset", &a, "--output", "json"]);
        want.extend(nt(&n));
        assert_eq!(fx.calls.borrow()[0], want);
    }

    #[test]
    fn wasm_hash_live_reports_exit_status_and_cleans_scratch() {
        let n = market_net(MAIN);
        let hash: WasmHash = "ab".repeat(32).parse().unwrap();
        let fx = FakeExecutor::new(vec![FakeExecutor::ok("", ""), FakeExecutor::fail("error: not found")]);
        let st = market_st(&n, &fx);
        assert!(st.wasm_hash_live(&hash));
        assert!(!st.wasm_hash_live(&hash));
        let calls = fx.calls.borrow();
        assert_ne!(calls[0][5], calls[1][5], "scratch file is unique per probe");
        for call in calls.iter() {
            assert_eq!(call[..5], s(&["contract", "fetch", "--wasm-hash", hash.as_str(), "--out-file"])[..]);
            assert!(!std::path::Path::new(&call[5]).exists());
            assert_eq!(call[6..], nt(&n)[..]);
        }
    }

    #[test]
    fn keys_generate_local_is_idempotent_and_local_only() {
        let n = market_net(MAIN);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::fail("error: no identity"),
            FakeExecutor::ok("", ""),
            FakeExecutor::ok(G_DST, ""),
            FakeExecutor::ok(G_DST, ""),
        ]);
        let st = market_st(&n, &fx);
        assert_eq!(st.keys_generate_local("run-1").unwrap(), acct(G_DST));
        assert_eq!(st.keys_generate_local("run-1").unwrap(), acct(G_DST));
        let calls = fx.calls.borrow();
        assert_eq!(calls[0], s(&["keys", "public-key", "run-1", "--config-dir", "/cfg"]));
        assert_eq!(calls[1], s(&["keys", "generate", "run-1", "--config-dir", "/cfg"]));
        assert_eq!(calls[2], s(&["keys", "public-key", "run-1", "--config-dir", "/cfg"]));
        assert_eq!(calls.len(), 4, "an existing key is returned without generate");
        assert!(calls.iter().all(|c| !c.iter().any(|a| a == "--fund" || a == "--overwrite" || a == "--rpc-url")));
        drop(calls);
        let fx = FakeExecutor::new(vec![]);
        for bad in ["", "-x", "a b", "a/b", "../x"] {
            assert!(market_st(&n, &fx).keys_generate_local(bad).is_err());
            assert!(market_st(&n, &fx).keys_remove(bad).is_err());
        }
        assert!(fx.calls.borrow().is_empty());
    }

    #[test]
    fn keys_remove_forces_and_tolerates_absence() {
        let n = market_net(MAIN);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(G_DST, ""),
            FakeExecutor::ok("", ""),
            FakeExecutor::fail("error: no identity"),
        ]);
        let st = market_st(&n, &fx);
        st.keys_remove("run-1").unwrap();
        st.keys_remove("run-1").unwrap();
        let calls = fx.calls.borrow();
        assert_eq!(calls[1], s(&["keys", "rm", "run-1", "--force", "--config-dir", "/cfg"]));
        assert_eq!(calls.len(), 3, "an absent key issues no rm");
    }

    #[test]
    fn assert_rpc_network_binds_both_networks_and_refuses_mismatch() {
        let info = |pp: &str| format!(r#"{{"friendbotUrl":"x","passphrase":"{pp}","protocolVersion":28}}"#);
        for (own, expect) in [(MAIN, MAIN), (TEST, TEST)] {
            let n = market_net(own);
            let fx = FakeExecutor::new(vec![
                FakeExecutor::ok("stellar 26.1.0", ""),
                FakeExecutor::ok(&info(expect), ""),
            ]);
            market_st(&n, &fx).assert_rpc_network(expect).unwrap();
            let mut want = s(&["network", "info", "--output", "json"]);
            want.extend(nt(&n));
            assert_eq!(fx.calls.borrow()[1], want);
        }
        // RPC reports the other network.
        let n = market_net(MAIN);
        let fx = FakeExecutor::new(vec![FakeExecutor::ok("stellar 26.1.0", ""), FakeExecutor::ok(&info(TEST), "")]);
        assert_eq!(
            market_st(&n, &fx).assert_rpc_network(MAIN).unwrap_err().to_string(),
            "market RPC did not prove the exact mainnet passphrase"
        );
        // Expected differs from the context passphrase, or is unknown: no calls at all.
        for expected in [TEST, "Standalone Network ; February 2017", ""] {
            let fx = FakeExecutor::new(vec![]);
            assert!(market_st(&n, &fx).assert_rpc_network(expected).is_err());
            assert!(fx.calls.borrow().is_empty());
        }
        // assert_rpc_testnet keeps its exact legacy refusal.
        let nt_ = market_net(TEST);
        let fx = FakeExecutor::new(vec![FakeExecutor::ok("stellar 26.1.0", ""), FakeExecutor::ok(&info(MAIN), "")]);
        assert_eq!(
            market_st(&nt_, &fx).assert_rpc_testnet().unwrap_err().to_string(),
            "market RPC did not prove the exact testnet passphrase"
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abtest-clinet-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("network")).unwrap();
        dir
    }

    #[test]
    fn cli_network_reads_entry_and_redacts_debug() {
        let dir = scratch("read");
        std::fs::write(
            dir.join("network/mn.toml"),
            "rpc_url = \"https://user:pw@rpc.example.org:8443/SECRETPATH?key=SECRETKEY\"\n\
             rpc_headers = [[\"Authorization\", \"Bearer SECRETHEADER\"]]\n\
             network_passphrase = \"Public Global Stellar Network ; September 2015\"\n",
        )
        .unwrap();
        let net = cli_network(&dir, "mn").unwrap();
        assert_eq!(net.passphrase, MAIN);
        assert_eq!(net.headers, vec![("Authorization".to_string(), "Bearer SECRETHEADER".to_string())]);
        assert_eq!(net.host_label(), "rpc.example.org:8443");
        let dbg = format!("{net:?}");
        assert!(dbg.contains("rpc.example.org"), "{dbg}");
        for secret in ["SECRET", "user", "pw@", "abtest", "Authorization", "Bearer"] {
            assert!(!dbg.contains(secret), "{secret} leaked: {dbg}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_network_refusals_are_input_free() {
        let dir = scratch("refuse");
        let dir_text = dir.display().to_string();
        std::fs::write(dir.join("network/bad.toml"), "rpc_url = \"ftp://SECRETHOST\"\nnetwork_passphrase = \"p\"\n").unwrap();
        std::fs::write(dir.join("network/junk.toml"), "SECRETJUNK = = =").unwrap();
        for name in ["missing", "mainnet", "bad", "junk", "../x", "", "-n", "a/b"] {
            let err = cli_network(&dir, name).unwrap_err().to_string();
            for leak in ["SECRET", dir_text.as_str(), "ftp"] {
                assert!(!err.contains(leak), "{name}: {err}");
            }
        }
        // Built-in testnet needs no file; mainnet always needs the operator's entry.
        let t = cli_network(&dir, "testnet").unwrap();
        assert_eq!((t.rpc_url.as_str(), t.passphrase.as_str()), ("https://soroban-testnet.stellar.org", TEST));
        assert!(t.headers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
