//! Simulation-only (never-sending) invocation of any live-exported market
//! function on a manifest- or attachment-bound role (plan §2).
//!
//! Every query routes through `market_explorer::resolve_function`, so the
//! target is live-ABI-validated (CLI26 gate, exact testnet, tagged-spec
//! well-formedness, and any operator attachment's recorded spec digest) in
//! one fetch before anything is invoked. Caller-supplied argument names
//! must match the live spec's declared inputs exactly; there is no static
//! function whitelist and no Rust-side ABI/XDR codec — arguments are passed
//! through to the native CLI as given, and the decoded return value is
//! returned unchanged (JSON nulls preserved, no numeric coercion).
//! Simulation uses the manifest admin address as `--source` with
//! `--send no`; no executor call in this module can mutate chain state.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::manifest::MarketManifest;
use crate::market_explorer;
use crate::stellar::{Executor, Safe, Stellar};

/// Hard ceiling on caller-supplied arguments per query.
pub const MAX_QUERY_ARGS: usize = 32;

/// Hard ceiling on one argument value, in bytes.
pub const MAX_ARG_VALUE_LEN: usize = 4_096;

/// Whether `name` is a well-formed argument name: an ASCII identifier of
/// at most 32 bytes (`[A-Za-z_][A-Za-z0-9_]*`). Bounded length and grammar
/// are enforced here, not just against the live spec.
pub(crate) fn arg_name_is_wellformed(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let first = bytes[0];
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Generic syntactic bounds for any caller that composes market arguments:
/// a bounded argument count, well-formed non-duplicate argument names, and
/// bounded control-free values. Every rejection is input-free, so malformed
/// caller bytes never reach an error or an egress channel. Whether a name is
/// an input of the selected live contract function is a live-spec question
/// and stays with the caller that resolves the interface.
pub fn validate_args(args: &[(String, String)]) -> anyhow::Result<()> {
    if args.len() > MAX_QUERY_ARGS {
        return Err(anyhow::Error::msg(Safe::literal(
            "market query argument list exceeds the bounded cap",
        )));
    }
    let mut seen = HashSet::new();
    for (name, value) in args {
        if !arg_name_is_wellformed(name) {
            return Err(anyhow::Error::msg(Safe::literal(
                "market query argument name is malformed",
            )));
        }
        if !seen.insert(name) {
            return Err(anyhow::Error::msg(Safe::literal(
                "market query argument names are duplicated",
            )));
        }
        if value.len() > MAX_ARG_VALUE_LEN || value.chars().any(char::is_control) {
            return Err(anyhow::Error::msg(Safe::literal(
                "market query argument value is oversized or contains control characters",
            )));
        }
    }
    Ok(())
}

/// Simulates `method` on the contract bound to `role` against the live
/// network through the pinned native CLI seam (`--send no`) and returns the
/// decoded return value unchanged. The manifest admin address is the only
/// simulation source. Duplicate, unknown, malformed, and over-cap argument
/// names or values are refused before any invocation is attempted, and a
/// missing required argument fails through the native CLI's own error, not
/// a locally encoded call.
pub fn query<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
    method: &str,
    args: &[(String, String)],
) -> anyhow::Result<Value> {
    validate_args(args)?;
    let live = market_explorer::resolve_function(st, out, manifest, role, method)?;
    for (name, _) in args {
        if !live.inputs.iter().any(|input| input == name) {
            return Err(anyhow::Error::msg(Safe::literal(
                "market query argument is not an input of the selected live contract function",
            )));
        }
    }
    let kv: Vec<(&str, String)> = args
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    st.view(live.target.as_str(), manifest.admin.as_str(), method, &kv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MARKET_TESTNET_PASSPHRASE;
    use crate::stellar::{fake::FakeExecutor, Net, Policy};
    use std::path::PathBuf;

    const V26: &str = "stellar 26.1.0 (1228cff)";
    const SPEC: &str = r#"[{"function_v0":{"doc":"","name":"get_quote","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"amount","type_":"i128"}],"outputs":[{"result":{"ok_type":{"vec":{"element_type":"u32"}},"error_type":{"udt":{"name":"ContractError"}}}}]}},{"function_v0":{"doc":"","name":"noargs","inputs":[],"outputs":[{"vec":{"element_type":{"udt":{"name":"Asset"}}}}]}}]"#;
    const RESULT: &str = r#"{"Ok":{"map":[["price","12500000000"],["ttl","300"],["note",null]]}}"#;
    const ID_A: &str = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT";
    const ID_B: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
    const ID_C: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
    const ID_D: &str = "CBBTYOXBO7SO3JJ5EM5OBV4WTV5TML4G2K327TYSX422WL47B6RVMJRI";
    const ID_E: &str = "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY";
    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abtest-market-query-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn fixture_manifest() -> MarketManifest {
        serde_json::from_value(serde_json::json!({
            "network": {"label": "testnet", "passphrase": MARKET_TESTNET_PASSPHRASE},
            "admin": G_ADMIN,
            "rpc_host": "rpc.testnet.stellar.example",
            "oracle": {
                "runtime": ID_B, "governance": ID_C, "adapter": ID_A,
                "sources": [ID_A, ID_B, ID_C],
                "min_sources": 3, "max_age_secs": 3600, "max_clock_drift_secs": 60,
                "decimals": 7, "resolution": 300, "base": "USD"
            },
            "tokens": {"blnd": ID_B},
            "pool": ID_D, "backstop": ID_E, "factory": ID_A,
            "comet": ID_B, "emitter": ID_C,
            "reserves": {}, "identities": {}, "deploy_ledger": 1
        }))
        .expect("fixture market manifest")
    }

    fn market_net(out: &Path) -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: MARKET_TESTNET_PASSPHRASE.into(),
            config_dir: out.join("cfg"),
            headers: vec![],
        }
    }

    fn network_info(passphrase: &str) -> String {
        serde_json::to_string(&serde_json::json!({"passphrase": passphrase}))
            .expect("network identity fixture")
    }

    /// A long-lived caller must not use a startup proof to authorize a
    /// later query after the endpoint identity changes. The queue stays
    /// armed with an interface and simulation result, so reusing stale
    /// local network state would make the fetch and call-count checks
    /// below fail.
    #[test]
    fn query_refuses_when_rpc_identity_changes_after_startup() {
        let out = scratch("query-identity-change");
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info(MARKET_TESTNET_PASSPHRASE), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(
                r#"{"passphrase":"Public Global Stellar Network ; September 2015"}"#,
                "",
            ),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(RESULT, ""),
        ]);
        let st = Stellar::new(&net, &fx, Policy::Market);
        st.assert_rpc_testnet()
            .expect("startup proves the exact testnet identity");
        let err = query(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &[("asset".into(), ID_B.into())],
        )
        .expect_err("a changed network must refuse before any interface read");
        assert_eq!(
            err.to_string(),
            "market RPC did not prove the exact testnet passphrase"
        );
        assert!(
            !err.to_string().contains("Public") && !err.to_string().contains(ID_B),
            "the refusal echoes neither the endpoint nor the target: {err}"
        );
        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            4,
            "only the two identity probes may run; the live interface and simulation must stay unfetched"
        );
        assert!(
            calls
                .iter()
                .all(|argv| argv.first().map(String::as_str) != Some("contract")),
            "no contract interface fetch may follow a failed identity check: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .all(|argv| argv.first().map(String::as_str) != Some("invoke")),
            "no simulation may follow a failed identity check: {calls:?}"
        );
        assert!(
            calls.iter().all(|argv| !argv.iter().any(|a| a == ID_B)),
            "the selected target never reaches a changed endpoint"
        );
    }

    #[test]
    fn query_returns_native_udt_json_unchanged_and_refuses_bad_args_before_send() {
        let out = scratch("query");
        let manifest = fixture_manifest();
        let net = market_net(&out);
        // Scripted responses: CLI version, exact-testnet network info, and
        // live interface for each query, plus the simulation results. The
        // final query must fail after its interface and before any invoke.
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info(MARKET_TESTNET_PASSPHRASE), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok(RESULT, ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info(MARKET_TESTNET_PASSPHRASE), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::ok("", ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info(MARKET_TESTNET_PASSPHRASE), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let st = Stellar::new(&net, &fx, Policy::Market);

        let got = query(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &[
                ("asset".into(), ID_B.into()),
                ("amount".into(), "10000000".into()),
            ],
        )
        .expect("valid live function query");
        let expected: Value = serde_json::from_str(RESULT).expect("fixture result");
        assert_eq!(got, expected, "native UDT JSON must pass through unchanged");
        assert_eq!(
            got.pointer("/Ok/map/2/1"),
            Some(&Value::Null),
            "JSON nulls must be preserved without coercion"
        );

        let void = query(&st, &out, &manifest, "adapter", "noargs", &[]).expect("void simulation");
        assert_eq!(
            void,
            Value::Null,
            "a void return must surface as Value::Null"
        );

        let err = query(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &[("bogus".into(), "1".into())],
        )
        .expect_err("unknown argument must fail before any send");
        assert_eq!(
            err.to_string(),
            "market query argument is not an input of the selected live contract function"
        );

        // Purely local rejections run before any executor call at all.
        let before = fx.calls.borrow().len();
        for bad in [
            vec![("bogus".to_string(), "1".to_string()); 33],
            vec![("asset".to_string(), "x".repeat(MAX_ARG_VALUE_LEN + 1))],
            vec![("asset".to_string(), "a\u{7}b".into())],
            vec![
                ("asset".to_string(), "x".into()),
                ("asset".to_string(), "y".into()),
            ],
            vec![("bad-name!".to_string(), "1".to_string())],
        ] {
            assert!(
                query(&st, &out, &manifest, "adapter", "get_quote", &bad).is_err(),
                "malformed argument list must be refused"
            );
        }
        assert_eq!(
            fx.calls.borrow().len(),
            before,
            "local argument refusals must not touch the executor"
        );

        // Only the scripted version, interface, and no-send simulations ran:
        // every recorded invoke was a `--send no` view sourced by the
        // manifest admin, and no mutation ever reached the executor.
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 14, "exactly the scripted calls may run");
        for call in calls.iter() {
            if call.iter().any(|a| a == "invoke") {
                let send = call.iter().position(|a| a == "--send");
                assert!(
                    send.is_some_and(|i| call.get(i + 1) == Some(&"no".to_string())),
                    "every simulation must be --send no"
                );
                assert!(
                    call.windows(2)
                        .all(|w| !(w[0] == "--send" && w[1] == "yes")),
                    "no query may ever request a send"
                );
                let source = call.iter().position(|a| a == "--source");
                assert!(
                    source.is_some_and(|i| call.get(i + 1) == Some(&G_ADMIN.to_string())),
                    "simulation source must be the manifest admin"
                );
            }
        }
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.iter().any(|a| a == "invoke"))
                .count(),
            2,
            "the unknown-argument query must never reach an invoke"
        );
    }

    #[test]
    fn native_query_rejection_is_bounded_and_never_requests_send() {
        let out = scratch("query-rejection");
        let manifest = fixture_manifest();
        let net = market_net(&out);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info(MARKET_TESTNET_PASSPHRASE), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
            FakeExecutor::fail(
                "error: transaction simulation failed: HostError: Error(Contract, #1000)\nsecret=do-not-show",
            ),
        ]);
        let st = Stellar::new(&net, &fx, Policy::Market);
        let error = query(
            &st,
            &out,
            &manifest,
            "adapter",
            "get_quote",
            &[("asset".into(), ID_B.into()), ("amount".into(), "1".into())],
        )
        .expect_err("failed query simulation must remain a failure");
        let diagnostic = Safe::market_error(&error).to_string();
        assert!(diagnostic.contains("BadRequest (1000)"));
        assert!(!diagnostic.contains("secret=do-not-show"));
        let calls = fx.calls.borrow();
        assert_eq!(calls.len(), 5);
        let invoke = calls.last().expect("simulation call");
        assert!(invoke.windows(2).any(|w| w[0] == "--send" && w[1] == "no"));
        assert!(!invoke.windows(2).any(|w| w[0] == "--send" && w[1] == "yes"));
    }

    #[test]
    fn invalid_and_duplicate_arguments_reject_before_interface_fetch_or_invoke() {
        let out = scratch("prefetch-reject");
        let manifest = fixture_manifest();
        let net = market_net(&out);
        // Only a version and an interface are scripted. Any interface fetch
        // or invoke that ran past validation would be recorded, which the
        // zero-call assertion below forbids.
        let fx = FakeExecutor::new(vec![FakeExecutor::ok(V26, ""), FakeExecutor::ok(SPEC, "")]);
        let st = Stellar::new(&net, &fx, Policy::Market);

        validate_args(&[
            ("asset".to_string(), ID_B.to_string()),
            ("amount".to_string(), "10000000".to_string()),
        ])
        .expect("well-formed scalar and UDT argument inputs must pass validation");

        for bad in [
            vec![
                ("asset".to_string(), "1".to_string()),
                ("asset".to_string(), "2".to_string()),
            ],
            vec![("bad-name!".to_string(), "1".to_string())],
            vec![("".to_string(), "1".to_string())],
            vec![("1asset".to_string(), "1".to_string())],
            vec![("a".repeat(33), "1".to_string())],
            vec![("asset".to_string(), "x".repeat(MAX_ARG_VALUE_LEN + 1))],
            vec![("asset".to_string(), "a\u{7}b".to_string())],
            vec![("bogus".to_string(), "1".to_string()); MAX_QUERY_ARGS + 1],
        ] {
            let sentinel = bad[0].1.clone();
            let err = validate_args(&bad).expect_err("malformed arguments must be refused");
            assert!(
                !err.to_string().contains(&sentinel),
                "a rejection must not echo caller-supplied bytes"
            );
            assert!(
                query(&st, &out, &manifest, "adapter", "get_quote", &bad).is_err(),
                "an invalid argument list must never reach an interface fetch or invoke"
            );
        }
        assert_eq!(
            fx.calls.borrow().len(),
            0,
            "invalid and duplicate arguments must be refused before any interface fetch or invoke"
        );

        assert_eq!(
            validate_args(&[
                ("asset".to_string(), "1".to_string()),
                ("asset".to_string(), "2".to_string()),
            ])
            .expect_err("duplicate argument names must be refused")
            .to_string(),
            "market query argument names are duplicated"
        );
        assert_eq!(
            validate_args(&[("bad-name!".to_string(), "1".to_string())])
                .expect_err("malformed argument names must be refused")
                .to_string(),
            "market query argument name is malformed"
        );
        let too_many: Vec<(String, String)> = (0..=MAX_QUERY_ARGS)
            .map(|_| ("asset".to_string(), "1".to_string()))
            .collect();
        assert_eq!(
            validate_args(&too_many)
                .expect_err("an over-cap argument list must be refused")
                .to_string(),
            "market query argument list exceeds the bounded cap"
        );
    }
}
