//! One side's activation sequence, mirroring
//! `blend-contracts-v2/test-suites/src/{test_fixture.rs,setup.rs}`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context};
use serde_json::json;

use crate::manifest::{
    begin_market_pending, finish_market_pending, Contracts, MarketConfig,
    MarketCustodyRecord, MarketManifest, MarketMutationKind, MarketNetworkRecord,
    MarketPendingResult, MarketStepRecord,
    MarketNetwork, OracleStackManifest, PairConfig, PairManifest,
    ReserveOverride, ReserveRef, Sha256Hex, SharedWasm, SideManifest, SideSpec, Tok, Who,
    MARKET_NETWORK_LABEL, MARKET_SOURCE_COUNT, MARKET_TESTNET_PASSPHRASE,
};
use crate::manifest::{expand_home, MARKET_ARTIFACT_ROLES};
use crate::manifest::{MARKET_BASE, MARKET_SOURCE_RESOLUTION};
use crate::market::{custody_payload_digest, sha256_hex};
use crate::scenario::{req, Req, E6, E9, I128_MAX, SCALAR_7};
use crate::stellar::{
    AccountId, Capture, CmdOutput, ContractId, Executor, Invocation, Net, Policy, RawKey,
    RealExecutor, Safe, Stellar, WasmHash, MARKET_CAPTURE_STDERR_CAP, MARKET_CAPTURE_STDOUT_CAP,
    MARKET_MUTATION_WALL, MARKET_READ_WALL,
};
use serde_json::Value;

use serde::{Deserialize, Serialize};
impl Executor for Box<dyn Executor> {
    fn run(&self, args: &[String]) -> anyhow::Result<CmdOutput> {
        (**self).run(args)
    }

    fn run_by(
        &self,
        args: &[String],
        start_by: std::time::SystemTime,
    ) -> anyhow::Result<CmdOutput> {
        (**self).run_by(args, start_by)
    }
}

pub struct SharedHashes {
    pub token: String,
    pub oracle: String,
    pub comet: String,
    pub emitter: String,
}

/// Deterministic 32-byte salt: nonce, role, side. Unique per (deployer, salt).
pub fn salt(nonce: u64, role: u32, side: char) -> String {
    format!(
        "{:016x}{:016x}{:016x}{:016x}",
        nonce, role as u64, side as u64, 0u64
    )
}

pub fn ensure_identities<E: Executor>(st: &Stellar<E>) -> anyhow::Result<BTreeMap<Who, String>> {
    let mut ids = BTreeMap::new();
    for who in Who::ALL {
        let addr = st.keys_ensure_funded(&who.key_name())?;
        ids.insert(who, addr.as_str().to_string());
    }
    Ok(ids)
}

pub fn upload_shared<E: Executor>(
    st: &Stellar<E>,
    w: &SharedWasm,
    admin: &str,
) -> anyhow::Result<SharedHashes> {
    Ok(SharedHashes {
        token: st.upload(&w.token_wasm, admin)?.as_str().to_string(),
        oracle: st.upload(&w.oracle_wasm, admin)?.as_str().to_string(),
        comet: st.upload(&w.comet_wasm, admin)?.as_str().to_string(),
        emitter: st.upload(&w.emitter_wasm, admin)?.as_str().to_string(),
    })
}

/// Fixture reserve configs (`test-suites/src/setup.rs`) with optional overrides.
pub fn reserve_config(tok: Tok, index: u32, ov: Option<&ReserveOverride>) -> serde_json::Value {
    let (decimals, c, l, util, cap) = match tok {
        Tok::Stable => (
            6,
            9_000_000,
            9_500_000,
            8_500_000,
            "1000000000000000000".to_string(),
        ),
        Tok::Xlm => (
            7,
            7_500_000,
            7_500_000,
            5_000_000,
            "1000000000000000000".to_string(),
        ),
        Tok::Weth => (9, 8_000_000, 8_000_000, 7_000_000, I128_MAX.to_string()),
        _ => (
            tok.decimals(),
            7_500_000,
            7_500_000,
            5_000_000,
            "1000000000000000000".to_string(),
        ),
    };
    let ov = ov.cloned().unwrap_or_default();
    json!({
        "index": index,
        "decimals": decimals,
        "c_factor": ov.c_factor.unwrap_or(c),
        "l_factor": ov.l_factor.unwrap_or(l),
        "util": ov.util.unwrap_or(util),
        "max_util": ov.max_util.unwrap_or(9_500_000),
        "r_base": ov.r_base.unwrap_or(100_000),
        "r_one": ov.r_one.unwrap_or(500_000),
        "r_two": ov.r_two.unwrap_or(5_000_000),
        "r_three": ov.r_three.unwrap_or(15_000_000),
        "reactivity": ov.reactivity.unwrap_or(20),
        "supply_cap": ov.supply_cap.unwrap_or(cap),
        // Pool-side `ReserveConfig` gained `min_position` (0 disables the
        // minimum collateral position); the CLI parses the map by field
        // name and rejects the config outright when the key is absent.
        // Like `supply_cap` it is an i128, which the CLI accepts only as
        // a string.
        "min_position": "0",
        "enabled": true
    })
}

pub fn request_json(reqs: &[Req], contracts: &Contracts) -> String {
    let v: Vec<_> = reqs
        .iter()
        .map(|r| json!({"request_type": r.kind, "address": contracts.token(r.asset), "amount": r.amount}))
        .collect();
    serde_json::to_string(&v).expect("json")
}

fn kv(k: &str, v: impl ToString) -> (&str, String) {
    (k, v.to_string())
}

/// Fixture prices in oracle order, matching the pair engine's published
/// set and the scenario fixtures: weth 2000, usdc 1, xlm 0.1, stable 1
/// (base units, 7 decimals).
pub fn fixture_price(tok: Tok) -> i128 {
    match tok {
        Tok::Weth => 2_000 * SCALAR_7,
        Tok::Usdc => SCALAR_7,
        Tok::Xlm => E6,
        Tok::Stable => SCALAR_7,
        _ => SCALAR_7,
    }
}

/// An i128 quantity from a CLI-rendered value: JSON numbers or strings,
/// which is how the RPC surface encodes 128-bit integers.
fn price_as_i128(v: &Value) -> Option<i128> {
    if let Some(n) = v.as_u64() {
        return Some(i128::from(n));
    }
    if let Some(n) = v.as_i64() {
        return Some(n as i128);
    }
    let raw = v.as_str()?;
    if let Some(hex) = raw.strip_prefix("0x") {
        return i128::from_str_radix(hex, 16).ok();
    }
    raw.parse::<i128>().ok()
}

/// Stored hex digest -> checksum-validated hash newtype. Strictly 32 bytes of
/// lowercase hex; malformed or oversized digests fail closed rather than
/// truncate, and no raw-string path reaches `deploy`.
fn hash_of(hex: &str) -> anyhow::Result<WasmHash> {
    hex.parse::<WasmHash>().map_err(|_| {
        anyhow::anyhow!(
            "{}",
            Safe::literal("stored wasm digest is not a valid sha256 hex hash")
        )
    })
}
fn must_ok(
    inv: crate::stellar::Invocation,
    what: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    anyhow::ensure!(inv.ok, "{}", Safe::pair_must_ok(&inv, what));
    Ok(inv.value)
}
// ---------------------------------------------------------------------------
// In-tree keccak256 (original Keccak padding, NOT NIST SHA3) and the
// factory-child contract-id prediction it feeds. The pool factory derives a
// child salt as keccak256(salt || admin-strkey padded to 56 bytes)
// (blend-contracts-v2 pool_factory.rs:92-96) and deploys at
// sha256(XDR(HashIdPreimage::ContractId)) with itself as sender
// (pool_factory.rs:98-109). The stellar CLI exposes no preimage prediction,
// so this host-side law is the only prediction path for factory children.
// Every function below is gated by known-answer tests pinned to the offline
// pool-proof harness (blend-contracts-v2/pool-proof), which proves the law
// against the real factory wasm.

/// Keccak-f[1600] round constants.
const KECCAK_RC: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];
/// Rho rotation offsets, paired with the Pi lane map below.
const KECCAK_ROT: [u32; 24] = [
    1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
];
/// Pi lane permutation, index-matched to `KECCAK_ROT`.
const KECCAK_PI: [usize; 24] = [
    10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
];

/// One Keccak-f[1600] permutation over the 5x5 lane state (`a[x + 5*y]`).
fn keccak_f1600(a: &mut [u64; 25]) {
    for &rc in KECCAK_RC.iter() {
        // theta
        let mut c = [0u64; 5];
        for (x, cx) in c.iter_mut().enumerate() {
            *cx = a[x] ^ a[x + 5] ^ a[x + 10] ^ a[x + 15] ^ a[x + 20];
        }
        let mut d = [0u64; 5];
        for x in 0..5 {
            d[x] = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
        }
        for y in 0..5 {
            for x in 0..5 {
                a[x + 5 * y] ^= d[x];
            }
        }
        // rho + pi
        let mut last = a[1];
        for i in 0..24 {
            let tmp = a[KECCAK_PI[i]];
            a[KECCAK_PI[i]] = last.rotate_left(KECCAK_ROT[i]);
            last = tmp;
        }
        // chi
        for y in 0..5 {
            let row = [
                a[5 * y],
                a[5 * y + 1],
                a[5 * y + 2],
                a[5 * y + 3],
                a[5 * y + 4],
            ];
            for x in 0..5 {
                a[5 * y + x] = row[x] ^ (!row[(x + 1) % 5] & row[(x + 2) % 5]);
            }
        }
        // iota
        a[0] ^= rc;
    }
}

/// keccak256 with the ORIGINAL Keccak padding (0x01 ... 0x80), which is what
/// the pool factory's `e.crypto().keccak256` applies — not NIST SHA3's
/// 0x06 domain byte. Known-answer tests below pin both properties.
pub(crate) fn keccak256(data: &[u8]) -> [u8; 32] {
    const RATE: usize = 136; // 1088-bit rate for a 256-bit digest
    let mut state = [0u64; 25];
    let absorb = |block: &[u8], state: &mut [u64; 25]| {
        for (i, lane) in block.as_chunks::<8>().0.iter().enumerate() {
            state[i] ^= u64::from_le_bytes(*lane);
        }
        keccak_f1600(state);
    };
    let (blocks, rem) = data.as_chunks::<RATE>();
    for block in blocks {
        absorb(block, &mut state);
    }
    // The remainder comes from the SAME split, so the tail is the true
    // tail — the same trap the sha256 896-bit KAT guards.
    let mut block = [0u8; RATE];
    block[..rem.len()].copy_from_slice(rem);
    block[rem.len()] ^= 0x01;
    block[RATE - 1] ^= 0x80;
    absorb(&block, &mut state);
    let mut out = [0u8; 32];
    for (i, lane) in state.iter().take(4).enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&lane.to_le_bytes());
    }
    out
}

#[cfg(test)]
const STRKEY_G: u8 = 0x30; // ed25519 account
const STRKEY_C: u8 = 2 << 3; // contract (first character must encode 'C')
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32, no padding (the strkey encoding).
fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in data {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
            // Emitted quintets must be cleared from the window, or the next
            // extraction re-reads them.
            buffer &= if bits >= 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            };
        }
    }
    if bits > 0 {
        buffer &= if bits >= 32 {
            u32::MAX
        } else {
            (1u32 << bits) - 1
        };
        out.push(BASE32_ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// CRC-16/XMODEM (poly 0x1021, init 0), the strkey checksum.
fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
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

/// Strkey encode: version byte, payload, CRC-16 checksum appended low-byte
/// first (little-endian), base32.
fn strkey_encode(version: u8, payload: &[u8]) -> String {
    let mut data = Vec::with_capacity(payload.len() + 3);
    data.push(version);
    data.extend_from_slice(payload);
    // The strkey checksum is appended LOW BYTE FIRST (little-endian).
    data.extend_from_slice(&crc16_xmodem(&data).to_le_bytes());
    base32_encode(&data)
}

/// Strkey decode with checksum validation: (version, payload) or `None`.
fn strkey_decode(s: &str) -> Option<(u8, Vec<u8>)> {
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    let mut data = Vec::with_capacity(s.len() * 5 / 8);
    for ch in s.bytes() {
        let v = BASE32_ALPHABET.iter().position(|&a| a == ch)? as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            data.push(((buffer >> bits) & 0xff) as u8);
            // Same law as the encoder: emitted bytes are cleared from the
            // window before the next extraction.
            buffer &= if bits >= 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            };
        }
    }
    if bits > 0
        && (buffer
            & if bits >= 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            })
            != 0
    {
        return None; // a valid strkey carries no stray trailing bits
    }
    if data.len() < 3 {
        return None;
    }
    let (body, crc) = data.split_at(data.len() - 2);
    let expect = u16::from_le_bytes([*crc.first()?, *crc.get(1)?]);
    (crc16_xmodem(body) == expect).then(|| (body[0], body[1..].to_vec()))
}

/// The market's network id: sha256 of the network passphrase. This is the
/// `network_id` the chain mixes into every HashIdPreimage.
fn market_network_id(network: MarketNetwork) -> [u8; 32] {
    hex32(&sha256_hex(network.passphrase().as_bytes())).expect("sha256 hex")
}

/// A Lazer source's `supported_feed_ids`: the contract's registry rule is
/// 1..=N ids in strictly increasing order, so the config's feed ids are parsed,
/// sorted and de-duplicated before the constructor sees them.
fn lazer_feed_registry(feeds: &[String]) -> anyhow::Result<Vec<u32>> {
    let mut ids = feeds
        .iter()
        .map(|feed| feed.parse::<u32>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| anyhow!("a Lazer feed id is not a u32"))?;
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// 32 bytes from exactly 64 lowercase hex characters; fail closed otherwise.
fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Host-side `sha256(XDR(HashIdPreimage::ContractId))` for a factory
/// sender. XDR layout (stellar-xdr 26.0.0, proven by pool-proof): union
/// discriminant 0x00000008 (ContractId), network id, inner-union arm
/// 0x00000000 (ContractIdPreimage::Address), ScAddress discriminant
/// 0x00000001 (Contract), sender, salt. Account-branch deploy prediction
/// lives in the CLI (`contract id wasm`), whose salt input is
/// sha256(wasm-hash || salt); this helper serves only factory children,
/// whose salt the factory itself derives.
fn predict_contract_child(
    network_id: &[u8; 32],
    factory: &[u8; 32],
    child_salt: &[u8; 32],
) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(108);
    preimage.extend_from_slice(&8u32.to_be_bytes());
    preimage.extend_from_slice(network_id);
    preimage.extend_from_slice(&0u32.to_be_bytes());
    preimage.extend_from_slice(&1u32.to_be_bytes());
    preimage.extend_from_slice(factory);
    preimage.extend_from_slice(child_salt);
    hex32(&sha256_hex(&preimage)).expect("sha256 hex")
}

/// The pool factory's child salt: keccak256(deploy salt || admin strkey
/// bytes copied into a zero-padded [u8;56]) (pool_factory.rs:92-96). The
/// raw role-235 salt never deploys anything by itself and must never be
/// bound as a prediction input elsewhere in this preimage.
fn pool_factory_child_salt(salt_hex: &str, admin_strkey: &str) -> Option<[u8; 32]> {
    let salt = hex32(salt_hex)?;
    let admin = admin_strkey.as_bytes();
    if admin.len() > 56 {
        return None; // the factory itself copies into a fixed [u8;56]
    }
    let mut preimage = [0u8; 88];
    preimage[..32].copy_from_slice(&salt);
    preimage[32..32 + admin.len()].copy_from_slice(admin);
    Some(keccak256(&preimage))
}

/// Compare a contract's answer about its configuration with what was asked
/// for it. A contract's answer is JSON and so is the request; the comparison
/// is by value, not by text, because a contract is free to reorder the fields
/// it returns and a list of sources is a set, not a sequence — the order a
/// quorum is written down in says nothing about the quorum. Numbers are
/// compared as numbers: a scale that a contract renders as an unquoted
/// integer is the same scale written as a decimal string.
fn config_equal(answer: &Value, asked: &Value) -> bool {
    match (asked, answer) {
        (Value::Object(want), Value::Object(got)) => {
            want.iter().all(|(key, value)| {
                got.get(key).is_some_and(|other| config_equal(value, other))
            })
        }
        (Value::Array(want), Value::Array(got)) => {
            want.len() == got.len()
                && want.iter().all(|value| {
                    got.iter().any(|other| config_equal(value, other))
                })
        }
        (Value::String(want), other) => {
            other.as_str() == Some(want.as_str())
                || i128::from_str(want).is_ok_and(|number| {
                    other.as_str().and_then(|text| i128::from_str(text).ok()) == Some(number)
                        || other.as_i64().map(i128::from) == Some(number)
                })
        }
        (want, other) => want == other,
    }
}

/// A number held at a path in a contract's answer, read as a number however
/// the contract chose to render it. A scale is compared by value, never by
/// the spelling of it, so an integer and its decimal string are one answer.
fn number_at(value: &Value, path: &[&str]) -> Option<i128> {
    let mut node = value;
    for segment in path {
        node = node.get(*segment)?;
    }
    if let Some(number) = node.as_i64() {
        return Some(i128::from(number));
    }
    if let Some(number) = node.as_u64() {
        return Some(i128::from(number));
    }
    node.as_str().and_then(|text| i128::from_str(text).ok())
}

/// Predict the pool a factory `deploy` will return, offline: derive the
/// child salt, then the XDR child id, then render the contract strkey.
fn predict_pool_child(
    network: MarketNetwork,
    factory_strkey: &str,
    salt_hex: &str,
    admin_strkey: &str,
) -> anyhow::Result<String> {
    let (_version, factory) = strkey_decode(factory_strkey)
        .filter(|(v, p)| *v == STRKEY_C && p.len() == 32)
        .ok_or_else(|| anyhow!("the settled factory id is not a contract strkey"))?;
    let factory: [u8; 32] = factory.try_into().expect("len checked");
    let child = pool_factory_child_salt(salt_hex, admin_strkey)
        .ok_or_else(|| anyhow!("the pool salt or operator strkey is not derivable"))?;
    let id = predict_contract_child(&market_network_id(network), &factory, &child);
    Ok(strkey_encode(STRKEY_C, &id))
}

pub fn deploy_side<E: Executor>(
    st: &Stellar<E>,
    spec: &SideSpec,
    side: char,
    nonce: u64,
    ids: &BTreeMap<Who, String>,
    shared: &SharedHashes,
) -> anyhow::Result<SideManifest> {
    let admin_key = Who::Admin.key_name();
    let frodo_key = Who::Frodo.key_name();
    let admin = ids[&Who::Admin].as_str();
    let frodo = ids[&Who::Frodo].as_str();
    let deploy_ledger = st.latest_ledger()?;
    let mut wasm = BTreeMap::new();
    let mut c = Contracts::default();
    let mut minted: BTreeMap<Tok, i64> = BTreeMap::new();

    // 1. side-specific code
    let pool_hash = st.upload(&spec.pool_wasm, &admin_key)?;
    let backstop_hash = st.upload(&spec.backstop_wasm, &admin_key)?;
    let factory_hash = st.upload(&spec.factory_wasm, &admin_key)?;
    wasm.insert("pool".into(), pool_hash.as_str().to_string());
    wasm.insert("backstop".into(), backstop_hash.as_str().to_string());
    wasm.insert("pool_factory".into(), factory_hash.as_str().to_string());
    wasm.insert("token".into(), shared.token.clone());
    wasm.insert("oracle".into(), shared.oracle.clone());
    wasm.insert("comet".into(), shared.comet.clone());
    wasm.insert("emitter".into(), shared.emitter.clone());

    // 2. mock tokens
    for (i, tok) in Tok::MOCK.iter().enumerate() {
        let id = st.deploy(
            &hash_of(&shared.token)?,
            &admin_key,
            Some(&salt(nonce, 10 + i as u32, side)),
            &[],
        )?;
        let sym = tok.as_str().to_uppercase();
        must_ok(
            st.invoke(
                id.as_str(),
                &admin_key,
                "initialize",
                &[
                    kv("admin", admin),
                    kv("decimal", tok.decimals()),
                    kv("name", &sym),
                    kv("symbol", &sym),
                ],
            )?,
            "token initialize",
        )?;
        c.set_token(*tok, id.as_str().to_string());
    }

    // 3. Comet LP seeded by admin
    mint_tok(
        st,
        &admin_key,
        &c,
        &mut minted,
        Tok::Blnd,
        admin,
        1_000 * SCALAR_7,
    )?;
    mint_tok(
        st,
        &admin_key,
        &c,
        &mut minted,
        Tok::Usdc,
        admin,
        25 * SCALAR_7,
    )?;
    let lp = st.deploy(
        &hash_of(&shared.comet)?,
        &admin_key,
        Some(&salt(nonce, 20, side)),
        &[],
    )?;
    must_ok(
        st.invoke(
            lp.as_str(),
            &admin_key,
            "init",
            &[
                kv("controller", admin),
                kv("tokens", json!([c.blnd, c.usdc])),
                kv("weights", json!(["8000000", "2000000"])),
                kv(
                    "balances",
                    json!([(1_000 * SCALAR_7).to_string(), (25 * SCALAR_7).to_string()]),
                ),
                kv("swap_fee", 30_000),
            ],
        )?,
        "comet init",
    )?;
    c.lp = lp.as_str().to_string();

    // 4. emitter (constructor metadata only; never called)
    c.emitter = st
        .deploy(
            &hash_of(&shared.emitter)?,
            &admin_key,
            Some(&salt(nonce, 30, side)),
            &[],
        )?
        .as_str()
        .to_string();

    // 5. backstop <-> factory cycle via predicted id
    let factory_salt = salt(nonce, 40, side);
    let predicted_factory = st.predict_id(&admin_key, &factory_salt)?;
    let drop_list = json!([
        [admin, (10_000_000 * SCALAR_7).to_string()],
        [frodo, (30_000_000 * SCALAR_7).to_string()]
    ]);
    c.backstop = st
        .deploy(
            &backstop_hash,
            &admin_key,
            Some(&salt(nonce, 41, side)),
            &[
                kv("backstop_token", &c.lp),
                kv("emitter", &c.emitter),
                kv("blnd_token", &c.blnd),
                kv("usdc_token", &c.usdc),
                kv("pool_factory", predicted_factory.as_str()),
                kv("drop_list", drop_list),
            ],
        )?
        .as_str()
        .to_string();
    let meta = json!({
        "backstop": c.backstop,
        "pool_hash": pool_hash.as_str(),
        "blnd_id": c.blnd
    });
    c.pool_factory = st
        .deploy(
            &factory_hash,
            &admin_key,
            Some(&factory_salt),
            &[kv("pool_init_meta", meta)],
        )?
        .as_str()
        .to_string();
    anyhow::ensure!(
        c.pool_factory == predicted_factory.as_str(),
        "factory id {} != predicted {}",
        c.pool_factory,
        predicted_factory.as_str()
    );

    // 6. oracle
    c.oracle = st
        .deploy(
            &hash_of(&shared.oracle)?,
            &admin_key,
            Some(&salt(nonce, 50, side)),
            &[],
        )?
        .as_str()
        .to_string();
    let assets: Vec<_> = Tok::ORACLE_ORDER
        .iter()
        .map(|t| json!({"Stellar": c.token(*t)}))
        .collect();
    must_ok(
        st.invoke(
            &c.oracle,
            &admin_key,
            "set_data",
            &[
                kv("admin", admin),
                kv("base", json!({"Other": "USD"})),
                kv("assets", json!(assets)),
                kv("decimals", 7),
                kv("resolution", 300),
            ],
        )?,
        "oracle set_data",
    )?;
    must_ok(
        set_prices(st, &c, &[2_000 * SCALAR_7, SCALAR_7, 1_000_000, SCALAR_7])?,
        "deploy set_price_stable",
    )?;

    // 7. whale funding
    mint_tok(
        st,
        &admin_key,
        &c,
        &mut minted,
        Tok::Blnd,
        frodo,
        100_000_000 * SCALAR_7,
    )?;
    mint_tok(
        st,
        &admin_key,
        &c,
        &mut minted,
        Tok::Usdc,
        frodo,
        2_600_000 * SCALAR_7,
    )?;
    mint_tok(
        st,
        &admin_key,
        &c,
        &mut minted,
        Tok::Stable,
        frodo,
        100_000 * E6,
    )?;
    mint_tok(
        st,
        &admin_key,
        &c,
        &mut minted,
        Tok::Xlm,
        frodo,
        1_000_000 * SCALAR_7,
    )?;
    mint_tok(st, &admin_key, &c, &mut minted, Tok::Weth, frodo, 100 * E9)?;

    // 8. Comet seeding
    must_ok(
        st.invoke(
            &c.lp,
            &frodo_key,
            "join_pool",
            &[
                kv("pool_amount_out", 10_000_000 * SCALAR_7),
                kv(
                    "max_amounts_in",
                    json!([
                        (110_000_000 * SCALAR_7).to_string(),
                        (2_600_000 * SCALAR_7).to_string()
                    ]),
                ),
                kv("user", frodo),
            ],
        )?,
        "comet join_pool",
    )?;

    // 9. pool
    let pool_val = must_ok(
        st.invoke(
            &c.pool_factory,
            &admin_key,
            "deploy",
            &[
                kv("admin", admin),
                kv("name", "Teapot"),
                kv("salt", salt(nonce, 60, side)),
                kv("oracle", &c.oracle),
                // Fixture divergence: upstream setup.rs passes 1_000_000. A zero take
                // rate keeps interest with suppliers so A/B deltas attribute to the
                // fork's logic, not backstop skimming; identical on both sides.
                kv("backstop_take_rate", 0),
                kv("max_positions", 6),
                kv("min_collateral", SCALAR_7),
            ],
        )?,
        "factory deploy",
    )?;
    c.pool = pool_val
        .and_then(|v| v.as_str().map(str::to_string))
        .context("factory deploy returned no address")?;

    // 10. reserves
    let mut reserves = BTreeMap::new();
    for (i, tok) in Tok::RESERVES.iter().enumerate() {
        let cfg = reserve_config(*tok, i as u32, spec.reserves.get(tok));
        must_ok(
            st.invoke(
                &c.pool,
                &admin_key,
                "queue_set_reserve",
                &[kv("asset", c.token(*tok)), kv("metadata", cfg)],
            )?,
            "queue_set_reserve",
        )?;
        let idx = must_ok(
            st.invoke(
                &c.pool,
                &admin_key,
                "set_reserve",
                &[kv("asset", c.token(*tok))],
            )?,
            "set_reserve",
        )?;
        let index = idx
            .and_then(|v| v.as_u64())
            .context("set_reserve returned no index")? as u32;
        anyhow::ensure!(
            index == i as u32,
            "reserve {tok:?} got index {index}, expected {i}"
        );
        reserves.insert(
            *tok,
            ReserveRef {
                index,
                decimals: tok.decimals(),
            },
        );
    }

    // 11. backstop deposit and activation
    must_ok(
        st.invoke(
            &c.backstop,
            &frodo_key,
            "deposit",
            &[
                kv("from", frodo),
                kv("pool_address", &c.pool),
                kv("amount", 50_000 * SCALAR_7),
            ],
        )?,
        "backstop deposit",
    )?;
    must_ok(
        st.invoke(&c.pool, &admin_key, "set_status", &[kv("pool_status", 3)])?,
        "set_status(3)",
    )?;
    let status = must_ok(
        st.invoke(&c.pool, &admin_key, "update_status", &[])?,
        "update_status",
    )?;
    anyhow::ensure!(
        status.as_ref().and_then(|v| v.as_u64()) == Some(1),
        "update_status returned {status:?}, expected 1 (backstop threshold not met?)"
    );

    // 12. seed positions (setup.rs:85-130)
    let seed = |asset: Tok, supply: i128, borrow: i128| -> Vec<Req> {
        vec![
            Req::new(req::SUPPLY_COLLATERAL, asset, supply),
            Req::new(req::BORROW, asset, borrow),
        ]
    };
    for reqs in [
        seed(Tok::Stable, 10_000 * E6, 8_000 * E6),
        seed(Tok::Weth, 10 * E9, 5 * E9),
        seed(Tok::Xlm, 100_000 * SCALAR_7, 65_000 * SCALAR_7),
    ] {
        must_ok(
            st.invoke(
                &c.pool,
                &frodo_key,
                "submit",
                &[
                    kv("from", frodo),
                    kv("spender", frodo),
                    kv("to", frodo),
                    kv("requests", request_json(&reqs, &c)),
                ],
            )?,
            "seed submit",
        )?;
    }

    Ok(SideManifest {
        label: spec.label.clone(),
        deploy_ledger,
        wasm,
        contracts: c,
        reserves,
        minted,
    })
}

fn mint_tok<E: Executor>(
    st: &Stellar<E>,
    admin_key: &str,
    c: &Contracts,
    minted: &mut BTreeMap<Tok, i64>,
    tok: Tok,
    to: &str,
    amount: i128,
) -> anyhow::Result<()> {
    must_ok(
        st.invoke(
            c.token(tok),
            admin_key,
            "mint",
            &[kv("to", to), kv("amount", amount)],
        )?,
        "mint",
    )?;
    *minted.entry(tok).or_insert(0) += i64::try_from(amount).context("mint amount exceeds i64")?;
    Ok(())
}

pub fn set_prices<E: Executor>(
    st: &Stellar<E>,
    c: &Contracts,
    prices: &[i128],
) -> anyhow::Result<Invocation> {
    let p: Vec<String> = prices.iter().map(|x| x.to_string()).collect();
    let inv = st.invoke(
        &c.oracle,
        &Who::Admin.key_name(),
        "set_price_stable",
        &[kv("prices", json!(p))],
    )?;
    Ok(inv)
}

pub fn deploy_pair<E: Executor>(
    st: &Stellar<E>,
    cfg: &PairConfig,
    nonce: u64,
) -> anyhow::Result<PairManifest> {
    let ids = ensure_identities(st)?;
    let shared = upload_shared(st, &cfg.shared, &Who::Admin.key_name())?;
    let a = deploy_side(st, &cfg.a, 'a', nonce, &ids, &shared)?;
    let b = deploy_side(st, &cfg.b, 'b', nonce, &ids, &shared)?;
    Ok(PairManifest {
        profile: cfg.profile,
        description: cfg.description.clone(),
        net: st.net.clone(),
        identities: ids,
        a,
        b,
    })
}

// ===================== Market deployment engine (plan §4) =====================

/// Non-serializable runtime context for one market run: the exact-testnet
/// RPC endpoint with its request headers, the external admin identity and
/// its explicit config directory, the run-local actor identities and their
/// config directory, the pinned CLI binary, and the isolated HOME/xdg paths.
/// It is deliberately not `Serialize`: no serializable market record can
/// carry a signer alias, config path, secret key, or seed phrase.
pub struct MarketExecutionContext {
    /// The network this context talks to. Every gate compares it against the
    /// network the frozen plan names; nothing else decides it.
    network: MarketNetwork,
    rpc_url: String,
    rpc_headers: Vec<(String, String)>,
    admin_alias: String,
    admin_config_dir: PathBuf,
    actor_config_dir: PathBuf,
    stellar_bin: PathBuf,
    xdg: PathBuf,
    cwd: PathBuf,
}

impl MarketExecutionContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc_url: String,
        rpc_headers: Vec<(String, String)>,
        admin_alias: String,
        admin_config_dir: PathBuf,
        actor_config_dir: PathBuf,
        stellar_bin: PathBuf,
        xdg: PathBuf,
        cwd: PathBuf,
    ) -> anyhow::Result<Self> {
        let alias = admin_alias.trim();
        anyhow::ensure!(!alias.is_empty(), "admin identity alias is empty");
        anyhow::ensure!(
            !alias
                .chars()
                .any(|c| c.is_whitespace() || c == '/' || c == '\\'),
            "admin identity alias must be a plain CLI name, not a path or a secret"
        );
        anyhow::ensure!(
            AccountId::from_str(alias).is_err(),
            "admin identity alias must not be a StrKey; give the CLI alias, not a public key"
        );
        anyhow::ensure!(
            !(admin_alias.len() == 56 && matches!(admin_alias.chars().next(), Some('S' | 'M'))),
            "admin identity alias looks like a private key or muxed account"
        );
        anyhow::ensure!(!rpc_url.trim().is_empty(), "market RPC URL is empty");
        // SEC001 residual: a manually constructed market context can never
        // smuggle a run-directory cwd or relative executable into the CLI
        // executor. The market path runs from the controlled root with a
        // previously resolved absolute CLI path; PathBuf execution needs no
        // shell quoting even when a legitimate path contains spaces.
        anyhow::ensure!(
            cwd == std::path::Path::new("/"),
            "market executor cwd must be the controlled root, not a run or launch directory"
        );
        anyhow::ensure!(
            stellar_bin.is_absolute() && !stellar_bin.as_os_str().is_empty(),
            "market CLI binary path must be absolute"
        );
        let ctx = Self {
            network: MarketNetwork::Testnet,
            rpc_url: rpc_url.trim().to_string(),
            rpc_headers,
            admin_alias: admin_alias.trim().to_string(),
            admin_config_dir,
            actor_config_dir,
            stellar_bin,
            xdg,
            cwd,
        };
        ctx.assert_testnet()?;
        Ok(ctx)
    }

    /// Bind this context to the network a real market was configured for.
    /// The lab stack stays on testnet; `assert_network` is the one gate.
    #[must_use]
    pub fn with_network(mut self, network: MarketNetwork) -> Self {
        self.network = network;
        self
    }

    /// The single network chokepoint of a market run: the network the plan
    /// and the config froze, against the network this context talks to. A
    /// wrong network is refused, never adapted; the message names neither a
    /// URL nor a path.
    pub fn assert_network(&self, expected: MarketNetwork) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.network == expected,
            "market execution is bound to {} but this run is frozen to {}",
            self.network.label(),
            expected.label()
        );
        Ok(())
    }

    pub fn assert_testnet(&self) -> anyhow::Result<()> {
        self.assert_network(MarketNetwork::Testnet)
    }

    pub fn stellar_bin(&self) -> &Path {
        &self.stellar_bin
    }
    pub fn xdg(&self) -> &Path {
        &self.xdg
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }
    pub fn rpc_headers(&self) -> &[(String, String)] {
        &self.rpc_headers
    }
    pub fn actor_config_dir(&self) -> &Path {
        &self.actor_config_dir
    }
    pub fn admin_config_dir(&self) -> &Path {
        &self.admin_config_dir
    }
    pub fn admin_alias(&self) -> &str {
        &self.admin_alias
    }

    fn admin_net(&self) -> Net {
        Net {
            rpc_url: self.rpc_url.clone(),
            passphrase: self.network.passphrase().to_string(),
            config_dir: self.admin_config_dir.clone(),
            headers: self.rpc_headers.clone(),
        }
    }

    /// A read-only view of this run's network: the same endpoint, headers
    /// and controlled child environment, with the read wall rather than the
    /// mutation wall. A settlement proof reads the chain and sends nothing,
    /// so it takes this and never an admin or actor context.
    pub fn read_net(&self) -> (Net, RealExecutor) {
        let net = Net {
            rpc_url: self.rpc_url.clone(),
            passphrase: self.network.passphrase().to_string(),
            config_dir: self.admin_config_dir.clone(),
            headers: self.rpc_headers.clone(),
        };
        let exec = RealExecutor {
            bin: self.stellar_bin.clone(),
            xdg: self.xdg.clone(),
            cwd: self.cwd.clone(),
            capture: Capture::Market {
                stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
                stderr_cap: MARKET_CAPTURE_STDERR_CAP,
                wall: Some(MARKET_READ_WALL),
            },
            rpc_env: crate::stellar::rpc_env_for(&net),
        };
        (net, exec)
    }

    fn actor_net(&self) -> Net {
        Net {
            rpc_url: self.rpc_url.clone(),
            passphrase: self.network.passphrase().to_string(),
            config_dir: self.actor_config_dir.clone(),
            headers: self.rpc_headers.clone(),
        }
    }

    fn rpc_host_label(&self) -> String {
        let rest = self
            .rpc_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.rpc_url);
        let authority = rest.split('/').next().unwrap_or("");
        authority.rsplit('@').next().unwrap_or("").to_string()
    }
}

/// One run's market journal: the numbered phase/slot custody journal under
/// the run directory. Addressed exclusively by numeric coordinates — the
/// name is never derived from record text, so no stored or supplied string
/// can steer a journal write outside `market-pending/`.
#[derive(Clone)]
pub struct MarketJournal {
    out: PathBuf,
}

impl MarketJournal {
    pub fn open(out: &Path) -> Self {
        Self {
            out: out.to_path_buf(),
        }
    }

    pub fn out(&self) -> PathBuf {
        self.out.clone()
    }

    pub fn begin(&self, phase: u32, slot: u32, record: &MarketCustodyRecord) -> anyhow::Result<()> {
        begin_market_pending(&self.out, phase, slot, record)
    }

    pub fn finish(
        &self,
        phase: u32,
        slot: u32,
        result: &MarketPendingResult,
    ) -> anyhow::Result<()> {
        finish_market_pending(&self.out, phase, slot, result)
    }

    pub fn read_intent(
        &self,
        phase: u32,
        slot: u32,
    ) -> anyhow::Result<Option<MarketCustodyRecord>> {
        // The pinned status reader is the sole path to journal records: an
        // absent coordinate is unreserved, while a malformed, relaxed-
        // network, or settlement-without-intent coordinate stays fail-closed.
        Ok(
            crate::market_status::read_journal_coordinate(&self.out, phase, slot)?
                .map(|(intent, _)| intent),
        )
    }

    /// Strictly verified settlement for one numbered coordinate, if any.
    /// `Ok(None)` means the coordinate is unreserved or holds an open
    /// intent only; a standalone, malformed, mismatched, or symlinked
    /// settlement is an error and can never read as a completed slot.
    pub fn result(&self, phase: u32, slot: u32) -> anyhow::Result<Option<MarketPendingResult>> {
        Ok(
            crate::market_status::read_journal_coordinate(&self.out, phase, slot)?
                .and_then(|(_, result)| result),
        )
    }
}

/// Operator commands to the deployment state machine. Headless and TUI
/// callers share this grammar; only the presentation differs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeployAct {
    Init,
    Continue,
    Confirm { token: String },
    Halt,
}

/// The deployment event grammar. Every value-moving mutation is gated: the
/// engine emits `Confirm` and blocks until `DeployAct::Confirm` carries the
/// matching token; `Continue` cannot satisfy it. `Custody { unresolved:
/// true, .. }` is terminal: the engine replays nothing and stops.
#[derive(Clone, Debug)]
pub enum DeployEvent {
    Phase { id: usize, label: Safe },
    Confirm { token: String, summary: Safe },
    Custody { unresolved: bool, note: Safe },
    Done(Box<MarketManifest>),
    Failed(Safe),
}

/// Market deployment state machine. Owns the frozen configuration, the
/// non-serializable execution context, and the numbered custody journal.
/// Every value-moving mutation is gated: the engine blocks until the
/// operator's confirmation token matches, never retries an ambiguous
/// submission, and stops on unresolved custody.
pub struct MarketDeployer {
    cfg: MarketConfig,
    ctx: MarketExecutionContext,
    journal: MarketJournal,
    exec: RealExecutor,
    halted: bool,
    unresolved: Option<Safe>,
    gate: Option<Gate>,
    progress: Progress,
    manifest: Option<MarketManifest>,
    terminal: Option<DeployEvent>,
    /// The violations a stopped step reported: set when the run ends because
    /// a confirmed step produced a definite refusal, empty otherwise.
    /// Surfaces print them; the archived intent is the durable copy.
    stopped_violations: Vec<Safe>,
    /// Release provenance of every release-form pin, taken before
    /// `materialize_missing_code` turns a pin the network already holds into
    /// a live hash pin (which carries no provenance). Frozen into the plan so
    /// a resume re-resolves the same exact versions.
    releases: BTreeMap<String, crate::release::Provenance>,
    plan_written: bool,
    /// The deployment nonce the salt table is keyed to. `0` means no plan
    /// has been bound, and with it no authorized mutation.
    nonce: u64,
    /// Re-derived artifact digests, role to SHA-256, proven at preflight.
    artifacts: BTreeMap<String, Sha256Hex>,
    /// Contracts this run has deployed, by role. A deployment is recorded
    /// only when its returned id matched its predicted id exactly.
    deployed: BTreeMap<String, ContractId>,
    /// The graph this run executes: the lab's, or the real market's derived
    /// from its configuration. Bound once, at construction, and frozen with
    /// the plan — every coordinate, salt and resume decision reads this and
    /// nothing else, so a run cannot execute a graph it did not freeze.
    schedule: Vec<MSlot>,
    /// The real market's derived shape; `None` is the lab graph.
    shape: Option<RealShape>,
    /// The run's bindings to the network its configuration froze, resolved
    /// once at construction. A CLI seam is a borrow of these, never a fresh
    /// description of the network assembled mid-run: the network a run talks
    /// to is decided before its first send and cannot change after it.
    admin_net: Net,
    actor_net: Net,
}
/// The operator's intent for one `market deploy`. The graph a run executes is
/// its configuration's, never an option's; what an option changes is whether
/// the final activation coordinate exists. It is frozen with the plan, so
/// resuming a run under a different intent is a different deployment and is
/// refused, not silently re-planned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarketDeployOptions {
    /// End the run with the pool at `set_status(0)`, the activation the
    /// reviewed pool config asks for. A run without it deploys and backs the
    /// pool and leaves it in its creation status, for an operator to activate
    /// after their own review.
    pub activate: bool,
    /// Where the not-yet-live Wasm bytes of a digest-pinned launch profile are
    /// found. Path-pinned configs need no such directory.
    pub wasm_dir: Option<PathBuf>,
}

struct Gate {
    phase: u32,
    slot: u32,
    token: String,
    record: MarketCustodyRecord,
    summary: Safe,
}

#[derive(Default)]
struct Progress {
    admin: Option<AccountId>,
    identities: BTreeMap<Who, AccountId>,
    phase_done: std::collections::BTreeSet<u32>,
    /// The synthetic legs' issuer accounts, recovered from the coordinates
    /// that created them. Their secrets are deleted after the lock, so the
    /// journal — not the keystore — is where a resumed run finds them.
    issuers: Vec<AccountId>,
}

/// Outcome of one resume-law reconciliation pass. `Settled` is returned only
/// when the kind's declared public proof uniquely attributes the effect;
/// anything else is `Unresolved` and the run is abandoned until reconciled.
enum Reconciled {
    Settled,
    /// The open intent provably applied nothing and was archived, so the
    /// coordinate runs again on the ordinary gated path.
    Replay,
    Unresolved(Safe),
}

/// What read-only preflight establishes. Nothing here is authorized to
/// mutate: it is the fact set a run must prove before a plan can bind.
struct Preflight {
    admin: AccountId,
    artifacts: BTreeMap<String, Sha256Hex>,
    nonce: u64,
}

/// The canonical SHA-256 of the parsed configuration. Two configurations
/// that parse to different values — including one whose artifact digests or
/// quorum shape were edited — produce different digests, so a plan frozen
/// against one cannot be honored by the other.
fn market_config_digest(cfg: &MarketConfig) -> anyhow::Result<String> {
    let text = toml::to_string_pretty(cfg).context("canonicalizing market config")?;
    Ok(sha256_hex(text.as_bytes()))
}

/// The deployment nonce, derived from the configuration digest and this run
/// directory. It is deterministic so a resumed run re-derives the same salt
/// table and the same predicted addresses rather than inventing new ones, and
/// it is unique to this configuration and directory so two different markets
/// never share a salt.
fn market_run_nonce(cfg: &MarketConfig, out: &Path) -> anyhow::Result<u64> {
    let digest = market_config_digest(cfg)?;
    let mut preimage = b"abtest-market-nonce-v1".to_vec();
    preimage.extend_from_slice(digest.as_bytes());
    preimage.extend_from_slice(out.to_string_lossy().as_bytes());
    let hash = sha256_hex(&preimage);
    let mut word = [0u8; 8];
    for (i, b) in word.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow!("the derived deployment nonce is not hexadecimal"))?;
    }
    Ok(u64::from_be_bytes(word))
}

// ============ Market deployment graph: schedule, salts, frozen plan ============

/// The market graph's salt side. The pair graph uses `'a'`/`'b'`, so no
/// market salt can ever collide with a pair salt.
pub(crate) const MARKET_SIDE: char = 'm';

/// The journal coordinate range the pre-command scan inspects. A coordinate
/// outside it would be invisible to the scan, so an ambiguous submission
/// there could never surface as unresolved custody and the no-replay law
/// would silently break. Every scheduled slot is checked against this range
/// at construction, so a slot added past the scanned range fails at startup.
pub(crate) const MARKET_SCANNED_PHASES: u32 = 12;
pub(crate) const MARKET_SCANNED_SLOTS: u32 = 40;

/// The coordinate every market graph publishes at. The resume law that reads
/// an interrupted publication must ask the journal before it can know which
/// graph a run froze, so both graphs keep publication here; `verify_frozen_plan`
/// refuses a graph that breaks the law rather than letting a publication be
/// invisible to the scan.
pub(crate) const MARKET_PUBLISH_PHASE: u32 = 12;
pub(crate) const MARKET_PUBLISH_SLOT: u32 = 1;

/// The frozen deployment plan's filename, inside the run directory.
pub(crate) const MARKET_PLAN_FILE: &str = "deployment-plan.toml";
/// The completed run's manifest filename, inside the run directory.
pub(crate) const MARKET_MANIFEST_FILE: &str = "market-manifest.toml";

/// One mutation of the protected-market graph, with its journal coordinate.
#[derive(Clone, Debug)]
pub(crate) struct MSlot {
    pub phase: u32,
    pub slot: u32,
    pub action: &'static str,
    pub kind: MarketMutationKind,
    pub op: MOp,
}

/// One row of the plan listing: what the frozen graph does at one journal
/// coordinate, the role it acts on, and whether the journal has already
/// settled it. A robot consumer approves this listing together with the
/// plan file's digest, so the rows it reads are the coordinates the engine
/// will attempt and the ones it will skip.
#[derive(Clone, Debug)]
pub(crate) struct PlanStep {
    pub phase: u32,
    pub slot: u32,
    pub action: &'static str,
    pub kind: MarketMutationKind,
    pub target_role: Option<String>,
    pub settled: bool,
}

/// One operation of the 12-phase protected-market graph (plan §4). The graph
/// is data: the schedule, its salt table, and its journal coordinates can be
/// proven consistent without executing anything, and the vector's order is
/// the order the engine runs operations in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MOp {
    FundKey {
        who: Who,
    },
    Upload {
        role: &'static str,
    },
    DeployToken {
        tok: Tok,
    },
    InitToken {
        tok: Tok,
    },
    DeploySource {
        index: usize,
    },
    InitSource {
        index: usize,
    },
    SourcePrice {
        index: usize,
    },
    DeployGovernance,
    DeployRuntime,
    /// One governance proposal. The proposal content is built at execution
    /// time from the settled phase-3/4 deployments; the operation the
    /// proposal asks governance to perform is never static data, because it
    /// must bind the concrete deployed source and asset addresses.
    Propose {
        tok: Tok,
    },
    ExecuteProposal {
        id: u64,
    },
    Refresh {
        tok: Tok,
    },
    DeployAdapter,
    DeployComet,
    InitComet,
    DeployEmitter,
    DeployBackstop,
    DeployFactory,
    DeployPool,
    QueueReserve {
        tok: Tok,
    },
    SetReserve {
        tok: Tok,
    },
    SeedBackstop,
    SetStatus {
        to: u32,
    },
    UpdateStatus,
    /// Mint one configured playable balance, by index into `cfg.balances`.
    Mint {
        index: usize,
    },
    SeedPositions,
    Publish,
    // ---- real market (ADR-0008 mainnet launch). These operations exist
    // only in a real market's graph; the lab graph never schedules them and
    // their arms never read the lab's mock tokens. A classic asset (the
    // synthetic shim legs) has no contract to configure: it is created,
    // trusted, deployed as an SAC, and minted by its own issuer key.
    /// Create and fund the run-local issuer account of one shim leg.
    CreateIssuer {
        leg: usize,
    },
    /// The operator's unlimited trustline in one leg, so a mint can be held.
    TrustLeg {
        leg: usize,
    },
    /// `contract asset deploy` for one leg: the network's own Stellar Asset
    /// Contract stands for the classic asset inside the pool's Comet.
    DeploySac {
        leg: usize,
    },
    /// Mint the leg's seeded balance to the operator, signed by the issuer.
    MintLeg {
        leg: usize,
    },
    /// The issuer's permanent lock: master weight 0, thresholds 1,1,1, with a
    /// Horizon-independent readback before the coordinate settles.
    LockIssuer {
        leg: usize,
    },
    /// Remove the operator's now-empty trustline in one leg, after the
    /// deposit has moved both legs into Comet.
    DropTrust {
        leg: usize,
    },
    /// Deploy one Pyth Lazer source instance serving its configured feeds.
    RealDeployLazer {
        instance: usize,
    },
    /// Deploy this market's own proxy runtime, governance-owned at birth.
    RealDeployRuntime,
    /// Verify a runtime the config reuses instead of deploying one.
    RealVerifyRuntime,
    /// Deploy this market's proxy governance.
    RealDeployGovernance,
    /// One SetProxy proposal for one asset's own ProxyConfig.
    RealProposeAsset {
        asset: usize,
    },
    /// Execute the proposal its propose coordinate created. The id is not
    /// a graph position: a shared governance already holds the first pool's
    /// proposals, so it is read from that coordinate's settled facts.
    RealExecuteAsset {
        asset: usize,
    },
    /// Ask the runtime to refresh one asset; settles on the freshness proof.
    RealRefreshAsset {
        asset: usize,
    },
    /// Deploy this market's own price adapter over its runtime.
    RealDeployAdapter,
    /// Verify the adapter the config names instead of deploying one.
    RealVerifyAdapter,
    /// Deploy the pool's Comet (its Wasm is pinned as already live).
    RealDeployComet,
    /// Initialize Comet: controller, the two leg SACs, and the readback that
    /// makes a first-come-first-served initializer race impossible.
    RealInitComet,
    /// Deploy the event emitter (pinned live, like Comet).
    RealDeployEmitter,
    /// Deploy the backstop against the predicted factory.
    RealDeployBackstop,
    /// Deploy the pool factory.
    RealDeployFactory,
    /// Deploy the pool through the factory, with the config's own parameters.
    RealDeployPool,
    /// Queue one real reserve's configuration on the pool.
    RealQueueReserve {
        asset: usize,
    },
    /// Enable one real reserve on the pool.
    RealSetReserve {
        asset: usize,
    },
    /// Deposit the LP into the backstop and read the threshold back.
    RealBackstopDeposit,
    /// Freeze the Comet's transfers once the backing is in place.
    RealFreezeComet,
    /// The activation itself: `set_status(0)`, where the pool re-checks the
    /// backstop threshold. Only scheduled with `--activate`.
    RealSetStatus {
        to: u32,
    },
    /// Phase 11. `propose_admin(pool.admin)`: the pool's admin is held by the
    /// operator until its final admin accepts. Two-step, so this coordinate
    /// settles on the pool's temporary `PropAdmin` entry, not on `get_admin`.
    RealHandoffPool,
    /// Phase 11. `transfer_ownership(oracle.admin, live_until_ledger)` on one
    /// Lazer source this run deployed. Two-step; the deadline is frozen into
    /// this coordinate's own custody record at its gate, so a resume asks
    /// about the transfer this run actually proposed.
    RealHandoffLazer {
        instance: usize,
    },
    /// Phase 11, one half of a governance `SetRole` handover: the proposal.
    /// `grant` is the third element of the operation — `true` puts the
    /// oracle admin in `Admin`, `false` takes the operator out of it.
    RealProposeRole {
        grant: bool,
    },
    /// Phase 11, the execution of the proposal its `RealProposeRole` twin
    /// created. Its id comes from that coordinate's settled facts, exactly as
    /// an asset configuration's does.
    RealExecuteRole {
        grant: bool,
    },
    /// Phase 11, last. `set_controller(pool.admin)`: one step, so the
    /// readback is the controller the Comet reports.
    RealHandoffComet,
}

/// The create2 salt role each deterministically deployed market contract is
/// created at, or `None` for operations that consume no salt: uploads are
/// content-addressed by their Wasm hash, and configuration or value-moving
/// invokes mutate state without creating an address. The table below is
/// therefore exactly the set of deterministic deployments.
///
/// Every role is distinct from every other role, and the market range
/// (201..236) is disjoint from the pair graph's roles (`10 + i`, `20`, `30`,
/// `40`, `41`, `50`, `60`); the market graph signs with side `'m'` while the
/// pair graph uses `'a'`/`'b'`. `market_salt_table_is_collision_free` proves
/// the whole table is collision-free.
pub(crate) fn market_salt_role(op: &MOp) -> Option<u32> {
    Some(match op {
        MOp::DeployToken { tok } => 201 + Tok::MOCK.iter().position(|t| *t == *tok)? as u32,
        MOp::DeploySource { index } if *index < 3 => 211 + *index as u32,
        MOp::DeployGovernance => 222,
        MOp::DeployRuntime => 221,
        MOp::DeployAdapter => 223,
        MOp::DeployComet => 231,
        MOp::DeployEmitter => 232,
        MOp::DeployBackstop => 233,
        MOp::DeployFactory => 234,
        MOp::DeployPool => 235,
        // A real market's deterministic deployments. The 240.. range is
        // disjoint from the lab table above and from the pair graph, and one
        // real market's graph never reuses a role twice: Lazer instances take
        // consecutive roles by position, and every other deployable has one
        // role.
        MOp::RealDeployLazer { instance } => 240 + *instance as u32,
        MOp::RealDeployRuntime => 260,
        MOp::RealDeployGovernance => 261,
        MOp::RealDeployAdapter => 262,
        MOp::RealDeployComet => 263,
        MOp::RealDeployEmitter => 264,
        MOp::RealDeployBackstop => 265,
        MOp::RealDeployFactory => 266,
        MOp::RealDeployPool => 267,
        _ => return None,
    })
}

/// The salt-role band a front-run Comet recovery deploys fresh backing pools
/// under, and how many it will try. `init` on the production Comet is
/// first-come-first-served, so a claimed instance is replaced rather than
/// accepted; the attempts are bounded so a hostile network cannot make one
/// coordinate deploy forever, and the band is disjoint from every role in
/// `market_salt_role`, which the collision test below covers.
pub(crate) const COMET_CLAIM_ROLE: u32 = 280;
pub(crate) const COMET_CLAIM_ATTEMPTS: u32 = 3;

/// How long a two-step ownership transfer this run proposes stays acceptable:
/// ten days of ledgers, the same bound the pool puts on its own admin
/// proposal (pool/src/storage.rs `set_proposed_admin`), so a curator signs one
/// deadline for a whole launch rather than one per contract.
pub(crate) const HANDOFF_TTL_LEDGERS: u32 = 172_800;

/// The `(nonce, role, side)` triples one graph consumes for one nonce. The
/// graph is passed in because a run's graph is the one it froze, not the
/// function the code happens to be compiled with.
pub(crate) fn market_salt_triples(schedule: &[MSlot], nonce: u64) -> Vec<(u64, u32, char)> {
    let mut triples: Vec<(u64, u32, char)> = schedule
        .iter()
        .filter_map(|s| market_salt_role(&s.op))
        .map(|role| (nonce, role, MARKET_SIDE))
        .collect();
    triples.sort();
    triples
}

/// Whether a journal coordinate falls inside the range the scan inspects.
pub(crate) fn market_coordinate_is_scanned(phase: u32, slot: u32) -> bool {
    (1..=MARKET_SCANNED_PHASES).contains(&phase) && (1..=MARKET_SCANNED_SLOTS).contains(&slot)
}

/// Whether this run directory holds an interrupted local publication: the
/// publication coordinate's frozen intent is open with no settlement. The
/// strict journal reader answers, so a malformed or foreign record is a
/// refusal rather than an admission, and a settled publication is never
/// reported pending. Both resume entrypoints need this, because a manifest
/// file on disk is not proof of a completed run: a run interrupted between the
/// manifest's atomic rename and its settlement holds a manifest with no
/// settlement, and only the journal can tell that apart from a finished run.
pub(crate) fn publication_is_unsettled(out: &Path) -> anyhow::Result<bool> {
    // Both graphs publish at the same coordinate, which is what lets this
    // resume law ask the journal without first knowing which graph a run
    // froze. `verify_frozen_plan` refuses any graph that breaks that law, so
    // a run whose publication is elsewhere is refused before it can begin.
    Ok(matches!(
        crate::market_status::read_journal_coordinate(
            out,
            MARKET_PUBLISH_PHASE,
            MARKET_PUBLISH_SLOT
        )?,
        Some((_, None))
    ))
}

/// The frozen 12-phase protected-market graph, in executable order.
///
/// Order law: every artifact is uploaded before anything is deployed; the
/// ownership cutover completes before the adapter is deployed; the adapter is
/// deployed before the pool is deployed; the manifest is published last,
/// after every battery has passed.
///
/// Why the cutover is not a transfer: the frozen proxy runtime's constructor
/// binds ownership to the governance contract *at deploy time*
/// (`contract/src/lib.rs:129`), and the frozen governance constructor takes
/// the proxy address in return (`governance-contract/src/lib.rs:43`).
/// Neither address can exist before the other is chosen, so both are
/// established by deterministic prediction, the runtime is deployed already
/// governance-owned, and ownership is never operator-held at any point — it
/// is verified as a read-only precondition rather than moved. Proxy
/// configuration is owner-gated, so it runs as governance proposals, and
/// every proposal is two journaled intents (create, then execute).
pub(crate) fn market_schedule() -> Vec<MSlot> {
    let mut slots: Vec<MSlot> = Vec::new();
    let mut add =
        |phase: u32, slot: u32, action: &'static str, kind: MarketMutationKind, op: MOp| {
            slots.push(MSlot {
                phase,
                slot,
                action,
                kind,
                op,
            })
        };
    // Phase 1: run-local actor identities. Provisioning submits a funding
    // operation, so it is gated and journaled like any other mutation.
    for (i, who) in [Who::Frodo, Who::Sam, Who::User].iter().enumerate() {
        add(
            1,
            1 + i as u32,
            "provision actor identity",
            MarketMutationKind::FundKey,
            MOp::FundKey { who: *who },
        );
    }
    // Phase 2: every pinned artifact, uploaded before anything is deployed.
    for (i, role) in MARKET_ARTIFACT_ROLES.iter().enumerate() {
        add(
            2,
            1 + i as u32,
            "upload artifact",
            MarketMutationKind::Upload,
            MOp::Upload { role },
        );
    }
    // Phase 3: the five fake tokens, each deployed and then initialized with
    // the operator G-address as its admin and the token's own decimals.
    for (i, tok) in Tok::MOCK.iter().enumerate() {
        add(
            3,
            1 + i as u32,
            "deploy token",
            MarketMutationKind::Deploy,
            MOp::DeployToken { tok: *tok },
        );
    }
    for (i, tok) in Tok::MOCK.iter().enumerate() {
        add(
            3,
            6 + i as u32,
            "initialize token",
            MarketMutationKind::Config,
            MOp::InitToken { tok: *tok },
        );
    }
    // Phase 4: three independent sources with the frozen SEP-40 shape.
    for i in 0..3usize {
        add(
            4,
            1 + i as u32,
            "deploy source",
            MarketMutationKind::Deploy,
            MOp::DeploySource { index: i },
        );
    }
    for i in 0..3usize {
        add(
            4,
            4 + i as u32,
            "configure source",
            MarketMutationKind::Config,
            MOp::InitSource { index: i },
        );
    }
    for i in 0..3usize {
        add(
            4,
            7 + i as u32,
            "publish source prices",
            MarketMutationKind::Invoke,
            MOp::SourcePrice { index: i },
        );
    }
    // Phase 5: the proxy runtime, governance-owned from the moment it exists.
    add(
        5,
        1,
        "deploy proxy runtime",
        MarketMutationKind::Deploy,
        MOp::DeployRuntime,
    );
    // Phase 7: governance materializes and then authorizes the proxy
    // configuration only it can perform — all three sources per asset,
    // 3-of-3, 3600s maximum age, 60s clock drift — as one proposal pair per
    // asset, each proposal two separate journaled intents.
    //
    // Proposal-id law (verified against the pinned sources): the kernel's
    // `next_id` starts at 0 and `create` rejects any id != next_id, so the
    // four SetProxy proposals occupy ids 0..=3 in oracle order. No enabling
    // proposals exist or are needed: `require_authorized` admits the caller
    // holding `Role::Admin` (governance-contract/src/engine.rs:37) and the
    // constructor grants Admin to the frozen operator. A zero uniform TTL at
    // construction is what the frozen adjacent execute slots require; the
    // execute arm still proves ttl_ns == 0 read-only before submitting.
    add(
        7,
        1,
        "deploy governance",
        MarketMutationKind::Deploy,
        MOp::DeployGovernance,
    );
    for (i, tok) in Tok::ORACLE_ORDER.iter().enumerate() {
        add(
            7,
            2 + i as u32,
            "propose proxy configuration",
            MarketMutationKind::Governance,
            MOp::Propose { tok: *tok },
        );
    }
    // The proposal id a slot executes is the oracle-order position of the
    // asset whose SetProxy proposal it is: governance assigns ids in create
    // order and the create slots above run in oracle order, so position,
    // scheduled slot, and proposal id stay one and the same law.
    for (id, _tok) in Tok::ORACLE_ORDER.iter().enumerate() {
        add(
            7,
            6 + id as u32,
            "execute proxy configuration",
            MarketMutationKind::Governance,
            MOp::ExecuteProposal { id: id as u64 },
        );
    }
    // Phase 6: one refresh per asset, proving the configured quorum serves.
    for (i, tok) in Tok::ORACLE_ORDER.iter().enumerate() {
        add(
            6,
            1 + i as u32,
            "refresh asset",
            MarketMutationKind::Invoke,
            MOp::Refresh { tok: *tok },
        );
    }
    // Phase 8: the pool's price adapter — governance-owned, reading only the
    // proxy runtime, resolution 1.
    add(
        8,
        1,
        "deploy adapter",
        MarketMutationKind::Deploy,
        MOp::DeployAdapter,
    );
    // Phase 9: Comet and emitter, the predicted-ID backstop/factory cycle,
    // and the pool with the operator as admin and the adapter as oracle.
    add(
        9,
        1,
        "deploy comet",
        MarketMutationKind::Deploy,
        MOp::DeployComet,
    );
    add(
        9,
        2,
        "initialize comet",
        MarketMutationKind::Config,
        MOp::InitComet,
    );
    add(
        9,
        3,
        "deploy emitter",
        MarketMutationKind::Deploy,
        MOp::DeployEmitter,
    );
    add(
        9,
        4,
        "deploy backstop",
        MarketMutationKind::Deploy,
        MOp::DeployBackstop,
    );
    add(
        9,
        5,
        "deploy factory",
        MarketMutationKind::Deploy,
        MOp::DeployFactory,
    );
    add(
        9,
        6,
        "deploy pool",
        MarketMutationKind::Deploy,
        MOp::DeployPool,
    );
    // Phase 10: reserves, backstop seeding, and the status transition.
    for (i, tok) in Tok::RESERVES.iter().enumerate() {
        add(
            10,
            1 + i as u32,
            "queue reserve",
            MarketMutationKind::Config,
            MOp::QueueReserve { tok: *tok },
        );
    }
    for (i, tok) in Tok::RESERVES.iter().enumerate() {
        add(
            10,
            4 + i as u32,
            "enable reserve",
            MarketMutationKind::Config,
            MOp::SetReserve { tok: *tok },
        );
    }
    add(
        10,
        7,
        "seed backstop",
        MarketMutationKind::Invoke,
        MOp::SeedBackstop,
    );
    add(
        10,
        8,
        "lower pool status",
        MarketMutationKind::Config,
        MOp::SetStatus { to: 3 },
    );
    add(
        10,
        9,
        "advance pool status",
        MarketMutationKind::Invoke,
        MOp::UpdateStatus,
    );
    // Phase 11: configured playable balances and the seeded healthy positions.
    for i in 0..MARKET_BALANCE_COUNT {
        add(
            11,
            1 + i as u32,
            "mint playable balance",
            MarketMutationKind::Invoke,
            MOp::Mint { index: i },
        );
    }
    add(
        11,
        1 + MARKET_BALANCE_COUNT as u32,
        "seed positions",
        MarketMutationKind::Invoke,
        MOp::SeedPositions,
    );
    // Phase 12: publication, after every battery has passed.
    add(
        12,
        1,
        "publish market manifest",
        MarketMutationKind::Config,
        MOp::Publish,
    );
    slots
}

/// The number of configured playable balances the graph mints. The count is
/// part of the frozen plan; a configuration with a different number of
/// balances is a different deployment, bound by its own config digest.
pub(crate) const MARKET_BALANCE_COUNT: usize = 3;

// ---------- real market graph (ADR-0008 mainnet launch) ----------
//
// A real market's graph is the same journaled machine as the lab's, with a
// different schedule and different operations: its legs are classic assets
// issued by run-local issuer accounts, its prices come from Lazer instances
// and existing SEP-40 contracts, its pool parameters are the registry's
// reviewed ones, and its activation is the backstop threshold check the pool
// performs at `set_status(0)`. Nothing here reads the lab's mock tokens, and
// the lab graph never schedules an operation from this section.

/// The single law of Lazer instance numbering: an instance is numbered by its
/// order among the Lazer sources of a reserve — never by its position in that
/// reserve's whole source list, which a SEP-40 oracle ahead of it would
/// shift. The shape that schedules the deploys, the `SetProxy` operation that
/// names the source contracts, and the manifest that mirrors them all number
/// instances through here, so a reserve that quotes an existing oracle first
/// cannot bind a contract no coordinate of this graph ever deployed.
pub(crate) fn lazer_instances(
    reserve: &crate::manifest::MarketReserve,
) -> impl Iterator<Item = (usize, &crate::manifest::OracleSource)> {
    reserve
        .sources
        .iter()
        .filter(|source| source.kind == crate::manifest::SourceKind::Lazer)
        .enumerate()
}

/// The role transfers one run owes at the end of its graph, and the only
/// fact about them that is not config: whether anyone is owed anything at all.
///
/// Why this is decided when the graph is built rather than when phase 11 runs:
/// the frozen plan records the exact slot list a run executes and every resume
/// must reproduce it, so a coordinate cannot be discovered to be needed partway
/// through. Its inputs are the configuration's two final admins and the
/// operator's own public key — the same keystore read preflight performs, and
/// the same address the plan already binds as `admin` and refuses a resume
/// over. Nothing here is a chain read; the graph stays a function of the
/// configuration and the run's own identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RealHandoff {
    /// The pool's final admin, which is also the Comet's next controller.
    pool_admin: String,
    /// The receiver of this run's oracle roles: `oracle.admin`, or the pool's
    /// final admin where the pool config names no separate one.
    oracle_admin: String,
}

impl RealHandoff {
    /// The roles this run owes away, or `None` when it owes none: both final
    /// admins are the operator itself, which is every configuration written
    /// before a curator could hold a pool, and such a run's graph stays
    /// byte-for-byte the graph it always was.
    fn owed(cfg: &MarketConfig, operator: Option<&str>) -> Option<Self> {
        let pool = cfg.pool.as_ref()?;
        let operator = operator?;
        let oracle_admin = pool
            .oracle_admin
            .clone()
            .unwrap_or_else(|| pool.admin.clone());
        (pool.admin != operator || oracle_admin != operator).then_some(Self {
            pool_admin: pool.admin.clone(),
            oracle_admin,
        })
    }
}

/// The graph shape a real market's config implies, derived from config alone.
/// The schedule must be frozen before the first mutation and reproduced
/// byte-for-byte on every resume, so no field here is ever a chain read:
/// whether an asset is already registered on a reused runtime is settled at
/// execution by a readback that skips the submission, never by omitting the
/// coordinate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RealShape {
    /// Roles whose bytes this run uploads: the required roles minus those
    /// pinned to a Wasm hash already live on the network.
    upload_roles: Vec<&'static str>,
    /// Roles pinned live: proved fetchable, never uploaded.
    live_roles: Vec<&'static str>,
    /// One entry per Lazer source instance: the feed ids it must serve. An
    /// instance serves the feed its position holds in every reserve that
    /// declares a Lazer source at that position, which is how one deployed
    /// source can stand behind several assets.
    lazer_feeds: Vec<Vec<String>>,
    /// Whether this run deploys its own proxy runtime (and governance).
    deploy_runtime: bool,
    /// Whether this run deploys its own price adapter.
    deploy_adapter: bool,
    /// Whether the run ends in `set_status(0)`; frozen with the schedule, so
    /// resuming a finished run under a different activation intent is a
    /// different deployment, never a silent re-plan.
    activate: bool,
    /// The role transfers this run owes, frozen with the schedule for the
    /// same reason `activate` is. `None` schedules no phase-11 coordinate.
    handoff: Option<RealHandoff>,
}

impl RealShape {
    /// Derive the graph shape from one validated real-market configuration.
    pub(crate) fn from_config(cfg: &MarketConfig) -> anyhow::Result<Self> {
        let pool = cfg
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("only a real market has a real graph"))?;
        let mut upload_roles = Vec::new();
        let mut live_roles = Vec::new();
        for role in cfg.required_roles() {
            let pin = cfg
                .artifacts
                .get(role)
                .ok_or_else(|| anyhow!("a required artifact role is unpinned"))?;
            // A hash the network already serves is proven fetchable, never
            // uploaded: mainnet upload fees make an unnecessary submission a
            // cost the operator did not authorize.
            if pin.wasm_hash().is_ok() && matches!(pin, crate::manifest::ArtifactPin::Live(_)) {
                live_roles.push(role);
            } else {
                upload_roles.push(role);
            }
        }
        // Lazer instances are positional among the Lazer sources: the k-th
        // Lazer source of every reserve shares one deployed contract, which
        // supports every feed any reserve asks that instance to serve.
        let mut lazer_feeds: Vec<Vec<String>> = Vec::new();
        for reserve in &pool.reserves {
            for (instance, source) in lazer_instances(reserve) {
                let feed = source
                    .feed_id
                    .clone()
                    .ok_or_else(|| anyhow!("a Lazer source names no feed id"))?;
                while lazer_feeds.len() <= instance {
                    lazer_feeds.push(Vec::new());
                }
                if !lazer_feeds[instance].contains(&feed) {
                    lazer_feeds[instance].push(feed.clone());
                }
            }
        }
        for feeds in &mut lazer_feeds {
            feeds.sort();
        }
        Ok(Self {
            upload_roles,
            live_roles,
            lazer_feeds,
            deploy_runtime: pool.runtime.is_none(),
            deploy_adapter: pool.adapter.is_none(),
            activate: false,
            // Set by the run that freezes the plan, from its own operator:
            // a shape on its own cannot know who is signing.
            handoff: None,
        })
    }

    /// The count of reserves, which bounds the per-asset coordinates.
    fn assets(&self, cfg: &MarketConfig) -> usize {
        cfg.pool.as_ref().map_or(0, |p| p.reserves.len())
    }
}

/// The frozen graph for one real market, in executable order.
///
/// Order law, and why each boundary is where it is:
/// - uploads first: nothing deploys code the network does not already hold;
/// - the shim legs before Comet: `comet.init` pulls both seeded balances
///   through the asset contract's `transfer`, so the SACs and the balances
///   must exist and the operator must hold them;
/// - the issuer lock with its leg: an issuer that can still mint after the
///   backing is in place would let the pool's collateral ratio be diluted
///   from outside the pool;
/// - sources before the runtime, the runtime before governance's proposals:
///   a `SetProxy` names concrete source contracts, so no proposal can exist
///   before what it names does;
/// - the runtime's per-asset configuration before the adapter, the adapter
///   before the pool: each is constructed with the address of the one
///   before it;
/// - reserves before the deposit: the backstop's fourth-power test reads the
///   pool's reserve list;
/// - the deposit and its threshold readback before the freeze, and the
///   freeze before activation: `set_status(0)` is where the pool re-checks
///   the backstop, and a Comet whose transfers are frozen cannot be
///   re-backed afterwards;
/// - the trustline removals after the deposit: a trustline may only be
///   removed at a zero balance, which is only true once both legs are inside
///   Comet;
/// - publication last, after every readback.
pub(crate) fn real_market_schedule(cfg: &MarketConfig, shape: &RealShape) -> Vec<MSlot> {
    let mut slots: Vec<MSlot> = Vec::new();
    let mut add =
        |phase: u32, slot: u32, action: &'static str, kind: MarketMutationKind, op: MOp| {
            slots.push(MSlot {
                phase,
                slot,
                action,
                kind,
                op,
            })
        };
    // Phase 1: the code this market owns. Roles pinned to a hash the network
    // already serves take no slot: there is nothing to submit for them.
    for (i, role) in shape.upload_roles.iter().enumerate() {
        add(
            1,
            1 + i as u32,
            "upload artifact",
            MarketMutationKind::Upload,
            MOp::Upload { role },
        );
    }
    // Phase 2: the activation shim. Two synthetic classic legs, each created,
    // trusted, deployed as an SAC, minted to the operator, and then locked so
    // nothing can ever mint behind the pool's backing again.
    for leg in 0..2usize {
        let base = 1 + leg as u32 * 5;
        add(
            2,
            base,
            "create shim issuer",
            MarketMutationKind::Invoke,
            MOp::CreateIssuer { leg },
        );
        add(
            2,
            base + 1,
            "trust shim leg",
            MarketMutationKind::Config,
            MOp::TrustLeg { leg },
        );
        add(
            2,
            base + 2,
            "deploy shim asset contract",
            MarketMutationKind::Deploy,
            MOp::DeploySac { leg },
        );
        add(
            2,
            base + 3,
            "mint shim leg backing",
            MarketMutationKind::Invoke,
            MOp::MintLeg { leg },
        );
        add(
            2,
            base + 4,
            "lock shim issuer",
            MarketMutationKind::Config,
            MOp::LockIssuer { leg },
        );
    }
    // Phase 3: this market's own Lazer source instances.
    for (instance, _) in shape.lazer_feeds.iter().enumerate() {
        add(
            3,
            1 + instance as u32,
            "deploy lazer source",
            MarketMutationKind::Deploy,
            MOp::RealDeployLazer { instance },
        );
    }
    // Phase 4: the proxy runtime, governance-owned at birth, or the read-only
    // verification of the runtime this pool shares.
    if shape.deploy_runtime {
        add(
            4,
            1,
            "deploy proxy runtime",
            MarketMutationKind::Deploy,
            MOp::RealDeployRuntime,
        );
    } else {
        add(
            4,
            1,
            "verify shared proxy runtime",
            MarketMutationKind::Config,
            MOp::RealVerifyRuntime,
        );
    }
    // Phase 5: governance, then one SetProxy proposal pair per asset. Each
    // asset carries its own quorum, freshness and sources: different assets
    // are pushed on different cadences, so no price parameter is shared.
    if shape.deploy_runtime {
        add(
            5,
            1,
            "deploy proxy governance",
            MarketMutationKind::Deploy,
            MOp::RealDeployGovernance,
        );
    }
    let assets = shape.assets(cfg);
    for asset in 0..assets {
        add(
            5,
            2 + asset as u32,
            "propose asset price configuration",
            MarketMutationKind::Governance,
            MOp::RealProposeAsset { asset },
        );
    }
    // Proposal ids follow creation order (the governance kernel assigns ids
    // from zero and rejects any other), so asset position, scheduled slot and
    // proposal id are one law, exactly as in the lab graph.
    for asset in 0..assets {
        add(
            5,
            20 + asset as u32,
            "execute asset price configuration",
            MarketMutationKind::Governance,
            MOp::RealExecuteAsset { asset },
        );
    }
    // Phase 6: one refresh per asset. A quorum that has never served is not
    // a quorum; the coordinate settles on the freshness proof and reports a
    // stale asset as a violation rather than inventing a price.
    for asset in 0..assets {
        add(
            6,
            1 + asset as u32,
            "refresh asset",
            MarketMutationKind::Config,
            MOp::RealRefreshAsset { asset },
        );
    }
    // Phase 7: the pool's price adapter, or the read-only verification of the
    // adapter the pool config already names.
    if shape.deploy_adapter {
        add(
            7,
            1,
            "deploy price adapter",
            MarketMutationKind::Deploy,
            MOp::RealDeployAdapter,
        );
    } else {
        add(
            7,
            1,
            "verify configured adapter",
            MarketMutationKind::Config,
            MOp::RealVerifyAdapter,
        );
    }
    // Phase 8: Comet and the emitter (both the network's own code), then the
    // backstop against the predicted factory, then the factory itself.
    add(
        8,
        1,
        "deploy comet",
        MarketMutationKind::Deploy,
        MOp::RealDeployComet,
    );
    add(
        8,
        2,
        "initialize comet",
        MarketMutationKind::Config,
        MOp::RealInitComet,
    );
    add(
        8,
        3,
        "deploy emitter",
        MarketMutationKind::Deploy,
        MOp::RealDeployEmitter,
    );
    add(
        8,
        4,
        "deploy backstop",
        MarketMutationKind::Deploy,
        MOp::RealDeployBackstop,
    );
    add(
        8,
        5,
        "deploy factory",
        MarketMutationKind::Deploy,
        MOp::RealDeployFactory,
    );
    // Phase 9: the pool through the factory, then its reserves.
    add(
        9,
        1,
        "deploy pool",
        MarketMutationKind::Deploy,
        MOp::RealDeployPool,
    );
    for asset in 0..assets {
        add(
            9,
            2 + asset as u32,
            "queue reserve",
            MarketMutationKind::Config,
            MOp::RealQueueReserve { asset },
        );
    }
    for asset in 0..assets {
        add(
            9,
            20 + asset as u32,
            "enable reserve",
            MarketMutationKind::Config,
            MOp::RealSetReserve { asset },
        );
    }
    // Phase 10: the backing, the trustline removals it unblocks, the freeze,
    // and the activation.
    add(
        10,
        1,
        "deposit backstop backing",
        MarketMutationKind::Invoke,
        MOp::RealBackstopDeposit,
    );
    for leg in 0..2usize {
        add(
            10,
            2 + leg as u32,
            "remove shim trustline",
            MarketMutationKind::Config,
            MOp::DropTrust { leg },
        );
    }
    add(
        10,
        4,
        "freeze comet transfers",
        MarketMutationKind::Config,
        MOp::RealFreezeComet,
    );
    if shape.activate {
        add(
            10,
            5,
            "activate pool",
            MarketMutationKind::Config,
            MOp::RealSetStatus { to: 0 },
        );
    }
    // Phase 11: the handoff, and only for a run that owes one. Every
    // coordinate here checks the chain before it speaks and settles on the
    // readback, so a run resumed after a half-finished handoff, or a role
    // the operator already holds, sends nothing. The order is the order the
    // proposal review approved: the pool first (its admin is the last thing
    // the operator can lose without breaking what follows), then the sources
    // this run owns, then governance — where the operator takes its own
    // `Admin` out last, so the deployer key keeps no power — and the
    // backing pool's controller, which is one step and so goes last.
    if shape.handoff.is_some() {
        let mut next = 1u32;
        add(
            11,
            next,
            "hand off the pool",
            MarketMutationKind::Config,
            MOp::RealHandoffPool,
        );
        next += 1;
        for instance in 0..shape.lazer_feeds.len() {
            add(
                11,
                next,
                "hand off a price source",
                MarketMutationKind::Config,
                MOp::RealHandoffLazer { instance },
            );
            next += 1;
        }
        // A shared runtime's governance belongs to the market that first
        // deployed it; only a runtime of this run's own has roles to move.
        if shape.deploy_runtime {
            for grant in [true, false] {
                add(
                    11,
                    next,
                    if grant {
                        "propose the oracle administrator"
                    } else {
                        "propose the operator's release"
                    },
                    MarketMutationKind::Governance,
                    MOp::RealProposeRole { grant },
                );
                next += 1;
                add(
                    11,
                    next,
                    if grant {
                        "grant the oracle administrator"
                    } else {
                        "release the operator's role"
                    },
                    MarketMutationKind::Governance,
                    MOp::RealExecuteRole { grant },
                );
                next += 1;
            }
        }
        add(
            11,
            next,
            "hand off the backing pool",
            MarketMutationKind::Config,
            MOp::RealHandoffComet,
        );
    }
    // Phase 12: publication, after every battery has passed. The coordinate
    // is the same as the lab graph's: the resume law that reads it needs no
    // graph to find it.
    add(
        12,
        1,
        "publish market manifest",
        MarketMutationKind::Config,
        MOp::Publish,
    );
    slots
}

/// The frozen deployment plan (plan §5): written atomically, create-new,
/// before the first journaled mutation, and the fact a resume reproduces.
/// A run whose plan cannot be reproduced is refused, not guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentPlan {
    pub schema: u32,
    pub config_digest: String,
    pub nonce: String,
    pub network_label: String,
    pub passphrase: String,
    pub admin: String,
    pub artifacts: BTreeMap<String, Sha256Hex>,
    /// Release-form roles' frozen provenance. Absent (empty) for path-form
    /// pins and for every run predating release resolution.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub releases: BTreeMap<String, crate::release::Provenance>,
    pub salt_roles: Vec<u32>,
    pub slots: Vec<(u32, u32)>,
    /// The roles this run uploads, decided by the run that froze the plan
    /// from what the network held then. A resume materializes exactly these
    /// from --wasm-dir, whatever the network holds now. Empty for lab runs
    /// and for plans predating the real market.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uploads: Vec<String>,
}

/// Read the frozen plan. `Ok(None)` when this run has none yet.
pub(crate) fn read_deployment_plan(out: &Path) -> anyhow::Result<Option<DeploymentPlan>> {
    let path = out.join(MARKET_PLAN_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).context("deployment plan is not UTF-8")?;
            let plan: DeploymentPlan =
                toml::from_str(&text).context("deployment plan is malformed")?;
            anyhow::ensure!(plan.schema == 1, "deployment plan has an unknown schema");
            // A plan names its network, and the networks it may name are the
            // two this tool is lawful on. Whether a plan is *this* run's plan
            // — the same network its configuration names — is a separate check,
            // in `verify_frozen_plan`, which is the one that has a config.
            anyhow::ensure!(
                MarketNetwork::from_pair(&plan.passphrase, &plan.network_label).is_ok(),
                "deployment plan is not bound to a known market network"
            );
            Ok(Some(plan))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("deployment plan is unreadable")),
    }
}

/// The network a frozen plan names. A plan that names neither of the two
/// lawful market networks has no network to bind a read to.
pub(crate) fn plan_network(plan: &DeploymentPlan) -> anyhow::Result<MarketNetwork> {
    MarketNetwork::from_pair(&plan.passphrase, &plan.network_label)
        .map_err(|_| anyhow!("the frozen deployment plan names an unknown market network"))
}

/// Write the frozen plan exactly once, atomically: a temporary file, its
/// bytes and file synchronization, a no-replace rename, then the directory's
/// synchronization. A plan that already exists is never rewritten, so a
/// halted run cannot be quietly re-planned.
pub(crate) fn write_deployment_plan(out: &Path, plan: &DeploymentPlan) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)
        .context("creating the run directory that holds its deployment plan")?;
    let final_path = out.join(MARKET_PLAN_FILE);
    anyhow::ensure!(
        !final_path.exists(),
        "a frozen deployment plan already exists; it can only be abandoned, never rewritten"
    );
    let text =
        toml::to_string_pretty(plan).map_err(|e| anyhow!("serializing deployment plan: {e}"))?;
    let tmp = out.join(format!("{MARKET_PLAN_FILE}.{}.tmp", std::process::id()));
    use std::io::Write as _;
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .context("creating the deployment plan staging file")?;
        file.write_all(text.as_bytes())
            .context("writing the deployment plan")?;
        file.sync_all().context("syncing the deployment plan")?;
    }
    std::fs::rename(&tmp, &final_path).context("freezing the deployment plan")?;
    sync_dir(out);
    anyhow::ensure!(
        read_deployment_plan(out)?.is_some(),
        "the frozen deployment plan is unreadable after publication"
    );
    Ok(())
}

/// Synchronize a directory so a rename or newly created entry is durable.
fn sync_dir(path: &Path) {
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.sync_all();
    }
}

impl MarketDeployer {
    pub fn new(
        cfg: MarketConfig,
        ctx: MarketExecutionContext,
        journal: MarketJournal,
    ) -> anyhow::Result<Self> {
        Self::build(cfg, ctx, journal, MarketDeployOptions::default())
    }

    /// The one-command market deployment: a pool config, an operator, an
    /// optional activation. Everything a real market needs is in the
    /// configuration it was reviewed with; nothing here is guessed.
    pub fn with_options(
        cfg: MarketConfig,
        ctx: MarketExecutionContext,
        journal: MarketJournal,
        options: MarketDeployOptions,
    ) -> anyhow::Result<Self> {
        Self::build(cfg, ctx, journal, options)
    }

    fn build(
        mut cfg: MarketConfig,
        ctx: MarketExecutionContext,
        journal: MarketJournal,
        options: MarketDeployOptions,
    ) -> anyhow::Result<Self> {
        // The network is config, frozen at parse: the context is bound to the
        // network the configuration names, and a mismatch is refused before
        // any executor call rather than corrected by a guess.
        let network = cfg.network()?;
        ctx.assert_network(network)?;
        // Deploy-time law: no reserve may inherit a fixture cap, factor or
        // rate curve. Checked before any executor call.
        cfg.require_explicit_reserves()?;
        let exec = RealExecutor {
            bin: ctx.stellar_bin().to_path_buf(),
            xdg: ctx.xdg().to_path_buf(),
            cwd: ctx.cwd().to_path_buf(),
            capture: Capture::Market {
                stdout_cap: MARKET_CAPTURE_STDOUT_CAP,
                stderr_cap: MARKET_CAPTURE_STDERR_CAP,
                wall: Some(MARKET_MUTATION_WALL),
            },
            // The run's RPC endpoint (and any provider key in it) reaches
            // every market child through the uid-private child environment,
            // never through world-readable argv.
            rpc_env: crate::stellar::rpc_env_for(&ctx.actor_net()),
        };
        // A real market's digest pins become uploads from --wasm-dir where
        // the network does not hold the code yet. The decision is made once,
        // on the run that freezes the plan, and the plan records it: a resume
        // must reproduce the plan, so it cannot re-ask a chain that now holds
        // what this very run uploaded and plan a different set of uploads.
        let releases = cfg.releases();
        if cfg.is_real() {
            let frozen = read_deployment_plan(&journal.out())?;
            let net = ctx.admin_net();
            let st = Stellar::new(&net, &exec, Policy::Market);
            cfg.materialize_missing_code(options.wasm_dir.as_deref(), |role, hash| match &frozen {
                Some(plan) => !plan.uploads.iter().any(|r| r == role),
                None => hash_of(hash.as_str()).is_ok_and(|h| st.wasm_hash_live(&h)),
            })?;
        }
        let (shape, schedule) = if cfg.is_real() {
            let mut shape = RealShape::from_config(&cfg)?;
            shape.activate = options.activate;
            // The handoff is decided here, with the rest of the graph, because
            // the graph is what the plan freezes. Its only input beyond the
            // configuration is the operator's own public key: the same keystore
            // read `preflight` performs, and the same address the plan already
            // binds as `admin` and refuses a resume over. It must resolve
            // here: a guess of "no operator" would freeze a graph with no
            // handoff and leave every role on the deploying key.
            let operator = Stellar::new(&ctx.admin_net(), &exec, Policy::Market)
                .keys_public_key(ctx.admin_alias())?
                .ok_or_else(|| anyhow!("the administrator identity did not resolve to a G-address"))?;
            shape.handoff = RealHandoff::owed(&cfg, Some(operator.as_str()));
            let schedule = real_market_schedule(&cfg, &shape);
            (Some(shape), schedule)
        } else {
            anyhow::ensure!(
                !options.activate,
                "the lab stack has no pool status to activate; \
                 --activate belongs to a real market"
            );
            (None, market_schedule())
        };
        // The run's two network bindings, resolved once from the context the
        // caller was given: the operator's own key directory for anything the
        // operator signs, and the run's own for the keys this run creates.
        let admin_net = ctx.admin_net();
        let actor_net = ctx.actor_net();
        let mut d = Self {
            cfg,
            ctx,
            journal,
            exec,
            halted: false,
            unresolved: None,
            gate: None,
            progress: Progress::default(),
            manifest: None,
            terminal: None,
            stopped_violations: Vec::new(),
            releases,
            plan_written: false,
            nonce: 0,
            artifacts: BTreeMap::new(),
            deployed: BTreeMap::new(),
            schedule,
            shape,
            admin_net,
            actor_net,
        };
        // Settled journal results rebuild deployment and identity state
        // first, so reconciliation and every later lookup bind from the
        // same facts the journal already attests.
        d.rebuild_from_journal()?;
        // Preflight first (read-only): reconciliation proofs bind the
        // resolved operator, so scanning before the admin is known would
        // silently fail every deterministic-deploy proof.
        match d.preflight() {
            Ok(p) => {
                d.progress.admin = Some(p.admin);
                d.artifacts = p.artifacts;
                d.nonce = p.nonce;
                // A run that already has a frozen plan is a resume, not a new
                // deployment. The plan is never rewritten; it is verified to
                // reproduce from this configuration and, if it does, the run
                // continues under the custody law. A plan that does not
                // reproduce means the configuration changed under the run, and
                // the run is refused rather than quietly re-planned.
                match read_deployment_plan(&d.journal.out()) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        if let Err(e) = write_deployment_plan(&d.journal.out(), &d.frozen_plan()) {
                            d.terminal =
                                Some(DeployEvent::Failed(Safe::error_chain(Policy::Market, &e)));
                            return Ok(d);
                        }
                    }
                    Err(e) => {
                        d.terminal =
                            Some(DeployEvent::Failed(Safe::error_chain(Policy::Market, &e)));
                        return Ok(d);
                    }
                }
                d.plan_written = true;
                if let Some(e) = d.verify_frozen_plan() {
                    d.terminal = Some(DeployEvent::Failed(e));
                }
            }
            Err(e) => {
                d.terminal = Some(DeployEvent::Failed(Safe::error_chain(Policy::Market, &e)));
            }
        }
        // Reconciliation is the one thing a refused resume must never do: it
        // can settle a coordinate and write the run's manifest. So the scan
        // runs only for a run its own validation accepted. A run refused by
        // preflight, by the plan write, or by the frozen-plan check leaves
        // every existing record exactly as it found it — including a resume
        // attempted under a different operator or configuration, whose records
        // belong to the run the plan froze. `step` already refuses to honor a
        // command once a terminal event is bound, so this is the same law the
        // rest of the machine already applies.
        if d.terminal.is_none() {
            d.scan_journal()?;
            if d.unresolved.is_some() {
                d.terminal = Some(DeployEvent::Custody {
                    unresolved: true,
                    note: d.unresolved.clone().unwrap(),
                });
            }
        }
        Ok(d)
    }

    /// The graph this run executes. Bound at construction from the
    /// configuration and frozen with the plan; every coordinate decision —
    /// scan, reconcile, gate, next slot, publication — reads this and nothing
    /// else, so a run can never execute a graph it did not freeze.
    fn schedule(&self) -> &[MSlot] {
        &self.schedule
    }

    /// The number of coordinates this run's frozen graph will attempt. Not a
    /// count of transactions: a readback-only coordinate is one of them, and
    /// a resumed run settles the ones its journal already proves. The operator
    /// is asked to authorize the graph, so this is the figure the run prints
    /// before its first confirmation.
    #[must_use]
    pub fn planned_coordinates(&self) -> usize {
        self.schedule.len()
    }

    /// The run directory that holds this run's frozen plan and journal.
    #[must_use]
    pub fn run_dir(&self) -> PathBuf {
        self.journal.out()
    }

    /// The frozen graph as plan rows, in graph order, each flagged with
    /// whether its journal coordinate already settled. This is the listing
    /// a robot approves: the coordinate, the role, and the action of every
    /// step the run will attempt or skip.
    pub fn plan_steps(&self) -> anyhow::Result<Vec<PlanStep>> {
        self.schedule()
            .iter()
            .map(|slot| {
                Ok(PlanStep {
                    phase: slot.phase,
                    slot: slot.slot,
                    action: slot.action,
                    kind: slot.kind,
                    target_role: self.slot_role(slot),
                    settled: self.journal.result(slot.phase, slot.slot)?.is_some(),
                })
            })
            .collect()
    }

    /// The violations that stopped this run, when it ended on a definite
    /// refusal rather than on an ambiguous submission. Empty otherwise.
    #[must_use]
    pub fn stopped_violations(&self) -> &[Safe] {
        &self.stopped_violations
    }

    /// The frozen graph as an owned iterator. Several walkers settle or
    /// reconcile coordinates from the same loop, which takes `&mut self`:
    /// the copy outlives that borrow, and the graph is written once at
    /// construction, so an owned copy is the graph.
    fn schedule_owned(&self) -> Vec<MSlot> {
        #[allow(clippy::useless_vec)] // the lint's fix would re-borrow `self`.
        let copied = self.schedule().to_vec();
        copied
    }

    /// Whether this run deploys a real market (its own pool, reserves and
    /// price sources) rather than the lab's mock stack.
    fn real(&self) -> bool {
        self.shape.is_some()
    }

    /// One reserve of the real market's pool, by its position in the config.
    fn reserve(&self, asset: usize) -> anyhow::Result<&crate::manifest::MarketReserve> {
        self.cfg
            .pool
            .as_ref()
            .and_then(|p| p.reserves.get(asset))
            .ok_or_else(|| anyhow!("the scheduled reserve is not part of this configuration"))
    }

    /// The pool spec this run deploys, when it has one.
    fn pool(&self) -> anyhow::Result<&crate::manifest::PoolSpec> {
        self.cfg
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("only a real market has a pool specification"))
    }

    /// The contract address of one synthetic leg, as this run's settled
    /// deployment recorded it.
    fn sac_id(&self, leg: usize) -> anyhow::Result<ContractId> {
        self.deployed_id(&format!("sac:{leg}"))
    }

    /// The run-local issuer account of one synthetic leg. Its secret is
    /// deleted after the lock, so the address is recovered from the journal
    /// fact the creating coordinate recorded, never from the keystore.
    fn issuer_of(&self, leg: usize) -> anyhow::Result<AccountId> {
        self.progress
            .issuers
            .get(leg)
            .cloned()
            .ok_or_else(|| anyhow!("a prior graph coordinate did not settle this leg's issuer"))
    }

    /// The facts one settled coordinate recorded in its own command text. A
    /// real market's graph needs a few facts no contract readback can give
    /// back — the issuer account it created, the amount it minted, the LP it
    /// deposited — and the only place they can live and still survive a
    /// resume is the journal record of the coordinate that produced them.
    /// Rendering and parsing are the same JSON, so a fact a run wrote is the
    /// fact a resume reads; a coordinate that recorded none is `None`, never
    /// a guess.
    fn settled_facts(&self, phase: u32, slot: u32) -> anyhow::Result<Option<Value>> {
        let Some(result) = self.journal.result(phase, slot)? else {
            return Ok(None);
        };
        let rendered = result.step.command.to_string();
        let trimmed = rendered.trim();
        if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
            return Ok(None);
        }
        Ok(serde_json::from_str::<Value>(trimmed).ok())
    }


    /// Read-only preflight (plan §4 phase 1). Proves the tool version, every
    /// pinned artifact's re-derived SHA-256, and the operator's resolved
    /// G-address, using no mutation, no simulation, and no ledger read. A run
    /// that cannot prove all three binds no plan and authorizes no mutation.
    ///
    /// The digest check re-reads each artifact and hashes its bytes rather
    /// than trusting the pin or the path: a swapped file or a build that
    /// drifted from what was reviewed fails here, while a malformed or
    /// placeholder-shaped pin never parsed as a `MarketConfig` at all.
    fn preflight(&self) -> anyhow::Result<Preflight> {
        self.cfg.validate()?;
        self.cfg.check_artifacts()?;
        let network = self.cfg.network()?;
        let net = self.ctx.actor_net();
        let st = Stellar::new(&net, &self.exec, Policy::Market);
        // The one network gate of a run: the RPC this context points at must
        // report the passphrase the configuration froze. Testnet and mainnet
        // are both lawful; anything else is refused here, before a plan can
        // bind and before any mutation is authorized.
        st.assert_rpc_network(network.passphrase())?;
        st.assert_version()?;
        // A lab run proves its fixed ten artifacts by their bytes. A real
        // market proves the code its configuration pins: bytes it will submit
        // are re-hashed against their digest, and a hash the network already
        // serves (Comet and the emitter are the network's own production
        // contracts) is proven by asking that network for the Wasm back.
        // Mainnet uploads cost about 1.5 XLM per KB, so a run never hashes,
        // uploads, or pins code it does not deploy.
        let mut artifacts: BTreeMap<String, Sha256Hex> = BTreeMap::new();
        let roles: Vec<String> = if self.cfg.is_real() {
            self.cfg
                .artifacts
                .keys()
                .map(|role| role.to_string())
                .collect()
        } else {
            MARKET_ARTIFACT_ROLES
                .iter()
                .map(|role| (*role).to_string())
                .collect()
        };
        for role in roles {
            let pin = self
                .cfg
                .artifacts
                .get(role.as_str())
                .ok_or_else(|| anyhow!("a required artifact role is unpinned"))?;
            if matches!(pin, crate::manifest::ArtifactPin::Live(_)) {
                let hash = pin.wasm_hash()?;
                anyhow::ensure!(
                    st.wasm_hash_live(&hash_of(hash.as_str())?),
                    "an artifact this run is pinned to a live Wasm hash for is not fetchable on this network"
                );
                artifacts.insert(role.to_string(), hash);
                continue;
            }
            // Every pin that is not live locates its bytes: a file pin by
            // path, a release pin at the path the resolver wrote; a digest
            // pin the network lacks became a file pin from --wasm-dir before
            // this plan was built.
            let (pin_path, pin_sha256) = pin.file_location()?;
            let path = expand_home(&pin_path);
            anyhow::ensure!(
                path.is_absolute() && path.is_file(),
                "a pinned artifact is missing from disk"
            );
            let bytes = std::fs::read(&path).context("reading a pinned artifact")?;
            let digest = Sha256Hex::try_from(sha256_hex(&bytes))?;
            anyhow::ensure!(
                digest == pin_sha256,
                "a pinned artifact's bytes do not match its digest"
            );
            artifacts.insert(role.to_string(), digest);
        }
        // A real market references contracts it does not deploy: the Lazer
        // verifier every Lazer source binds, and every sep-40 oracle its
        // reserves quote through. Each must exist on the network this run
        // is bound to before a single XLM is spent — a run that learns at
        // the configure coordinate that the oracle it was pointed at is
        // not here has paid for every upload and deploy before it. The
        // refusal names the reserve, because the operator fixes a config,
        // not a hash.
        if self.cfg.is_real() {
            let pool = self.pool()?;
            let mut referenced: Vec<(String, String)> = Vec::new();
            if let Some(verifier) = pool.lazer_verifier.as_deref() {
                referenced.push((
                    "the market's Lazer verifier".to_string(),
                    verifier.to_string(),
                ));
            }
            for reserve in &pool.reserves {
                for source in &reserve.sources {
                    if source.kind == crate::manifest::SourceKind::Sep40 {
                        if let Some(contract) = source.contract.as_deref() {
                            referenced.push((
                                format!("reserve {}'s sep-40 oracle", reserve.label),
                                contract.to_string(),
                            ));
                        }
                    }
                }
            }
            for (what, text) in referenced {
                let id = ContractId::from_str(&text)
                    .map_err(|_| anyhow!("{what} is not a well-formed contract id"))?;
                anyhow::ensure!(
                    st.contract_exists(&id),
                    "{what} ({text}) does not exist on this network; the run is refused before any send"
                );
            }
        }
        let admin = Stellar::new(&self.ctx.admin_net(), &self.exec, Policy::Market)
            .keys_public_key(self.ctx.admin_alias())?
            .ok_or_else(|| anyhow!("the administrator identity did not resolve to a G-address"))?;
        // The pool's final admins and a shared runtime's governance are proved
        // here, before any send. Both were once discovered at their own phase —
        // the admin at the pool's coordinate, the governance after the
        // uploads, the locked issuers, the Comet initialization and the
        // backstop deposit — where a mismatch is an unrecoverable halt with
        // every XLM spent. An operator fixes a configuration, so the
        // configuration is asked first.
        //
        // The pool's admin is no longer asked to *be* the operator: a curator
        // deploys a pool it is about to hand over. What is asked instead is
        // that whoever it is handed to is on this network — a role proposed to
        // an address that does not exist is discovered at phase 11, after
        // every upload, deploy and deposit has been paid for, and a run cannot
        // be resumed past it.
        if self.cfg.is_real() {
            let pool = self.pool()?;
            if let Some(handoff) = RealHandoff::owed(&self.cfg, Some(admin.as_str())) {
                for target in [handoff.pool_admin.as_str(), handoff.oracle_admin.as_str()] {
                    if target == admin.as_str() {
                        continue;
                    }
                    // An account is proved by its ledger entry and a contract
                    // by its code: the two ways an address can be here, each
                    // asked the way the rest of this tool asks it.
                    let here = if let Ok(account) = AccountId::from_str(target) {
                        self.reader().ledger_account(account.as_str())?.is_some()
                    } else if let Ok(contract) = ContractId::from_str(target) {
                        st.contract_exists(&contract)
                    } else {
                        false
                    };
                    anyhow::ensure!(
                        here,
                        "the handoff target {target} does not exist on this network; \
                         the run is refused before any send"
                    );
                }
            }
            if let Some(configured) = pool.runtime.as_deref() {
                let runtime = ContractId::from_str(configured)
                    .map_err(|_| anyhow!("the configured runtime address is malformed"))?;
                // The ownership read is also the existence proof: a contract
                // that is not here, or answers nothing, refuses the run the
                // same way, before a single XLM is spent.
                let reader = self.operator_reader();
                let owner = reader
                    .try_view(runtime.as_str(), self.ctx.admin_alias(), "get_owner", &[])
                    .ok()
                    .filter(|invocation| invocation.ok)
                    .and_then(|invocation| invocation.value)
                    .and_then(|value| value.as_str().map(str::to_string))
                    .ok_or_else(|| {
                        anyhow!(
                            "the shared proxy runtime ({configured}) answers no owner; \
                             the run is refused before any send"
                        )
                    })?;
                let governance = owner.as_str();
                let admin_role = reader
                    .try_view(
                        governance,
                        self.ctx.admin_alias(),
                        "has_role",
                        &[kv("account", admin.as_str()), kv("role", "Admin")],
                    )
                    .ok()
                    .filter(|invocation| invocation.ok)
                    .and_then(|invocation| invocation.value)
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                // A run whose every reserve the shared runtime already serves
                // exactly proposes nothing (each propose and execute
                // coordinate is read-only), so it needs no Admin role: the
                // role may already have moved to the multisig.
                anyhow::ensure!(
                    admin_role || self.shared_runtime_serves_every_reserve(runtime.as_str())?,
                    "the shared proxy runtime's governance ({governance}) does not grant the operator \
                     the Admin role; the run is refused before any send"
                );
            }
        }
        if !self.cfg.is_real() {
            anyhow::ensure!(
                self.cfg.balances.len() == MARKET_BALANCE_COUNT,
                "the configured playable balances do not match the frozen mint schedule"
            );
        }
        let nonce = market_run_nonce(&self.cfg, &self.journal.out())?;
        Ok(Preflight {
            admin,
            artifacts,
            nonce,
        })
    }

    /// The plan this run must freeze: the configuration digest, the nonce,
    /// the exact testnet, the resolved operator, the proven artifact
    /// digests, and every journal coordinate and salt role the graph
    /// consumes.
    fn frozen_plan(&self) -> DeploymentPlan {
        DeploymentPlan {
            schema: 1,
            config_digest: market_config_digest(&self.cfg).unwrap_or_else(|_| "0".repeat(64)),
            nonce: format!("{:016x}", self.nonce),
            network_label: self.cfg.network().ok().map_or_else(
                || MARKET_NETWORK_LABEL.to_string(),
                |network| network.label().to_string(),
            ),
            passphrase: self.cfg.network().ok().map_or_else(
                || MARKET_TESTNET_PASSPHRASE.to_string(),
                |network| network.passphrase().to_string(),
            ),
            admin: self
                .progress
                .admin
                .as_ref()
                .map(|a| a.as_str().to_string())
                .unwrap_or_default(),
            artifacts: self.artifacts.clone(),
            releases: self.releases.clone(),
            salt_roles: market_salt_triples(self.schedule(), self.nonce)
                .into_iter()
                .map(|(_, role, _)| role)
                .collect(),
            slots: self
                .schedule()
                .iter()
                .map(|s| (s.phase, s.slot))
                .collect(),
            uploads: self
                .schedule()
                .iter()
                .filter_map(|s| match &s.op {
                    MOp::Upload { role } => Some((*role).to_string()),
                    _ => None,
                })
                .collect(),
        }
    }

    /// Prove the frozen plan is reproducible and internally lawful. A
    /// coordinate outside the scanned range would be invisible to the
    /// no-replay scan, and a reused salt would deploy two contracts at one
    /// address, so the run is refused rather than silently endangered.
    /// Returns the rejection, if any.
    fn verify_frozen_plan(&self) -> Option<Safe> {
        let plan = match read_deployment_plan(&self.journal.out()) {
            Ok(Some(plan)) => plan,
            Ok(None) => self.frozen_plan(),
            Err(_) => {
                return Some(Safe::literal(
                    "the frozen deployment plan on disk is unreadable; the run is refused, not re-planned",
                ))
            }
        };
        if plan != self.frozen_plan() {
            return Some(Safe::literal(
                "the frozen deployment plan does not reproduce from this configuration; the run is refused, not re-planned",
            ));
        }
        let schedule = self.schedule();
        if schedule
            .iter()
            .any(|s| !market_coordinate_is_scanned(s.phase, s.slot))
        {
            return Some(Safe::literal(
                "the deployment graph schedules a journal coordinate outside the range the no-replay scan inspects",
            ));
        }
        let coordinates: std::collections::BTreeSet<(u32, u32)> =
            schedule.iter().map(|s| (s.phase, s.slot)).collect();
        if coordinates.len() != schedule.len() {
            return Some(Safe::literal(
                "the deployment graph reuses a journal coordinate; the run is refused before any mutation",
            ));
        }
        let triples = market_salt_triples(self.schedule(), self.nonce);
        let roles: std::collections::BTreeSet<u32> =
            triples.iter().map(|(_, role, _)| *role).collect();
        if roles.len() != triples.len() {
            return Some(Safe::literal(
                "two deterministic deployments would share a create2 salt; the run is refused before any mutation",
            ));
        }
        // The graph must publish where the resume law looks for it, or an
        // interrupted publication would be invisible to every later run.
        if !schedule.iter().any(|s| {
            s.op == MOp::Publish && s.phase == MARKET_PUBLISH_PHASE && s.slot == MARKET_PUBLISH_SLOT
        }) {
            return Some(Safe::literal(
                "the deployment graph does not publish at the coordinate the resume law reads",
            ));
        }
        // The plan's network is the configuration's. A plan frozen for one
        // network cannot be honored by a run whose configuration names the
        // other, even when every other byte of it reproduces.
        match (
            MarketNetwork::from_pair(&plan.passphrase, &plan.network_label),
            self.cfg.network(),
        ) {
            (Ok(planned), Ok(configured)) if planned == configured => {}
            _ => {
                return Some(Safe::literal(
                    "the frozen deployment plan names a network this configuration does not",
                ))
            }
        }
        None
    }

    /// Scan every numbered phase/slot coordinate for custody state before
    /// any command is honored: an intent without a settled result is
    /// ambiguous and must be reconciled under the resume law (plan §4)
    /// before the run may proceed. No replay happens before this completes.
    fn scan_journal(&mut self) -> anyhow::Result<()> {
        for phase in 1..=12u32 {
            let mut settled = 0usize;
            let mut touched = 0usize;
            for slot in 1..=40u32 {
                let intent = self.journal.read_intent(phase, slot)?;
                let Some(record) = intent else { continue };
                touched += 1;
                if self.journal.result(phase, slot)?.is_some() {
                    settled += 1;
                    continue;
                }
                match self.reconcile_pending(phase, slot, &record)? {
                    Reconciled::Settled => settled += 1,
                    Reconciled::Replay => {}
                    Reconciled::Unresolved(note) => {
                        self.unresolved = Some(note);
                        return Ok(());
                    }
                }
            }
            if touched > 0 && settled == touched {
                self.progress.phase_done.insert(phase);
            }
        }
        Ok(())
    }

    /// Resume law (plan §4), per mutation kind. Ambiguity is never retried
    /// blindly; only the kind's declared public proof may settle it.
    fn reconcile_pending(
        &mut self,
        phase: u32,
        slot: u32,
        record: &MarketCustodyRecord,
    ) -> anyhow::Result<Reconciled> {
        let unresolved_note = || {
            DeployEvent::Custody {
                unresolved: true,
                note: Safe::literal(
                    "unresolved market custody: the declared public proof did not uniquely attribute the submission; the run is abandoned until reconciled",
                ),
            }
        };
        let _ = unresolved_note;
        // Upload: observed, never settled on inspection. See `observe_upload`
        // for why no local evidence can answer whether the chain holds the
        // code, and why a wrong answer here is unrecoverable.
        if record.kind == MarketMutationKind::Upload {
            return Ok(self.observe_upload(phase, slot, record));
        }
        // The adjudicated no-op coordinate settles on resume the same way it
        // settles when executed: nothing was or will be submitted for it.
        if self
            .schedule_owned()
            .into_iter()
            .any(|s| s.phase == phase && s.slot == slot && s.op == MOp::SeedPositions)
        {
            let step = MarketStepRecord {
                phase,
                action: record.action.clone(),
                command: Safe::render_parsed(
                    Policy::Market,
                    &json!({"reconciled": true, "phase": phase, "slot": slot}),
                    200,
                ),
                outcome: Safe::literal(
                    "position seeding deliberately not performed; adjudicated no-op, nothing was submitted",
                ),
                target: record.target.clone(),
                violations: Vec::new(),
            };
            self.journal.finish(
                phase,
                slot,
                &MarketPendingResult {
                    custody: record.clone(),
                    step,
                },
            )?;
            return Ok(Reconciled::Settled);
        }
        // The publication coordinate submits nothing to the chain, so its
        // open intent is never an ambiguous submission: it is completed under
        // its own local law rather than asked of a contract read that could
        // not answer whether a local file was written.
        if let Some(coordinate) = self
            .schedule_owned()
            .into_iter()
            .find(|s| s.phase == phase && s.slot == slot && s.op == MOp::Publish)
        {
            return match self.reconcile_publish(&coordinate, record) {
                Ok(()) => Ok(Reconciled::Settled),
                // A refusal here is the publication's own strict law (a
                // manifest that does not reproduce this run's state), so the
                // coordinate stays unresolved and nothing is replayed.
                Err(e) => Ok(Reconciled::Unresolved(Safe::error_chain(
                    Policy::Market,
                    &e,
                ))),
            };
        }
        if self.real() {
            let coordinate = self
                .schedule_owned()
                .into_iter()
                .find(|s| s.phase == phase && s.slot == slot);
            // A read-only check submits nothing, so its open intent applied
            // nothing: it is archived and run again, which re-reads the chain
            // and records the facts the coordinates after it bind to.
            if matches!(
                coordinate.as_ref().map(|s| &s.op),
                Some(MOp::RealVerifyRuntime | MOp::RealVerifyAdapter)
            ) {
                crate::market_history::archive_never_applied(
                    &self.journal.out(),
                    phase,
                    slot,
                    &Safe::literal("a read-only verification; nothing was submitted"),
                )?;
                return Ok(Reconciled::Replay);
            }
            let proof = match &coordinate {
                Some(slot) => self.real_effect_present(&slot.op, record)?,
                None => false,
            };
            return Ok(if proof {
                self.finish_reconciled(phase, slot, record, coordinate.as_ref().map(|s| &s.op))?;
                // The same registration a restart rebuilds from the settled
                // record, so the next coordinate in this run can bind to it.
                if let (Some(role), Some(predicted)) = (
                    coordinate.as_ref().and_then(|s| Self::deployed_role(&s.op)),
                    record.predicted.clone(),
                ) {
                    self.deployed.insert(role, predicted);
                }
                Reconciled::Settled
            } else {
                Reconciled::Unresolved(Safe::literal(
                    "unresolved market custody: this coordinate's effect is not yet attributable on the network; the run is abandoned until reconciled",
                ))
            });
        }
        let proof = match record.kind {
            // Upload is handled above and can never settle here.
            MarketMutationKind::Upload => Ok(false),
            // Funded key: the coordinate names the actor; the run-local
            // keystore proves the generate-and-fund call landed.
            MarketMutationKind::FundKey => self.reconcile_funded_key(phase, slot),
            // Deterministic deploy: predicted id, declared code, readback.
            MarketMutationKind::Deploy => self.reconcile_deterministic_deploy(record, None),
            // Idempotent config: exact readback of configured state.
            MarketMutationKind::Config => self.reconcile_config(record),
            // Non-idempotent value movements and governance composites can
            // only be attributed from recorded pre-state plus attributable
            // evidence and post-state deltas; that attribution requires the
            // settled record, which is exactly what is missing.
            MarketMutationKind::Invoke | MarketMutationKind::Governance => Ok(false),
        }?;
        if proof {
            let step = MarketStepRecord {
                phase,
                action: record.action.clone(),
                command: Safe::render_parsed(
                    Policy::Market,
                    &json!({"reconciled": true, "phase": phase, "slot": slot}),
                    200,
                ),
                outcome: Safe::literal("reconciled by declared public proof; not replayed"),
                target: record.target.clone().or_else(|| record.predicted.clone()),
                violations: Vec::new(),
            };
            self.journal.finish(
                phase,
                slot,
                &MarketPendingResult {
                    custody: record.clone(),
                    step,
                },
            )?;
            return Ok(Reconciled::Settled);
        }
        Ok(Reconciled::Unresolved(Safe::literal(
            "unresolved market custody: the declared public proof did not uniquely attribute the submission; the run is abandoned until reconciled",
        )))
    }

    /// Funded-key reconciliation. The coordinate names its actor: the
    /// schedule is frozen, so the (phase, slot) of an open fund_key intent
    /// identifies exactly the one `keys generate <actor> --fund` that can
    /// have produced this submission, and the run-local keystore holds only
    /// what that coordinate's own generate call created. A first-run gate
    /// freezes the operator as the signer while the key does not exist
    /// yet, so the frozen signer cannot be compared to the generated key;
    /// the settlement proof is the coordinate's actor key resolving in the
    /// run-local keystore to an address that is not the operator (the CLI
    /// creates and funds the account atomically, so a resolvable key is a
    /// funded account).
    fn reconcile_funded_key(&self, phase: u32, slot: u32) -> anyhow::Result<bool> {
        let name = match self
            .schedule_owned()
            .into_iter()
            .find(|s| s.phase == phase && s.slot == slot)
        {
            Some(slot) => match slot.op {
                MOp::FundKey { who } => who.key_name(),
                _ => return Ok(false),
            },
            None => return Ok(false),
        };
        let net = self.ctx.actor_net();
        let st = Stellar::new(&net, &self.exec, Policy::Market);
        let found = st.keys_public_key(&name)?;
        // An identity resolving to the operator proves nothing was funded:
        // the CLI falls back to the operator instead of creating the actor.
        Ok(found.is_some_and(|addr| Some(&addr) != self.progress.admin.as_ref()))
    }

    /// Upload custody observation. An open upload intent is **never settled
    /// on inspection**, and this is the law rather than a placeholder.
    ///
    /// Work the crash case this arm exists for: an intent is written, the
    /// process dies before anything is submitted, and no transaction ever
    /// reaches the chain. On resume the pinned artifact is still sitting on
    /// the operator's disk with exactly the digest the intent declared, so any
    /// local comparison — against the pin, against the intent, against
    /// anything — returns the same answer it would return had the upload
    /// landed. Settling on it would mark the coordinate complete, move the
    /// graph past the upload phase, and deploy contracts whose code the chain
    /// never received, carrying a journal record that asserts an upload that
    /// never happened. That is unrecoverable: a coordinate recorded complete
    /// is never uploaded.
    ///
    /// No public read in this surface answers whether the chain holds a Wasm
    /// hash, so the coordinate stays unresolved and reaches the operator. The
    /// artifact is still re-derived, as an observation only, because two
    /// different emergencies need two different answers: bytes that changed
    /// underneath a booked mutation, and an intact artifact whose submission
    /// simply remains an unanswered question about the chain.
    fn observe_upload(&self, phase: u32, slot: u32, record: &MarketCustodyRecord) -> Reconciled {
        let unresolved = |drift: bool| {
            Reconciled::Unresolved(Safe::literal(if drift {
                "unresolved market custody: the artifact behind an open upload intent no longer matches the digest that intent declared; the pinned bytes changed underneath this run and nothing was replayed"
            } else {
                "unresolved market custody: an upload intent is open, the pinned artifact is intact on disk, and no public read can show the chain holds the code; nothing was assumed complete and nothing was replayed"
            }))
        };
        let Some(declared) = record.artifact_digest.as_ref() else {
            return unresolved(false);
        };
        let Some(coordinate) = self
            .schedule_owned()
            .into_iter()
            .find(|s| s.phase == phase && s.slot == slot)
        else {
            return unresolved(false);
        };
        let MOp::Upload { role } = &coordinate.op else {
            return unresolved(false);
        };
        let Some(pin) = self.cfg.artifacts.get(*role) else {
            return unresolved(false);
        };
        let Ok(Ok(bytes)) = pin
            .file_location()
            .map(|(p, _)| std::fs::read(expand_home(&p)))
        else {
            return unresolved(false);
        };
        if Self::artifact_drifts_from_intent(&bytes, declared) {
            return unresolved(true);
        }
        // Public reconciliation read: `contract fetch --wasm-hash` retrieves
        // installed code from the chain, so a successful fetch proves the
        // chain holds exactly the declared hash's code (the fetch output
        // re-hashes to the requested hash by construction of the ledger
        // entry key). Intact-local + fetched-public settles the coordinate;
        // a fetch that fails leaves the intent unresolved for the operator.
        let net = self.ctx.admin_net();
        let st = Stellar::new(&net, &self.exec, Policy::Market);
        let scratch = self.journal.out().join(".fetch-probe.wasm");
        let fetched = st.probe_ok(&[
            "contract",
            "fetch",
            "--wasm-hash",
            declared.as_str(),
            "--out-file",
            &scratch.display().to_string(),
        ]);
        let _ = std::fs::remove_file(&scratch);
        if fetched {
            let step = MarketStepRecord {
                phase,
                action: record.action.clone(),
                command: Safe::render_parsed(
                    Policy::Market,
                    &json!({"reconciled": true, "phase": phase, "slot": slot}),
                    200,
                ),
                outcome: Safe::literal(
                    "chain holds the declared wasm hash; reconciled by public fetch, not replayed",
                ),
                target: record.target.clone(),
                violations: Vec::new(),
            };
            if self
                .journal
                .finish(
                    phase,
                    slot,
                    &MarketPendingResult {
                        custody: record.clone(),
                        step,
                    },
                )
                .is_ok()
            {
                return Reconciled::Settled;
            }
        }
        unresolved(false)
    }

    /// Whether an artifact's bytes differ from the digest an open custody
    /// intent declared for them.
    fn artifact_drifts_from_intent(bytes: &[u8], declared: &Sha256Hex) -> bool {
        sha256_hex(bytes) != declared.as_str()
    }

    /// Deterministic-deploy reconciliation: predicted id, the exact code
    /// this coordinate declared installed at it, and its presence readback.
    ///
    /// The readback alone is not a proof. It is a call to a contract at an
    /// address that answers — and any other contract answering the same way
    /// would settle a coordinate whose code never landed. The declared
    /// digest is the only fact that ties the address to *this* coordinate,
    /// so it is required here; its absence leaves the coordinate
    /// unresolved, never settled on the weaker half.
    ///
    /// A deployment of the network's own code (a classic asset's contract)
    /// declares no digest: no upload of ours precedes it and no address
    /// but that derivation can hold it, so its presence is the whole proof.
    fn reconcile_deterministic_deploy(
        &self,
        record: &MarketCustodyRecord,
        readback: Option<Stellar<'_, RealExecutor>>,
    ) -> anyhow::Result<bool> {
        let Some(predicted) = &record.predicted else {
            return Ok(false);
        };
        let net = self.ctx.admin_net();
        let own = Stellar::new(&net, &self.exec, Policy::Market);
        let st = readback.as_ref().unwrap_or(&own);
        let Some(declared) = &record.artifact_digest else {
            // A real run's digest-less deploy is a classic asset's contract:
            // the network's own code at an address only the asset derives.
            if self.real() {
                return Ok(st
                    .try_view(predicted.as_str(), self.ctx.admin_alias(), "name", &[])
                    .is_ok_and(|inv| inv.ok));
            }
            return Ok(self.deploy_readback(predicted).unwrap_or(false));
        };
        // A contract id derives from the deployer and the salt, so our own
        // code at our predicted id can only be our own deploy. The lab keeps
        // its pool-admin readback on top; a real role has no such getter.
        let code = st.wasm_matches(predicted, declared.as_str());
        Ok(if self.real() {
            code
        } else {
            code && self.deploy_readback(predicted).unwrap_or(false)
        })
    }

    /// Idempotent-config reconciliation: exact readback of the configured
    /// state. The blend pool is the only market role with a public
    /// `get_admin`; every other configured target is proven by its phase
    /// battery (proxy `get_proxy`/`get_cached`, governance
    /// `next_proposal_id`/`get_proposal`, adapter/pool effects), so an
    /// open config intent on any other target stays unresolved.
    fn reconcile_config(&self, record: &MarketCustodyRecord) -> anyhow::Result<bool> {
        let Some(target) = &record.target else {
            return Ok(false);
        };
        Ok(self.pool_admin_readback(target).unwrap_or(false))
    }

    /// Resume law for the publication coordinate. It freezes no target and
    /// submits nothing to the chain, so an open intent there is an interrupted
    /// local write rather than an ambiguous submission, and the proof it
    /// declared is local: a manifest already on disk that reproduces this
    /// run's settled facts settles the coordinate it froze. A manifest not yet
    /// written is completed through the publisher's own idempotent path, which
    /// assembles only from settled facts. The intent must also carry the
    /// operator this run resolved as its signer, because the manifest names
    /// that operator: an intent frozen by another operator is not this run's
    /// publication. The frozen intent is never rewritten, and no mutation is
    /// replayed.
    fn reconcile_publish(
        &mut self,
        coordinate: &MSlot,
        record: &MarketCustodyRecord,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            record.phase == coordinate.phase
                && record.kind == coordinate.kind
                && record.action.to_string() == coordinate.action
                && record.target.is_none()
                && record.predicted.is_none()
                && self.progress.admin.as_ref() == Some(&record.signer),
            "the open intent at the publication coordinate is not the local publication this graph froze"
        );
        let step = self.run_slot(coordinate, record)?;
        self.journal.finish(
            coordinate.phase,
            coordinate.slot,
            &MarketPendingResult {
                custody: record.clone(),
                step,
            },
        )
    }

    /// Admin-binding readback for the blend pool. The proxy runtime,
    /// governance, and adapter expose no `get_admin`; their proofs are the
    /// real getters named above.
    fn pool_admin_readback(&self, id: &ContractId) -> anyhow::Result<bool> {
        let net = self.ctx.actor_net();
        let st = Stellar::new(&net, &self.exec, Policy::Market);
        let Some(admin) = &self.progress.admin else {
            return Ok(false);
        };
        // The readback itself is the proof: predicted id + frozen admin.
        // Gating on the deployed map would deadlock a resume (that map is
        // only rebuilt from a settled result, which is what this proves).
        let source = Who::User.key_name();
        let inv = st.try_view(id.as_str(), &source, "get_admin", &[]);
        let Ok(inv) = inv else { return Ok(false) };
        Ok(inv.ok && inv.value.as_ref().and_then(Value::as_str) == Some(admin.as_str()))
    }

    /// Deployed-presence proof for deterministic deploys: only the blend
    /// pool has a public presence reader, and it must carry the frozen
    /// operator as admin. All other roles are proven by their phase
    /// batteries and downstream effects that require them.
    fn deploy_readback(&self, id: &ContractId) -> anyhow::Result<bool> {
        self.pool_admin_readback(id)
    }

    /// The bookkeeping key a deterministic deployment is recorded under, or
    /// `None` for operations that create no address.
    fn deployed_role(op: &MOp) -> Option<String> {
        Some(match op {
            MOp::DeployToken { tok } => format!("token:{}", tok.as_str()),
            MOp::DeploySource { index } => format!("source:{index}"),
            MOp::DeployRuntime => "proxy-runtime".to_string(),
            MOp::DeployGovernance => "proxy-governance".to_string(),
            MOp::DeployAdapter => "adapter".to_string(),
            MOp::DeployComet => "comet".to_string(),
            MOp::DeployEmitter => "emitter".to_string(),
            MOp::DeployBackstop => "backstop".to_string(),
            MOp::DeployFactory => "factory".to_string(),
            MOp::DeployPool => "pool".to_string(),
            MOp::RealDeployLazer { instance } => format!("lazer:{instance}"),
            MOp::RealDeployRuntime => "proxy-runtime".to_string(),
            MOp::RealDeployGovernance => "proxy-governance".to_string(),
            MOp::RealDeployAdapter => "adapter".to_string(),
            MOp::RealDeployComet => "comet".to_string(),
            MOp::RealDeployEmitter => "emitter".to_string(),
            MOp::RealDeployBackstop => "backstop".to_string(),
            MOp::RealDeployFactory => "factory".to_string(),
            MOp::RealDeployPool => "pool".to_string(),
            // A synthetic leg's asset contract is the network's own code for
            // that classic asset. Its address is a function of the asset, and
            // the gate binds it by derivation below, so a settled one is
            // restored from its own custody record like any other deployment.
            MOp::DeploySac { leg } => format!("sac:{leg}"),
            _ => return None,
        })
    }

    /// A contract this run has already deployed and settled. Deployment
    /// state is rebuilt from settled journal results on every start, so a
    /// role absent here has no settled deployment in this run directory.
    fn deployed_id(&self, role: &str) -> anyhow::Result<ContractId> {
        self.deployed.get(role).cloned().ok_or_else(|| {
            anyhow!("a prior graph coordinate did not settle the deployed contract this mutation requires")
        })
    }

    fn token_id(&self, tok: Tok) -> anyhow::Result<ContractId> {
        self.deployed_id(&format!("token:{}", tok.as_str()))
    }

    /// Reconstruct deployment and identity state from settled journal
    /// results only: an actor's provisioned address from its settled
    /// funding coordinate, and each deterministic deployment from the
    /// predicted id its own settled custody record carries. Nothing here
    /// opens an intent or observes the chain.
    fn rebuild_from_journal(&mut self) -> anyhow::Result<()> {
        for slot in self.schedule_owned() {
            let Some(result) = self.journal.result(slot.phase, slot.slot)? else {
                continue;
            };
            if let MOp::FundKey { who } = &slot.op {
                // The funded key's own address, never the frozen signer: a
                // first-run booking freezes the operator as the signer while
                // the key does not exist yet, so treating the signer as the
                // actor's identity invents an actor that IS the operator and
                // the attribution guards then refuse the run. The run-local
                // keystore holds the key its generate call created; a key
                // that is gone stays unprovisioned rather than polluted.
                let net = self.ctx.actor_net();
                let st = Stellar::new(&net, &self.exec, Policy::Market);
                let addr = match st.keys_public_key(&who.key_name()) {
                    Ok(Some(addr)) => addr,
                    _ => result.custody.signer.clone(),
                };
                self.progress.identities.insert(*who, addr);
                continue;
            }
            if let MOp::CreateIssuer { leg } = &slot.op {
                // The issuer's own address, recovered from the record of the
                // coordinate that created it. Its secret is deleted once the
                // lock is proven, so the keystore is deliberately not the
                // source a resume reads: the journal is, and a leg whose
                // creating coordinate never settled stays unresolved rather
                // than inventing an issuer.
                if let Some(facts) = self.settled_facts(slot.phase, slot.slot)? {
                    if let Some(address) = facts.get("issuer").and_then(Value::as_str) {
                        if let Ok(issuer) = AccountId::from_str(address) {
                            while self.progress.issuers.len() <= *leg {
                                self.progress.issuers.push(issuer.clone());
                            }
                            self.progress.issuers[*leg] = issuer;
                        }
                    }
                }
                continue;
            }
            if let MOp::RealVerifyRuntime = &slot.op {
                // A shared runtime and its governance are not deployments this
                // run made, so no prediction of ours carries them: the
                // coordinate that verified them wrote both addresses into its
                // own record, and that record is what a resume reads. Without
                // this a resumed reuse run would lose the stack its reserves
                // and adapter were configured against.
                if let Some(facts) = self.settled_facts(slot.phase, slot.slot)? {
                    for (field, role) in [
                        ("runtime", "proxy-runtime"),
                        ("governance", "proxy-governance"),
                    ] {
                        if let Some(address) = facts.get(field).and_then(Value::as_str) {
                            if let Ok(id) = ContractId::from_str(address) {
                                self.deployed.insert(role.to_string(), id);
                            }
                        }
                    }
                }
                continue;
            }
            if let MOp::RealVerifyAdapter = &slot.op {
                // A pool that names an existing adapter deploys none, so no
                // prediction of ours carries it. The coordinate that verified
                // it froze the configured address as its target, and that
                // frozen target is what a resume reads: without it a restart
                // after this coordinate would lose the adapter the pool is
                // constructed against, and the next deployment would refuse
                // with "a prior graph coordinate did not settle the deployed
                // contract this mutation requires".
                if let Some(adapter) = result.custody.target.clone() {
                    self.deployed.insert("adapter".to_string(), adapter);
                }
                continue;
            }
            if let MOp::RealInitComet = &slot.op {
                // A recovery binds the instance this coordinate initialized,
                // not the one the deploy coordinate created: the instance
                // someone else claimed first is not the backing of this pool,
                // and the backstop, the factory and the pool are constructed
                // against whichever one this coordinate left bound. The
                // coordinate records which it left; a resume that read only
                // the frozen prediction would hand those three a contract
                // nobody here initialized.
                let recovered = self
                    .settled_facts(slot.phase, slot.slot)?
                    .and_then(|facts| {
                        facts
                            .get("comet")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                if let Some(address) = recovered {
                    if let Ok(id) = ContractId::from_str(&address) {
                        self.deployed.insert("comet".to_string(), id);
                        continue;
                    }
                }
            }
            if let (Some(role), Some(predicted)) = (
                Self::deployed_role(&slot.op),
                result.custody.predicted.clone(),
            ) {
                self.deployed.insert(role, predicted);
            }
        }
        Ok(())
    }

    /// The create2 salt the schedule consumes for this slot's salt role, or
    /// `None` for operations that deploy nothing.
    fn slot_salt(&self, slot: &MSlot) -> Option<String> {
        market_salt_role(&slot.op).map(|role| salt(self.nonce, role, MARKET_SIDE))
    }

    /// The salt a front-run recovery deploys the next backing pool under. The
    /// role band is reserved (280..) below every deployable in the market
    /// table, and the salt is a pure function of this run's nonce, so the
    /// sequence of instances one run may claim is fixed at the moment its
    /// nonce is — never something a resume has to remember.
    fn comet_claim_salt(&self, attempt: u32) -> String {
        salt(self.nonce, COMET_CLAIM_ROLE + attempt, MARKET_SIDE)
    }

    /// The exact governance operation and proxy configuration the schedule
    /// intends for one market asset: all three deployed source contracts —
    /// distinct oracles, which `validate_proxy_config` requires — serving
    /// the asset the proxy is asked to protect, three-of-three, 3600 seconds
    /// maximum age, 60 seconds clock drift. Every address is read from this
    /// run's settled deployments; a missing one refuses rather than
    /// substituting a placeholder.
    fn set_proxy_operation(&self, tok: Tok) -> anyhow::Result<(Value, Value)> {
        let asset = json!({ "Stellar": self.token_id(tok)?.as_str() });
        let mut sources = Vec::with_capacity(MARKET_SOURCE_COUNT);
        for index in 0..MARKET_SOURCE_COUNT {
            sources.push(json!({
                "oracle": self.deployed_id(&format!("source:{index}"))?.as_str(),
                "asset": asset,
            }));
        }
        let config = json!({
            "sources": sources,
            "min_sources": self.cfg.oracle.min_sources,
            "max_age_secs": self.cfg.oracle.max_age_secs,
            "max_clock_drift_secs": self.cfg.oracle.max_clock_drift_secs,
        });
        Ok((
            json!({ "SetProxy": [asset.clone(), config.clone()] }),
            config,
        ))
    }

    /// The pool's own config read-back, including its live status. Used to
    /// prove a status or reserve coordinate is lawful to submit at all, and
    /// to settle only on state the pool itself reports.
    fn pool_config(&self, pool: &ContractId) -> anyhow::Result<Value> {
        let admin_net = self.ctx.admin_net();
        let st = Stellar::new(&admin_net, &self.exec, Policy::Market);
        let config = st.view(pool.as_str(), self.ctx.admin_alias(), "get_config", &[])?;
        anyhow::ensure!(
            config.get("status").and_then(Value::as_u64).is_some(),
            "the pool did not report its status"
        );
        Ok(config)
    }

    /// Read `get_reserve` for `tok` and require its live `config` to equal,
    /// field for field, the configuration this market derives from its
    /// manifest. Any difference refuses: the pool seals reserve economics on
    /// leaving setup, so a drifted cap, factor or rate curve must be caught
    /// while it can still be fixed.
    fn require_reserve_matches_manifest(
        &self,
        pool: &ContractId,
        tok: Tok,
        index: u32,
    ) -> anyhow::Result<()> {
        let admin_net = self.ctx.admin_net();
        let st = Stellar::new(&admin_net, &self.exec, Policy::Market);
        let live = st.view(
            pool.as_str(),
            self.ctx.admin_alias(),
            "get_reserve",
            &[kv("asset", self.token_id(tok)?.as_str())],
        )?;
        let live_config = live
            .get("config")
            .ok_or_else(|| anyhow!("the pool did not report a reserve configuration"))?;
        let expected = reserve_config(tok, index, self.cfg.reserves.get(&tok));
        for (field, want) in expected.as_object().expect("reserve_config is an object") {
            let got = live_config.get(field);
            let equal = match want {
                Value::Bool(b) => got.and_then(Value::as_bool) == Some(*b),
                _ => got.and_then(price_as_i128) == price_as_i128(want),
            };
            anyhow::ensure!(
                equal,
                "live reserve '{}' field '{field}' does not match the market config",
                tok.as_str()
            );
        }
        Ok(())
    }

    /// The completed market's manifest, assembled only from this run's
    /// settled facts: provisioned identities, deployed addresses, and the
    /// configured network and oracle stack. Any gap refuses; publication
    /// never carries an unproven field. `deploy_ledger` is the only fact not
    /// held by the journal, so the caller supplies it: an assembly built from
    /// a published manifest's own stamp needs no observation at all.
    fn assemble_manifest(&self, deploy_ledger: u32) -> anyhow::Result<MarketManifest> {
        if self.real() {
            return self.assemble_real_manifest(deploy_ledger);
        }
        let admin = self
            .progress
            .admin
            .as_ref()
            .ok_or_else(|| anyhow!("the operator is unresolved"))?;
        let mut tokens = std::collections::BTreeMap::new();
        for tok in Tok::MOCK {
            tokens.insert(tok, self.token_id(tok)?);
        }
        let mut sources = Vec::with_capacity(MARKET_SOURCE_COUNT);
        for index in 0..MARKET_SOURCE_COUNT {
            sources.push(self.deployed_id(&format!("source:{index}"))?);
        }
        let mut identities = std::collections::BTreeMap::new();
        for who in [Who::Frodo, Who::Sam, Who::User] {
            identities.insert(
                who,
                self.progress
                    .identities
                    .get(&who)
                    .cloned()
                    .ok_or_else(|| anyhow!("a run-local actor is not provisioned"))?,
            );
        }
        let rpc_host = self.ctx.rpc_host_label();
        let mut reserves = std::collections::BTreeMap::new();
        for (index, tok) in Tok::RESERVES.iter().enumerate() {
            reserves.insert(
                *tok,
                ReserveRef {
                    index: index as u32,
                    decimals: tok.decimals(),
                },
            );
        }
        Ok(MarketManifest {
            real: None,
            network: MarketNetworkRecord {
                label: Safe::literal(MARKET_NETWORK_LABEL),
                passphrase: Safe::literal(MARKET_TESTNET_PASSPHRASE),
            },
            admin: admin.clone(),
            rpc_host: Safe::render_parsed(Policy::Market, &json!({"host": rpc_host}), 256),
            oracle: OracleStackManifest {
                runtime: self.deployed_id("proxy-runtime")?,
                governance: Some(self.deployed_id("proxy-governance")?),
                adapter: self.deployed_id("adapter")?,
                sources,
                min_sources: self.cfg.oracle.min_sources,
                max_age_secs: self.cfg.oracle.max_age_secs,
                max_clock_drift_secs: self.cfg.oracle.max_clock_drift_secs,
                decimals: self.cfg.oracle.decimals,
                resolution: self.cfg.oracle.resolution,
                base: Safe::literal(MARKET_BASE),
            },
            tokens,
            pool: self.deployed_id("pool")?,
            backstop: self.deployed_id("backstop")?,
            factory: self.deployed_id("factory")?,
            comet: self.deployed_id("comet")?,
            emitter: self.deployed_id("emitter")?,
            reserves,
            identities,
            deploy_ledger,
        })
    }

    /// The completed real market's manifest, assembled only from settled
    /// facts and bound contract identifiers. Its price stack is configured
    /// per asset, so the shared oracle block is a mirror of the first
    /// reserve and the per-asset truth lives in `real.proxies`.
    fn assemble_real_manifest(&self, deploy_ledger: u32) -> anyhow::Result<MarketManifest> {
        let admin = self
            .progress
            .admin
            .as_ref()
            .ok_or_else(|| anyhow!("the operator is unresolved"))?;
        let network = self.cfg.network().context("the configured network is unresolvable")?;
        let pool = self.pool()?;
        let shim = self
            .cfg
            .shim
            .as_ref()
            .ok_or_else(|| anyhow!("a real market declares its shim's asset codes"))?;
        let runtime = self.deployed_id("proxy-runtime")?;
        let adapter = self.deployed_id("adapter")?;
        let base = self.price_base();
        let base_safe = Safe::render_parsed(Policy::Market, &base, 256);
        // The source identities each asset's proxy was configured with, in
        // registration order: `lazer:<feed id>` or `sep40:<contract id>`.
        let source_identities = |asset: usize| -> anyhow::Result<Vec<Safe>> {
            let reserve = self.reserve(asset)?;
            let mut names = Vec::with_capacity(reserve.sources.len());
            for source in &reserve.sources {
                let text = match source.kind {
                    crate::manifest::SourceKind::Lazer => format!(
                        "lazer:{}",
                        source
                            .feed_id
                            .as_deref()
                            .ok_or_else(|| anyhow!("a Lazer source names no feed id"))?
                    ),
                    crate::manifest::SourceKind::Sep40 => format!(
                        "sep40:{}",
                        source
                            .contract
                            .as_deref()
                            .ok_or_else(|| anyhow!("an existing oracle names no contract"))?
                    ),
                    crate::manifest::SourceKind::Mock => {
                        return Err(anyhow!("the lab source is not a price source of a real market"))
                    }
                };
                names.push(Safe::render_parsed(
                    Policy::Market,
                    &json!({"source": text}),
                    256,
                ));
            }
            Ok(names)
        };
        let first = self.reserve(0)?;
        // The first reserve's sources as the contracts the runtime was
        // configured to ask: each Lazer instance this run deployed, and each
        // existing SEP-40 contract the configuration named verbatim.
        let mut mirrored_sources = Vec::with_capacity(first.sources.len());
        let mut lazer = lazer_instances(first);
        for source in &first.sources {
            match source.kind {
                crate::manifest::SourceKind::Lazer => {
                    let (instance, _) = lazer
                        .next()
                        .ok_or_else(|| anyhow!("a Lazer source has no instance of its own"))?;
                    mirrored_sources.push(self.deployed_id(&format!("lazer:{instance}"))?);
                }
                crate::manifest::SourceKind::Sep40 => {
                    let named = source
                        .contract
                        .as_deref()
                        .ok_or_else(|| anyhow!("an existing oracle names no contract"))?;
                    mirrored_sources.push(
                        ContractId::from_str(named)
                            .map_err(|_| anyhow!("an existing oracle's address is malformed"))?,
                    );
                }
                crate::manifest::SourceKind::Mock => {
                    return Err(anyhow!("the lab source is not a price source of a real market"))
                }
            }
        }
        // Every Lazer instance this run deployed, in the one numbering law the
        // graph, the proxies and this manifest share. A reserve that quotes an
        // existing SEP-40 oracle adds none: that contract is not ours to name.
        let lazer_count = (0..pool.reserves.len())
            .map(|index| {
                self.reserve(index).map(|reserve| lazer_instances(reserve).count())
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        let mut lazer_sources = Vec::with_capacity(lazer_count);
        for instance in 0..lazer_count {
            lazer_sources.push(self.deployed_id(&format!("lazer:{instance}"))?);
        }
        let mut proxies = Vec::with_capacity(pool.reserves.len());
        for (index, reserve) in pool.reserves.iter().enumerate() {
            let asset = ContractId::from_str(&reserve.asset)
                .map_err(|_| anyhow!("a reserve's asset address is malformed"))?;
            proxies.push(crate::manifest::AssetProxyManifest {
                label: Safe::render_parsed(Policy::Market, &json!({"label": reserve.label}), 256),
                asset,
                adapter: adapter.clone(),
                runtime: runtime.clone(),
                sources: source_identities(index)?,
                min_sources: reserve.min_sources,
                max_age_secs: reserve.max_age_secs,
                max_clock_drift_secs: reserve.max_clock_drift_secs,
                decimals: reserve.decimals,
                base: base_safe.clone(),
            });
        }
        let mut reserves = Vec::with_capacity(pool.reserves.len());
        for (index, reserve) in pool.reserves.iter().enumerate() {
            let asset = ContractId::from_str(&reserve.asset)
                .map_err(|_| anyhow!("a reserve's asset address is malformed"))?;
            reserves.push(crate::manifest::RealReserveManifest {
                label: Safe::render_parsed(Policy::Market, &json!({"label": reserve.label}), 256),
                asset,
                index: index as u32,
                decimals: reserve.decimals,
            });
        }
        let leg = |leg: usize, code: &str| -> anyhow::Result<crate::manifest::ShimLegManifest> {
            Ok(crate::manifest::ShimLegManifest {
                code: Safe::render_parsed(Policy::Market, &json!({"code": code}), 256),
                issuer: self.issuer_of(leg)?,
                sac: self.sac_id(leg)?,
            })
        };
        // The chain, not the step record: a deposit coordinate settled by
        // reconciliation carries no facts, and the backstop is the authority.
        let backstop_data = self.backstop_pool_data(
            &self.deployed_id("backstop")?,
            &self.deployed_id("pool")?,
        )?;
        // TOML integers are 64-bit; both figures are far inside that range.
        let figure = |key: &str| {
            backstop_data
                .get(key)
                .and_then(price_as_i128)
                .and_then(|v| i64::try_from(v).ok())
        };
        let backstop_deposit = figure("tokens")
            .ok_or_else(|| anyhow!("the backstop reports no deposit for this pool"))?;
        let q4w_pct = figure("q4w_pct")
            .ok_or_else(|| anyhow!("the backstop reports no queued-withdrawal figure"))?;
        Ok(MarketManifest {
            real: Some(crate::manifest::RealMarketManifest {
                pool_name: Safe::render_parsed(Policy::Market, &json!({"pool": pool.name}), 256),
                reserves,
                blnd: leg(0, &shim.blnd_code)?,
                usdc: leg(1, &shim.usdc_code)?,
                backstop_deposit,
                q4w_pct,
                proxies,
                lazer_sources,
            }),
            network: MarketNetworkRecord {
                label: Safe::literal(network.label()),
                passphrase: Safe::literal(network.passphrase()),
            },
            admin: admin.clone(),
            rpc_host: Safe::render_parsed(
                Policy::Market,
                &json!({"host": self.ctx.rpc_host_label()}),
                256,
            ),
            oracle: OracleStackManifest {
                runtime,
                governance: Some(self.deployed_id("proxy-governance")?),
                adapter,
                // ponytail: a real market's price truth is per asset and
                // lives in `real.proxies`; this mirror of the first reserve
                // exists only so one reader can load both graphs.
                sources: mirrored_sources,
                min_sources: first.min_sources,
                max_age_secs: first.max_age_secs,
                max_clock_drift_secs: first.max_clock_drift_secs,
                decimals: first.decimals,
                resolution: self.cfg.oracle.resolution,
                base: base_safe.clone(),
            },
            tokens: BTreeMap::new(),
            pool: self.deployed_id("pool")?,
            backstop: self.deployed_id("backstop")?,
            factory: self.deployed_id("factory")?,
            comet: self.deployed_id("comet")?,
            emitter: self.deployed_id("emitter")?,
            reserves: BTreeMap::new(),
            identities: BTreeMap::new(),
            deploy_ledger,
        })
    }

    /// The published manifest for this run directory, if one exists and
    /// reproduces the state assembled from this run's settled facts. This is
    /// the local proof a publication completed: `deploy_ledger` is a stamp of
    /// when the file was proven, not a fact a resume must re-observe, so the
    /// published stamp is fed back into assembly and the rendered bytes are
    /// required to be identical. Anything less is a refusal, never a guess.
    /// Performs no observation: it reads this run directory and nothing else.
    fn published_manifest(&self) -> anyhow::Result<Option<MarketManifest>> {
        if !self.journal.out().join(MARKET_MANIFEST_FILE).exists() {
            return Ok(None);
        }
        let published = MarketManifest::load(self.journal.out().as_path())
            .context("the published market manifest is unreadable")?;
        let assembled = self.assemble_manifest(published.deploy_ledger)?;
        anyhow::ensure!(
            toml::to_string_pretty(&published).context("rendering the market manifest")?
                == toml::to_string_pretty(&assembled).context("rendering the market manifest")?,
            "a different market manifest was already published here"
        );
        Ok(Some(published))
    }

    /// Assembly plus the one fact no journal can hold: the ledger the run was
    /// proven against. Read-only, and needed only when a manifest is
    /// published for the first time.
    fn build_manifest(&self) -> anyhow::Result<MarketManifest> {
        let admin_net = self.ctx.admin_net();
        let st = Stellar::new(&admin_net, &self.exec, Policy::Market);
        self.assemble_manifest(st.latest_ledger()?)
    }
}

/// Whether this run has a frozen deployment plan. Its mere existence binds
/// the run to custody reconciliation (plan §4): it forbids auto-continuation
/// after a halt, and it is the only fact the halt path needs. The plan lives
/// beside the journal, so this is the path that law reads.
fn read_plan(out: &Path) -> anyhow::Result<Option<()>> {
    Ok(out.join(MARKET_PLAN_FILE).exists().then_some(()))
}

impl MarketDeployer {
    /// The shared deployment state machine driven by every operator surface.
    /// Terminal events are replayed forever; nothing auto-continues past a
    /// gate, a halt, or unresolved custody.
    pub fn step(&mut self, act: DeployAct) -> anyhow::Result<DeployEvent> {
        if let Some(ev) = &self.terminal {
            return Ok(ev.clone());
        }
        self.scan_journal()?;
        if let Some(note) = &self.unresolved {
            return Ok(DeployEvent::Custody {
                unresolved: true,
                note: note.clone(),
            });
        }
        match act {
            DeployAct::Confirm { token }
                if self.gate.as_ref().is_some_and(|g| g.token == token) =>
            {
                let gate = self.gate.take().unwrap();
                match self.execute_gate(&gate) {
                    Ok(None) => Ok(DeployEvent::Phase {
                        id: gate.phase as usize,
                        label: Safe::literal("mutation submitted after confirmation"),
                    }),
                    // A step that ran to a definite answer and reported
                    // violations did not establish what the coordinate exists
                    // to establish — a refresh with no Accepted price is the
                    // live case. It is not a settled mutation and the run does
                    // not walk past it: the coordinate was archived for
                    // replay, so `--resume` asks again once the condition
                    // holds, and nothing later in the graph runs meanwhile.
                    Ok(Some(violations)) => {
                        let failed = DeployEvent::Failed(Safe::literal(
                            "the confirmed step reported a violation; nothing after it was attempted",
                        ));
                        self.stopped_violations = violations;
                        self.terminal = Some(failed.clone());
                        Ok(failed)
                    }
                    Err(e) => Ok(self.unresolved(
                        gate.phase,
                        gate.slot,
                        Some(Safe::error_chain(Policy::Market, &e)),
                    )),
                }
            }
            DeployAct::Confirm { .. } if self.gate.is_some() => {
                let gate = self.gate.as_ref().unwrap();
                Ok(DeployEvent::Confirm {
                    token: gate.token.clone(),
                    summary: gate.summary.clone(),
                })
            }
            DeployAct::Confirm { .. } => Ok(DeployEvent::Failed(Safe::literal(
                "unexpected confirmation: nothing is awaiting a token",
            ))),
            DeployAct::Halt => {
                if self.gate.is_some() {
                    self.gate = None;
                }
                if self.plan_written {
                    let e =
                        anyhow!("deployment halted with a frozen plan; resume reconciles custody");
                    return Ok(DeployEvent::Failed(Safe::error_chain(Policy::Market, &e)));
                }
                self.halted = true;
                let e = anyhow!("deployment halted before any journaled mutation");
                Ok(DeployEvent::Failed(Safe::error_chain(Policy::Market, &e)))
            }
            DeployAct::Init | DeployAct::Continue => {
                self.plan_written = read_plan(&self.journal.out())?.is_some();
                self.advance()
            }
        }
    }

    /// Advance the graph to its next required step: open the confirmation
    /// gate for the first scheduled coordinate that has no settled result, or
    /// finish the run. Journal completion drives progress; nothing is skipped
    /// that the journal has not settled, and nothing auto-continues past a
    /// gate. Without a frozen plan and a bound nonce the graph authorizes no
    /// mutation and ends in a terminal event, never at a gate.
    fn advance(&mut self) -> anyhow::Result<DeployEvent> {
        if let Some(m) = &self.manifest {
            return Ok(DeployEvent::Done(Box::new(m.clone())));
        }
        if self.nonce == 0 || !self.plan_written {
            let e = anyhow!("no frozen deployment plan is bound, so no mutation is authorized");
            self.terminal = Some(DeployEvent::Failed(Safe::error_chain(Policy::Market, &e)));
            return Ok(self.terminal.clone().unwrap());
        }
        if let Some(slot) = self.next_unsettled_slot()? {
            return self.prepare_gate(&slot);
        }
        self.publish()
    }

    /// The first scheduled coordinate with no verified settlement, in graph
    /// order, or an error when a settlement cannot be verified: an unknown
    /// custody stops the run before it can mutate or publish anything.
    fn next_unsettled_slot(&self) -> anyhow::Result<Option<MSlot>> {
        for slot in self.schedule_owned() {
            if self.journal.result(slot.phase, slot.slot)?.is_none() {
                return Ok(Some(slot));
            }
        }
        Ok(None)
    }

    /// The proposal id one propose coordinate froze into its own custody
    /// record at the gate, before it sent. It is the only fact that tells
    /// this run's proposal apart from another client's on a governance more
    /// than one pool configures, so it is asked of the frozen intent and
    /// never of a counter that other submissions move.
    fn frozen_proposal_id(record: &MarketCustodyRecord) -> Option<u64> {
        record.pre_state.iter().find_map(|anchor| {
            serde_json::from_str::<Value>(&anchor.to_string())
                .ok()
                .and_then(|value| value.get("expected_proposal_id")?.as_u64())
        })
    }

    /// The ownership-transfer deadline one Lazer handoff froze into its own
    /// custody record at its gate. Like the proposal id above, it is a fact
    /// this run chose and no later reading can recover, so it is asked of the
    /// frozen intent and never of the ledger.
    fn frozen_live_until(record: &MarketCustodyRecord) -> Option<u32> {
        record.pre_state.iter().find_map(|anchor| {
            serde_json::from_str::<Value>(&anchor.to_string())
                .ok()
                .and_then(|value| value.get("pending_live_until_ledger")?.as_u64())
                .map(|n| n as u32)
        })
    }

    /// Open the confirmation gate for one scheduled mutation. The custody
    /// record and its payload digest are computed *before* the gate is
    /// offered, so the token the operator is asked to confirm is bound to the
    /// exact mutation content: phase, coordinate, kind, action, target,
    /// artifact digest, payload digest, and the exact testnet. Confirming one
    /// gate cannot authorize a different mutation, and a re-frozen plan cannot
    /// quietly change what a retained token releases.
    fn prepare_gate(&mut self, slot: &MSlot) -> anyhow::Result<DeployEvent> {
        if let Some(gate) = &self.gate {
            if gate.phase == slot.phase && gate.slot == slot.slot {
                return Ok(DeployEvent::Confirm {
                    token: gate.token.clone(),
                    summary: gate.summary.clone(),
                });
            }
        }
        let mut record = MarketCustodyRecord {
            phase: slot.phase,
            kind: slot.kind,
            action: Safe::literal(slot.action),
            network: self.cfg.network()?,
            signer: self.slot_signer(slot)?,
            payload_digest: Sha256Hex::default(),
            target: self.slot_target(slot),
            artifact_digest: self.slot_artifact_digest(slot),
            predicted: None,
            pre_state: vec![Safe::literal(
                "no settled result exists at this coordinate; nothing has been replayed",
            )],
            tool_error: None,
        };
        // Deterministic deployments bind their predicted id into the custody
        // record — and through it into the payload digest the confirmation
        // token signs — before any mutation is offered. Most deployments
        // derive from the offline account-branch law (`contract id wasm
        // --salt --source`), not a chain observation. The pool is no
        // exception, only no longer excluded: a factory child is predictable
        // under a second proven law — keccak256(salt || admin-strkey padded
        // to 56) then sha256(XDR HashIdPreimage::ContractId) with the
        // settled factory as sender (pool_factory.rs:92-109, proven offline
        // against the real factory wasm by blend-contracts-v2/pool-proof and
        // gated by known-answer tests in this file). The raw role-235 salt
        // deploys nothing on its own and must never be the bound value; the
        // binding below is always the derived child id.
        // Two real coordinates name an address no salt of this run creates.
        // A synthetic leg's asset contract is the network's own code for that
        // classic asset, so its address is a pure function of the asset and is
        // bound by derivation. A shared runtime is not this run's to create:
        // its address is the one the configuration names, and it is exactly
        // that contract the coordinate will ask who owns it. Both are bound
        // here rather than exempted, because the gate must approve the address
        // the coordinate acts on, not an absent one.
        match &slot.op {
            MOp::DeploySac { leg } => {
                if record.target.is_none() && record.predicted.is_none() {
                    let asset = self.leg_asset(*leg)?;
                    record.predicted = Some(self.reader().asset_contract_id(&asset)?);
                }
            }
            MOp::RealVerifyRuntime => {
                if record.target.is_none() {
                    let configured = self
                        .pool()?
                        .runtime
                        .clone()
                        .ok_or_else(|| anyhow!("this market names no runtime to verify"))?;
                    record.target = Some(
                        ContractId::from_str(&configured)
                            .map_err(|_| anyhow!("the configured runtime address is malformed"))?,
                    );
                }
            }
            // The mirror of the shared runtime: a pool that names an existing
            // adapter deploys none, so no deployment of ours records one and
            // no salt predicts it. The adapter the configuration names is the
            // contract this read-only coordinate acts on, so it is bound here
            // — otherwise the gate has nothing to bind, refuses to open, and
            // the coordinate the step then settles against would carry a
            // target its own intent never froze.
            MOp::RealVerifyAdapter => {
                if record.target.is_none() {
                    let configured = self
                        .pool()?
                        .adapter
                        .clone()
                        .ok_or_else(|| anyhow!("this market names no adapter to verify"))?;
                    record.target = Some(
                        ContractId::from_str(&configured)
                            .map_err(|_| anyhow!("the configured adapter address is malformed"))?,
                    );
                }
            }
            MOp::RealProposeAsset { .. } | MOp::RealProposeRole { .. } => {
                // A proposal's id is governance's counter at the moment this
                // coordinate is authorized. Reading it *here*, before the
                // send, and freezing it into the payload the token signs is
                // what makes an interrupted propose reconcilable: the resume
                // asks the governance for exactly this proposal and for no
                // other, so another pool's proposal on a shared governance —
                // or this run's first reserve's proposal — can never be
                // mistaken for a submission this coordinate made.
                let governance = record.target.clone().ok_or_else(|| {
                    anyhow!("a propose coordinate names no governance to bind its proposal to")
                })?;
                if Self::frozen_proposal_id(&record).is_none() {
                    let counter = self.operator_reader().view(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "next_proposal_id",
                        &[],
                    )?;
                    let Some(id) = counter
                        .as_u64()
                        .or_else(|| counter.get("id").and_then(Value::as_u64))
                    else {
                        anyhow::bail!(
                            "governance reports no proposal counter to bind this proposal to"
                        );
                    };
                    record
                        .pre_state
                        .push(Safe::render_market_json(&json!({ "expected_proposal_id": id }), 256)?);
                }
            }
            MOp::RealHandoffLazer { .. }
                if Self::frozen_live_until(&record).is_none() =>
            {
                // The mirror of the frozen proposal id, for the one handoff
                // whose proof is a value with a deadline in it. The deadline
                // is the curator's, but it is *this* coordinate's own reading
                // of the ledger that sets it, and freezing it here is what
                // lets a resume tell "the transfer I proposed is waiting for
                // its new owner" from "someone else's transfer is" — the
                // temporary entry names both an address and a ledger, and an
                // address alone would not distinguish them.
                let now = self.operator_reader().latest_ledger()?;
                let live_until_ledger = now.saturating_add(HANDOFF_TTL_LEDGERS);
                record.pre_state.push(Safe::render_market_json(
                    &json!({ "pending_live_until_ledger": live_until_ledger }),
                    256,
                )?);
            }
            _ => {}
        }
        if let Some(salt) = self.slot_salt(slot) {
            record.predicted = Some(match slot.op {
                MOp::DeployPool | MOp::RealDeployPool => {
                    // The settled factory is the only lawful sender for a
                    // pool child; it is deployed one coordinate earlier and
                    // rebuilt from its settled custody record on every
                    // start, so the gate cannot bind against an absent or
                    // substituted factory.
                    let factory = self.deployed_id("factory")?;
                    let admin = self
                        .progress
                        .admin
                        .clone()
                        .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                    let predicted = predict_pool_child(
                        self.cfg.network()?,
                        factory.as_str(),
                        &salt,
                        admin.as_str(),
                    )?;
                    ContractId::from_str(&predicted)
                        .map_err(|_| anyhow!("the predicted pool id is malformed"))?
                }
                _ => {
                    let admin_net = self.ctx.admin_net();
                    let st = Stellar::new(&admin_net, &self.exec, Policy::Market);
                    st.predict_id(self.ctx.admin_alias(), &salt)?
                }
            });
        }
        // A classic coordinate of the activation shim has neither a contract
        // to act on nor an address to predict: its authority is the account
        // and asset named in the configuration, and the record of the
        // coordinate that created them. It is the one family of operations
        // that is lawful unbaked, and only within the real graph.
        let classic = matches!(
            slot.op,
            MOp::CreateIssuer { .. }
                | MOp::TrustLeg { .. }
                | MOp::MintLeg { .. }
                | MOp::LockIssuer { .. }
                | MOp::DropTrust { .. }
        );
        anyhow::ensure!(
            record.target.is_some()
                || record.predicted.is_some()
                || classic && self.real()
                || matches!(
                    slot.op,
                    MOp::FundKey { .. } | MOp::Upload { .. } | MOp::Publish
                ),
            "market target is not bound before the confirmation gate"
        );
        record.payload_digest = custody_payload_digest(&record)?;
        // The confirmation the operator approves is the whole run in one
        // screen: which network, which key signs, and the contract or account
        // each remaining send will touch.
        let summary = Safe::render_parsed(
            Policy::Market,
            &json!({
                "phase": slot.phase,
                "slot": slot.slot,
                "action": slot.action,
                "network": self.cfg.network().ok().map(|network| network.label()),
                "signer": record.signer.as_str(),
                "target": record.target.as_ref().map(|t| t.as_str()),
                "artifact_digest": record.artifact_digest.as_deref(),
                "payload_digest": record.payload_digest.as_str(),
            }),
            900,
        );
        let token = self.confirm_token(slot, &record);
        self.gate = Some(Gate {
            phase: slot.phase,
            slot: slot.slot,
            token: token.clone(),
            record,
            summary: summary.clone(),
        });
        Ok(DeployEvent::Confirm { token, summary })
    }

    /// The one-time confirmation token for one mutation: derived from the
    /// frozen configuration digest and the custody record, so it is
    /// reproducible for that mutation and useless for any other.
    fn confirm_token(&self, slot: &MSlot, record: &MarketCustodyRecord) -> String {
        let mut preimage = b"abtest-market-confirm-v1".to_vec();
        preimage.extend_from_slice(
            market_config_digest(&self.cfg)
                .unwrap_or_default()
                .as_bytes(),
        );
        preimage.extend_from_slice(&slot.phase.to_le_bytes());
        preimage.extend_from_slice(&slot.slot.to_le_bytes());
        preimage.extend_from_slice(record.payload_digest.as_bytes());
        sha256_hex(&preimage)[..40].to_uppercase()
    }

    /// The signer frozen for one scheduled mutation. Only run-local actor
    /// provisioning is signed by an actor; every other mutation is signed by
    /// the resolved operator, and an unresolved operator authorizes nothing.
    fn slot_signer(&self, slot: &MSlot) -> anyhow::Result<AccountId> {
        match &slot.op {
            MOp::FundKey { who } => {
                if let Some(addr) = self.progress.identities.get(who) {
                    return Ok(addr.clone());
                }
                let net = self.ctx.actor_net();
                let st = Stellar::new(&net, &self.exec, Policy::Market);
                // Provisioning creates the identity, so an actor that does
                // not exist yet is expected: the operator authorizes the
                // generation. Once provisioned, resume binds the actor's
                // own address.
                Ok(st.keys_public_key(&who.key_name())?.unwrap_or_else(|| {
                    self.progress
                        .admin
                        .clone()
                        .expect("preflight resolved the operator before any gate")
                }))
            }
            // A classic asset's mint and its lock belong to the asset's
            // issuer and to nobody else: no other account can mint a classic
            // asset or change its own account's options. The address is the
            // one this run created and the journal recorded, and the key that
            // signs for it lives only in this run's keystore — so a pinned
            // signer refusal here means the keystore was rewritten underneath
            // the run, which is exactly what the law is for.
            MOp::MintLeg { leg } | MOp::LockIssuer { leg } => self.issuer_of(*leg),
            _ => {
                self.progress.admin.clone().ok_or_else(|| {
                    anyhow!("the operator is unresolved, so no mutation is authorized")
                })
            }
        }
    }

    /// The base every price of a real market is denominated in: the market's
    /// configured USD base, exactly as the pinned proxy contracts and the
    /// network's existing runtime report it.
    fn price_base(&self) -> Value {
        json!({ "Other": self.cfg.oracle.base })
    }

    /// The operator-signed CLI seam, bound to the frozen signer.
    fn operator_st(
        &self,
        record: &MarketCustodyRecord,
    ) -> anyhow::Result<Stellar<'_, RealExecutor>> {
        anyhow::ensure!(
            self.progress.admin.as_ref() == Some(&record.signer),
            "the frozen signer is not the resolved operator"
        );
        let alias = self.ctx.admin_alias().to_string();
        Ok(Stellar::new(&self.admin_net, &self.exec, Policy::Market)
            .with_market_signer(&alias, &record.signer))
    }

    /// A read-only seam: no signature is needed to ask a contract a question.
    fn reader(&self) -> Stellar<'_, RealExecutor> {
        Stellar::new(&self.actor_net, &self.exec, Policy::Market)
    }

    /// A read-only seam on the operator's own keystore: a view signed as the
    /// admin alias must find that alias, which the run-local reader never holds.
    fn operator_reader(&self) -> Stellar<'_, RealExecutor> {
        Stellar::new(&self.admin_net, &self.exec, Policy::Market)
    }

    /// The CLI seam bound to one synthetic leg's issuer — the only party that
    /// can mint a classic asset or change its own account's options. The key
    /// lives in this run's own keystore and its address is the one this run
    /// created, so a pinned-signer refusal here means the keystore was
    /// rewritten underneath the run.
    fn issuer_st(&self, leg: usize) -> anyhow::Result<Stellar<'_, RealExecutor>> {
        let issuer = self.issuer_of(leg)?;
        let alias = Self::issuer_key(leg);
        Ok(Stellar::new(&self.actor_net, &self.exec, Policy::Market)
            .with_market_signer(&alias, &issuer))
    }

    /// The keystore name of one leg's issuer key. Names are per-run (the
    /// keystore is this run's own directory), so two markets never share an
    /// issuer, and a name carries no secret.
    fn issuer_key(leg: usize) -> String {
        format!("market-shim-issuer-{leg}")
    }

    /// The classic asset of one leg: the configured synthetic code, issued by
    /// this run's own issuer account.
    fn leg_asset(&self, leg: usize) -> anyhow::Result<String> {
        let shim = self
            .cfg
            .shim
            .as_ref()
            .ok_or_else(|| anyhow!("a real market declares its shim's asset codes"))?;
        let code = match leg {
            0 => &shim.blnd_code,
            _ => &shim.usdc_code,
        };
        Ok(crate::stellar::classic_asset(code, &self.issuer_of(leg)?))
    }

    /// The seeded backing of one leg, in stroops: the runbook's minimum whose
    /// fourth-power product clears the backstop's threshold, derived from the
    /// configuration rather than typed by an operator, so two runs of one
    /// configuration back a pool identically.
    fn leg_seed(&self, leg: usize) -> i64 {
        let (blnd, usdc) = crate::manifest::market_shim_seed();
        i64::try_from(if leg == 0 { blnd } else { usdc }).unwrap_or(i64::MAX)
    }


    /// Execute one coordinate of a real market's graph, or `None` for the one
    /// coordinate the two graphs share (publication), whose arm lives with
    /// the lab graph's and is graph-independent by construction.
    ///
    /// Every mutation here follows the same three steps, and the order is the
    /// whole law of a resumable deployment:
    /// 1. ask the network for the effect — an RPC ledger entry or a contract
    ///    read, never a lagging index;
    /// 2. if the effect is absent, submit once, signed by the frozen signer;
    /// 3. require the effect to read back before the coordinate settles.
    fn run_real_slot(
        &mut self,
        slot: &MSlot,
        record: &MarketCustodyRecord,
    ) -> anyhow::Result<Option<MarketStepRecord>> {
        let step = |outcome: &str, target: Option<ContractId>, facts: Value| {
            Ok(Some(MarketStepRecord {
                phase: slot.phase,
                action: Safe::literal(slot.action),
                command: Safe::render_parsed(Policy::Market, &facts, 900),
                outcome: Safe::render_parsed(Policy::Market, &json!({"outcome": outcome}), 900),
                target,
                violations: Vec::new(),
            }))
        };
        let read_only = |detail: &str, target: Option<ContractId>, facts: Value| {
            Ok(Some(MarketStepRecord {
                phase: slot.phase,
                action: Safe::literal(slot.action),
                command: Safe::render_parsed(Policy::Market, &facts, 900),
                outcome: Safe::render_parsed(
                    Policy::Market,
                    &json!({"read_only": true, "detail": detail}),
                    900,
                ),
                target,
                violations: Vec::new(),
            }))
        };
        match &slot.op {
            // ---- the activation shim: two synthetic classic legs ----
            MOp::CreateIssuer { leg } => {
                // The key is made locally first: it *is* the address the
                // account will have, and generating a key that exists reports
                // it without overwriting. Creating the classic account is the
                // only send, and an account already present at that address
                // is proof the send happened — so a resume can replay this
                // coordinate and cannot fund an issuer twice.
                let local = self.reader();
                let issuer = local.keys_generate_local(&Self::issuer_key(*leg))?;
                if local.ledger_account(issuer.as_str())?.is_none() {
                    // Two min-reserve units plus fees: enough for the account
                    // to exist, to hold the line the operator is about to
                    // extend, and to pay for the issuer-signed mint and lock
                    // that follow.
                    let st = self.operator_st(record)?;
                    st.tx_create_account(&issuer, 20_000_000)?;
                    anyhow::ensure!(
                        self.reader()
                            .ledger_account(issuer.as_str())?
                            .is_some(),
                        "the created issuer account does not read back"
                    );
                }
                let mut issuers = self.progress.issuers.clone();
                while issuers.len() <= *leg {
                    issuers.push(issuer.clone());
                }
                issuers[*leg] = issuer.clone();
                self.progress.issuers = issuers;
                step(
                    "the leg's issuer account exists, funded by the operator, holding this run's only key",
                    None,
                    json!({"phase": slot.phase, "slot": slot.slot, "leg": leg,
                           "issuer": issuer.as_str()}),
                )
            }
            MOp::TrustLeg { leg } => {
                let asset = self.leg_asset(*leg)?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let reader = self.reader();
                if reader.ledger_trustline(admin.as_str(), &asset)?.is_none() {
                    // An unlimited line: the seeded backing is the only
                    // balance it will ever hold, and a bounded one would fail
                    // the mint that follows it.
                    let st = self.operator_st(record)?;
                    st.tx_change_trust(&asset, None)?;
                    anyhow::ensure!(
                        self.reader()
                            .ledger_trustline(admin.as_str(), &asset)?
                            .is_some(),
                        "the trustline this coordinate submitted does not read back"
                    );
                }
                read_only(
                    "the operator holds an unlimited line in the leg, which the mint needs and the deposit will empty",
                    None,
                    json!({"phase": slot.phase, "slot": slot.slot, "leg": leg}),
                )
            }
            MOp::DeploySac { leg } => {
                // The asset contract is the network's own code for a classic
                // asset. Its address is a function of the asset itself, so it
                // is computed rather than predicted from a salt, and deploying
                // it again returns the same address instead of creating a
                // second one.
                let asset = self.leg_asset(*leg)?;
                let st = self.operator_st(record)?;
                let expected = st.asset_contract_id(&asset)?;
                let id = st.asset_deploy(&asset)?;
                anyhow::ensure!(
                    id == expected,
                    "the deployed asset contract is not the address this asset has"
                );
                self.deployed.insert(format!("sac:{leg}"), id.clone());
                step(
                    "the leg's asset contract exists at its deterministic address",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot, "leg": leg}),
                )
            }
            MOp::MintLeg { leg } => {
                // Never mint twice: the balance is read from the ledger
                // before the payment is considered, and a balance that
                // already holds the seed settles this coordinate with no send
                // at all. That is the whole answer to a resumed or lagged
                // mint — the payment is issued only in the case where the
                // network says it has not been.
                let amount = self.leg_seed(*leg);
                let asset = self.leg_asset(*leg)?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let held = self
                    .reader()
                    .ledger_trustline(admin.as_str(), &asset)?
                    .map_or(0, |line| line.balance_stroops);
                if held < amount {
                    let issuer_st = self.issuer_st(*leg)?;
                    issuer_st.tx_payment(&admin, &asset, amount)?;
                    anyhow::ensure!(
                        self.reader()
                            .ledger_trustline(admin.as_str(), &asset)?
                            .map_or(0, |line| line.balance_stroops)
                            >= amount,
                        "the minted backing does not read back in the operator's balance"
                    );
                }
                step(
                    "the leg's seeded backing is held by the operator, who will place it as the pool admin",
                    None,
                    json!({"phase": slot.phase, "slot": slot.slot, "leg": leg,
                           "amount": amount.to_string()}),
                )
            }
            MOp::LockIssuer { leg } => {
                // The lock is what makes the backing permanent: with the
                // master weight at zero and every threshold at one, nothing
                // this account signs can move again. The proof is the ledger
                // entry of the issuer itself, so a Horizon lag cannot show a
                // lock that has not been applied.
                let issuer = self.issuer_of(*leg)?;
                let before = self
                    .reader()
                    .ledger_account(issuer.as_str())?
                    .ok_or_else(|| anyhow!("the leg's issuer account is not on the network"))?;
                if before.master_weight != 0 || before.low != 1 || before.med != 1 || before.high != 1
                {
                    let issuer_st = self.issuer_st(*leg)?;
                    issuer_st.tx_lock_account()?;
                }
                let locked = self
                    .reader()
                    .ledger_account(issuer.as_str())?
                    .ok_or_else(|| anyhow!("the leg's issuer account is not on the network"))?;
                anyhow::ensure!(
                    locked.master_weight == 0
                        && locked.low == 1
                        && locked.med == 1
                        && locked.high == 1
                        && locked.signers.is_empty(),
                    "the issuer is not locked: master weight zero, thresholds one, and no signers"
                );
                // Locked on the ledger, the key can sign nothing; delete it so
                // no copy outlives the run. Addresses come from the journal,
                // never this keystore, and an absent key is already the goal.
                self.reader().keys_remove(&Self::issuer_key(*leg))?;
                step(
                    "the leg's issuer can never mint again: master weight zero, thresholds one, no signers",
                    None,
                    json!({"phase": slot.phase, "slot": slot.slot, "leg": leg}),
                )
            }
            // The remaining coordinates of a real market's graph follow.
            _ => self.run_real_market(slot, record, &step, &read_only),
        }
    }

    /// The code, price stack and pool of a real market: everything after the
    /// activation shim. See `run_real_slot` for the read-submit-readback law
    /// each coordinate follows.
    #[allow(clippy::type_complexity)]
    fn run_real_market(
        &mut self,
        slot: &MSlot,
        record: &MarketCustodyRecord,
        step: &dyn Fn(&str, Option<ContractId>, Value) -> anyhow::Result<Option<MarketStepRecord>>,
        read_only: &dyn Fn(
            &str,
            Option<ContractId>,
            Value,
        ) -> anyhow::Result<Option<MarketStepRecord>>,
    ) -> anyhow::Result<Option<MarketStepRecord>> {
        use crate::manifest::SourceKind;
        let predicted_target = |label: &str| -> anyhow::Result<ContractId> {
            record
                .predicted
                .clone()
                .ok_or_else(|| anyhow!("{label} reached execution without a bound prediction"))
        };
        match &slot.op {
            // ---- this market's price stack ----
            MOp::RealDeployLazer { instance } => {
                let feeds = self
                    .shape
                    .as_ref()
                    .and_then(|s| s.lazer_feeds.get(*instance))
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!("a scheduled Lazer instance is not part of this configuration")
                    })?;
                let artifact = self
                    .artifacts
                    .get(crate::manifest::MARKET_LAZER_SOURCE_ROLE)
                    .ok_or_else(|| anyhow!("the Lazer source artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled source deploy lost its salt role"))?;
                let pool = self.pool()?;
                let verifier = pool
                    .lazer_verifier
                    .as_deref()
                    .ok_or_else(|| anyhow!("a market with Lazer sources names no verifier"))?
                    .to_string();
                // A source that serves several assets is as fresh as the most
                // demanding of them, so the tightest configured bound wins.
                let max_age = pool
                    .reserves
                    .iter()
                    .filter(|reserve| {
                        reserve
                            .sources
                            .iter()
                            .filter(|source| source.kind == SourceKind::Lazer)
                            .filter_map(|source| source.feed_id.as_deref())
                            .any(|feed| feeds.iter().any(|served| served == feed))
                    })
                    .map(|reserve| reserve.max_age_secs)
                    .min()
                    .unwrap_or(120);
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let base = self.price_base();
                let decimals = self.cfg.oracle.decimals;
                let st = self.operator_st(record)?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("owner", admin.as_str()),
                        kv(
                            "config",
                            json!({
                                "base": base,
                                "decimals": decimals,
                                "channel": "FixedRate200ms",
                                "freshness": {"max_age_secs": max_age, "max_clock_drift_secs": 0},
                                "verifier": verifier
                            })
                            .to_string(),
                        ),
                        // The contract takes `Vec<u32>` in strictly increasing
                        // order (its registry rule); the CLI refuses the
                        // config's string spelling of a feed id.
                        kv("supported_feed_ids", json!(lazer_feed_registry(&feeds)?).to_string()),
                    ],
                )?;
                anyhow::ensure!(
                    id == predicted_target("a Lazer source")?,
                    "the deployed source is not the contract this gate bound"
                );
                let bound = st.view(id.as_str(), self.ctx.admin_alias(), "config", &[])?;
                anyhow::ensure!(
                    bound.get("verifier").and_then(Value::as_str) == Some(verifier.as_str())
                        && bound.get("base") == Some(&base),
                    "the deployed source does not read back the verifier and base this market requires"
                );
                self.deployed.insert(format!("lazer:{instance}"), id.clone());
                step(
                    "a Lazer source instance exists at its predicted id, bound to the configured verifier and serving its configured feeds",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot, "instance": instance}),
                )
            }
            MOp::RealDeployRuntime => {
                // The runtime is constructed with governance it does not yet
                // have: it is bound to the address governance *will* have,
                // and the governance coordinate below must land exactly
                // there. A runtime owned by anything else would put this
                // pool's prices under a key this market never created.
                let artifact = self
                    .artifacts
                    .get("proxy-runtime")
                    .ok_or_else(|| anyhow!("the proxy runtime artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled runtime deploy lost its salt role"))?;
                let governance_salt = salt(
                    self.nonce,
                    market_salt_role(&MOp::RealDeployGovernance)
                        .ok_or_else(|| anyhow!("the governance salt role is gone"))?,
                    MARKET_SIDE,
                );
                // Predict with the operator's own keystore: the run-local
                // reader does not hold the admin alias.
                let st = self.operator_st(record)?;
                let expected_owner = st.predict_id(self.ctx.admin_alias(), &governance_salt)?;
                let base = self.price_base();
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("governance", expected_owner.as_str()),
                        kv("base", base.to_string()),
                    ],
                )?;
                anyhow::ensure!(
                    id == predicted_target("the runtime")?,
                    "the deployed runtime is not the contract this gate bound"
                );
                let owner = st.view(id.as_str(), self.ctx.admin_alias(), "get_owner", &[])?;
                anyhow::ensure!(
                    owner.as_str() == Some(expected_owner.as_str()),
                    "the runtime is not owned by the governance it was constructed with"
                );
                self.deployed.insert("proxy-runtime".to_string(), id.clone());
                step(
                    "the proxy runtime exists at its predicted id, governance-owned from its first block",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealVerifyRuntime => {
                // A shared runtime is not this run's to create or configure:
                // it is verified and used. Ownership is the whole question —
                // a runtime owned by another operator's governance would mean
                // this pool's prices are someone else's to change — and so is
                // the base, because a market cannot price in a unit its
                // sources do not quote.
                let configured = self
                    .pool()?
                    .runtime
                    .as_deref()
                    .ok_or_else(|| anyhow!("this market names no runtime to verify"))?
                    .to_string();
                let runtime = ContractId::from_str(&configured)
                    .map_err(|_| anyhow!("the configured runtime address is malformed"))?;
                let base = self.price_base();
                let reader = self.operator_reader();
                let owner = reader.view(runtime.as_str(), self.ctx.admin_alias(), "get_owner", &[])?;
                let served =
                    reader.view(runtime.as_str(), self.ctx.admin_alias(), "source_base", &[])?;
                anyhow::ensure!(
                    served == base,
                    "the shared runtime quotes a different base than this market's configuration"
                );
                let governance = match self.deployed.get("proxy-governance") {
                    Some(known) => known.clone(),
                    // A reused runtime's governance was not deployed here, so
                    // the runtime itself is asked who owns it. That answer is
                    // only accepted when the configuration names the same
                    // governance or names none.
                    None => ContractId::from_str(
                        owner
                            .as_str()
                            .ok_or_else(|| anyhow!("the shared runtime reports no owner"))?,
                    )
                    .map_err(|_| anyhow!("the shared runtime names a malformed owner"))?,
                };
                // The self-equal comparison above only proves the runtime
                // names an *owner*. A reused governance must also be the
                // operator's own: it is the contract every later coordinate
                // asks to act, and a governance the operator cannot admin
                // fails at the first create_proposal. Prove it here.
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                anyhow::ensure!(
                    owner.as_str() == Some(governance.as_str()),
                    "the shared runtime is not owned by the governance this market is configured with"
                );
                let admin_role = reader.view(
                    governance.as_str(),
                    self.ctx.admin_alias(),
                    "has_role",
                    &[kv("account", admin.as_str()), kv("role", "Admin")],
                )?;
                anyhow::ensure!(
                    admin_role.as_bool() == Some(true)
                        || self.shared_runtime_serves_every_reserve(runtime.as_str())?,
                    "the shared runtime's governance does not read back the operator's admin role"
                );
                self.deployed.insert("proxy-runtime".to_string(), runtime.clone());
                self.deployed
                    .insert("proxy-governance".to_string(), governance.clone());
                // Both addresses are written into this coordinate's own
                // record. A reused stack was not created by this run, so no
                // prediction of ours names it, and a resume may not go asking
                // the network again: the journal is the only place a fact
                // survives, and this is where it is written down.
                let facts = json!({
                    "phase": slot.phase,
                    "slot": slot.slot,
                    "runtime": runtime.as_str(),
                    "governance": governance.as_str(),
                });
                read_only(
                    "the shared runtime quotes this market's base and is owned by the governance that configures it; nothing was submitted",
                    Some(runtime),
                    facts,
                )
            }
            MOp::RealDeployGovernance => {
                let artifact = self
                    .artifacts
                    .get("proxy-governance")
                    .ok_or_else(|| anyhow!("the governance artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled governance deploy lost its salt role"))?;
                let runtime = self.deployed_id("proxy-runtime")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let st = self.operator_st(record)?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("admin", admin.as_str()),
                        kv("proxy_oracle", runtime.as_str()),
                        // The one proposal this graph makes is created and
                        // executed by the same key in adjacent coordinates.
                        // A nonzero timelock would leave it undecidable
                        // forever, so it is frozen at zero at construction
                        // and re-read before anything is ever proposed.
                        kv("initial_uniform_ttl_ns", "0"),
                    ],
                )?;
                anyhow::ensure!(
                    id == predicted_target("the governance contract")?,
                    "the deployed governance contract is not the contract this gate bound"
                );
                let bound = st.view(id.as_str(), self.ctx.admin_alias(), "proxy_oracle", &[])?;
                let admin_role = st.view(
                    id.as_str(),
                    self.ctx.admin_alias(),
                    "has_role",
                    &[kv("account", admin.as_str()), kv("role", "Admin")],
                )?;
                // v0.1.5 governance: the timelock this graph depends on is the
                // SetProxy operation's own TTL.
                let ttl = st.view(
                    id.as_str(),
                    self.ctx.admin_alias(),
                    "get_operation_ttl",
                    &[kv("kind", "SetProxy")],
                )?;
                anyhow::ensure!(
                    bound.as_str() == Some(runtime.as_str())
                        && admin_role.as_bool() == Some(true)
                        && ttl.as_u64() == Some(0),
                    "the deployed governance contract does not read back the bound runtime, the operator's admin role, and the zero timelock"
                );
                self.deployed.insert("proxy-governance".to_string(), id.clone());
                step(
                    "governance exists at its predicted id; its proposal timelock is zero, as the adjacent execute coordinates require",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealProposeAsset { asset } | MOp::RealExecuteAsset { asset } => {
                // One asset's price law is one proposal: its quorum, its
                // freshness bound, and the exact sources it is allowed to
                // count. Two assets never share one, because a proposal that
                // configures two assets at once can only be executed as a
                // unit — and an asset whose feed is behind would then hold
                // the other one's activation hostage.
                let governance = self.deployed_id("proxy-governance")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let proposing = matches!(&slot.op, MOp::RealProposeAsset { .. });
                // A proposal's id is whatever governance's counter says at
                // propose time — never the asset's position. A shared
                // governance already holds the first pool's proposals (and
                // served assets shift positions), so the execute coordinate
                // takes the id from the settled propose record's facts.
                let (operation, config, already) = self.real_set_proxy_operation(*asset)?;
                if already {
                    // The runtime already serves this asset with exactly this
                    // configuration. Asking again would clear the asset's
                    // breakers, cache and history — the state the first pool
                    // of this runtime group built — so neither coordinate
                    // proposes or executes, and no proposal id exists to read.
                    return read_only(
                        "this asset is already served by the runtime with this exact configuration; re-registering it would clear its cache, so nothing was submitted",
                        Some(governance),
                        json!({"phase": slot.phase, "slot": slot.slot, "asset": asset}),
                    );
                }
                let id = if proposing {
                    // The id this coordinate froze into its own custody
                    // record when the gate opened. Re-reading the counter
                    // here would let a proposal another client created in the
                    // meantime move this submission to a proposal this run
                    // never made, and would leave the reconciliation with
                    // nothing to compare the network's answer against.
                    Self::frozen_proposal_id(record).ok_or_else(|| {
                        anyhow!(
                            "this propose coordinate froze no proposal id at its gate; \
                             it refuses to submit a proposal it cannot name"
                        )
                    })?
                } else {
                    let propose_slot = self
                        .schedule_owned()
                        .into_iter()
                        .find(|s| matches!(s.op, MOp::RealProposeAsset { asset: a } if a == *asset))
                        .ok_or_else(|| {
                            anyhow!("the frozen graph has no propose coordinate for this asset")
                        })?;
                    let id = self
                        .settled_facts(propose_slot.phase, propose_slot.slot)?
                        .and_then(|facts| {
                            facts.get("proposal").and_then(|value| match value {
                                Value::String(text) => text.parse::<u64>().ok(),
                                other => other.as_u64(),
                            })
                        })
                        .ok_or_else(|| {
                            anyhow!(
                                "the propose coordinate of this asset recorded no proposal id; \
                                 the execution refuses rather than guess one"
                            )
                        })?;
                    // The proposal the propose coordinate recorded must
                    // exist and target this asset; that readback is also
                    // what makes a crash after create_proposal
                    // reconcilable.
                    let stored = self.operator_reader().view(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "get_proposal",
                        &[kv("id", id.to_string())],
                    )?;
                    anyhow::ensure!(
                        stored
                            .get("operation")
                            .and_then(|o| o.get("SetProxy"))
                            .and_then(|a| a.get(0))
                            == operation
                                .get("SetProxy")
                                .and_then(|a| a.get(0)),
                        "the recorded proposal is absent or targets another asset; \
                         the execution refuses before submitting"
                    );
                    id
                };
                let st = self.operator_st(record)?;
                if proposing {
                    must_ok(
                        st.invoke(
                            governance.as_str(),
                            self.ctx.admin_alias(),
                            "create_proposal",
                            &[
                                kv("caller", admin.as_str()),
                                kv("id", id.to_string()),
                                kv("operation", operation.to_string()),
                                // The graph creates and executes in adjacent
                                // coordinates: a timed proposal would be left
                                // undecidable forever.
                                kv("requested_ttl", "0"),
                            ],
                        )?,
                        "create_proposal",
                    )?;
                    return step(
                        "governance holds the proposal that configures this asset's price quorum",
                        Some(governance),
                        json!({"phase": slot.phase, "slot": slot.slot, "asset": asset,
                               "proposal": id.to_string()}),
                    );
                }
                must_ok(
                    st.invoke(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "execute_proposal",
                        &[kv("caller", admin.as_str()), kv("id", id.to_string())],
                    )?,
                    "execute_proposal",
                )?;
                step(
                    "the asset's ProxyConfig is registered on the runtime and reads back as configured",
                    Some(governance),
                    json!({"phase": slot.phase, "slot": slot.slot, "asset": asset,
                           "proposal": id.to_string(), "config": config}),
                )
            }
            // ---- the pool, its backing, and its status ----
            MOp::Upload { role } => {
                // The bytes were re-hashed at preflight against the pin; what
                // this coordinate proves is that the network agrees, by
                // content address, with the artifact that was reviewed.
                let pin = self
                    .cfg
                    .artifacts
                    .get(*role)
                    .ok_or_else(|| anyhow!("the artifact role this upload is pinned to is gone"))?;
                let (pin_path, pin_sha256) = pin.file_location()?;
                let path = expand_home(&pin_path);
                anyhow::ensure!(path.is_file(), "a pinned artifact is missing from disk");
                let bytes = std::fs::read(&path).context("re-reading the artifact to upload")?;
                anyhow::ensure!(
                    sha256_hex(&bytes) == pin_sha256.as_str(),
                    "the bytes about to be uploaded are not the bytes that were reviewed"
                );
                let st = self.operator_st(record)?;
                let hash = st.upload(&path, self.ctx.admin_alias())?;
                anyhow::ensure!(
                    hash.as_str() == pin_sha256.as_str(),
                    "the network returned a Wasm hash other than the pinned one"
                );
                step(
                    "the artifact is on the network under the content address it was pinned to",
                    None,
                    json!({"phase": slot.phase, "slot": slot.slot, "role": *role}),
                )
            }
            MOp::RealRefreshAsset { asset } => {
                // The one coordinate whose proof is a price. A quorum that has
                // never resolved Accepted is reported, not invented: the feeds
                // are pushed by their keeper, outside this graph, and this
                // coordinate proves the runtime serves the asset once what it
                // is given is fresh enough for this asset's own bound.
                let runtime = self.deployed_id("proxy-runtime")?;
                let reserve = self.reserve(*asset)?;
                let asset_key = json!({ "Stellar": reserve.asset }).to_string();
                let st = self.operator_st(record)?;
                let pushed = st.invoke(
                    runtime.as_str(),
                    self.ctx.admin_alias(),
                    "refresh",
                    &[kv("asset", asset_key.clone())],
                )?;
                let cached = st.view(
                    runtime.as_str(),
                    self.ctx.admin_alias(),
                    "get_cached",
                    &[kv("asset", asset_key)],
                )?;
                let accepted = Self::cached_is_accepted(&cached);
                if !(pushed.ok && accepted) {
                    return Ok(Some(MarketStepRecord {
                        phase: slot.phase,
                        action: Safe::literal(slot.action),
                        command: Safe::render_parsed(
                            Policy::Market,
                            &json!({"phase": slot.phase, "slot": slot.slot}),
                            900,
                        ),
                        outcome: Safe::literal(
                            "the asset was refreshed and the runtime reports no Accepted price for it",
                        ),
                        target: Some(runtime),
                        violations: vec![Safe::literal(
                            "no Accepted price: this asset's sources have not been pushed with data fresh enough for its quorum",
                        )],
                    }));
                }
                step(
                    "the asset's configured quorum resolved Accepted and the runtime caches an Accepted price",
                    Some(runtime),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealDeployAdapter => {
                // The adapter is the pool's only window on prices, and it is
                // this pool's own: its constructor binds the runtime it may
                // ask, the owner that may change that, and the unit and scale
                // of the numbers it will hand the pool.
                let artifact = self
                    .artifacts
                    .get("adapter")
                    .ok_or_else(|| anyhow!("the adapter artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled adapter deploy lost its salt role"))?;
                let runtime = self.deployed_id("proxy-runtime")?;
                let governance = self.deployed_id("proxy-governance")?;
                let base = self.price_base();
                let decimals = self.cfg.adapter.decimals;
                let resolution = self.cfg.adapter.resolution;
                let st = self.operator_st(record)?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("deployment_owner", governance.as_str()),
                        kv("parent_oracle", runtime.as_str()),
                        kv("decimals", decimals),
                        kv("resolution", resolution),
                        kv("base", base.to_string()),
                    ],
                )?;
                anyhow::ensure!(
                    id == predicted_target("the adapter")?,
                    "the deployed adapter is not the contract this gate bound"
                );
                self.deployed.insert("adapter".to_string(), id.clone());
                step(
                    "the pool's price adapter exists at its predicted id over this market's runtime",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealVerifyAdapter => {
                let configured = self
                    .pool()?
                    .adapter
                    .as_deref()
                    .ok_or_else(|| anyhow!("this market names no adapter to verify"))?
                    .to_string();
                let id = ContractId::from_str(&configured)
                    .map_err(|_| anyhow!("the configured adapter address is malformed"))?;
                let runtime = self.deployed_id("proxy-runtime")?;
                let config = self
                    .reader()
                    .view(id.as_str(), self.ctx.admin_alias(), "config", &[])?;
                anyhow::ensure!(
                    config.get("parent_oracle").and_then(Value::as_str) == Some(runtime.as_str()),
                    "the configured adapter does not read from this market's runtime"
                );
                self.deployed.insert("adapter".to_string(), id.clone());
                read_only(
                    "the adapter the pool config names reads from this market's runtime; nothing was submitted",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealDeployComet => {
                // The pool's backing pool: the contract that holds the two
                // legs and mints the LP the backstop will be given. Its Wasm
                // is the network's own production code, pinned live — this
                // coordinate deploys an instance, it never uploads a build.
                let artifact = self
                    .artifacts
                    .get("comet")
                    .ok_or_else(|| anyhow!("the comet artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled comet deploy lost its salt role"))?;
                let st = self.operator_st(record)?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[],
                )?;
                anyhow::ensure!(
                    id == predicted_target("the backing pool")?,
                    "the deployed backing pool is not the contract this gate bound"
                );
                self.deployed.insert("comet".to_string(), id.clone());
                step(
                    "the backing pool exists at its predicted id; initialization is its own coordinate",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealInitComet => {
                // Initialization is first-come-first-served, which makes the
                // pre-check the whole safety of this coordinate: a backing
                // pool already owned by this admin is this run's work
                // finished. The production Comet's `__constructor` is inert —
                // what initializes it is the public `init`, which anyone may
                // call first — so a pool answering another controller is not
                // a dead end but an instance someone else claimed between our
                // deploy and this call. The recovery is the deployment this
                // run already knows how to make, under a salt no one else
                // could have used: a fresh instance initialized here becomes
                // the contract the backstop, the factory and the pool are
                // constructed against, because those coordinates come after
                // this one and read the Comet this one leaves bound. The
                // attempt number is a function of this run's nonce alone, so a
                // resume walks the identical sequence and a salt this run
                // already deployed answers with the same contract.
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let tokens = json!([self.sac_id(0)?.as_str(), self.sac_id(1)?.as_str()]);
                let balances =
                    json!([self.leg_seed(0).to_string(), self.leg_seed(1).to_string()]);
                let hash = hash_of(
                    self.artifacts
                        .get("comet")
                        .ok_or_else(|| anyhow!("the comet artifact pin is gone"))?,
                )?;
                for attempt in 0..COMET_CLAIM_ATTEMPTS {
                    let comet = if attempt == 0 {
                        self.deployed_id("comet")?
                    } else {
                        let salt = self.comet_claim_salt(attempt);
                        let claimed = self.operator_st(record)?.deploy(
                            &hash,
                            self.ctx.admin_alias(),
                            Some(&salt),
                            &[],
                        )?;
                        // Whatever comes after this coordinate — the backstop,
                        // the factory, the pool — reads the Comet through this
                        // binding, so the instance someone else claimed is
                        // left behind here and nowhere else.
                        self.deployed.insert("comet".to_string(), claimed.clone());
                        claimed
                    };
                    let claimed_by = self
                        .operator_reader()
                        .try_view(
                            comet.as_str(),
                            self.ctx.admin_alias(),
                            "get_controller",
                            &[],
                        )
                        .ok()
                        .filter(|seen| seen.ok)
                        .and_then(|seen| seen.value);
                    if let Some(owner) = claimed_by {
                        if owner.as_str() != Some(admin.as_str()) {
                            // Someone else holds it. This run does not back a
                            // pool it does not control; it deploys its own.
                            continue;
                        }
                        return read_only(
                            "the backing pool is already initialized by this pool's admin; nothing was submitted",
                            // The settlement binds what its gate bound; a
                            // recovery's fresh instance is recorded in this
                            // coordinate's own facts, which a resume reads
                            // first. A settlement that disagreed with its own
                            // frozen intent would be refused, and rightly.
                            // This coordinate is a config of a contract that
                            // already exists, so its gate froze a `target`
                            // and no `predicted`: the same expression the
                            // reconciliation of an open intent uses.
                            record.target.clone().or_else(|| record.predicted.clone()),
                            json!({"phase": slot.phase, "slot": slot.slot,
                                   "comet": comet.as_str(), "comet_attempt": attempt}),
                        );
                    }
                    must_ok(
                        self.operator_st(record)?.invoke(
                            comet.as_str(),
                            self.ctx.admin_alias(),
                            "init",
                            &[
                                kv("controller", admin.as_str()),
                                kv("tokens", tokens.clone()),
                                // 80/20 is the reviewed allocation for a pool of
                                // this shape; it is the backing's composition, not
                                // the pool's collateral ratio.
                                kv("weights", json!(["8000000", "2000000"])),
                                kv("balances", balances.clone()),
                                kv("swap_fee", 30_000),
                            ],
                        )?,
                        "backing pool initialization",
                    )?;
                    // Both readbacks name the operator's own alias, so both
                    // are asked through the operator's handle: the run-local
                    // keystore holds no `market-admin` identity and answers
                    // nothing for one.
                    let controller = self
                        .operator_reader()
                        .view(comet.as_str(), self.ctx.admin_alias(), "get_controller", &[])?;
                    let listed = self
                        .operator_reader()
                        .view(comet.as_str(), self.ctx.admin_alias(), "get_tokens", &[])?;
                    anyhow::ensure!(
                        controller.as_str() == Some(admin.as_str())
                            && listed.get(0).and_then(Value::as_str)
                                == Some(self.sac_id(0)?.as_str())
                            && listed.get(1).and_then(Value::as_str)
                                == Some(self.sac_id(1)?.as_str()),
                        "the initialized backing pool does not read back this pool's admin and its two legs in order"
                    );
                    return step(
                        "the backing pool holds the two legs in order under this pool's admin, and the seeded balances read back",
                        record.target.clone().or_else(|| record.predicted.clone()),
                        json!({"phase": slot.phase, "slot": slot.slot,
                               "comet": comet.as_str(), "comet_attempt": attempt}),
                    );
                }
                anyhow::bail!(
                    "every backing pool this run deployed was initialized by someone else \
                     before this run could; the run stops rather than backing a pool it does not control"
                )
            }
            MOp::RealDeployEmitter => {
                let artifact = self
                    .artifacts
                    .get("emitter")
                    .ok_or_else(|| anyhow!("the emitter artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled emitter deploy lost its salt role"))?;
                let st = self.operator_st(record)?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[],
                )?;
                anyhow::ensure!(
                    id == predicted_target("the emitter")?,
                    "the deployed emitter is not the contract this gate bound"
                );
                self.deployed.insert("emitter".to_string(), id.clone());
                step(
                    "the event emitter exists at its predicted id; it is constructor metadata and is never called",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealDeployBackstop => {
                // The backstop is deployed before the factory that will know
                // it, against the factory's predicted address — the cycle is
                // closed by prediction, exactly as the lab graph does it, and
                // for the same reason: a deploy that cannot be predicted
                // cannot be gated.
                let artifact = self
                    .artifacts
                    .get("backstop")
                    .ok_or_else(|| anyhow!("the backstop artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled backstop deploy lost its salt role"))?;
                let st = self.operator_st(record)?;
                let factory_prediction = match self.deployed.get("factory") {
                    Some(known) => known.clone(),
                    None => {
                        let factory_salt = salt(
                            self.nonce,
                            market_salt_role(&MOp::RealDeployFactory)
                                .ok_or_else(|| anyhow!("the factory salt role is gone"))?,
                            MARKET_SIDE,
                        );
                        // The operator's keystore, not the run-local reader:
                        // only it holds the admin alias.
                        st.predict_id(self.ctx.admin_alias(), &factory_salt)?
                    }
                };
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("backstop_token", self.deployed_id("comet")?.as_str()),
                        kv("emitter", self.deployed_id("emitter")?.as_str()),
                        kv("blnd_token", self.sac_id(0)?.as_str()),
                        kv("usdc_token", self.sac_id(1)?.as_str()),
                        kv("pool_factory", factory_prediction.as_str()),
                        // Nothing is distributed: this backstop exists to back
                        // this one pool, not to pay a claim.
                        kv("drop_list", json!([])),
                    ],
                )?;
                anyhow::ensure!(
                    id == predicted_target("the backstop")?,
                    "the deployed backstop is not the contract this gate bound"
                );
                self.deployed.insert("backstop".to_string(), id.clone());
                step(
                    "the backstop exists at its predicted id, constructed against the factory's predicted address",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealDeployFactory => {
                let artifact = self
                    .artifacts
                    .get("factory")
                    .ok_or_else(|| anyhow!("the factory artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled factory deploy lost its salt role"))?;
                let meta = json!({
                    "backstop": self.deployed_id("backstop")?.as_str(),
                    "pool_hash": self
                        .artifacts
                        .get("pool")
                        .ok_or_else(|| anyhow!("the pool artifact pin is gone"))?
                        .as_str(),
                    "blnd_id": self.sac_id(0)?.as_str()
                });
                let st = self.operator_st(record)?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[kv("pool_init_meta", meta)],
                )?;
                anyhow::ensure!(
                    id == predicted_target("the factory")?,
                    "the deployed factory is not the contract this gate bound"
                );
                self.deployed.insert("factory".to_string(), id.clone());
                step(
                    "the factory exists at its predicted id with the backstop/factory cycle closed",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealDeployPool => {
                let factory = self
                    .deployed_id("factory")
                    .map_err(|_| anyhow!("the factory deploy has not settled"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled pool deploy lost its salt role"))?;
                let pool = self.pool()?.clone();
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                // The key that deploys a pool *is* its admin until the
                // handover, so the factory is always asked for the operator's
                // own address here — never the configuration's final admin,
                // which the pool's handoff coordinate proposes later. The
                // configuration's admin is checked at preflight, where a
                // missing one costs nothing.
                let oracle = self.deployed_id("adapter")?;
                let st = self.operator_st(record)?;
                let returned = must_ok(
                    st.invoke(
                        factory.as_str(),
                        self.ctx.admin_alias(),
                        "deploy",
                        &[
                            kv("admin", admin.as_str()),
                            kv("name", pool.name.clone()),
                            kv("salt", deploy_salt),
                            kv("oracle", oracle.as_str()),
                            kv("backstop_take_rate", pool.backstop_take_rate),
                            kv("max_positions", pool.max_positions),
                            kv("min_collateral", pool.min_collateral.clone()),
                        ],
                    )?,
                    "factory pool deployment",
                )?;
                let id = ContractId::from_str(
                    returned
                        .as_ref()
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("the factory returned no pool address"))?,
                )
                .map_err(|_| anyhow!("the factory returned a malformed pool address"))?;
                anyhow::ensure!(
                    id == predicted_target("the pool")?,
                    "the deployed pool is not the contract this gate bound"
                );
                let registered = st.view(
                    factory.as_str(),
                    self.ctx.admin_alias(),
                    "is_pool",
                    &[kv("pool_address", id.as_str())],
                )?;
                anyhow::ensure!(
                    registered == Value::Bool(true),
                    "the factory does not recognize the pool it was asked to deploy"
                );
                let config = self.pool_config(&id)?;
                anyhow::ensure!(
                    config.get("oracle").and_then(Value::as_str) == Some(oracle.as_str())
                        && config.get("status").and_then(Value::as_u64) == Some(6)
                        && config
                            .get("bstop_rate")
                            .and_then(Value::as_u64)
                            == Some(u64::from(pool.backstop_take_rate))
                        && config.get("max_positions").and_then(Value::as_u64)
                            == Some(u64::from(pool.max_positions))
                        && config
                            .get("min_collateral")
                            .and_then(price_as_i128)
                            == i128::from_str(&pool.min_collateral).ok(),
                    "the deployed pool does not read back the reviewed parameters"
                );
                self.deployed.insert("pool".to_string(), id.clone());
                step(
                    "the pool exists at its predicted child address with its reviewed parameters and this market's adapter",
                    Some(id),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealQueueReserve { asset } => {
                let pool = self.deployed_id("pool")?;
                let reserve = self.reserve(*asset)?.clone();
                let config = self.pool_config(&pool)?;
                anyhow::ensure!(
                    config.get("status").and_then(Value::as_u64) == Some(6),
                    "reserve initialization is setup-only law and the pool is not in setup status"
                );
                let metadata = self.real_reserve_config(&reserve, *asset)?;
                let st = self.operator_st(record)?;
                must_ok(
                    st.invoke(
                        pool.as_str(),
                        self.ctx.admin_alias(),
                        "queue_set_reserve",
                        &[kv("asset", reserve.asset.clone()), kv("metadata", metadata)],
                    )?,
                    "reserve queueing",
                )?;
                step(
                    "the reserve's reviewed parameters are queued on the pool",
                    Some(pool),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealSetReserve { asset } => {
                let pool = self.deployed_id("pool")?;
                let reserve = self.reserve(*asset)?.clone();
                let config = self.pool_config(&pool)?;
                anyhow::ensure!(
                    config.get("status").and_then(Value::as_u64) == Some(6),
                    "reserve enablement is setup-only law and the pool is not in setup status"
                );
                let st = self.operator_st(record)?;
                let index = must_ok(
                    st.invoke(
                        pool.as_str(),
                        self.ctx.admin_alias(),
                        "set_reserve",
                        &[kv("asset", reserve.asset.clone())],
                    )?,
                    "reserve enablement",
                )?;
                anyhow::ensure!(
                    index.as_ref().and_then(price_as_i128)
                        == Some(i128::try_from(*asset).unwrap_or(-1)),
                    "the enabled reserve did not read back at its configured index"
                );
                let enabled = st.view(
                    pool.as_str(),
                    self.ctx.admin_alias(),
                    "get_reserve",
                    &[kv("asset", reserve.asset.clone())],
                )?;
                let want = self.real_reserve_config(&reserve, *asset)?;
                for field in [
                    "index",
                    "c_factor",
                    "l_factor",
                    "util",
                    "max_util",
                    "r_base",
                    "r_one",
                    "r_two",
                    "r_three",
                    "reactivity",
                ] {
                    anyhow::ensure!(
                        number_at(&enabled, &["config", field]) == number_at(&want, &[field]),
                        "the enabled reserve does not read back its reviewed parameters"
                    );
                }
                step(
                    "the reserve is enabled on the pool and reads back its reviewed parameters",
                    Some(pool),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealBackstopDeposit => {
                // The one value-moving coordinate of a real market's launch,
                // and the only one whose replay would move value twice. Its
                // proof is the backstop's own record of this pool: a backing
                // already at the full LP amount settles the coordinate with no
                // call at all, so neither a lagging read nor a resumed run can
                // deposit the same backing twice.
                let backstop = self.deployed_id("backstop")?;
                let pool = self.deployed_id("pool")?;
                let comet = self.deployed_id("comet")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let held = self.operator_reader().view(
                    comet.as_str(),
                    self.ctx.admin_alias(),
                    "balance",
                    &[kv("id", admin.as_str())],
                )?;
                let lp = price_as_i128(&held)
                    .ok_or_else(|| anyhow!("the backing pool reports no balance for its controller"))?;
                let before = self.backstop_pool_data(&backstop, &pool)?;
                if before.get("tokens").and_then(price_as_i128).unwrap_or(0) < lp {
                    let st = self.operator_st(record)?;
                    must_ok(
                        st.invoke(
                            backstop.as_str(),
                            self.ctx.admin_alias(),
                            "deposit",
                            &[
                                kv("from", admin.as_str()),
                                kv("pool_address", pool.as_str()),
                                kv("amount", lp.to_string()),
                            ],
                        )?,
                        "backstop deposit",
                    )?;
                }
                let after = self.backstop_pool_data(&backstop, &pool)?;
                let tokens = after
                    .get("tokens")
                    .and_then(price_as_i128)
                    .ok_or_else(|| anyhow!("the backstop reports no deposited tokens"))?;
                // The law the seed was derived under (manifest's shim value,
                // `(blnd)^4 * usdc` in whole 7-decimal units): on a fresh
                // deposit the backstop's own q4w_pct is 0 — the runbook
                // requires the threshold to be proven from the reserves,
                // never from that always-zero field.
                let required = i128::try_from(crate::manifest::MARKET_SHIM_MIN_BACKSTOP_VALUE)
                    .unwrap_or(i128::MAX);
                let fourth = after.get("q4w_pct").and_then(price_as_i128);
                // `pool_data` reports 7-decimal stroops; the pool floors each
                // leg to whole tokens before the product
                // (shared/backstop_threshold.rs), and so does this check.
                let whole = |key: &str| after.get(key).and_then(price_as_i128).map(|v| v / 10_000_000);
                let value = match (whole("blnd"), whole("usdc")) {
                    (Some(blnd), Some(usdc)) => blnd
                        .checked_pow(4)
                        .and_then(|b| b.checked_mul(usdc)),
                    _ => None,
                };
                anyhow::ensure!(
                    tokens >= lp
                        && tokens > 0
                        && fourth == Some(0)
                        && value.is_some_and(|v| v >= required),
                    "the deposited backing does not meet the backstop's fourth-power threshold"
                );
                step(
                    "the pool's full LP is held by the backstop and the fourth-power threshold reads back met",
                    Some(backstop),
                    json!({"phase": slot.phase, "slot": slot.slot,
                           "tokens": tokens.to_string(), "deposit": tokens.to_string(),
                           "q4w_pct": fourth.map(|v| v.to_string())}),
                )
            }
            MOp::DropTrust { leg } => {
                let asset = self.leg_asset(*leg)?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                if let Some(line) = self.reader().ledger_trustline(admin.as_str(), &asset)? {
                    // A line may only be removed at a zero balance, which is
                    // only true once both legs are inside the backing pool.
                    // Refusing here is the deposit's ordering, enforced by
                    // the ledger rather than by this graph's trust in it.
                    anyhow::ensure!(
                        line.balance_stroops == 0,
                        "this leg still holds a balance outside the backing pool; its line cannot be removed yet"
                    );
                    let st = self.operator_st(record)?;
                    st.tx_change_trust(&asset, Some(0))?;
                    anyhow::ensure!(
                        self.reader()
                            .ledger_trustline(admin.as_str(), &asset)?
                            .is_none(),
                        "the removed trustline is still on the ledger"
                    );
                }
                read_only(
                    "the operator holds no line in this leg; its only holder is the pool's backing pool",
                    None,
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealFreezeComet => {
                // Freezing the backing pool is what stops the composition of
                // a pool's collateral from being changed by its owner after
                // the backing has been placed. There is no getter for the
                // flag, so the proof is the observable half of it: the reads
                // that must keep working do, and the call is idempotent —
                // setting an already-frozen pool frozen again is the same
                // state, never an error.
                let comet = self.deployed_id("comet")?;
                let st = self.operator_st(record)?;
                must_ok(
                    st.invoke(
                        comet.as_str(),
                        self.ctx.admin_alias(),
                        "set_freeze_status",
                        &[kv("val", true)],
                    )?,
                    "backing pool freeze",
                )?;
                let total = self
                    .reader()
                    .view(comet.as_str(), self.ctx.admin_alias(), "get_total_supply", &[])?;
                anyhow::ensure!(
                    total.is_number() || total.is_string(),
                    "the backing pool's reads do not survive its freeze"
                );
                step(
                    "the backing pool's transfers are frozen and its reads still answer",
                    Some(comet),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            MOp::RealSetStatus { to } => {
                // Activation is the pool's own test, not this tool's: at
                // `set_status(0)` the pool re-checks the backstop threshold
                // for every reserve. A refusal here is the pool saying its
                // backing is not there, and the coordinate stays unresolved
                // with that answer rather than being retried into a lie.
                let pool = self.deployed_id("pool")?;
                let config = self.pool_config(&pool)?;
                if config.get("status").and_then(Value::as_u64) != Some(u64::from(*to)) {
                    let st = self.operator_st(record)?;
                    must_ok(
                        st.invoke(
                            pool.as_str(),
                            self.ctx.admin_alias(),
                            "set_status",
                            &[kv("pool_status", *to)],
                        )?,
                        "pool activation",
                    )?;
                }
                let after = self.pool_config(&pool)?;
                anyhow::ensure!(
                    after.get("status").and_then(Value::as_u64) == Some(u64::from(*to)),
                    "the pool did not read back the status this activation asked for"
                );
                step(
                    "the pool is active: the status it reports is the one the reviewed configuration asked for, which is where the pool re-checks its backstop",
                    Some(pool),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            // ---- phase 11: the handoff ----
            //
            // Each of these asks the chain before it speaks, submits only in
            // the one state where the role has not yet moved, and refuses on
            // a readback that does not show the move. A coordinate whose
            // target is the operator settles as a read-only check: a role
            // cannot be handed to the key that already holds it, and a run
            // that submitted anyway would pay a fee to change nothing.
            MOp::RealHandoffPool => {
                let pool = self.deployed_id("pool")?;
                let handoff = self.handoff()?.clone();
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let reader = self.operator_reader();
                let already = reader
                    .view(pool.as_str(), self.ctx.admin_alias(), "get_admin", &[])
                    .ok()
                    .and_then(|current| current.as_str().map(str::to_string))
                    .is_some_and(|current| current == handoff.pool_admin);
                if admin.as_str() == handoff.pool_admin || already {
                    return read_only(
                        "the pool's admin is already the address this run was told to hand it to; nothing was proposed",
                        Some(pool),
                        json!({"phase": slot.phase, "slot": slot.slot,
                               "pool_admin": handoff.pool_admin}),
                    );
                }
                // A proposal this run already made is not a second proposal:
                // the pool keeps one proposed admin and stamps it with an
                // expiry, so asking again moves a deadline no one asked to
                // move. The temporary entry is the only proof a proposal is
                // open, because no getter reports one.
                if self.pool_proposes_admin(&pool, &handoff.pool_admin)? {
                    return read_only(
                        "the pool already names this run's proposed admin; the curator's acceptance is what remains",
                        Some(pool.clone()),
                        json!({"phase": slot.phase, "slot": slot.slot,
                               "accept": {"contract": pool.as_str(), "method": "accept_admin",
                                          "by": handoff.pool_admin}}),
                    );
                }
                let st = self.operator_st(record)?;
                must_ok(
                    st.invoke(
                        pool.as_str(),
                        self.ctx.admin_alias(),
                        "propose_admin",
                        &[kv("new_admin", handoff.pool_admin.as_str())],
                    )?,
                    "pool admin proposal",
                )?;
                anyhow::ensure!(
                    self.pool_proposes_admin(&pool, &handoff.pool_admin)?,
                    "the pool does not report this run's proposed admin; the proposal is not on the ledger"
                );
                step(
                    "the pool holds a proposal naming its final admin, which that admin accepts within ten days",
                    Some(pool.clone()),
                    json!({"phase": slot.phase, "slot": slot.slot,
                           "accept": {"contract": pool.as_str(), "method": "accept_admin",
                                      "by": handoff.pool_admin}}),
                )
            }
            MOp::RealHandoffLazer { instance } => {
                let source = self.deployed_id(&format!("lazer:{instance}"))?;
                let handoff = self.handoff()?.clone();
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let reader = self.operator_reader();
                let already = reader
                    .view(source.as_str(), self.ctx.admin_alias(), "get_owner", &[])
                    .ok()
                    .and_then(|owner| owner.as_str().map(str::to_string))
                    .is_some_and(|owner| owner == handoff.oracle_admin);
                if admin.as_str() == handoff.oracle_admin || already {
                    return read_only(
                        "this source's owner is already the address this run was told to hand it to; nothing was proposed",
                        Some(source),
                        json!({"phase": slot.phase, "slot": slot.slot,
                               "oracle_admin": handoff.oracle_admin}),
                    );
                }
                // The deadline is the one this coordinate froze at its own
                // gate, never a fresh reading of the ledger: an entry that
                // matches it is this run's own proposal, and one that matches
                // everything except it is someone else's business.
                let live_until_ledger = Self::frozen_live_until(record).ok_or_else(|| {
                    anyhow!("this handoff froze no ownership deadline at its gate")
                })?;
                if self
                    .lazer_proposes_owner(&source, &handoff.oracle_admin, live_until_ledger)?
                {
                    return read_only(
                        "this source already holds this run's pending ownership transfer; the curator's acceptance is what remains",
                        Some(source.clone()),
                        json!({"phase": slot.phase, "slot": slot.slot,
                               "accept": {"contract": source.as_str(), "method": "accept_ownership",
                                          "by": handoff.oracle_admin,
                                          "before_ledger": live_until_ledger}}),
                    );
                }
                let st = self.operator_st(record)?;
                must_ok(
                    st.invoke(
                        source.as_str(),
                        self.ctx.admin_alias(),
                        "transfer_ownership",
                        &[
                            kv("new_owner", handoff.oracle_admin.as_str()),
                            kv("live_until_ledger", live_until_ledger),
                        ],
                    )?,
                    "source ownership transfer",
                )?;
                anyhow::ensure!(
                    self.lazer_proposes_owner(&source, &handoff.oracle_admin, live_until_ledger)?,
                    "the source does not report this run's pending ownership transfer"
                );
                step(
                    "the source holds a transfer of ownership to the oracle administrator, acceptable before its deadline",
                    Some(source.clone()),
                    json!({"phase": slot.phase, "slot": slot.slot,
                           "accept": {"contract": source.as_str(), "method": "accept_ownership",
                                      "by": handoff.oracle_admin,
                                      "before_ledger": live_until_ledger}}),
                )
            }
            MOp::RealProposeRole { grant } | MOp::RealExecuteRole { grant } => {
                // One half of the governance handover is one proposal, exactly
                // as one asset's price law is: a grant and a release executed
                // as a unit would leave the runtime with two administrators or
                // none if the second call were lost.
                let governance = self.deployed_id("proxy-governance")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let proposing = matches!(&slot.op, MOp::RealProposeRole { .. });
                // The account whose role moves, and the state that settles
                // this half: the grant is settled when the oracle
                // administrator holds `Admin`, the release when the operator
                // no longer does.
                let (account, want) = self.role_pair(*grant)?;
                // The pair is skipped on who *receives* the role, never on
                // whose role moves: the release half always moves the
                // operator's own role, so asking about `account` there would
                // skip every release and leave the deployer an administrator.
                if self.handoff()?.oracle_admin == admin.as_str() {
                    // Both halves of the pair are skipped together, and this
                    // is the one case where skipping them is right rather
                    // than lazy: the operator *is* this run's oracle
                    // administrator, so there is no power to move, and taking
                    // the operator's `Admin` away would leave the runtime this
                    // market depends on with no administrator at all.
                    return read_only(
                        "the operator is this run's own oracle administrator, so its governance roles need no transfer",
                        Some(governance),
                        json!({"phase": slot.phase, "slot": slot.slot, "grant": grant}),
                    );
                }
                if self.holds_admin(&governance, &account)? == want {
                    return read_only(
                        if *grant {
                            "the oracle administrator already holds the administrator role; nothing was proposed"
                        } else {
                            "the operator already holds no administrator role; nothing was proposed"
                        },
                        Some(governance),
                        json!({"phase": slot.phase, "slot": slot.slot, "grant": grant}),
                    );
                }
                let operation = self.real_set_role_operation(&account, *grant);
                let id = if proposing {
                    Self::frozen_proposal_id(record).ok_or_else(|| {
                        anyhow!(
                            "this propose coordinate froze no proposal id at its gate; \
                             it refuses to submit a proposal it cannot name"
                        )
                    })?
                } else {
                    let propose_slot = self
                        .schedule_owned()
                        .into_iter()
                        .find(|s| matches!(&s.op, MOp::RealProposeRole { grant: g } if *g == *grant))
                        .ok_or_else(|| {
                            anyhow!("the frozen graph has no propose coordinate for this role change")
                        })?;
                    let id = self
                        .settled_facts(propose_slot.phase, propose_slot.slot)?
                        .and_then(|facts| {
                            facts.get("proposal").and_then(|value| match value {
                                Value::String(text) => text.parse::<u64>().ok(),
                                other => other.as_u64(),
                            })
                        })
                        .ok_or_else(|| {
                            anyhow!(
                                "the propose coordinate of this role change recorded no proposal id; \
                                 the execution refuses rather than guess one"
                            )
                        })?;
                    let stored = self.operator_reader().view(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "get_proposal",
                        &[kv("id", id.to_string())],
                    )?;
                    anyhow::ensure!(
                        stored.get("operation") == Some(&operation),
                        "the recorded proposal is absent or is another run's role change; \
                         the execution refuses before submitting"
                    );
                    id
                };
                if proposing && *grant {
                    // The only refusal in the handover, and the one that must
                    // happen before anything is proposed: a deployer key that
                    // holds a role beyond `Admin` is holding something this
                    // graph never gave it and this graph will not take away,
                    // and a run that quietly left it there would report a
                    // deployer that keeps no power while it keeps some.
                    let held = self.governance_roles(&governance, admin.as_str())?;
                    anyhow::ensure!(
                        held.iter().all(|role| role == "Admin"),
                        "the operator holds {} beyond the administrator role; \
                         the handoff refuses rather than leave an unaccounted role behind",
                        held.iter()
                            .filter(|role| *role != "Admin")
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                let st = self.operator_st(record)?;
                if proposing {
                    must_ok(
                        st.invoke(
                            governance.as_str(),
                            self.ctx.admin_alias(),
                            "create_proposal",
                            &[
                                kv("caller", admin.as_str()),
                                kv("id", id.to_string()),
                                kv("operation", operation.to_string()),
                                kv("requested_ttl", "0"),
                            ],
                        )?,
                        "create_proposal",
                    )?;
                    return step(
                        "governance holds the proposal that moves this administrator role",
                        Some(governance),
                        json!({"phase": slot.phase, "slot": slot.slot, "grant": grant,
                               "proposal": id.to_string()}),
                    );
                }
                must_ok(
                    st.invoke(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "execute_proposal",
                        &[kv("caller", admin.as_str()), kv("id", id.to_string())],
                    )?,
                    "execute_proposal",
                )?;
                anyhow::ensure!(
                    self.holds_admin(&governance, &account)? == want,
                    "governance did not read back the role change this execution submitted"
                );
                if !*grant {
                    // The deployer's own proof, asked separately from the
                    // single-role read above because it is a different claim:
                    // not that one role is gone, but that nothing is left.
                    let held = self.governance_roles(&governance, admin.as_str())?;
                    anyhow::ensure!(
                        held.is_empty(),
                        "the operator still holds roles on this governance: {}",
                        held.join(", ")
                    );
                }
                step(
                    if *grant {
                        "the oracle administrator holds the administrator role this run was deployed to hand over"
                    } else {
                        "the operator holds no role of this governance: the deployer key keeps no power"
                    },
                    Some(governance),
                    json!({"phase": slot.phase, "slot": slot.slot, "grant": grant,
                           "proposal": id.to_string()}),
                )
            }
            MOp::RealHandoffComet => {
                let comet = self.deployed_id("comet")?;
                let handoff = self.handoff()?.clone();
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?
                    .clone();
                let reader = self.operator_reader();
                let controller = reader
                    .view(comet.as_str(), self.ctx.admin_alias(), "get_controller", &[])
                    .ok()
                    .and_then(|current| current.as_str().map(str::to_string));
                if admin.as_str() == handoff.pool_admin || controller.as_deref()
                    == Some(handoff.pool_admin.as_str())
                {
                    return read_only(
                        "the backing pool's controller is already the pool's final admin; nothing was submitted",
                        Some(comet),
                        json!({"phase": slot.phase, "slot": slot.slot}),
                    );
                }
                // Last, and one step: `set_controller` takes effect the way the
                // pool's admin proposal does not, so it goes after every
                // coordinate that still needs the operator's authority.
                let st = self.operator_st(record)?;
                must_ok(
                    st.invoke(
                        comet.as_str(),
                        self.ctx.admin_alias(),
                        "set_controller",
                        &[kv("manager", handoff.pool_admin.as_str())],
                    )?,
                    "backing pool controller",
                )?;
                anyhow::ensure!(
                    self.operator_reader()
                        .view(comet.as_str(), self.ctx.admin_alias(), "get_controller", &[])
                        .ok()
                        .and_then(|current| current.as_str().map(str::to_string))
                        .is_some_and(|current| current == handoff.pool_admin),
                    "the backing pool did not read back the controller this run asked for"
                );
                step(
                    "the backing pool's controller is the pool's final admin",
                    Some(comet),
                    json!({"phase": slot.phase, "slot": slot.slot}),
                )
            }
            // Shared with the lab graph: publication assembles from settled
            // facts and sends nothing to a chain.
            MOp::Publish => Ok(None),
            _ => Err(anyhow!(
                "a real market's graph scheduled an operation neither of its two halves handles"
            )),
        }
    }

    /// The role transfers this run's frozen graph owes, or a refusal naming
    /// that it scheduled none. Every phase-11 coordinate asks this before it
    /// asks the chain, so a coordinate reached by a graph that owes nothing
    /// cannot act on an invented target.
    fn handoff(&self) -> anyhow::Result<&RealHandoff> {
        self.shape
            .as_ref()
            .and_then(|shape| shape.handoff.as_ref())
            .ok_or_else(|| anyhow!("this run's graph scheduled no handoff, so it has no role to hand over"))
    }

    /// The account whose governance role one handover coordinate moves, and
    /// whether the role is expected to be held afterwards. The grant moves the
    /// oracle administrator into `Admin`; the release moves the operator out of
    /// it — the two halves of one transfer, stated once so the execution and
    /// its reconciliation cannot disagree about who they are about to ask for.
    fn role_pair(&self, grant: bool) -> anyhow::Result<(String, bool)> {
        let handoff = self.handoff()?;
        if grant {
            Ok((handoff.oracle_admin.clone(), true))
        } else {
            let operator = self.progress.admin.as_ref().ok_or_else(|| anyhow!("the operator is unresolved"))?;
            Ok((operator.as_str().to_string(), false))
        }
    }

    /// The governance operation one role-handover coordinate proposes: the
    /// same `GovernanceAction::SetRole` a manual grant is made with, named by
    /// the account whose role moves and by whether it is being given or taken.
    fn real_set_role_operation(&self, account: &str, grant: bool) -> Value {
        json!({ "SetRole": [account, "Admin", grant] })
    }

    /// Whether governance reports that one account holds `Admin`. A read that
    /// fails or answers nothing is an error, never "does not hold": the
    /// release half settles on "does not hold", so a failed read reported as
    /// false would end the run saying the deployer keeps no power while it
    /// still administers the runtime.
    fn holds_admin(&self, governance: &ContractId, account: &str) -> anyhow::Result<bool> {
        self.operator_reader()
            .view(
                governance.as_str(),
                self.ctx.admin_alias(),
                "has_role",
                &[kv("account", account), kv("role", "Admin")],
            )?
            .as_bool()
            .ok_or_else(|| anyhow!("governance answered has_role with something other than a boolean"))
    }

    /// Every role governance reports for one account. `get_roles` answers a
    /// vector of the contract's own `Role` enum, which the CLI renders either
    /// as its name or as a single-key object; both are read here, and only a
    /// response that is not a vector at all is refused — a role this tool
    /// cannot enumerate is a role it must not assume it has seen.
    fn governance_roles(&self, governance: &ContractId, account: &str) -> anyhow::Result<Vec<String>> {
        let roles = self.operator_reader().view(
            governance.as_str(),
            self.ctx.admin_alias(),
            "get_roles",
            &[kv("account", account)],
        )?;
        let entries = roles
            .as_array()
            .ok_or_else(|| anyhow!("governance reported its roles in a shape this tool does not read"))?;
        Ok(entries
            .iter()
            .filter_map(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry.as_object().and_then(|object| object.keys().next().map(String::as_str)))
                    .map(str::to_string)
            })
            .collect())
    }

    /// Whether the pool's temporary `PropAdmin` entry names one address as its
    /// proposed admin and has not expired. The entry is the `(Address, u32
    /// expiry)` tuple `set_proposed_admin` writes (pool/src/storage.rs:165,
    /// 293-320); the pool treats a proposal past its expiry as absent even
    /// while anyone keeps the entry alive, so the expiry must still be ahead.
    /// There is no getter that reports a proposal without risking a trap, so
    /// the entry is read raw.
    fn pool_proposes_admin(&self, pool: &ContractId, target: &str) -> anyhow::Result<bool> {
        let reader = self.operator_reader();
        let want = reader.xdr_encode_scval(&json!({"vec": [{"address": target}, {"u32": 0}]}))?;
        let Some(entry) = reader.read_temp(pool.as_str(), RawKey::Symbol("PropAdmin".to_string()))?
        else {
            return Ok(false);
        };
        let Some(expiry) = crate::stellar::xdr_entry_tail_u32(want.as_str(), &entry.val) else {
            return Ok(false);
        };
        Ok(expiry > reader.latest_ledger()?)
    }

    /// Whether one Lazer source's temporary `PendingOwner` entry holds exactly
    /// this run's proposal. The key is the fieldless `OwnableStorageKey`
    /// variant and the value the `PendingTransfer { address,
    /// live_until_ledger }` struct it holds (stellar-access
    /// ownable/storage.rs, role_transfer/storage.rs), which a contracttype
    /// struct encodes as a map of symbol keys in sorted order. Nothing here
    /// is ignored: the deadline compared is the one this coordinate froze at
    /// its own gate.
    fn lazer_proposes_owner(
        &self,
        source: &ContractId,
        target: &str,
        live_until_ledger: u32,
    ) -> anyhow::Result<bool> {
        let reader = self.operator_reader();
        let key = reader.xdr_encode_scval(&json!({"vec": [{"symbol": "PendingOwner"}]}))?;
        let want = reader.xdr_encode_scval(&json!({"map": [
            {"key": {"symbol": "address"}, "val": {"address": target}},
            {"key": {"symbol": "live_until_ledger"}, "val": {"u32": live_until_ledger}}
        ]}))?;
        let Some(entry) = reader.read_temp(
            source.as_str(),
            RawKey::Xdr(key.as_str().to_string()),
        )? else {
            return Ok(false);
        };
        Ok(crate::stellar::xdr_entry_matches(want.as_str(), &entry.val))
    }

    /// The `SetProxy` operation for one real asset, the configuration it
    /// carries, and whether the runtime already serves exactly that. The key a
    /// source is asked for by belongs to the source, not to the asset: a
    /// Lazer instance is keyed by its feed id, and an existing SEP-40
    /// contract by the asset key it advertises.
    fn real_set_proxy_operation(
        &self,
        asset: usize,
    ) -> anyhow::Result<(Value, Value, bool)> {
        use crate::manifest::SourceKind;
        let reserve = self.reserve(asset)?;
        let asset_key = json!({ "Stellar": reserve.asset });
        let mut sources = Vec::with_capacity(reserve.sources.len());
        // The instance numbers come from the one law (`lazer_instances`), so
        // the contract this operation names is always the one the graph
        // scheduled — whatever order the reserve lists its sources in.
        let mut lazer = lazer_instances(reserve);
        for source in &reserve.sources {
            let (oracle, key) = match source.kind {
                SourceKind::Lazer => {
                    let feed = source
                        .feed_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("a Lazer source names no feed id"))?;
                    let (instance, _) = lazer
                        .next()
                        .ok_or_else(|| anyhow!("a Lazer source has no instance of its own"))?;
                    let id = self.deployed_id(&format!("lazer:{instance}"))?;
                    (id, json!({ "Other": feed }))
                }
                SourceKind::Sep40 => {
                    let contract = source
                        .contract
                        .as_deref()
                        .ok_or_else(|| anyhow!("an existing oracle names no contract"))?
                        .to_string();
                    let contract = ContractId::from_str(&contract)
                        .map_err(|_| anyhow!("an existing oracle's address is malformed"))?;
                    let key = source
                        .asset
                        .clone()
                        .ok_or_else(|| anyhow!("an existing oracle names no asset key"))?;
                    (contract, key)
                }
                SourceKind::Mock => {
                    return Err(anyhow!("the lab source is not a price source of a real market"))
                }
            };
            sources.push(json!({"oracle": oracle.as_str(), "asset": key}));
        }
        let config = json!({
            "sources": sources,
            "min_sources": reserve.min_sources,
            "max_age_secs": reserve.max_age_secs,
            "max_clock_drift_secs": reserve.max_clock_drift_secs,
        });
        let served = self.runtime_serves_asset(&asset_key, &config)?;
        Ok((json!({ "SetProxy": [asset_key, config.clone()] }), config, served))
    }

    /// Whether this market's runtime already serves one asset with exactly the
    /// configuration asked for. A runtime that serves it is not asked to
    /// register it again. One that serves it *differently* is not this run's
    /// to correct: re-SetProxy-ing an already-served asset (a shared runtime
    /// already holding e.g. USDC under another quorum) would silently clear
    /// that pool's breakers and history, so a run that shares its runtime
    /// refuses and names the asset instead. Only a run that deploys its own
    /// runtime may propose a configuration for an asset it serves.
    fn runtime_serves_asset(&self, asset_key: &Value, config: &Value) -> anyhow::Result<bool> {
        let shares_runtime = self.shape.as_ref().is_some_and(|shape| !shape.deploy_runtime);
        let runtime = match self.deployed.get("proxy-runtime") {
            Some(id) => id.clone(),
            None => return Ok(false),
        };
        self.runtime_serves_asset_on(runtime.as_str(), shares_runtime, asset_key, config)
    }

    /// Whether a shared runtime already serves every reserve exactly as this
    /// configuration declares it. Such a run proposes nothing to the
    /// runtime's governance: every propose and execute coordinate is
    /// read-only. A reserve with a Lazer source of its own is never served
    /// yet, because this run deploys that source.
    fn shared_runtime_serves_every_reserve(&self, runtime: &str) -> anyhow::Result<bool> {
        use crate::manifest::SourceKind;
        for reserve in &self.pool()?.reserves {
            let mut sources = Vec::with_capacity(reserve.sources.len());
            for source in &reserve.sources {
                let (SourceKind::Sep40, Some(contract), Some(key)) =
                    (&source.kind, source.contract.as_deref(), source.asset.clone())
                else {
                    return Ok(false);
                };
                sources.push(json!({"oracle": contract, "asset": key}));
            }
            let config = json!({
                "sources": sources,
                "min_sources": reserve.min_sources,
                "max_age_secs": reserve.max_age_secs,
                "max_clock_drift_secs": reserve.max_clock_drift_secs,
            });
            let asset_key = json!({ "Stellar": reserve.asset });
            if !self.runtime_serves_asset_on(runtime, true, &asset_key, &config)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn runtime_serves_asset_on(
        &self,
        runtime: &str,
        shares_runtime: bool,
        asset_key: &Value,
        config: &Value,
    ) -> anyhow::Result<bool> {
        let reader = self.operator_reader();
        // Absence is `ok && None`. A failed read (rate limit, timeout, 5xx) is
        // never "absent": on a shared runtime it would re-SetProxy an asset
        // the first pool already serves, so it refuses instead.
        let invocation = match reader.try_view(
            runtime,
            self.ctx.admin_alias(),
            "get_proxy",
            &[kv("asset", asset_key.clone())],
        ) {
            Ok(invocation) if invocation.ok => invocation,
            Ok(_) | Err(_) if shares_runtime => anyhow::bail!(
                "could not read whether the shared proxy runtime already serves {}; \
                 refusing to propose a SetProxy for it",
                asset_key
            ),
            Ok(_) | Err(_) => return Ok(false),
        };
        // `Option<ProxyConfig>` renders its absence as JSON `null`.
        let Some(current) = invocation.value.filter(|value| !value.is_null()) else {
            return Ok(false);
        };
        let served = config_equal(&current, config);
        anyhow::ensure!(
            served || !shares_runtime,
            "the shared proxy runtime already serves {} with a different configuration; \
             this run never re-sets an existing asset",
            asset_key
        );
        Ok(served)
    }

    /// The `ReserveConfig` the pool freezes for one real reserve. Every field
    /// is the reviewed configuration's own — the preflight refused any fixture
    /// default before this point — and `index` is the reserve's position in
    /// the configuration, which is the position the pool seals it at.
    fn real_reserve_config(
        &self,
        reserve: &crate::manifest::MarketReserve,
        index: usize,
    ) -> anyhow::Result<Value> {
        Ok(json!({
            "index": index,
            "decimals": reserve.decimals,
            "c_factor": reserve.c_factor,
            "l_factor": reserve.l_factor,
            "util": reserve.util,
            "max_util": reserve.max_util,
            "supply_cap": reserve.supply_cap,
            "min_position": reserve.min_position,
            "r_base": reserve.r_base,
            "r_one": reserve.r_one,
            "r_two": reserve.r_two,
            "r_three": reserve.r_three,
            "reactivity": reserve.reactivity,
            "enabled": reserve.enabled,
        }))
    }

    /// What the backstop records about one pool: the LP it holds and the
    /// fourth-power figure an activation is judged against.
    fn backstop_pool_data(
        &self,
        backstop: &ContractId,
        pool: &ContractId,
    ) -> anyhow::Result<Value> {
        self.operator_reader().view(
            backstop.as_str(),
            self.ctx.admin_alias(),
            "pool_data",
            &[kv("pool", pool.as_str())],
        )
    }

    /// Whether one classic coordinate of the activation shim has already had
    /// its effect on the ledger. This is the readback that makes a resumed
    /// run safe: it answers from the network's current state, so a coordinate
    /// whose effect is already present settles without submitting anything,
    /// and one whose effect is absent is the only case a send is issued.
    fn classic_effect_present(&self, op: &MOp) -> anyhow::Result<bool> {
        let reader = self.reader();
        let admin = self
            .progress
            .admin
            .as_ref()
            .ok_or_else(|| anyhow!("the operator is unresolved"))?;
        match op {
            // The issuer's key was generated in this run's own keystore
            // before the account was funded, so its address is this run's
            // own knowledge and does not depend on the settlement that is
            // missing: an account present at it is the whole proof. Reading
            // only the settled facts would make an intent left open by a
            // crash after the send permanently unresolvable, because the
            // facts it needs are written by the very settlement absent.
            MOp::CreateIssuer { leg } => {
                let issuer = match self.issuer_of(*leg) {
                    Ok(issuer) => Some(issuer),
                    Err(_) => reader.keys_public_key(&Self::issuer_key(*leg))?,
                };
                let Some(issuer) = issuer else {
                    return Ok(false);
                };
                Ok(reader.ledger_account(issuer.as_str())?.is_some())
            }
            MOp::TrustLeg { leg } => {
                let asset = self.leg_asset(*leg)?;
                Ok(reader.ledger_trustline(admin.as_str(), &asset)?.is_some())
            }
            MOp::MintLeg { leg } => {
                let asset = self.leg_asset(*leg)?;
                Ok(reader
                    .ledger_trustline(admin.as_str(), &asset)?
                    .is_some_and(|line| line.balance_stroops >= self.leg_seed(*leg)))
            }
            MOp::LockIssuer { leg } => {
                let issuer = match self.issuer_of(*leg) {
                    Ok(issuer) => issuer,
                    Err(_) => return Ok(false),
                };
                Ok(reader
                    .ledger_account(issuer.as_str())?
                    .is_some_and(|account| {
                        account.master_weight == 0
                            && account.low == 1
                            && account.med == 1
                            && account.high == 1
                            && account.signers.is_empty()
                    }))
            }
            MOp::DropTrust { leg } => {
                let asset = self.leg_asset(*leg)?;
                Ok(reader.ledger_trustline(admin.as_str(), &asset)?.is_none())
            }
            _ => Ok(false),
        }
    }

    /// Whether a real market's coordinate has had its effect on the network.
    /// This is the reconciliation half of the same readback its execution
    /// half does, so an intent left open by an interrupted run is answered by
    /// the same question, asked the same way, at the same source: the ledger
    /// entry or the contract read that only the completed effect explains.
    ///
    /// What cannot be attributed is left unresolved, deliberately. A
    /// deployment is not a queue to retry: a second mint behind a pool, a
    /// second initialization of a backing pool, or a second proposal counter
    /// advance is not recoverable by asking twice, and the operator's
    /// reconciliation of one open intent is cheaper than any of them.
    fn real_effect_present(
        &self,
        op: &MOp,
        record: &MarketCustodyRecord,
    ) -> anyhow::Result<bool> {
        if matches!(
            op,
            MOp::CreateIssuer { .. }
                | MOp::TrustLeg { .. }
                | MOp::MintLeg { .. }
                | MOp::LockIssuer { .. }
                | MOp::DropTrust { .. }
        ) {
            return self.classic_effect_present(op);
        }
        Ok(match op {
            // The asset contract of a leg is a deterministic function of the
            // asset, so nothing but that asset can hold its address. What
            // proves it is the token's own `name`, asked through the
            // operator's handle: a `get_admin` asked of the run-local
            // keystore answers nothing in a real run, because that keystore
            // never holds the operator's alias.
            MOp::DeploySac { leg } => {
                let asset = self.leg_asset(*leg)?;
                let predicted = self
                    .reader()
                    .asset_contract_id(&asset)
                    .map_err(|_| anyhow!("the leg asset has no derivable contract id"))?;
                self.operator_reader()
                    .try_view(predicted.as_str(), self.ctx.admin_alias(), "name", &[])
                    .is_ok_and(|invocation| invocation.ok)
            }
            // The backing pool was initialized by this run if it names this
            // run's operator as its controller.
            MOp::RealInitComet => {
                let Ok(comet) = self.deployed_id("comet") else {
                    return Ok(false);
                };
                let Some(admin) = self.progress.admin.as_ref() else {
                    return Ok(false);
                };
                self.operator_reader()
                    .view(comet.as_str(), self.ctx.admin_alias(), "get_controller", &[])
                    .ok()
                    .and_then(|controller| controller.as_str().map(String::from))
                    .is_some_and(|controller| controller == admin.as_str())
            }
            // An execute whose effect landed leaves the asset served by
            // exactly the configuration this market asked for.
            MOp::RealExecuteAsset { asset } => self.real_set_proxy_operation(*asset)?.2,
            // A propose whose `create_proposal` landed left a proposal at the
            // id this coordinate froze into its own custody record before it
            // sent, and that proposal carries this asset's `SetProxy`. That
            // readback is the whole proof. The counter advancing proves
            // nothing: every other pool sharing this governance advances it,
            // and any launch pool's second reserve finds it already above
            // zero, so a counter above zero cannot distinguish this run's
            // submission from another client's — or from none.
            MOp::RealProposeAsset { asset } => {
                if self.real_set_proxy_operation(*asset).is_ok_and(|(_, _, served)| served) {
                    return Ok(true);
                }
                let (Ok(governance), Some(expected)) = (
                    self.deployed_id("proxy-governance"),
                    Self::frozen_proposal_id(record),
                ) else {
                    return Ok(false);
                };
                let Ok(operation) = self
                    .real_set_proxy_operation(*asset)
                    .map(|(operation, _, _)| operation)
                else {
                    return Ok(false);
                };
                let Ok(stored) = self.operator_reader().view(
                    governance.as_str(),
                    self.ctx.admin_alias(),
                    "get_proposal",
                    &[kv("id", expected.to_string())],
                ) else {
                    return Ok(false);
                };
                stored
                    .get("operation")
                    .and_then(|operation| operation.get("SetProxy"))
                    .and_then(|sources| sources.get(0))
                    == operation
                        .get("SetProxy")
                        .and_then(|sources| sources.get(0))
            }
            // An enabled reserve reads back at its configured index with its
            // reviewed parameters.
            MOp::RealSetReserve { asset } => {
                let Ok(pool) = self.deployed_id("pool") else {
                    return Ok(false);
                };
                let Ok(reserve) = self.reserve(*asset) else {
                    return Ok(false);
                };
                let Ok(enabled) = self.operator_reader().view(
                    pool.as_str(),
                    self.ctx.admin_alias(),
                    "get_reserve",
                    &[kv("asset", reserve.asset.clone())],
                ) else {
                    return Ok(false);
                };
                let Ok(want) = self.real_reserve_config(reserve, *asset) else {
                    return Ok(false);
                };
                ["index", "c_factor", "l_factor", "util", "max_util", "r_base"]
                    .iter()
                    .all(|field| {
                        number_at(&enabled, &["config", field]) == number_at(&want, &[field])
                    })
            }
            // Activation is a status the pool reports; nothing else sets it.
            MOp::RealSetStatus { to } => {
                let Ok(pool) = self.deployed_id("pool") else {
                    return Ok(false);
                };
                self.pool_config(&pool)
                    .ok()
                    .and_then(|config| config.get("status").and_then(Value::as_u64))
                    == Some(u64::from(*to))
            }
            // The deposit is attributable when the backstop holds the LP this
            // pool's backing minted — a number no other submission explains.
            MOp::RealBackstopDeposit => {
                let (Ok(backstop), Ok(pool), Ok(comet)) = (
                    self.deployed_id("backstop"),
                    self.deployed_id("pool"),
                    self.deployed_id("comet"),
                ) else {
                    return Ok(false);
                };
                let Some(admin) = self.progress.admin.as_ref() else {
                    return Ok(false);
                };
                let Ok(lp) = self.operator_reader().view(
                    comet.as_str(),
                    self.ctx.admin_alias(),
                    "balance",
                    &[kv("id", admin.as_str())],
                ) else {
                    return Ok(false);
                };
                let Some(lp) = price_as_i128(&lp) else {
                    return Ok(false);
                };
                self.backstop_pool_data(&backstop, &pool)
                    .ok()
                    .and_then(|data| data.get("tokens").and_then(price_as_i128))
                    .is_some_and(|held| held >= lp && held > 0)
            }
            // A refresh's effect is not "a refresh was sent": it is the
            // runtime serving this asset. An open intent is settled only when
            // the price the coordinate exists to make servable is actually
            // Accepted — the same predicate its execution half failed on.
            // Anything less leaves it unresolved rather than replayed, which
            // is the one direction this law may err in.
            MOp::RealRefreshAsset { asset } => self.refresh_accepted(*asset)?,
            // The handoff coordinates are idempotent in the same way, so an
            // interrupted one is reconciled against exactly the proof its own
            // execution asks for — never a weaker guess about what the send
            // was meant to do. Each is the same predicate, written once.
            MOp::RealHandoffPool => {
                let (Ok(pool), Ok(handoff)) = (self.deployed_id("pool"), self.handoff()) else {
                    return Ok(false);
                };
                self.operator_reader()
                    .view(pool.as_str(), self.ctx.admin_alias(), "get_admin", &[])
                    .ok()
                    .and_then(|current| current.as_str().map(str::to_string))
                    .is_some_and(|current| current == handoff.pool_admin)
                    || self.pool_proposes_admin(&pool, &handoff.pool_admin)?
            }
            MOp::RealHandoffLazer { instance } => {
                let (Ok(source), Ok(handoff)) = (
                    self.deployed_id(&format!("lazer:{instance}")),
                    self.handoff(),
                ) else {
                    return Ok(false);
                };
                // Without the deadline this coordinate froze there is no way
                // to tell its own transfer from anyone else's, so the intent
                // stays unresolved rather than settled on an address alone.
                let Some(live_until_ledger) = Self::frozen_live_until(record) else {
                    return Ok(false);
                };
                self.operator_reader()
                    .view(source.as_str(), self.ctx.admin_alias(), "get_owner", &[])
                    .ok()
                    .and_then(|owner| owner.as_str().map(str::to_string))
                    .is_some_and(|owner| owner == handoff.oracle_admin)
                    || self
                        .lazer_proposes_owner(&source, &handoff.oracle_admin, live_until_ledger)?
            }
            MOp::RealProposeRole { grant } => {
                let (Ok(governance), Ok((account, want)), Ok(handoff), Some(operator)) = (
                    self.deployed_id("proxy-governance"),
                    self.role_pair(*grant),
                    self.handoff(),
                    self.progress.admin.as_ref(),
                ) else {
                    return Ok(false);
                };
                // The two states its execution settles on without proposing
                // anything settle its reconciliation too: otherwise an intent
                // opened for a step that never needed a proposal would wait
                // forever for one. The execute half checks both again before
                // it reads any proposal id.
                if handoff.oracle_admin == operator.as_str()
                    || self.holds_admin(&governance, &account)? == want
                {
                    return Ok(true);
                }
                // Otherwise the same proof its execution would leave: the
                // governance holds exactly this operation at exactly the id
                // this coordinate froze.
                let Some(id) = Self::frozen_proposal_id(record) else {
                    return Ok(false);
                };
                self.operator_reader()
                    .view(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "get_proposal",
                        &[kv("id", id.to_string())],
                    )
                    .ok()
                    .is_some_and(|stored| {
                        stored.get("operation")
                            == Some(&self.real_set_role_operation(&account, *grant))
                    })
            }
            MOp::RealExecuteRole { grant } => {
                let (Ok(governance), Ok((account, want))) =
                    (self.deployed_id("proxy-governance"), self.role_pair(*grant))
                else {
                    return Ok(false);
                };
                // A failed read is an error, never a settled release.
                self.holds_admin(&governance, &account)? == want
            }
            MOp::RealHandoffComet => {
                let (Ok(comet), Ok(handoff)) = (self.deployed_id("comet"), self.handoff()) else {
                    return Ok(false);
                };
                self.operator_reader()
                    .view(comet.as_str(), self.ctx.admin_alias(), "get_controller", &[])
                    .ok()
                    .and_then(|current| current.as_str().map(str::to_string))
                    .is_some_and(|current| current == handoff.pool_admin)
            }
            // A deterministic deployment carries its predicted id in the
            // frozen intent: the presence readback that settles a lab
            // deployment settles its real twin, so a process loss after the
            // send no longer bricks the run directory.
            // A read-only verify submits nothing and is always re-checkable,
            // so it settles on its kind. Anything else that has not reached an
            // observable state stays unresolved — a guess is not a proof.
            _ => match record.kind {
                // A real coordinate's code is read back through the
                // operator's own context: identity-bearing reads belong to
                // the operator's config directory, never the run-local
                // actor keystore, which cannot resolve the admin alias.
                MarketMutationKind::Deploy => {
                    // The plan's pin is the authority, not the intent's copy:
                    // an intent journaled before digests were bound still names
                    // its code through the scheduled role.
                    let mut record = record.clone();
                    if record.artifact_digest.is_none() {
                        record.artifact_digest = self
                            .schedule()
                            .iter()
                            .find(|slot| &slot.op == op)
                            .and_then(|slot| self.slot_artifact_digest(slot));
                    }
                    self.reconcile_deterministic_deploy(&record, self.operator_st(&record).ok())?
                }
                // A config coordinate has no proof of its own here: the one
                // real config step is the refresh, which settled above on the
                // price it exists to make servable.
                _ => false,
            },
        })
    }

    /// What a finished run's curator still has to sign.
    ///
    /// Reconstructed from the frozen graph, the configuration and the settled
    /// custody records rather than carried in memory: a run that resumes after
    /// its handoff can name the same acceptances as one that never stopped,
    /// and a run that owed no handoff has no list to print at all. Only the
    /// two-step transfers appear — a one-step controller change is settled the
    /// moment it is sent, and a governance role this run executed itself needs
    /// no signature from anyone.
    pub fn handoff_acceptance(&self) -> Option<Value> {
        let handoff = self.shape.as_ref()?.handoff.clone()?;
        let mut accept = Vec::new();
        for slot in self.schedule() {
            // A coordinate that has not settled is not an acceptance the
            // curator can sign yet, and a run that never finished is not a
            // run that should be telling anyone what to sign.
            let settled = self.journal.result(slot.phase, slot.slot).ok()?;
            let result = settled.as_ref()?;
            // An acceptance already performed is not owed: asked of the
            // chain, since a resumed run settles "already accepted" and
            // "still proposed" alike. A failed read keeps the entry — telling
            // a curator to sign what they signed is harmless, leaving out
            // what they owe is not.
            let holds = |contract: &ContractId, getter: &str, who: &str| {
                self.operator_reader()
                    .view(contract.as_str(), self.ctx.admin_alias(), getter, &[])
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .is_some_and(|holder| holder == who)
            };
            match &slot.op {
                MOp::RealHandoffPool => {
                    let pool = self.deployed.get("pool")?;
                    if !holds(pool, "get_admin", &handoff.pool_admin) {
                        accept.push(json!({
                            "contract": pool.as_str(),
                            "method": "accept_admin",
                            "by": handoff.pool_admin,
                        }));
                    }
                }
                MOp::RealHandoffLazer { instance } => {
                    let source = self.deployed.get(&format!("lazer:{instance}"))?;
                    if !holds(source, "get_owner", &handoff.oracle_admin) {
                        accept.push(json!({
                            "contract": source.as_str(),
                            "method": "accept_ownership",
                            "by": handoff.oracle_admin,
                            // The deadline this coordinate froze for its own transfer.
                            "before_ledger": Self::frozen_live_until(&result.custody)?,
                        }));
                    }
                }
                _ => {}
            }
        }
        Some(json!({
            "pool_admin": handoff.pool_admin,
            "oracle_admin": handoff.oracle_admin,
            "accept": accept,
        }))
    }

    /// Complete an open intent on the strength of its readback, recording the
    /// reconciliation as the answer's source so a later reader of the journal
    /// can tell a settled execution from a settled reconciliation.
    fn finish_reconciled(
        &self,
        phase: u32,
        slot: u32,
        record: &MarketCustodyRecord,
        op: Option<&MOp>,
    ) -> anyhow::Result<()> {
        // The facts this coordinate's own execution would have written are
        // written here too. A propose reconciled without its proposal id
        // would settle the coordinate and still leave the execute behind it
        // unable to name a proposal — a settled coordinate the run can never
        // get past.
        let mut facts = json!({"reconciled": true, "phase": phase, "slot": slot});
        if let Some(MOp::RealProposeAsset { .. } | MOp::RealProposeRole { .. }) = op {
            let id = Self::frozen_proposal_id(record).ok_or_else(|| {
                anyhow!("a propose intent carrying no frozen proposal id cannot be reconciled")
            })?;
            facts["proposal"] = json!(id.to_string());
        }
        match op {
            Some(MOp::RealProposeAsset { asset }) => facts["asset"] = json!(asset),
            Some(MOp::RealProposeRole { grant }) => facts["grant"] = json!(grant),
            _ => {}
        }
        let step = MarketStepRecord {
            phase,
            action: record.action.clone(),
            command: Safe::render_parsed(Policy::Market, &facts, 200),
            outcome: Safe::literal(
                "reconciled by the effect its submission alone explains; not replayed",
            ),
            target: record.target.clone().or_else(|| record.predicted.clone()),
            violations: Vec::new(),
        };
        self.journal.finish(
            phase,
            slot,
            &MarketPendingResult {
                custody: record.clone(),
                step,
            },
        )
    }
    /// The contract a scheduled mutation acts on, when it is already known.
    /// Deterministic deployments name their target only once they are
    /// deployed; their custody record is completed with the predicted id
    /// inside the executor, before the deploying call is made.
    ///
    /// Gate preparation binds only from facts already proven, so this is a
    /// lookup against recorded deployments and never observes the chain.
    ///
    /// A refresh runs against the proxy runtime, not the asset's token: the
    /// runtime is the contract that aggregates and caches prices, and the
    /// token names only the asset argument of the call. Binding a refresh to
    /// the token would journal a target that cannot serve the call and submit
    /// the transaction to a contract that cannot apply it.
    fn slot_target(&self, slot: &MSlot) -> Option<ContractId> {
        self.deployed.get(&self.slot_role(slot)?).cloned()
    }

    /// The role key of the contract a scheduled mutation acts on, or `None`
    /// for coordinates that name no contract (the classic shim coordinates,
    /// uploads, key funding, publication). `slot_target` looks it up among
    /// the recorded deployments; the plan listing shows it as the target role.
    fn slot_role(&self, slot: &MSlot) -> Option<String> {
        // A leg's asset contract is a real address once its own coordinate
        // has settled; the classic coordinates of the shim name no contract
        // at all — an issuer account, a trustline, a balance — and their
        // custody is bound by the account and asset named in the
        // configuration and the journal record of the coordinate instead.
        if let MOp::DeploySac { leg } = &slot.op {
            return Some(format!("sac:{leg}"));
        }
        if matches!(
            &slot.op,
            MOp::CreateIssuer { .. }
                | MOp::TrustLeg { .. }
                | MOp::MintLeg { .. }
                | MOp::LockIssuer { .. }
                | MOp::DropTrust { .. }
        ) {
            return None;
        }
        Some(match &slot.op {
            MOp::InitToken { tok } => format!("token:{}", tok.as_str()),
            MOp::InitSource { index } | MOp::SourcePrice { index } => format!("source:{index}"),
            MOp::InitComet | MOp::RealInitComet => "comet".to_string(),
            MOp::Refresh { .. } | MOp::RealRefreshAsset { .. } => "proxy-runtime".to_string(),
            MOp::Propose { .. }
            | MOp::ExecuteProposal { .. }
            | MOp::RealProposeAsset { .. }
            | MOp::RealExecuteAsset { .. } => "proxy-governance".to_string(),
            MOp::QueueReserve { .. }
            | MOp::SetReserve { .. }
            | MOp::RealQueueReserve { .. }
            | MOp::RealSetReserve { .. }
            | MOp::SetStatus { .. }
            | MOp::RealSetStatus { .. }
            | MOp::UpdateStatus
            | MOp::SeedBackstop
            | MOp::SeedPositions => "pool".to_string(),
            // The real deposit is a call on the backstop (`deposit(from,
            // pool_address, amount)`), and its step records the backstop.
            MOp::RealBackstopDeposit => "backstop".to_string(),
            MOp::RealDeployLazer { instance } => format!("lazer:{instance}"),
            MOp::RealVerifyRuntime | MOp::RealDeployRuntime => "proxy-runtime".to_string(),
            MOp::RealDeployGovernance => "proxy-governance".to_string(),
            MOp::RealVerifyAdapter | MOp::RealDeployAdapter => "adapter".to_string(),
            MOp::RealDeployComet => "comet".to_string(),
            MOp::RealDeployEmitter => "emitter".to_string(),
            MOp::RealDeployBackstop => "backstop".to_string(),
            MOp::RealDeployFactory => "factory".to_string(),
            MOp::RealDeployPool => "pool".to_string(),
            MOp::RealFreezeComet => "comet".to_string(),
            // The handoff coordinates act on the contract whose role they
            // move, which is the same contract a sibling coordinate of theirs
            // already acts on.
            MOp::RealHandoffPool => "pool".to_string(),
            MOp::RealHandoffLazer { instance } => format!("lazer:{instance}"),
            MOp::RealProposeRole { .. } | MOp::RealExecuteRole { .. } => {
                "proxy-governance".to_string()
            }
            MOp::RealHandoffComet => "comet".to_string(),
            MOp::Mint { index } => {
                let b = self.cfg.balances.get(*index)?;
                format!("token:{}", b.token.as_str())
            }
            _ => return None,
        })
    }

    /// Execute one confirmed and journaled mutation, returning the settled
    /// step only on a verified outcome.
    ///
    /// A mutation is submitted only where this engine knows the pinned
    /// contract's real interface from its source. Where it does not, the
    /// mutation is refused before the call rather than guessed: a wrong
    /// function name against a live market spends a transaction and leaves a
    /// journal entry claiming an effect the chain never applied. Refusal is a
    /// definite outcome, so no ambiguity can accumulate.
    ///
    /// Settlement reads the chain back. A submission that merely reported
    /// success is not proof: an upload must return the digest it was pinned
    /// to, and a provisioned actor must resolve to an address distinct from
    /// every other actor and from the operator.
    fn run_slot(
        &mut self,
        slot: &MSlot,
        record: &MarketCustodyRecord,
    ) -> anyhow::Result<MarketStepRecord> {
        // A real market's graph is executed by its own half of the executor,
        // which builds its own signing seams per coordinate (a leg's mint is
        // signed by that leg's issuer, not by the operator). Publication is
        // the one coordinate the two graphs share: it returns `None` here and
        // the shared arm below runs unchanged, because assembling a manifest
        // from settled facts depends on no chain and no graph. This is asked
        // before any seam is built, because the seam below is bound to the
        // operator's signer and a leg's mint is not signed by the operator.
        if self.real() {
            if let Some(step) = self.run_real_slot(slot, record)? {
                return Ok(step);
            }
        }
        // The executing net follows the frozen signer: run-local actor
        // provisioning signs from the run-local config; every other
        // mutation is operator-signed and must resolve + sign from the
        // admin config dir.
        let net = match &slot.op {
            MOp::FundKey { .. } => self.ctx.actor_net(),
            _ => self.ctx.admin_net(),
        };
        // SEC003: bind the pinned signer once at the drive chokepoint — the
        // frozen record signer G is the transaction source and the CLI
        // signs with the alias that must own it (never a seed). The alias
        // and pinned G are bound as long-lived values before the market
        // context exists; the CLI seam re-resolves the alias immediately
        // before every mutation and refuses on any mismatch, so a rebound
        // alias can never sign an unpinned key.
        let alias = match &slot.op {
            MOp::FundKey { who } => who.key_name(),
            _ => self.ctx.admin_alias().to_string(),
        };
        let st = Stellar::new(&net, &self.exec, Policy::Market)
            .with_market_signer(&alias, &record.signer);
        let command = Safe::render_parsed(
            Policy::Market,
            &json!({"phase": slot.phase, "slot": slot.slot}),
            200,
        );
        let settled = |outcome: &'static str| MarketStepRecord {
            phase: slot.phase,
            action: Safe::literal(slot.action),
            command: command.clone(),
            outcome: Safe::literal(outcome),
            target: record.target.clone(),
            violations: Vec::new(),
        };
        match &slot.op {
            // A real market's coordinates are executed by `run_real_slot`
            // above. Reaching this arm means one of them appeared in the lab
            // graph, whose schedule never produces one — and the arm is
            // spelled out variant by variant rather than with a wildcard so
            // that a future operation of either graph is refused by the
            // compiler until it is placed in one schedule or the other.
            MOp::CreateIssuer { .. }
            | MOp::TrustLeg { .. }
            | MOp::DeploySac { .. }
            | MOp::MintLeg { .. }
            | MOp::LockIssuer { .. }
            | MOp::DropTrust { .. }
            | MOp::RealDeployLazer { .. }
            | MOp::RealDeployRuntime
            | MOp::RealVerifyRuntime
            | MOp::RealDeployGovernance
            | MOp::RealProposeAsset { .. }
            | MOp::RealExecuteAsset { .. }
            | MOp::RealRefreshAsset { .. }
            | MOp::RealDeployAdapter
            | MOp::RealVerifyAdapter
            | MOp::RealDeployComet
            | MOp::RealInitComet
            | MOp::RealDeployEmitter
            | MOp::RealDeployBackstop
            | MOp::RealDeployFactory
            | MOp::RealDeployPool
            | MOp::RealQueueReserve { .. }
            | MOp::RealSetReserve { .. }
            | MOp::RealBackstopDeposit
            | MOp::RealFreezeComet
            | MOp::RealSetStatus { .. }
            | MOp::RealHandoffPool
            | MOp::RealHandoffLazer { .. }
            | MOp::RealProposeRole { .. }
            | MOp::RealExecuteRole { .. }
            | MOp::RealHandoffComet => Err(anyhow!(
                "a real market's coordinate was scheduled by the lab graph",
            )),
            MOp::FundKey { who } => {
                let addr = st.keys_ensure_funded(&who.key_name())?;
                let mut identities = self.progress.identities.clone();
                identities.insert(*who, addr.clone());
                let mut seen = std::collections::BTreeSet::new();
                for address in identities.values() {
                    anyhow::ensure!(
                        seen.insert(address.as_str().to_string()),
                        "two run-local actor identities resolve to one address; \
                         the market cannot attribute a signer"
                    );
                }
                if let Some(admin) = &self.progress.admin {
                    anyhow::ensure!(
                        !identities.values().any(|a| a == admin),
                        "a run-local actor identity is the operator's own address; \
                         the market cannot attribute a signer"
                    );
                }
                self.progress.identities = identities;
                Ok(settled(
                    "actor identity provisioned and read back distinct from every other signer",
                ))
            }
            MOp::Upload { role } => {
                let pin =
                    self.cfg.artifacts.get(*role).ok_or_else(|| {
                        anyhow!("the artifact role this upload is pinned to is gone")
                    })?;
                let (pin_path, pin_sha256) = pin.file_location()?;
                let path = expand_home(&pin_path);
                let bytes = std::fs::read(&path).context("re-reading the artifact to upload")?;
                anyhow::ensure!(sha256_hex(&bytes) == pin_sha256.as_str(),);
                let hash = st.upload(&path, self.ctx.admin_alias())?;
                anyhow::ensure!(hash.as_str() == pin_sha256.as_str(),);
                Ok(settled(
                    "artifact uploaded and content-addressed to its pin",
                ))
            }
            MOp::DeployToken { tok } => {
                let artifact = self
                    .artifacts
                    .get("token")
                    .ok_or_else(|| anyhow!("the token artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled token deploy lost its salt role"))?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a token deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed token is not the contract this gate bound"
                );
                self.deployed
                    .insert(format!("token:{}", tok.as_str()), id.clone());
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "token deployed at its predicted id; the pinned sep-41 mock takes no constructor arguments",
                    ),
                    target: Some(id),
                    violations: Vec::new(),
                })
            }
            MOp::InitToken { tok } => {
                let id = self.token_id(*tok)?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                let sym = tok.as_str().to_uppercase();
                must_ok(
                    st.invoke(
                        id.as_str(),
                        self.ctx.admin_alias(),
                        "initialize",
                        &[
                            kv("admin", admin.as_str()),
                            kv("decimal", tok.decimals()),
                            kv("name", &sym),
                            kv("symbol", &sym),
                        ],
                    )?,
                    "token initialize",
                )?;
                let decimals = st.view(id.as_str(), self.ctx.admin_alias(), "decimals", &[])?;
                let name = st.view(id.as_str(), self.ctx.admin_alias(), "name", &[])?;
                let symbol = st.view(id.as_str(), self.ctx.admin_alias(), "symbol", &[])?;
                anyhow::ensure!(
                    decimals.as_u64() == Some(u64::from(tok.decimals()))
                        && name.as_str() == Some(sym.as_str())
                        && symbol.as_str() == Some(sym.as_str()),
                    "the initialized token does not read back its scheduled metadata"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "token initialized with the operator as admin; decimals/name/symbol read back exactly",
                    ),
                    target: Some(id),
                    violations: Vec::new(),
                })
            }
            MOp::DeploySource { index } => {
                let artifact = self
                    .artifacts
                    .get("source")
                    .ok_or_else(|| anyhow!("the source artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled source deploy lost its salt role"))?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[kv("owner", admin.as_str())],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a source deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed source is not the contract this gate bound"
                );
                self.deployed.insert(format!("source:{index}"), id.clone());
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "price source deployed at its predicted id with the operator as its owner",
                    ),
                    target: Some(id),
                    violations: Vec::new(),
                })
            }
            MOp::InitSource { index } => {
                let id = self.deployed_id(&format!("source:{index}"))?;
                let base = st.view(id.as_str(), self.ctx.admin_alias(), "base", &[])?;
                let decimals = st.view(id.as_str(), self.ctx.admin_alias(), "decimals", &[])?;
                let resolution = st.view(id.as_str(), self.ctx.admin_alias(), "resolution", &[])?;
                anyhow::ensure!(
                    base == json!({ "Other": MARKET_BASE })
                        && decimals.as_u64() == Some(u64::from(self.cfg.oracle.decimals)),
                    "the deployed source does not read back the base and decimals this market requires"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::render_parsed(
                        Policy::Market,
                        &json!({
                            "read_only": true,
                            "detail": "the pinned source exposes no post-constructor configuration; this coordinate settles on the read-only proof alone. resolution read from the chain:",
                            "resolution": resolution,
                            // A metadata mismatch is reported, not gated: the
                            // source's own resolution is a property of the
                            // pinned code, nothing this coordinate can change,
                            // so refusing here would stop a graph that is
                            // otherwise exactly what it was reviewed as.
                            "resolution_matches_config": resolution.as_u64()
                                == Some(u64::from(MARKET_SOURCE_RESOLUTION)),
                            "reported": if resolution.as_u64() == Some(u64::from(MARKET_SOURCE_RESOLUTION)) {
                                Value::Null
                            } else {
                                json!("the market config declares source resolution 300; the pinned source reports resolution 1 (latest-only feed). metadata mismatch, reported not gated")
                            },
                        }),
                        900,
                    ),
                    target: Some(id),
                    violations: Vec::new(),
                })
            }
            MOp::SourcePrice { index } => {
                let source = self.deployed_id(&format!("source:{index}"))?;
                // Fresh, strictly-increasing, never future-dated: publication
                // time trails the wall clock by two minutes — inside the
                // proxy's 60-second clock-drift bound and the 3600-second
                // freshness bound, and monotonic per asset within this slot.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| anyhow!("the wall clock is behind the epoch"))?
                    .as_secs()
                    .saturating_sub(120);
                for (i, tok) in Tok::ORACLE_ORDER.iter().enumerate() {
                    let asset = json!({ "Stellar": self.token_id(*tok)?.as_str() }).to_string();
                    let timestamp = now + i as u64;
                    let price = fixture_price(*tok);
                    must_ok(
                        st.invoke(
                            source.as_str(),
                            self.ctx.admin_alias(),
                            "set_data",
                            &[
                                kv("asset", asset.clone()),
                                kv("price", price.to_string()),
                                kv("timestamp", timestamp.to_string()),
                            ],
                        )?,
                        "source set_data",
                    )?;
                    let lastprice = st.view(
                        source.as_str(),
                        self.ctx.admin_alias(),
                        "lastprice",
                        &[kv("asset", asset)],
                    )?;
                    anyhow::ensure!(
                        lastprice.get("price").and_then(price_as_i128) == Some(price)
                            && lastprice.get("timestamp").and_then(Value::as_u64)
                                == Some(timestamp),
                        "a published source price does not read back"
                    );
                }
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the source published a fresh, strictly increasing price for every market asset; each published price read back from lastprice",
                    ),
                    target: Some(source),
                    violations: Vec::new(),
                })
            }
            MOp::DeployRuntime => {
                let artifact = self
                    .artifacts
                    .get("proxy-runtime")
                    .ok_or_else(|| anyhow!("the proxy runtime artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled runtime deploy lost its salt role"))?;
                let governance_salt = salt(
                    self.nonce,
                    market_salt_role(&MOp::DeployGovernance)
                        .ok_or_else(|| anyhow!("the governance salt role is gone"))?,
                    MARKET_SIDE,
                );
                let admin_net = self.ctx.admin_net();
                let st_pred = Stellar::new(&admin_net, &self.exec, Policy::Market);
                let predicted_governance =
                    st_pred.predict_id(self.ctx.admin_alias(), &governance_salt)?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("governance", predicted_governance.as_str()),
                        kv("base", json!({ "Other": MARKET_BASE }).to_string()),
                    ],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a runtime deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed runtime is not the contract this gate bound"
                );
                self.deployed
                    .insert("proxy-runtime".to_string(), id.clone());
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "proxy runtime deployed at its predicted id, governance-owned from its first block",
                    ),
                    target: Some(id),
                    violations: Vec::new(),
                })
            }
            MOp::DeployGovernance => {
                let artifact = self
                    .artifacts
                    .get("proxy-governance")
                    .ok_or_else(|| anyhow!("the governance artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled governance deploy lost its salt role"))?;
                let runtime = self.deployed_id("proxy-runtime")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("admin", admin.as_str()),
                        kv("proxy_oracle", runtime.as_str()),
                        kv("initial_uniform_ttl_ns", "0"),
                    ],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a governance deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed governance contract is not the contract this gate bound"
                );
                self.deployed
                    .insert("proxy-governance".to_string(), id.clone());
                let bound = st.view(id.as_str(), self.ctx.admin_alias(), "proxy_oracle", &[])?;
                let admin_role = st.view(
                    id.as_str(),
                    self.ctx.admin_alias(),
                    "has_role",
                    &[kv("account", admin.as_str()), kv("role", "Admin")],
                )?;
                let next_id =
                    st.view(id.as_str(), self.ctx.admin_alias(), "next_proposal_id", &[])?;
                anyhow::ensure!(
                    bound.as_str() == Some(runtime.as_str())
                        && admin_role.as_bool() == Some(true)
                        && next_id.as_u64() == Some(0),
                    "the deployed governance contract does not read back the bound runtime, the operator's admin role, and a fresh proposal counter"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "governance deployed at its predicted id; uniform proposal timelock constructed at zero, as the frozen adjacent execute schedule requires",
                    ),
                    target: Some(id),
                    violations: Vec::new(),
                })
            }
            MOp::Propose { tok } => {
                let governance = self.deployed_id("proxy-governance")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                let expected = Tok::ORACLE_ORDER
                    .iter()
                    .position(|t| *t == *tok)
                    .ok_or_else(|| {
                        anyhow!("a scheduled proposal names an asset outside oracle order")
                    })? as u64;
                let next_id = st.view(
                    governance.as_str(),
                    self.ctx.admin_alias(),
                    "next_proposal_id",
                    &[],
                )?;
                anyhow::ensure!(
                    next_id.as_u64() == Some(expected),
                    "governance's proposal counter is not the id this slot is scheduled to create; the slot refuses rather than renumber the graph"
                );
                let (operation, _config) = self.set_proxy_operation(*tok)?;
                must_ok(
                    st.invoke(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "create_proposal",
                        &[
                            kv("caller", admin.as_str()),
                            kv("id", expected.to_string()),
                            kv("operation", operation.to_string()),
                            kv("requested_ttl", "0"),
                        ],
                    )?,
                    "create_proposal",
                )?;
                let stored = st.view(
                    governance.as_str(),
                    self.ctx.admin_alias(),
                    "get_proposal",
                    &[kv("id", expected.to_string())],
                )?;
                anyhow::ensure!(
                    stored
                        .get("created_by")
                        .and_then(Value::as_str)
                        == Some(admin.as_str())
                        && stored.get("ttl_ns").and_then(Value::as_u64) == Some(0)
                        && stored.get("operation").and_then(|o| o.get("SetProxy")).and_then(|a| a.get(0))
                            == operation.get("SetProxy").and_then(|a| a.get(0)),
                    "the created proposal does not read back as the scheduled SetProxy for this asset with a zero timelock"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the SetProxy proposal for this asset was created, created by the operator, and reads back with a zero timelock",
                    ),
                    target: Some(governance),
                    violations: Vec::new(),
                })
            }
            MOp::ExecuteProposal { id } => {
                let governance = self.deployed_id("proxy-governance")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                let tok = Tok::ORACLE_ORDER.get(*id as usize).ok_or_else(|| {
                    anyhow!("a scheduled execution id has no asset in oracle order")
                })?;
                let (operation, config) = self.set_proxy_operation(*tok)?;
                // Read-only proof before any submission: the proposal must
                // exist, carry the zero timelock construction established,
                // and target exactly the asset this slot schedules. A slot
                // that cannot prove maturity refuses instead of submitting
                // into a timelock trap that would burn its custody forever.
                let stored = st.view(
                    governance.as_str(),
                    self.ctx.admin_alias(),
                    "get_proposal",
                    &[kv("id", id.to_string())],
                )?;
                anyhow::ensure!(
                    stored.get("ttl_ns").and_then(Value::as_u64) == Some(0)
                        && stored
                            .get("operation")
                            .and_then(|o| o.get("SetProxy"))
                            .and_then(|a| a.get(0))
                            == operation.get("SetProxy").and_then(|a| a.get(0)),
                    "proposal is absent or timelocked; the execution refuses before submitting"
                );
                must_ok(
                    st.invoke(
                        governance.as_str(),
                        self.ctx.admin_alias(),
                        "execute_proposal",
                        &[kv("caller", admin.as_str()), kv("id", id.to_string())],
                    )?,
                    "execute_proposal",
                )?;
                let runtime = self.deployed_id("proxy-runtime")?;
                let asset = operation
                    .get("SetProxy")
                    .and_then(|a| a.get(0))
                    .ok_or_else(|| anyhow!("the scheduled operation lost its asset"))?;
                let registered = st.view(
                    runtime.as_str(),
                    self.ctx.admin_alias(),
                    "get_proxy",
                    &[kv("asset", asset.to_string())],
                )?;
                anyhow::ensure!(
                    registered == config,
                    "the runtime does not serve the exact configuration this proposal authorized"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the proposal executed and the runtime now serves the scheduled quorum configuration for this asset",
                    ),
                    target: Some(governance),
                    violations: Vec::new(),
                })
            }
            MOp::Refresh { tok } => {
                let runtime = self.deployed_id("proxy-runtime")?;
                let asset = json!({ "Stellar": self.token_id(*tok)?.as_str() }).to_string();
                let inv = st.invoke(
                    runtime.as_str(),
                    self.ctx.admin_alias(),
                    "refresh",
                    &[kv("asset", asset.clone())],
                )?;
                let accepted = inv.value.as_ref().and_then(|v| v.get("Accepted")).is_some();
                anyhow::ensure!(
                    inv.ok && accepted,
                    "a refresh did not return an Accepted price"
                );
                let cached = st.view(
                    runtime.as_str(),
                    self.ctx.admin_alias(),
                    "get_cached",
                    &[kv("asset", asset)],
                )?;
                anyhow::ensure!(
                    cached
                        .get("status")
                        .and_then(|s| s.get("Accepted"))
                        .is_some(),
                    "the cached price for this asset is not Accepted"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the asset's configured quorum resolved Accepted and the runtime caches an Accepted price",
                    ),
                    target: Some(runtime),
                    violations: Vec::new(),
                })
            }
            MOp::DeployAdapter => {
                let artifact = self
                    .artifacts
                    .get("adapter")
                    .ok_or_else(|| anyhow!("the adapter artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled adapter deploy lost its salt role"))?;
                let runtime = self.deployed_id("proxy-runtime")?;
                let governance = self.deployed_id("proxy-governance")?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("deployment_owner", governance.as_str()),
                        kv("parent_oracle", runtime.as_str()),
                        kv("decimals", self.cfg.adapter.decimals),
                        kv("resolution", self.cfg.adapter.resolution),
                        kv("base", json!({ "Other": MARKET_BASE }).to_string()),
                    ],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("an adapter deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed adapter is not the contract this gate bound"
                );
                self.deployed.insert("adapter".to_string(), id.clone());
                let config = st.view(id.as_str(), self.ctx.admin_alias(), "config", &[])?;
                anyhow::ensure!(
                    config.get("parent_oracle").and_then(Value::as_str) == Some(runtime.as_str())
                        && config.get("decimals").and_then(Value::as_u64)
                            == Some(u64::from(self.cfg.adapter.decimals))
                        && config.get("resolution").and_then(Value::as_u64)
                            == Some(u64::from(self.cfg.adapter.resolution))
                        && config.get("base") == Some(&json!({ "Other": MARKET_BASE })),
                    "the deployed adapter does not read back its immutable configuration"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the pool's oracle adapter deployed at its predicted id and reads back its immutable configuration",
                    ),
                    target: Some(id),
                    violations: Vec::new(),
                })
            }
            MOp::DeployComet => {
                let artifact = self
                    .artifacts
                    .get("comet")
                    .ok_or_else(|| anyhow!("the comet artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled comet deploy lost its salt role"))?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a comet deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed comet is not the contract this gate bound"
                );
                self.deployed.insert("comet".to_string(), id.clone());
                Ok(settled(
                    "comet deployed at its predicted id; initialization follows at its own coordinate",
                ))
            }
            MOp::InitComet => {
                let comet = self.deployed_id("comet")?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                for (tok, amount) in [(Tok::Blnd, 1_000 * SCALAR_7), (Tok::Usdc, 25 * SCALAR_7)] {
                    must_ok(
                        st.invoke(
                            self.token_id(tok)?.as_str(),
                            self.ctx.admin_alias(),
                            "mint",
                            &[kv("to", admin.as_str()), kv("amount", amount)],
                        )?,
                        "controller seed mint",
                    )?;
                }
                must_ok(
                    st.invoke(
                        comet.as_str(),
                        self.ctx.admin_alias(),
                        "init",
                        &[
                            kv("controller", admin.as_str()),
                            kv(
                                "tokens",
                                json!([
                                    self.token_id(Tok::Blnd)?.as_str(),
                                    self.token_id(Tok::Usdc)?.as_str()
                                ]),
                            ),
                            kv("weights", json!(["8000000", "2000000"])),
                            kv(
                                "balances",
                                json!([
                                    (1_000 * SCALAR_7).to_string(),
                                    (25 * SCALAR_7).to_string()
                                ]),
                            ),
                            kv("swap_fee", 30_000),
                        ],
                    )?,
                    "comet init",
                )?;
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the controller's seed balances were minted and the LP pool initialized with the fixture weights and fee",
                    ),
                    target: Some(comet),
                    violations: Vec::new(),
                })
            }
            MOp::DeployEmitter => {
                let artifact = self
                    .artifacts
                    .get("emitter")
                    .ok_or_else(|| anyhow!("the emitter artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled emitter deploy lost its salt role"))?;
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("an emitter deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed emitter is not the contract this gate bound"
                );
                self.deployed.insert("emitter".to_string(), id.clone());
                Ok(settled(
                    "emitter deployed at its predicted id; constructor metadata only, never called",
                ))
            }
            MOp::DeployBackstop => {
                let artifact = self
                    .artifacts
                    .get("backstop")
                    .ok_or_else(|| anyhow!("the backstop artifact pin is gone"))?;
                let deploy_salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled backstop deploy lost its salt role"))?;
                let predicted_factory = self.deployed_id("factory").or_else(|_| {
                    let factory_salt = salt(
                        self.nonce,
                        market_salt_role(&MOp::DeployFactory)
                            .ok_or_else(|| anyhow!("the factory salt role is gone"))?,
                        MARKET_SIDE,
                    );
                    let admin_net = self.ctx.admin_net();
                    let st_pred = Stellar::new(&admin_net, &self.exec, Policy::Market);
                    st_pred.predict_id(self.ctx.admin_alias(), &factory_salt)
                })?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                let frodo =
                    self.progress.identities.get(&Who::Frodo).ok_or_else(|| {
                        anyhow!("the backstop drop-list actor is not provisioned")
                    })?;
                let drop_list = json!([
                    [admin.as_str(), (10_000_000 * SCALAR_7).to_string()],
                    [frodo.as_str(), (30_000_000 * SCALAR_7).to_string()]
                ]);
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&deploy_salt),
                    &[
                        kv("backstop_token", self.deployed_id("comet")?.as_str()),
                        kv("emitter", self.deployed_id("emitter")?.as_str()),
                        kv("blnd_token", self.token_id(Tok::Blnd)?.as_str()),
                        kv("usdc_token", self.token_id(Tok::Usdc)?.as_str()),
                        kv("pool_factory", predicted_factory.as_str()),
                        kv("drop_list", drop_list),
                    ],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a backstop deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed backstop is not the contract this gate bound"
                );
                self.deployed.insert("backstop".to_string(), id.clone());
                Ok(settled(
                    "backstop deployed at its predicted id against the predicted factory address",
                ))
            }
            MOp::DeployFactory => {
                let artifact = self
                    .artifacts
                    .get("factory")
                    .ok_or_else(|| anyhow!("the factory artifact pin is gone"))?;
                let salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled factory deploy lost its salt role"))?;
                let meta = json!({
                    "backstop": self.deployed_id("backstop")?.as_str(),
                    "pool_hash": self
                        .artifacts
                        .get("pool")
                        .ok_or_else(|| anyhow!("the pool artifact pin is gone"))?,
                    "blnd_id": self.token_id(Tok::Blnd)?.as_str()
                });
                let id = st.deploy(
                    &hash_of(artifact)?,
                    self.ctx.admin_alias(),
                    Some(&salt),
                    &[kv("pool_init_meta", meta)],
                )?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a factory deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed factory is not the contract this gate bound"
                );
                self.deployed.insert("factory".to_string(), id.clone());
                Ok(settled(
                    "factory deployed at its predicted id with the backstop/factory cycle closed",
                ))
            }
            MOp::DeployPool => {
                let factory = self
                    .deployed_id("factory")
                    .map_err(|_| anyhow!("the factory deploy has not settled"))?;
                let salt = self
                    .slot_salt(slot)
                    .ok_or_else(|| anyhow!("a scheduled pool deploy lost its salt role"))?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                // Init-args adjudication (ADR-0005, doc/adr): the protected
                // market is a non-pair protected pool, so per the ADR-0002/
                // 0005/0007 freeze its oracle is this market's settled
                // adapter (`pool_oracle()` == the adapter), its backstop
                // take rate is 0 (interest stays with suppliers, matching
                // the pair-side frozen divergence), and max_positions and
                // min_collateral keep the harness-proven pair-side tuple
                // (6 and SCALAR_7) within the factory's bounds
                // (pool_factory.rs:76-90). The name is an honest non-pair
                // label; none of these values are live-discovered.
                let oracle = self.deployed_id("adapter")?;
                let pool_val = must_ok(
                    st.invoke(
                        factory.as_str(),
                        self.ctx.admin_alias(),
                        "deploy",
                        &[
                            kv("admin", admin.as_str()),
                            kv("name", "ProtectedMarket"),
                            kv("salt", salt),
                            kv("oracle", oracle.as_str()),
                            kv("backstop_take_rate", 0),
                            kv("max_positions", 6),
                            kv("min_collateral", SCALAR_7),
                        ],
                    )?,
                    "factory deploy pool",
                )?;
                let id = ContractId::from_str(
                    pool_val
                        .as_ref()
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("the factory deploy returned no address"))?,
                )
                .map_err(|_| anyhow!("the factory returned a malformed pool address"))?;
                let predicted = record
                    .predicted
                    .as_ref()
                    .ok_or_else(|| anyhow!("a pool deploy reached execution unpredicated"))?;
                anyhow::ensure!(
                    &id == predicted,
                    "the deployed pool is not the contract this gate bound"
                );
                let is_pool = st.view(
                    factory.as_str(),
                    self.ctx.admin_alias(),
                    "is_pool",
                    &[kv("pool_address", id.as_str())],
                )?;
                anyhow::ensure!(
                    is_pool == Value::Bool(true),
                    "the factory does not recognize the deployed pool"
                );
                let pool_admin = st.view(id.as_str(), self.ctx.admin_alias(), "get_admin", &[])?;
                anyhow::ensure!(
                    pool_admin.as_str() == Some(admin.as_str()),
                    "the pool admin did not read back as the operator"
                );
                let config = self.pool_config(&id)?;
                anyhow::ensure!(
                    config.get("oracle").and_then(Value::as_str) == Some(oracle.as_str())
                        && config.get("status").and_then(Value::as_u64) == Some(6)
                        && config.get("bstop_rate").and_then(Value::as_u64) == Some(0)
                        && config.get("max_positions").and_then(Value::as_u64) == Some(6)
                        && config.get("min_collateral").and_then(price_as_i128) == Some(SCALAR_7),
                    "the deployed pool's configuration is not the frozen protected-market shape"
                );
                self.deployed.insert("pool".to_string(), id.clone());
                Ok(settled(
                    "pool deployed through the factory at its predicted child id, with admin, oracle and configuration read back",
                ))
            }
            MOp::QueueReserve { tok } => {
                let pool = self.deployed_id("pool")?;
                let index = Tok::RESERVES
                    .iter()
                    .position(|t| *t == *tok)
                    .ok_or_else(|| anyhow!("a scheduled reserve is outside the reserve list"))?
                    as u32;
                let config = self.pool_config(&pool)?;
                anyhow::ensure!(
                    config.get("status").and_then(Value::as_u64) == Some(6),
                    "reserve initialization is setup-only law; the pool is not in setup status"
                );
                must_ok(
                    st.invoke(
                        pool.as_str(),
                        self.ctx.admin_alias(),
                        "queue_set_reserve",
                        &[
                            kv("asset", self.token_id(*tok)?.as_str()),
                            kv(
                                "metadata",
                                reserve_config(*tok, index, self.cfg.reserves.get(tok)),
                            ),
                        ],
                    )?,
                    "queue_set_reserve",
                )?;
                Ok(settled(
                    "the reserve initialization was queued during setup status, with the fixture's parameters and any operator override",
                ))
            }
            MOp::SetReserve { tok } => {
                let pool = self.deployed_id("pool")?;
                let index = Tok::RESERVES
                    .iter()
                    .position(|t| *t == *tok)
                    .ok_or_else(|| anyhow!("a scheduled reserve is outside the reserve list"))?;
                let config = self.pool_config(&pool)?;
                anyhow::ensure!(
                    config.get("status").and_then(Value::as_u64) == Some(6),
                    "reserve enablement is setup-only law; the pool is not in setup status"
                );
                let index_value = must_ok(
                    st.invoke(
                        pool.as_str(),
                        self.ctx.admin_alias(),
                        "set_reserve",
                        &[kv("asset", self.token_id(*tok)?.as_str())],
                    )?,
                    "set_reserve",
                )?;
                anyhow::ensure!(
                    index_value.as_ref().and_then(Value::as_u64) == Some(index as u64),
                    "the reserve did not receive the index this graph schedules"
                );
                self.require_reserve_matches_manifest(&pool, *tok, index as u32)?;
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the reserve was enabled during setup status, returned its scheduled index, and read back equal to the market config",
                    ),
                    target: Some(pool),
                    violations: Vec::new(),
                })
            }
            MOp::SeedBackstop => {
                let pool = self.deployed_id("pool")?;
                let comet = self
                    .deployed_id("comet")
                    .map_err(|_| anyhow!("the comet deploy has not settled"))?;
                let backstop = self
                    .deployed_id("backstop")
                    .map_err(|_| anyhow!("the backstop deploy has not settled"))?;
                let admin = self
                    .progress
                    .admin
                    .as_ref()
                    .ok_or_else(|| anyhow!("the operator is unresolved"))?;
                // Fixture sequence (setup.rs:19-27 and :79, mirrored live by
                // the pair side): mint both LP legs, join the LP pool, then
                // deposit LP into the pool's backstop. Under this engine's
                // signer law every leg is operator-signed: the legs are
                // minted to the operator under the mock-token admin law the
                // adapter-seed coordinate already uses, and the join and
                // deposit are the operator's own value movements. No token
                // approve is needed for the deposit: execute_deposit pulls
                // via a plain SEP-41 transfer under the deposit's own
                // `from.require_auth` (backstop/src/backstop/deposit.rs:19).
                for (tok, amount) in [
                    (Tok::Blnd, 110_000_000 * SCALAR_7),
                    (Tok::Usdc, 2_600_000 * SCALAR_7),
                ] {
                    must_ok(
                        st.invoke(
                            self.token_id(tok)?.as_str(),
                            self.ctx.admin_alias(),
                            "mint",
                            &[kv("to", admin.as_str()), kv("amount", amount)],
                        )?,
                        "lp leg mint",
                    )?;
                }
                must_ok(
                    st.invoke(
                        comet.as_str(),
                        self.ctx.admin_alias(),
                        "join_pool",
                        &[
                            kv("pool_amount_out", 10_000_000 * SCALAR_7),
                            kv(
                                "max_amounts_in",
                                json!([
                                    (110_000_000 * SCALAR_7).to_string(),
                                    (2_600_000 * SCALAR_7).to_string()
                                ]),
                            ),
                            kv("user", admin.as_str()),
                        ],
                    )?,
                    "comet join_pool",
                )?;
                must_ok(
                    st.invoke(
                        backstop.as_str(),
                        self.ctx.admin_alias(),
                        "deposit",
                        &[
                            kv("from", admin.as_str()),
                            kv("pool_address", pool.as_str()),
                            kv("amount", 50_000 * SCALAR_7),
                        ],
                    )?,
                    "backstop deposit",
                )?;
                let data = st.view(
                    backstop.as_str(),
                    self.ctx.admin_alias(),
                    "pool_data",
                    &[kv("pool", pool.as_str())],
                )?;
                anyhow::ensure!(
                    data.get("tokens").and_then(price_as_i128) == Some(50_000 * SCALAR_7),
                    "the pool's backstop did not read back the seeded balance"
                );
                Ok(settled(
                    "the backstop was seeded 50k LP from the operator's own join, and the pool's backstop balance read back",
                ))
            }
            MOp::SetStatus { to } => {
                let pool = self.deployed_id("pool")?;
                let config = self.pool_config(&pool)?;
                let observed = config.get("status").and_then(Value::as_u64);
                anyhow::ensure!(
                    *to == 3 && observed == Some(6),
                    "the graph knows exactly one lawful status transition, setup to on-ice; \
                     the pool's live status is not setup, so this coordinate refuses"
                );
                // Leaving setup seals reserve economics for the pool's life
                // (ADR 0008 restriction 3). Every scheduled reserve must read
                // back equal to the market config before that door closes.
                for (index, tok) in Tok::RESERVES.iter().enumerate() {
                    self.require_reserve_matches_manifest(&pool, *tok, index as u32)?;
                }
                must_ok(
                    st.invoke(
                        pool.as_str(),
                        self.ctx.admin_alias(),
                        "set_status",
                        &[kv("pool_status", *to)],
                    )?,
                    "set_status",
                )?;
                let after = self.pool_config(&pool)?;
                anyhow::ensure!(
                    after.get("status").and_then(Value::as_u64) == Some(u64::from(*to)),
                    "the pool status did not read back as set"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the pool status read back on-ice after the operator set it",
                    ),
                    target: Some(pool),
                    violations: Vec::new(),
                })
            }
            MOp::UpdateStatus => {
                let pool = self.deployed_id("pool")?;
                let config = self.pool_config(&pool)?;
                anyhow::ensure!(
                    config.get("status").and_then(Value::as_u64) == Some(3),
                    "status advance is lawful only from on-ice; the pool's live status is not \
                     on-ice, so this coordinate refuses"
                );
                let status = must_ok(
                    st.invoke(pool.as_str(), self.ctx.admin_alias(), "update_status", &[])?,
                    "update_status",
                )?;
                anyhow::ensure!(
                    status.as_ref().and_then(Value::as_u64) == Some(1),
                    "the status advance did not return active; the backstop threshold is not met"
                );
                let after = self.pool_config(&pool)?;
                anyhow::ensure!(
                    after.get("status").and_then(Value::as_u64) == Some(1),
                    "the pool status did not read back active"
                );
                Ok(MarketStepRecord {
                    phase: slot.phase,
                    action: Safe::literal(slot.action),
                    command: command.clone(),
                    outcome: Safe::literal(
                        "the pool status read back active; the market is live on testnet only",
                    ),
                    target: Some(pool),
                    violations: Vec::new(),
                })
            }
            MOp::Mint { index } => {
                let balance = self
                    .cfg
                    .balances
                    .get(*index)
                    .ok_or_else(|| anyhow!("a scheduled mint names no configured balance"))?;
                let to =
                    self.progress.identities.get(&balance.who).ok_or_else(|| {
                        anyhow!("the mint recipient's identity is not provisioned")
                    })?;
                must_ok(
                    st.invoke(
                        self.token_id(balance.token)?.as_str(),
                        self.ctx.admin_alias(),
                        "mint",
                        &[kv("to", to.as_str()), kv("amount", balance.amount)],
                    )?,
                    "mint",
                )?;
                Ok(settled(
                    "the configured playable balance was minted to a run-local actor",
                ))
            }
            MOp::SeedPositions => Ok(settled(
                "position seeding deliberately not performed: the frozen plan mints only stable \
                 to run-local actors, so no lawful submitter holds the fixture's whale \
                 collateral shape; positions are opened through market play (supply stable, \
                 borrow against it) instead of pre-seeded — adjudicated, not silently skipped",
            )),
            MOp::Publish => {
                // A publication already on disk that reproduces this run's
                // settled facts is complete, and honoring that proof needs no
                // observation at all. It is checked before the manifest is
                // assembled so a resume finishing an interrupted publication
                // never needs the chain. The run directory is single-writer
                // under the market run lock.
                if let Some(published) = self.published_manifest()? {
                    self.manifest = Some(published);
                    return Ok(settled(
                        "the market manifest was already published; the published bytes reproduce this run's state",
                    ));
                }
                let manifest = self.build_manifest()?;
                let text =
                    toml::to_string_pretty(&manifest).context("rendering the market manifest")?;
                let final_path = self.journal.out().join(MARKET_MANIFEST_FILE);
                let tmp = self
                    .journal
                    .out()
                    .join(format!("{MARKET_MANIFEST_FILE}.{}.tmp", std::process::id()));
                use std::io::Write as _;
                {
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&tmp)
                        .context("creating the market manifest staging file")?;
                    file.write_all(text.as_bytes())
                        .context("writing the market manifest")?;
                    file.sync_all().context("syncing the market manifest")?;
                }
                std::fs::rename(&tmp, &final_path).context("publishing the market manifest")?;
                sync_dir(&self.journal.out());
                let published_text = std::fs::read_to_string(&final_path)
                    .context("reading back the published market manifest")?;
                anyhow::ensure!(
                    published_text == text,
                    "the published manifest does not reproduce this run's state"
                );
                let published = MarketManifest::load(self.journal.out().as_path())
                    .context("the published market manifest is unreadable")?;
                self.manifest = Some(published);
                Ok(settled(
                    "the market manifest was published atomically and reads back complete",
                ))
            }
        }
    }

    /// The artifact digest a scheduled mutation is bound to, when it uses one.
    fn slot_artifact_digest(&self, slot: &MSlot) -> Option<Sha256Hex> {
        let role = match &slot.op {
            MOp::Upload { role } => *role,
            MOp::DeployToken { .. } => "token",
            MOp::DeploySource { .. } => "source",
            MOp::DeployGovernance => "proxy-governance",
            MOp::DeployRuntime => "proxy-runtime",
            MOp::DeployAdapter => "adapter",
            MOp::DeployComet | MOp::InitComet => "comet",
            MOp::DeployEmitter => "emitter",
            MOp::DeployBackstop => "backstop",
            MOp::DeployFactory => "factory",
            MOp::DeployPool => "pool",
            // A real market deploys its own bytes for these; a classic
            // asset's contract is the network's own code and declares no
            // digest of ours, so its presence alone is its proof.
            MOp::RealDeployLazer { .. } => crate::manifest::MARKET_LAZER_SOURCE_ROLE,
            MOp::RealDeployRuntime => "proxy-runtime",
            MOp::RealDeployGovernance => "proxy-governance",
            MOp::RealDeployAdapter => "adapter",
            MOp::RealDeployComet => "comet",
            MOp::RealDeployEmitter => "emitter",
            MOp::RealDeployBackstop => "backstop",
            MOp::RealDeployFactory => "factory",
            MOp::RealDeployPool => "pool",
            _ => return None,
        };
        self.artifacts.get(role).cloned()
    }

    /// Completion. Publication itself is the journal coordinate at phase 12
    /// slot 1; this path only ever reports `Done` from a manifest that is
    /// already published and readable in this run directory — otherwise it
    /// refuses terminally rather than emitting an unproven `Done`.
    fn publish(&mut self) -> anyhow::Result<DeployEvent> {
        if let Some(manifest) = self.manifest.clone() {
            self.terminal = Some(DeployEvent::Done(Box::new(manifest)));
            return Ok(self.terminal.clone().unwrap());
        }
        let settled = self.publish_proof_settled()?;
        match MarketManifest::load(self.journal.out().as_path()) {
            Ok(manifest) if settled => {
                self.manifest = Some(manifest.clone());
                self.terminal = Some(DeployEvent::Done(Box::new(manifest)));
            }
            _ => {
                self.terminal = Some(DeployEvent::Failed(Safe::literal(
                    "market completion is unproven: no published manifest is reconcilable with this run's journal",
                )));
            }
        }
        Ok(self.terminal.clone().unwrap())
    }

    /// Whether the journal's phase 12 slot 1 result attests to the
    /// publication, i.e. the manifest coordinate completed and settled.
    fn publish_proof_settled(&self) -> anyhow::Result<bool> {
        Ok(self
            .journal
            .result(12, 1)?
            .is_some_and(|result| result.custody.tool_error.is_none()))
    }

    /// Execute one confirmed mutation. The custody intent is written before
    /// the submission is attempted and settled only on a definite, verified
    /// outcome, so a submission whose fate is unknown is reported as
    /// unresolved custody and never replayed. An already-settled coordinate is
    /// honored as done, and an already-open intent is reported, not re-run.
    ///
    /// A step that returns a record with violations is a definite refusal:
    /// the coordinate's own proof says the network is not in the state the
    /// step exists to establish. It is not settled; its intent is archived so
    /// the next resume replays it, and the caller stops the run. `Ok(None)` is
    /// a settled step, `Ok(Some(violations))` a refusal that stopped it.
    fn execute_gate(&mut self, gate: &Gate) -> anyhow::Result<Option<Vec<Safe>>> {
        if self.journal.result(gate.phase, gate.slot)?.is_some() {
            return Ok(None);
        }
        if let Some(open) = self.journal.read_intent(gate.phase, gate.slot)? {
            return if self.intent_matches_gate(&open, gate) {
                Err(anyhow!(
                    "a market custody intent for this mutation is already open; \
                     it must be reconciled, not replayed"
                ))
            } else {
                Err(anyhow!(
                    "the open custody intent at this coordinate is not the mutation \
                     this token bound; the journal cannot say which submission is in flight"
                ))
            };
        }
        let slot = self
            .schedule_owned()
            .into_iter()
            .find(|s| s.phase == gate.phase && s.slot == gate.slot)
            .ok_or_else(|| anyhow!("the confirmed coordinate is not part of the frozen graph"))?;
        if custody_payload_digest(&gate.record)? != gate.record.payload_digest {
            return Err(anyhow!(
                "the confirmed mutation no longer matches the payload its token bound"
            ));
        }
        // The same network gate preflight runs, immediately before the send:
        // the context must be bound to the network the configuration froze,
        // and the endpoint must report that network's passphrase. Testnet and
        // mainnet are both lawful; anything else is refused here.
        let network = self.cfg.network()?;
        self.ctx.assert_network(network)?;
        Stellar::new(&self.ctx.actor_net(), &self.exec, Policy::Market)
            .assert_rpc_network(network.passphrase())?;
        self.journal.begin(gate.phase, gate.slot, &gate.record)?;
        match self.run_slot(&slot, &gate.record) {
            Ok(step) if step.violations.is_empty() => {
                self.journal.finish(
                    gate.phase,
                    gate.slot,
                    &MarketPendingResult {
                        custody: gate.record.clone(),
                        step,
                    },
                )?;
                Ok(None)
            }
            // The step ran to a definite answer that reports violations, so
            // the network is not in the state this coordinate exists to
            // establish. Settling it would record the refusal as done and let
            // every later coordinate build on it. Its intent is archived
            // instead — a resume replays exactly this coordinate, which is
            // lawful because a rejected step's effect is re-askable (a refresh
            // is the same call to the same contract) — and the run stops.
            Ok(step) => {
                crate::market_history::archive_for_replay(
                    &self.journal.out(),
                    gate.phase,
                    gate.slot,
                    &Self::violation_note(&step.violations),
                )?;
                Ok(Some(step.violations))
            }
            Err(e) => {
                if self.journal.result(gate.phase, gate.slot)?.is_some() {
                    return Ok(None);
                }
                Err(e)
            }
        }
    }

    /// The one rendered note a refused coordinate leaves in its archive: the
    /// violations it reported, verbatim, so an operator reading
    /// `market-unapplied/` sees why the run stopped there.
    fn violation_note(violations: &[Safe]) -> Safe {
        Safe::render_parsed(
            Policy::Market,
            &json!({
                "unapplied": "the step reported violations; nothing after it was attempted",
                "violations": violations.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
            900,
        )
    }

    /// The one predicate for "this runtime serves an Accepted price for this
    /// asset". A refresh's execution half and its resume proof both ask it, so
    /// the step's own success condition and the reconciliation that settles
    /// an interrupted one can never disagree.
    fn cached_is_accepted(cached: &serde_json::Value) -> bool {
        cached.get("status").is_some_and(|status| {
            status.get("Accepted").is_some() || status.as_str() == Some("Accepted")
        })
    }

    /// Whether the runtime this market deployed caches an Accepted price for
    /// one reserve's asset. Asked of the chain, never of the journal: an open
    /// refresh intent is only its own effect when the price it exists to make
    /// servable is actually served.
    fn refresh_accepted(&self, asset: usize) -> anyhow::Result<bool> {
        let (Ok(runtime), Ok(reserve)) = (self.deployed_id("proxy-runtime"), self.reserve(asset))
        else {
            return Ok(false);
        };
        let asset_key = json!({ "Stellar": reserve.asset }).to_string();
        Ok(self
            .operator_reader()
            .view(
                runtime.as_str(),
                self.ctx.admin_alias(),
                "get_cached",
                &[kv("asset", asset_key)],
            )
            .ok()
            .is_some_and(|cached| Self::cached_is_accepted(&cached)))
    }

    /// Whether an open intent is the mutation a token bound. A mismatch means
    /// the journal cannot say which submission is in flight.
    fn intent_matches_gate(&self, open: &MarketCustodyRecord, gate: &Gate) -> bool {
        open.phase == gate.record.phase
            && open.kind == gate.record.kind
            && open.payload_digest == gate.record.payload_digest
            && open.signer == gate.record.signer
            && open.target == gate.record.target
    }

    fn unresolved(&mut self, phase: u32, slot: u32, note: Option<Safe>) -> DeployEvent {
        let gate = self.gate.take();
        if gate
            .as_ref()
            .is_some_and(|g| g.phase == phase && g.slot == slot)
        {
            self.gate = None;
        }
        let note = note.unwrap_or_else(|| {
            Safe::literal(
                "unresolved market custody: a potentially submitted mutation ended without a definite outcome; nothing was replayed",
            )
        });
        self.unresolved = Some(note.clone());
        DeployEvent::Custody {
            unresolved: true,
            note,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stellar::fake::FakeExecutor;
    use crate::stellar::Net;

    #[test]
    fn salts_are_32_bytes_and_distinct() {
        let a = salt(1, 40, 'a');
        let b = salt(1, 40, 'b');
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_ne!(salt(1, 40, 'a'), salt(2, 40, 'a'));
    }

    #[test]
    fn reserve_config_matches_fixture_and_applies_overrides() {
        let stable = reserve_config(Tok::Stable, 0, None);
        assert_eq!(stable["decimals"], 6);
        assert_eq!(stable["c_factor"], 9_000_000);
        let weth = reserve_config(
            Tok::Weth,
            2,
            Some(&ReserveOverride {
                c_factor: Some(1),
                r_two: Some(7_000_000),
                reactivity: Some(5),
                ..Default::default()
            }),
        );
        assert_eq!(weth["c_factor"], 1);
        assert_eq!(weth["supply_cap"], I128_MAX);
        assert_eq!(weth["r_two"], 7_000_000);
        assert_eq!(weth["reactivity"], 5);
        assert_eq!(
            weth["r_base"], 100_000,
            "unset rate fields keep the fixture value"
        );
        assert_eq!(
            weth["min_position"], "0",
            "the pool i128 field is always serialized, as a string like supply_cap"
        );
        let mut keys: Vec<&String> = weth.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "c_factor",
                "decimals",
                "enabled",
                "index",
                "l_factor",
                "max_util",
                "min_position",
                "r_base",
                "r_one",
                "r_three",
                "r_two",
                "reactivity",
                "supply_cap",
                "util",
            ],
            "the map keys must be exactly the fields of pool/src/storage.rs ReserveConfig"
        );
    }

    #[test]
    fn market_deploy_refuses_reserves_with_fixture_defaults() {
        let out =
            std::env::temp_dir().join(format!("abtest-explicit-reserves-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let (_, full) = fixture_market_config(&out);
        full.require_explicit_reserves()
            .expect("a fully explicit config passes the gate");
        let mut partial = full.clone();
        partial.reserves.get_mut(&Tok::Xlm).unwrap().supply_cap = None;
        partial.reserves.get_mut(&Tok::Xlm).unwrap().r_three = None;
        let err = partial
            .require_explicit_reserves()
            .expect_err("a reserve inheriting a cap or rate must refuse")
            .to_string();
        assert!(
            err.contains("'xlm'") && err.contains("supply_cap, r_three"),
            "{err}"
        );
        let mut absent = full;
        absent.reserves.remove(&Tok::Weth);
        assert!(absent.require_explicit_reserves().is_err());
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn request_json_uses_contract_addresses() {
        let c = Contracts {
            stable: "CSTABLE".into(),
            ..Default::default()
        };
        let s = request_json(&[Req::new(req::BORROW, Tok::Stable, 5)], &c);
        assert_eq!(
            s,
            r#"[{"address":"CSTABLE","amount":"5","request_type":4}]"#
        );
    }

    #[test]
    fn identities_reuse_existing_keys() {
        let n = Net {
            rpc_url: "u".into(),
            passphrase: "p".into(),
            config_dir: "/c".into(),
            headers: Vec::new(),
        };
        // Checksum-valid StrKey fixtures: `keys_ensure_funded` now hard-rejects
        // a successful lookup whose stdout is not a valid G-address, so the
        // reuse path must present real ones (checksum-valid zero-key vectors).
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(
                "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
                "",
            ),
            FakeExecutor::fail("not found"),
            FakeExecutor::ok("", "funded"),
            FakeExecutor::ok(
                "GAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHV4",
                "",
            ),
            FakeExecutor::ok(
                "GABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABVCX",
                "",
            ),
            FakeExecutor::ok(
                "GABQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABEQO",
                "",
            ),
        ]);
        let st = Stellar::new(&n, &fx, Policy::Pair);
        let ids = ensure_identities(&st).unwrap();
        assert_eq!(
            ids[&Who::Frodo],
            "GAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHV4"
        );
        let calls = fx.calls.borrow();
        assert!(
            calls[2].contains(&"generate".to_string()) && calls[2].contains(&"--fund".to_string())
        );
        assert!(
            !calls[1].contains(&"--rpc-url".to_string()),
            "public-key takes no network flags"
        );
        assert!(calls[1].contains(&"--config-dir".to_string()));
    }

    /// Every deterministic market deployment must consume a distinct create2
    /// salt, and no market salt may collide with a pair-graph salt: two
    /// contracts created from one salt land at one address, silently.
    #[test]
    fn market_salt_table_is_collision_free() {
        let roles: Vec<u32> = market_schedule()
            .iter()
            .filter_map(|s| market_salt_role(&s.op))
            .collect();
        assert!(!roles.is_empty(), "the graph must deploy something");
        let unique: std::collections::BTreeSet<u32> = roles.iter().copied().collect();
        assert_eq!(
            unique.len(),
            roles.len(),
            "two deterministic deployments share a salt role"
        );
        let triples = market_salt_triples(&market_schedule(), 0x0123_4567_89ab_cdef);
        assert_eq!(
            triples.len(),
            roles.len(),
            "the salt table must cover exactly the deploying operations"
        );
        let unique_triples: std::collections::BTreeSet<(u64, u32, char)> =
            triples.iter().copied().collect();
        assert_eq!(unique_triples.len(), triples.len());
        for (nonce, role, side) in &triples {
            assert_eq!(*side, MARKET_SIDE);
            for pair_role in [10u32, 20, 30, 40, 41, 50, 60] {
                assert_ne!(
                    salt(*nonce, *role, MARKET_SIDE),
                    salt(*nonce, pair_role, 'a'),
                    "market salt collides with a pair salt"
                );
                assert_ne!(
                    salt(*nonce, *role, MARKET_SIDE),
                    salt(*nonce, pair_role, 'b'),
                    "market salt collides with a pair salt"
                );
            }
        }
    }

    /// The no-replay law depends on every scheduled mutation living at a
    /// journal coordinate the resume scan actually inspects. A coordinate
    /// outside that range would let an ambiguous submission be replayed
    /// without ever surfacing as unresolved custody, so it is refused here
    /// rather than discovered on a live market.
    #[test]
    fn every_scheduled_coordinate_is_scanned_and_unique() {
        let schedule = market_schedule();
        assert!(!schedule.is_empty());
        for slot in &schedule {
            assert!(
                market_coordinate_is_scanned(slot.phase, slot.slot),
                "coordinate ({}, {}) would be invisible to the no-replay scan",
                slot.phase,
                slot.slot
            );
        }
        let coordinates: std::collections::BTreeSet<(u32, u32)> =
            schedule.iter().map(|s| (s.phase, s.slot)).collect();
        assert_eq!(
            coordinates.len(),
            schedule.len(),
            "two mutations share one journal coordinate"
        );
    }

    /// The graph's ordering laws. Uploading must precede deploying, the
    /// adapter must exist before the pool that reads price through it, and
    /// the manifest must be published last — after every battery — because a
    /// published manifest is a claim that the market is safe to use.
    #[test]
    fn the_graph_uploads_before_deploying_and_publishes_last() {
        let schedule = market_schedule();
        let position = |wanted: &dyn Fn(&MOp) -> bool| {
            schedule
                .iter()
                .position(|s| wanted(&s.op))
                .unwrap_or(usize::MAX)
        };
        let uploads = position(&|op| matches!(op, MOp::Upload { .. }));
        let deploys = position(&|op| matches!(op, MOp::DeployToken { .. } | MOp::DeployPool));
        assert!(
            uploads < deploys,
            "the graph deploys before any artifact was uploaded"
        );
        let adapter = position(&|op| matches!(op, MOp::DeployAdapter));
        let pool = position(&|op| matches!(op, MOp::DeployPool));
        assert!(
            adapter < pool,
            "the pool is deployed before the adapter that supplies its price"
        );
        let publish = position(&|op| matches!(op, MOp::Publish));
        assert_eq!(
            publish,
            schedule.len() - 1,
            "the manifest must be the last mutation of the graph"
        );
        let cutover = position(&|op| matches!(op, MOp::DeployGovernance));
        assert!(
            cutover < adapter,
            "the adapter was deployed before ownership was established"
        );
    }

    /// A frozen deployment plan is written once, atomically, and never
    /// rewritten. If a halted run could re-plan itself, the record of what
    /// was authorized would no longer describe what was done.
    #[test]
    fn a_frozen_plan_is_written_once_and_never_rewritten() {
        let dir = std::env::temp_dir().join(format!("abtest-plan-law-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let plan = DeploymentPlan {
            schema: 1,
            config_digest: "a".repeat(64),
            nonce: "7".to_string(),
            network_label: MARKET_NETWORK_LABEL.to_string(),
            passphrase: MARKET_TESTNET_PASSPHRASE.to_string(),
            admin: String::new(),
            artifacts: BTreeMap::new(),
            releases: BTreeMap::new(),
            salt_roles: vec![221, 222, 223],
            slots: vec![(1, 1), (2, 1)],
            uploads: Vec::new(),
        };
        write_deployment_plan(&dir, &plan).expect("the plan is written atomically");
        assert_eq!(
            read_deployment_plan(&dir).expect("readable").as_ref(),
            Some(&plan),
            "the plan a resume reads must be the plan that was frozen"
        );
        let mut drifted = plan.clone();
        drifted.nonce = "8".to_string();
        assert!(
            write_deployment_plan(&dir, &drifted).is_err(),
            "a frozen plan must be abandoned, never quietly re-planned"
        );
        assert_eq!(
            read_deployment_plan(&dir).expect("readable").as_ref(),
            Some(&plan),
            "the rejected re-plan must not have touched the frozen plan"
        );
        // A run dir written before release resolution — a plan without the
        // `releases` table — still loads, unchanged.
        std::fs::write(
            dir.join(MARKET_PLAN_FILE),
            "schema = 1\nconfig_digest = \"aaaa\"\nnonce = \"7\"\nnetwork_label = \"testnet\"\npassphrase = \"Test SDF Network ; September 2015\"\nadmin = \"\"\nartifacts = {}\nsalt_roles = [221, 222, 223]\nslots = [[1, 1], [2, 1]]\n",
        )
        .expect("legacy plan planted");
        let legacy = read_deployment_plan(&dir)
            .expect("a plan without the releases table is still readable")
            .expect("legacy plan present");
        assert_eq!(legacy.nonce, "7");
        assert!(
            legacy.releases.is_empty(),
            "an old plan resolves no releases"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resumed run re-verifies the bytes of the row it froze, so a frozen
    /// provenance has to survive the plan file exactly as it was written.
    #[test]
    fn a_frozen_plan_round_trips_its_release_provenance() {
        let dir = std::env::temp_dir().join(format!("abtest-plan-releases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let provenance = crate::release::Provenance {
            artifact: "soroban-proxy-oracle".to_string(),
            version: "0.1.2".to_string(),
            tag: "templar-proxy-oracle-soroban-contract-v0.1.2".to_string(),
            asset: "templar_proxy_oracle_soroban_contract-0.1.2.wasm".to_string(),
            sha256: Sha256Hex::try_from("a".repeat(64)).unwrap(),
            length: 508_680,
            catalog_commit: "b".repeat(40),
        };
        let plan = DeploymentPlan {
            schema: 1,
            config_digest: "a".repeat(64),
            nonce: "7".to_string(),
            network_label: MARKET_NETWORK_LABEL.to_string(),
            passphrase: MARKET_TESTNET_PASSPHRASE.to_string(),
            admin: String::new(),
            artifacts: BTreeMap::new(),
            releases: BTreeMap::from([("proxy-runtime".to_string(), provenance.clone())]),
            salt_roles: vec![221, 222, 223],
            slots: vec![(1, 1), (2, 1)],
            uploads: Vec::new(),
        };
        write_deployment_plan(&dir, &plan).expect("the plan is written");
        let read = read_deployment_plan(&dir)
            .expect("readable")
            .expect("plan present");
        assert_eq!(
            read, plan,
            "the frozen provenance a resume re-verifies must be the row that was frozen"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The crash case the upload arm exists for: an intent was written, the
    /// process died before anything was submitted, and the pinned artifact is
    /// still exactly what it was. Local evidence is identical whether or not
    /// the upload ever landed, which is why an open upload intent is never
    /// settled on inspection — a coordinate marked complete is never uploaded,
    /// and the graph would deploy contracts whose code the chain never got.
    #[test]
    fn an_intact_artifact_is_not_evidence_that_an_upload_happened() {
        let bytes = b"pinned artifact bytes";
        let declared: Sha256Hex = sha256_hex(bytes).try_into().unwrap();
        // Nothing was submitted and the bytes never changed. A predicate that
        // treated this match as settlement would close the coordinate falsely.
        assert!(!MarketDeployer::artifact_drifts_from_intent(
            bytes, &declared
        ));
        // Drift is a different emergency, and it is detected.
        assert!(MarketDeployer::artifact_drifts_from_intent(
            b"swapped bytes",
            &declared
        ));
    }

    fn hex_str(bytes: &[u8]) -> String {
        let mut text = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(text, "{b:02x}");
        }
        text
    }

    /// Keccak256 known answers. The "abc" vector is the ORIGINAL Keccak
    /// digest (4e03657a...12d6c45), not NIST SHA3-256 (...0724ddd): the
    /// pool factory's on-chain keccak256 uses the pre-standardization pad,
    /// and a SHA3 implementation would silently mispredict every child id.
    #[test]
    fn keccak256_known_answers_gate_the_helper() {
        // Primitive isolation probes first: the CRC-16/XMODEM check value
        // and the all-zero strkey vectors pin crc16_xmodem, base32 and the
        // little-endian checksum order before any derivation is trusted.
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
        assert_eq!(
            strkey_encode(STRKEY_G, &[0u8; 32]),
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
        );
        assert_eq!(
            strkey_encode(STRKEY_C, &[0u8; 32]),
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
        );
        assert_eq!(
            hex_str(&keccak256(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
        assert_eq!(
            hex_str(&keccak256(b"abc")),
            "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45"
        );
    }

    /// The frozen testnet network id, bound from the passphrase constant so
    /// a wrong network cannot be quietly substituted into any preimage.
    #[test]
    fn the_market_network_id_is_the_frozen_testnet_id() {
        assert_eq!(
            hex_str(&market_network_id(MarketNetwork::Testnet)),
            "cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472"
        );
    }

    /// Strkey round-trips and checksum discrimination, including the
    /// harness factory contract id used by the derivation vectors below.
    #[test]
    fn strkeys_round_trip_and_reject_corruption() {
        let operator = "GCNQ4ZLK45KOYNIQLIYI7GT6TYWGK2OZEVZEY7A3W5L7QSPBNCBWSJAQ";
        let (version, payload) = strkey_decode(operator).expect("operator strkey");
        assert_eq!(version, STRKEY_G);
        assert_eq!(payload.len(), 32);
        assert_eq!(strkey_encode(STRKEY_G, &payload), operator);
        let harness_factory = "face000000000000000000000000000000000000000000000000000000000001";
        let factory = hex32(harness_factory).expect("harness factory bytes");
        assert_eq!(
            strkey_encode(STRKEY_C, &factory),
            "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53"
        );
        let (v, back) = strkey_decode("CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53")
            .expect("harness factory strkey");
        assert_eq!((v, back.as_slice()), (STRKEY_C, factory.as_slice()));
        // A flipped payload byte must fail the checksum, not decode.
        let mut broken = operator.as_bytes().to_vec();
        broken[7] = if broken[7] == b'2' { b'3' } else { b'2' };
        let broken = String::from_utf8(broken).expect("ascii");
        assert_ne!(broken, operator);
        if let Some((_, p)) = strkey_decode(&broken) {
            assert_ne!(p, payload, "a corrupted strkey must not decode valid");
        }
    }

    /// End-to-end pool-child proof, both pool-proof vectors: the child salt
    /// is keccak256(salt || admin56), and the predicted id is
    /// sha256(0x00000008 || XDR(ContractId, network, Contract(factory),
    /// child-salt)) — never the raw deploy salt.
    #[test]
    fn factory_child_prediction_reproduces_the_pinned_pool_vectors() {
        let admin = "GCNQ4ZLK45KOYNIQLIYI7GT6TYWGK2OZEVZEY7A3W5L7QSPBNCBWSJAQ";
        // Sample vector (pool-proof [sample]).
        let sample_salt = "112233445566778899aabbccddeeff000102030405060708090a0b0c0d0e0f10";
        assert_eq!(
            hex_str(&pool_factory_child_salt(sample_salt, admin).expect("sample child salt")),
            "317b77f579a309a7e1e9ae862848be7de63812de9c710e92fd585595ddb116c7"
        );
        assert_eq!(
            predict_pool_child(
                MarketNetwork::Testnet,
                "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53",
                sample_salt,
                admin
            )
            .expect("sample prediction"),
            "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY"
        );
        // Runtime-fixture vector (pool-proof [runtime]).
        let runtime_salt = "d8".repeat(32);
        assert_eq!(
            hex_str(&pool_factory_child_salt(&runtime_salt, admin).expect("runtime child salt")),
            "e0043250bd4781585e624ee079b1e8d748b4158705d5b0fbeb4414d90d5237a5"
        );
        let runtime_factory = hex32(&"f8".repeat(32)).expect("runtime factory");
        assert_eq!(
            predict_contract_child(
                &market_network_id(MarketNetwork::Testnet),
                &runtime_factory,
                &pool_factory_child_salt(&runtime_salt, admin).expect("runtime child salt")
            ),
            hex32("433c3ae177e4eda53d233ae0d7969d7b362f86d2b7afcf12bf35ab2f9f0fa356")
                .expect("runtime child id")
        );
        assert_eq!(
            strkey_encode(
                STRKEY_C,
                &predict_contract_child(
                    &market_network_id(MarketNetwork::Testnet),
                    &runtime_factory,
                    &pool_factory_child_salt(&runtime_salt, admin).expect("runtime child salt")
                )
            ),
            "CBBTYOXBO7SO3JJ5EM5OBV4WTV5TML4G2K327TYSX422WL47B6RVMJRI"
        );
    }

    /// The frozen mainnet network id and the pool-child prediction under it.
    /// The network id is sha256 of the mainnet passphrase, computed
    /// independently of this code; the child id is that id mixed into the
    /// same HashIdPreimage the pinned testnet vectors prove, and the
    /// rendering is the strkey law those vectors prove. A wrong network
    /// cannot be quietly substituted into any mainnet preimage.
    #[test]
    fn the_market_network_id_is_the_frozen_mainnet_id() {
        assert_eq!(
            hex_str(&market_network_id(MarketNetwork::Mainnet)),
            "7ac33997544e3175d266bd022439b22cdb16508c01163f26e5cb2a3e1045a979"
        );
        let admin = "GCNQ4ZLK45KOYNIQLIYI7GT6TYWGK2OZEVZEY7A3W5L7QSPBNCBWSJAQ";
        let sample_salt = "112233445566778899aabbccddeeff000102030405060708090a0b0c0d0e0f10";
        assert_eq!(
            predict_pool_child(
                MarketNetwork::Mainnet,
                "CD5M4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACW53",
                sample_salt,
                admin
            )
            .expect("mainnet prediction"),
            "CAXUPBJKNEQFPEOK625AMJM3JPKVL7OJN6EJBD4XAOCSCVBO57AA7EUE"
        );
    }

    /// The settlement trust law for every deployment decision: a
    /// fabricated, corrupt, mismatched, or symlinked result is refused as a
    /// question, never honored as a completed slot, and the action gate
    /// stays closed over it. A legitimate settlement remains readable so a
    /// real resume still skips its completed coordinate.
    #[test]
    fn fabricated_settlements_cannot_complete_a_deploy_coordinate() {
        let out =
            std::env::temp_dir().join(format!("abtest-deploy-fabricated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let journal = MarketJournal::open(&out);
        let signer: AccountId = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            .parse()
            .expect("fixture signer");
        let target: ContractId = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT"
            .parse()
            .expect("fixture target");
        let mut custody = MarketCustodyRecord {
            phase: 1,
            kind: MarketMutationKind::Deploy,
            action: Safe::literal("deploy-pool"),
            network: MarketNetwork::Testnet,
            signer,
            payload_digest: Sha256Hex::default(),
            target: Some(target),
            artifact_digest: None,
            predicted: None,
            pre_state: Vec::new(),
            tool_error: None,
        };
        custody.payload_digest = custody_payload_digest(&custody).expect("fixture digest");
        assert!(journal
            .read_intent(1, 0)
            .expect("absent intent is readable")
            .is_none());
        assert!(journal
            .result(1, 0)
            .expect("absent result is readable")
            .is_none());
        begin_market_pending(&out, 1, 0, &custody).expect("reserve fixture intent");
        let step = MarketStepRecord {
            phase: 1,
            action: custody.action.clone(),
            command: Safe::literal("test settlement"),
            outcome: Safe::literal("settled"),
            target: custody.target.clone(),
            violations: Vec::new(),
        };
        journal
            .finish(
                1,
                0,
                &MarketPendingResult {
                    custody: custody.clone(),
                    step: step.clone(),
                },
            )
            .expect("settle fixture");
        assert!(
            journal
                .result(1, 0)
                .expect("a legitimate settlement must read as settled")
                .is_some(),
            "a valid settlement still lets the resume skip its completed slot"
        );
        assert!(
            crate::market_status::journal_clear_for_action(&out),
            "a legitimate settlement clears the action gate so a real run can proceed"
        );
        let coordinate = out
            .join("market-pending")
            .join("001-00")
            .join("result.json");
        std::fs::write(&coordinate, b"{").expect("corrupt settlement");
        assert!(
            journal.result(1, 0).is_err(),
            "a malformed settlement is a custody question, never a completed slot"
        );
        assert!(!crate::market_status::journal_clear_for_action(&out));
        std::fs::write(
            &coordinate,
            serde_json::to_vec(&MarketPendingResult {
                custody: MarketCustodyRecord {
                    action: Safe::literal("forged-pool"),
                    ..custody.clone()
                },
                step: step.clone(),
            })
            .expect("forged bytes"),
        )
        .expect("plant forged settlement");
        assert!(
            journal.result(1, 0).is_err(),
            "a settlement carrying another custody record cannot pass as this coordinate's"
        );
        assert!(!crate::market_status::journal_clear_for_action(&out));
        std::fs::remove_file(&coordinate).expect("remove planted settlement");
        std::os::unix::fs::symlink(&out, &coordinate).expect("plant symlink settlement");
        assert!(
            journal.result(1, 0).is_err(),
            "a symlinked settlement must never be followed into completion"
        );
        assert!(!crate::market_status::journal_clear_for_action(&out));
        let first = &market_schedule()[0];
        let (phase, slot) = (first.phase, first.slot);
        let planted = out
            .join("market-pending")
            .join(format!("{phase:03}-{slot:02}"));
        std::fs::create_dir_all(&planted).expect("plant coordinate directory");
        let orphan = MarketCustodyRecord {
            phase,
            ..custody.clone()
        };
        let orphan = MarketCustodyRecord {
            payload_digest: custody_payload_digest(&orphan).expect("rebind orphan digest"),
            ..orphan
        };
        std::fs::write(
            planted.join("result.json"),
            serde_json::to_vec(&MarketPendingResult {
                custody: orphan.clone(),
                step: MarketStepRecord {
                    phase,
                    action: orphan.action.clone(),
                    command: Safe::literal("planted"),
                    outcome: Safe::literal("settled"),
                    target: orphan.target.clone(),
                    violations: Vec::new(),
                },
            })
            .expect("planted bytes"),
        )
        .expect("plant settlement without an intent");
        assert!(
            journal.result(phase, slot).is_err(),
            "a settlement without its pre-send intent can never complete a scheduled slot"
        );
        assert!(
            !crate::market_status::journal_clear_for_action(&out),
            "a journal holding an orphaned settlement must never clear for a new send"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Target law and producer-to-reader law for the seed phase: a seed
    /// slot has no frozen target while nothing is deployed, both seed slots
    /// bind exactly the deployed pool once one is recorded, a settlement
    /// that abandons the pool for another target is refused before it is
    /// written, and a real settlement the producer writes at the scheduled
    /// seed coordinates is accepted by the strict journal reader and clears
    /// the action gate for a legitimate resume.
    #[test]
    fn seed_slots_bind_the_pool_and_real_settlements_pass_the_strict_reader() {
        let out =
            std::env::temp_dir().join(format!("abtest-deploy-seed-custody-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let schedule = market_schedule();
        let seed_slots: Vec<&MSlot> = schedule
            .iter()
            .filter(|s| matches!(s.op, MOp::SeedBackstop | MOp::SeedPositions))
            .collect();
        assert_eq!(
            seed_slots.len(),
            2,
            "the frozen graph schedules both seed operations"
        );
        // Target law: the engine may not freeze a target for a seed slot
        // before the pool deployment exists, and binds both seed slots to
        // the deployed pool once the deployment is recorded.
        let cfg_dir = out.join("cfg");
        std::fs::create_dir_all(&cfg_dir).expect("fixture directory");
        let mut cfg_text = String::from(
            "[network]\npassphrase = \"Test SDF Network ; September 2015\"\nlabel = \"testnet\"\n\n",
        );
        for role in crate::manifest::MARKET_ARTIFACT_ROLES {
            let artifact = cfg_dir.join(format!("{role}.wasm"));
            std::fs::write(&artifact, b"wasm").expect("fixture artifact");
            cfg_text.push_str(&format!(
                "[artifacts.{role}]\npath = \"{}\"\nsha256 = \"{}\"\n\n",
                artifact.display(),
                "0".repeat(64)
            ));
        }
        cfg_text.push_str(
            "[oracle]\nsources = 3\nmin_sources = 3\nmax_age_secs = 3600\nmax_clock_drift_secs = 60\nbase = \"USD\"\ndecimals = 7\nresolution = 300\n\n[adapter]\ndecimals = 7\nresolution = 1\nbase = \"USD\"\n\n",
        );
        cfg_text.push_str(FIXTURE_RESERVES_TOML);
        cfg_text
            .push_str("[[balances]]\nwho = \"frodo\"\ntoken = \"stable\"\namount = 100000000000\n");
        let cfg_file = cfg_dir.join("config.toml");
        std::fs::write(&cfg_file, &cfg_text).expect("fixture config");
        let cfg = crate::manifest::MarketConfig::load(&cfg_file)
            .expect("fixture config must load through the strict parser");
        for dir in ["admin", "actors", "xdg"] {
            std::fs::create_dir_all(out.join(dir)).expect("fixture directories");
        }
        let ctx = MarketExecutionContext::new(
            "https://soroban-testnet.stellar.org".to_string(),
            Vec::new(),
            "market-admin".to_string(),
            out.join("admin"),
            out.join("actors"),
            out.join("absent-stellar"),
            out.join("xdg"),
            PathBuf::from("/"),
        )
        .expect("fixture market context");
        let mut d = MarketDeployer::new(cfg, ctx, MarketJournal::open(&out))
            .expect("a closed market constructs with its gate failed, never with authority");
        for slot in &seed_slots {
            assert!(
                d.slot_target(slot).is_none(),
                "a seed slot cannot freeze a target while nothing is deployed"
            );
        }
        let pool: ContractId = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
            .parse()
            .expect("fixture pool");
        d.deployed.insert("pool".to_string(), pool.clone());
        for slot in &seed_slots {
            assert_eq!(
                d.slot_target(slot).as_ref(),
                Some(&pool),
                "the frozen seed slot target must be the deployed pool"
            );
        }
        // Producer-to-reader law at the real scheduled seed coordinates:
        // the strict reader accepts exactly what the producer legitimately
        // settles, and a target-abandoning settlement never reaches disk.
        let journal = MarketJournal::open(&out);
        let signer: AccountId = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            .parse()
            .expect("fixture signer");
        let wrong: ContractId = "CBBTYOXBO7SO3JJ5EM5OBV4WTV5TML4G2K327TYSX422WL47B6RVMJRI"
            .parse()
            .expect("fixture witness target");
        for slot in &seed_slots {
            let (phase, numbered) = (slot.phase, slot.slot);
            let mut custody = MarketCustodyRecord {
                phase,
                kind: slot.kind,
                action: Safe::literal(slot.action),
            network: MarketNetwork::Testnet,
                signer: signer.clone(),
                payload_digest: Sha256Hex::default(),
                target: Some(pool.clone()),
                artifact_digest: None,
                predicted: Some(pool.clone()),
                pre_state: vec![Safe::literal("pool status = 1")],
                tool_error: None,
            };
            custody.payload_digest = custody_payload_digest(&custody).expect("fixture digest");
            begin_market_pending(&out, phase, numbered, &custody)
                .expect("seed intent reserves at its scheduled coordinate");
            let mut step = MarketStepRecord {
                phase,
                action: custody.action.clone(),
                command: Safe::literal("test settlement"),
                outcome: Safe::literal("settled"),
                target: Some(wrong.clone()),
                violations: Vec::new(),
            };
            assert!(
                journal
                    .finish(
                        phase,
                        numbered,
                        &MarketPendingResult {
                            custody: custody.clone(),
                            step: step.clone(),
                        },
                    )
                    .is_err(),
                "a seed settlement that abandons the pool for another target cannot be written"
            );
            assert!(
                journal
                    .result(phase, numbered)
                    .expect("an open coordinate is readable")
                    .is_none(),
                "a rejected settlement must leave the coordinate unresolved"
            );
            step.target = Some(pool.clone());
            journal
                .finish(
                    phase,
                    numbered,
                    &MarketPendingResult {
                        custody: custody.clone(),
                        step,
                    },
                )
                .expect("the pool-bound settlement the producer writes must be accepted");
            assert!(
                journal
                    .result(phase, numbered)
                    .expect("a settled coordinate is readable")
                    .is_some(),
                "the strict journal reader must accept the settlement the producer actually wrote"
            );
        }
        assert!(
            crate::market_status::journal_clear_for_action(&out),
            "settled seed coordinates must clear the action gate so a legitimate resume proceeds"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A contract address per index, produced by the tool's own strkey encoder
    /// so no fixture carries a byte string that could drift out of validity.
    /// Index 0 is always another run's address; the roles a fixture run deploys
    /// take the indices after it, so no two roles can share an address and a
    /// manifest naming the pool cannot be naming some other role by accident.
    fn fixture_contract(index: u8) -> ContractId {
        strkey_encode(STRKEY_C, &[index; 32])
            .parse()
            .expect("checksum-valid fixture contract")
    }

    /// The same for operator-shaped addresses: index 0 is a fixture run's
    /// operator, and the indices after it are its run-local actors.
    fn fixture_actor(index: u8) -> AccountId {
        strkey_encode(STRKEY_G, &[index; 32])
            .parse()
            .expect("checksum-valid fixture actor")
    }

    /// A `stellar` stand-in. It answers the read-only calls a market run makes
    /// before it publishes (the CLI version probe, the RPC network identity,
    /// the operator lookup, and the ledger stamp a first publication needs),
    /// refuses every other command, and appends each invocation to a log, so a
    /// test proves a recovery issued no mutation instead of assuming none was.
    fn read_only_stellar(dir: &Path, admin: &AccountId, other: &AccountId, ledger: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("stellar-calls.log");
        let script = dir.join("stellar");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1 $2\" in\n'network info') printf '{{\"passphrase\":\"{}\"}}\\n'; exit 0 ;;\n'keys public-key') case \"$3\" in market-admin) printf '{}\\n' ;; *) printf '{}\\n' ;; esac; exit 0 ;;\n'ledger latest') printf '{{\"sequence\":{}}}\\n'; exit 0 ;;\nesac\ncase \"$1\" in\n'--version') printf 'stellar 26.1.0 (1228cff)\\n'; exit 0 ;;\nesac\nprintf 'the read-only stand-in refuses this command\\n' >&2\nexit 1\n",
                log.display(),
                MARKET_TESTNET_PASSPHRASE,
                admin.as_str(),
                other.as_str(),
                ledger,
            ),
        )
        .expect("stub stellar script");
        std::fs::set_permissions(&script, PermissionsExt::from_mode(0o755))
            .expect("stub stellar must be executable");
        script
    }

    /// Every command the stand-in was asked to run that could change the chain.
    fn mutating_calls(log: &str) -> Vec<String> {
        const MUTATIONS: [&str; 5] = [
            "contract invoke",
            "contract deploy",
            "contract upload",
            "contract restore",
            "keys generate",
        ];
        log.lines()
            .filter(|line| MUTATIONS.iter().any(|verb| line.starts_with(*verb)))
            .map(str::to_string)
            .collect()
    }

    /// How many times the run observed the ledger.
    fn ledger_reads(log: &str) -> usize {
        log.lines()
            .filter(|line| line.starts_with("ledger latest"))
            .count()
    }

    /// Every scheduled reserve with every parameter explicit, as the deploy
    /// gate requires. Values are the fixture defaults, spelled out.
    const FIXTURE_RESERVES_TOML: &str = "\
[reserves.stable]\nc_factor = 9000000\nl_factor = 9500000\nutil = 8500000\nmax_util = 9500000\nsupply_cap = \"1000000000000000000\"\nr_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
[reserves.xlm]\nc_factor = 7500000\nl_factor = 7500000\nutil = 5000000\nmax_util = 9500000\nsupply_cap = \"1000000000000000000\"\nr_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
[reserves.weth]\nc_factor = 8000000\nl_factor = 8000000\nutil = 7000000\nmax_util = 9500000\nsupply_cap = \"170141183460469231731687303715884105727\"\nr_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n";

    /// A market configuration that survives preflight: every pinned artifact
    /// present with the real digest of its own bytes, the frozen oracle and
    /// adapter parameters, and the three playable balances the mint schedule
    /// freezes. Returns the config path with the parsed configuration.
    fn fixture_market_config(out: &Path) -> (PathBuf, MarketConfig) {
        let cfg_dir = out.join("cfg");
        std::fs::create_dir_all(&cfg_dir).expect("fixture directory");
        let mut cfg_text = String::from(
            "[network]\npassphrase = \"Test SDF Network ; September 2015\"\nlabel = \"testnet\"\n\n",
        );
        for role in crate::manifest::MARKET_ARTIFACT_ROLES {
            let artifact = cfg_dir.join(format!("{role}.wasm"));
            let bytes = format!("fixture wasm bytes for {role}").into_bytes();
            let digest = sha256_hex(&bytes);
            std::fs::write(&artifact, &bytes).expect("fixture artifact");
            cfg_text.push_str(&format!(
                "[artifacts.{role}]\npath = \"{}\"\nsha256 = \"{digest}\"\n\n",
                artifact.display()
            ));
        }
        cfg_text.push_str("[oracle]\nsources = 3\nmin_sources = 3\nmax_age_secs = 3600\nmax_clock_drift_secs = 60\nbase = \"USD\"\ndecimals = 7\nresolution = 300\n\n[adapter]\ndecimals = 7\nresolution = 1\nbase = \"USD\"\n\n");
        cfg_text.push_str(FIXTURE_RESERVES_TOML);
        cfg_text.push_str("[[balances]]\nwho = \"frodo\"\ntoken = \"stable\"\namount = 100000000000\n\n[[balances]]\nwho = \"sam\"\ntoken = \"stable\"\namount = 200000000000\n\n[[balances]]\nwho = \"user\"\ntoken = \"stable\"\namount = 300000000000\n");
        let config = cfg_dir.join("config.toml");
        std::fs::write(&config, &cfg_text).expect("fixture config");
        let cfg = MarketConfig::load(&config)
            .expect("fixture config must load through the strict parser");
        (config, cfg)
    }

    /// The CLI context for a fixture run, bound to the read-only stand-in and
    /// the operator alias the caller names.
    fn fixture_market_context(out: &Path, stellar: &Path, alias: &str) -> MarketExecutionContext {
        for dir in ["admin", "actors", "xdg"] {
            std::fs::create_dir_all(out.join(dir)).expect("fixture directories");
        }
        MarketExecutionContext::new(
            "https://soroban-testnet.stellar.org".to_string(),
            Vec::new(),
            alias.to_string(),
            out.join("admin"),
            out.join("actors"),
            stellar.to_path_buf(),
            out.join("xdg"),
            PathBuf::from("/"),
        )
        .expect("fixture market context")
    }

    /// Settle every coordinate whose facts a publication assembles from: each
    /// deterministic deployment through the id its own custody record predicts,
    /// and each run-local actor through the signer its funding coordinate
    /// settles. Both records go through the journal's reservation and
    /// settlement path, so nothing the strict reader would reject can be
    /// planted here. Returns the pool this run settled.
    fn settle_publication_inputs(journal: &MarketJournal, admin: &AccountId) -> ContractId {
        let mut pool = None;
        let mut deployment = 1u8;
        let mut actor = 1u8;
        for slot in market_schedule() {
            let funding = matches!(&slot.op, MOp::FundKey { .. });
            let role = MarketDeployer::deployed_role(&slot.op);
            if !funding && role.is_none() {
                continue;
            }
            let predicted = if funding {
                None
            } else {
                let id = fixture_contract(deployment);
                deployment += 1;
                if role.as_deref() == Some("pool") {
                    pool = Some(id.clone());
                }
                Some(id)
            };
            let signer = if funding {
                let addr = fixture_actor(actor);
                actor += 1;
                addr
            } else {
                admin.clone()
            };
            let mut record = MarketCustodyRecord {
                phase: slot.phase,
                kind: slot.kind,
                action: Safe::literal(slot.action),
            network: MarketNetwork::Testnet,
                signer,
                payload_digest: Sha256Hex::default(),
                target: predicted.clone(),
                artifact_digest: None,
                predicted: predicted.clone(),
                pre_state: vec![Safe::literal("the coordinate settled")],
                tool_error: None,
            };
            record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
            journal
                .begin(slot.phase, slot.slot, &record)
                .expect("reserve the coordinate");
            journal
                .finish(
                    slot.phase,
                    slot.slot,
                    &MarketPendingResult {
                        custody: record,
                        step: MarketStepRecord {
                            phase: slot.phase,
                            action: Safe::literal(slot.action),
                            command: Safe::render_parsed(
                                Policy::Market,
                                &json!({"coordinate": format!("{:03}-{:02}", slot.phase, slot.slot)}),
                                256,
                            ),
                            outcome: Safe::literal("the coordinate settled"),
                            target: predicted,
                            violations: Vec::new(),
                        },
                    },
                )
                .expect("settle the coordinate");
        }
        pool.expect("the frozen graph deploys a pool")
    }

    /// A market run directory built the way a real run builds it: the
    /// deployer's own constructor writes the frozen plan after a preflight that
    /// really passed, then every coordinate a publication assembles from is
    /// settled through the strict journal path. The publication coordinate is
    /// left unreserved, so each test stages the interruption it means to
    /// recover from.
    struct ACompletedMarket {
        out: PathBuf,
        config: PathBuf,
        stellar: PathBuf,
        call_log: PathBuf,
        admin: AccountId,
        ledger: u32,
        pool: ContractId,
        publish: MSlot,
        manifest: MarketManifest,
    }

    fn a_completed_market(tag: &str, ledger: u32) -> ACompletedMarket {
        let admin = fixture_actor(0);
        let out = std::env::temp_dir().join(format!("abtest-deploy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let stellar = read_only_stellar(&out, &admin, &fixture_actor(9), ledger);
        let (config, cfg) = fixture_market_config(&out);
        // The run's start: preflight proves the CLI, the artifacts, and the
        // operator, and the plan is written once for the graph to freeze.
        MarketDeployer::new(
            cfg,
            fixture_market_context(&out, &stellar, "market-admin"),
            MarketJournal::open(&out),
        )
        .expect("a market whose preflight passes constructs");
        let pool = settle_publication_inputs(&MarketJournal::open(&out), &admin);
        // A restart with nothing pending: it rebuilds deployment and identity
        // state the way a resume does, and assembles the manifest the publisher
        // would have written at the ledger the stand-in reports.
        let d = MarketDeployer::new(
            MarketConfig::load(&config).expect("fixture config reloads"),
            fixture_market_context(&out, &stellar, "market-admin"),
            MarketJournal::open(&out),
        )
        .expect("a market with a reproducible plan constructs");
        let manifest = d
            .assemble_manifest(ledger)
            .expect("the settled state assembles a manifest");
        let publish = market_schedule()
            .into_iter()
            .find(|s| s.op == MOp::Publish)
            .expect("the graph publishes last");
        ACompletedMarket {
            call_log: out.join("stellar-calls.log"),
            admin,
            config,
            ledger,
            manifest,
            out,
            pool,
            publish,
            stellar,
        }
    }

    impl ACompletedMarket {
        /// The operator-resuming constructor: the real entrypoint behavior,
        /// including whatever reconciliation the no-replay scan performs.
        fn resume(&self, alias: &str) -> MarketDeployer {
            MarketDeployer::new(
                MarketConfig::load(&self.config).expect("fixture config reloads"),
                fixture_market_context(&self.out, &self.stellar, alias),
                MarketJournal::open(&self.out),
            )
            .expect("a market resume constructs and refuses through its own law")
        }

        /// Freeze the publication intent the way the confirmation gate does: no
        /// target, no prediction, no artifact, bound by its payload digest.
        fn freeze_publication(&self, signer: &AccountId) {
            let mut record = MarketCustodyRecord {
                phase: self.publish.phase,
                kind: self.publish.kind,
                action: Safe::literal(self.publish.action),
            network: MarketNetwork::Testnet,
                signer: signer.clone(),
                payload_digest: Sha256Hex::default(),
                target: None,
                artifact_digest: None,
                predicted: None,
                pre_state: vec![Safe::literal(
                    "no settled result exists at this coordinate; nothing has been replayed",
                )],
                tool_error: None,
            };
            record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
            MarketJournal::open(&self.out)
                .begin(self.publish.phase, self.publish.slot, &record)
                .expect("freeze the publication intent");
        }

        fn journal(&self) -> MarketJournal {
            MarketJournal::open(&self.out)
        }

        fn publication_settled(&self) -> bool {
            self.journal()
                .result(self.publish.phase, self.publish.slot)
                .expect("the publication coordinate is readable")
                .is_some()
        }

        fn published_bytes(&self) -> Option<Vec<u8>> {
            std::fs::read(self.out.join(MARKET_MANIFEST_FILE)).ok()
        }

        fn publish(&self, manifest: &MarketManifest) {
            std::fs::write(
                self.out.join(MARKET_MANIFEST_FILE),
                toml::to_string_pretty(manifest).expect("render the published manifest"),
            )
            .expect("plant the published manifest");
        }

        fn intent_bytes(&self) -> Vec<u8> {
            std::fs::read(
                crate::manifest::market_pending_dir(
                    &self.out,
                    self.publish.phase,
                    self.publish.slot,
                )
                .join("intent.json"),
            )
            .expect("read the frozen intent")
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(&self.call_log).unwrap_or_default()
        }
    }

    /// The manifest's pool comes from the same settled record every other
    /// deployed address comes from. The deployment arm records a pool only in
    /// the deployed map, so an assembly reading it from a mirror of its own
    /// refuses a forward run at its publication coordinate while a restart
    /// happens to work. This pins the one record both paths must read, and that
    /// an unsettled pool is never published as if it were proven.
    #[test]
    fn the_manifest_names_the_pool_the_deployment_settled() {
        let run = a_completed_market("manifest-pool", 1_234_567);
        let mut d = run.resume("market-admin");
        let manifest = d
            .assemble_manifest(run.ledger)
            .expect("a run that settled its pool assembles its manifest");
        assert_eq!(
            manifest.pool, run.pool,
            "the manifest names the pool this run deployed"
        );
        assert_eq!(manifest.deploy_ledger, run.ledger);
        d.deployed.remove("pool");
        assert!(
            d.assemble_manifest(run.ledger).is_err(),
            "publication never carries a pool this run did not settle"
        );
        let _ = std::fs::remove_dir_all(&run.out);
    }

    /// The crash this recovery exists for: the graph settled, the publication
    /// intent froze, and the manifest never reached disk. Nothing was ever
    /// submitted, so a real resume completes that local publication, names the
    /// pool this run deployed, stamps the ledger it actually read, and settles
    /// the coordinate whose intent was already frozen. The stand-in was never
    /// asked for a mutation, and only the one read-only ledger observation a
    /// first publication legitimately needs was made.
    #[test]
    fn a_publication_that_never_reached_disk_publishes_on_resume() {
        let run = a_completed_market("publish-missing", 4_242_424);
        assert!(
            run.published_bytes().is_none(),
            "nothing is published before the interruption"
        );
        run.freeze_publication(&run.admin);
        assert!(
            publication_is_unsettled(&run.out).expect("the journal is readable"),
            "an interrupted publication is admissible to resume"
        );
        let d = run.resume("market-admin");
        assert!(
            d.terminal.is_none(),
            "an interrupted local publication resumes instead of abandoning custody"
        );
        let manifest = d.manifest.expect("the resume completed the publication");
        assert_eq!(
            manifest.pool, run.pool,
            "the manifest names the pool this run deployed"
        );
        assert_eq!(manifest.deploy_ledger, run.ledger);
        assert!(run.published_bytes().is_some(), "the manifest is on disk");
        assert!(
            run.publication_settled(),
            "the coordinate whose intent was frozen settled"
        );
        assert_eq!(
            ledger_reads(&run.calls()),
            1,
            "a first publication stamps itself from one read-only ledger observation"
        );
        assert!(
            mutating_calls(&run.calls()).is_empty(),
            "resuming a publication submitted nothing: {:?}",
            mutating_calls(&run.calls())
        );
        assert!(
            !publication_is_unsettled(&run.out).expect("the journal is readable"),
            "a settled publication is never reported pending, so a completed run stays refused"
        );
        let _ = std::fs::remove_dir_all(&run.out);
    }

    /// The funded-actor attribution law on resume. A first-run gate books a
    /// fund_key coordinate with the operator as its signer (the actor key
    /// does not exist yet); rebuilding identities from that frozen signer
    /// invented actors that ARE the operator, so the attribution guards
    /// refused every later step and an open funding intent could never be
    /// attributed. The run-local keystore — which only the coordinate's own
    /// `keys generate --fund` fills — is the identity and settlement proof.
    #[test]
    fn a_frozen_operator_funded_record_never_becomes_the_actor_identity() {
        let admin = fixture_actor(0);
        let actor_key = fixture_actor(7);
        let out = std::env::temp_dir().join(format!(
            "abtest-deploy-funded-attribution-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let stellar = read_only_stellar(&out, &admin, &actor_key, 500);
        let (config, cfg) = fixture_market_config(&out);
        let ctx = || fixture_market_context(&out, &stellar, "market-admin");
        // Bind the frozen plan the way a first run does.
        MarketDeployer::new(cfg.clone(), ctx(), MarketJournal::open(&out))
            .expect("a market whose preflight passes constructs");
        let fund = market_schedule()
            .into_iter()
            .find(|s| matches!(s.op, MOp::FundKey { .. }))
            .expect("the graph provisions actors");
        let who = match fund.op {
            MOp::FundKey { who } => who,
            _ => unreachable!("selected above as a fund_key coordinate"),
        };
        let journal = MarketJournal::open(&out);
        let mut record = MarketCustodyRecord {
            phase: fund.phase,
            kind: fund.kind,
            action: Safe::literal(fund.action),
            network: MarketNetwork::Testnet,
            // Exactly the first-run shape: the operator is booked while the
            // actor key does not exist yet.
            signer: admin.clone(),
            payload_digest: Sha256Hex::default(),
            target: None,
            artifact_digest: None,
            predicted: None,
            pre_state: vec![Safe::literal("the coordinate settled")],
            tool_error: None,
        };
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        journal
            .begin(fund.phase, fund.slot, &record)
            .expect("reserve the funding coordinate");
        journal
            .finish(
                fund.phase,
                fund.slot,
                &MarketPendingResult {
                    custody: record.clone(),
                    step: MarketStepRecord {
                        phase: fund.phase,
                        action: record.action.clone(),
                        command: Safe::literal("test settlement"),
                        outcome: Safe::literal("settled"),
                        target: None,
                        violations: Vec::new(),
                    },
                },
            )
            .expect("settle the funding coordinate");
        let d = MarketDeployer::new(
            MarketConfig::load(&config).expect("fixture config reloads"),
            ctx(),
            MarketJournal::open(&out),
        )
        .expect("a funded-actor resume constructs");
        assert!(
            d.terminal.is_none(),
            "a settled operator-signed funding record does not endanger the resume: {:?}",
            d.terminal
        );
        assert_eq!(
            d.progress.identities.get(&who),
            Some(&actor_key),
            "the actor's identity is the run-local keystore key, never the frozen operator signer"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// An open funding intent booked while the key did not exist yet is
    /// attributable from the coordinate alone: the frozen schedule names the
    /// actor, and the run-local keystore holding that actor's key is the
    /// public-equivalent proof the generate-and-fund call landed.
    #[test]
    fn an_open_funding_intent_settles_from_the_coordinate_keystore() {
        let admin = fixture_actor(0);
        let actor_key = fixture_actor(7);
        let out = std::env::temp_dir().join(format!(
            "abtest-deploy-funded-open-intent-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let stellar = read_only_stellar(&out, &admin, &actor_key, 500);
        let (config, cfg) = fixture_market_config(&out);
        let ctx = || fixture_market_context(&out, &stellar, "market-admin");
        MarketDeployer::new(cfg.clone(), ctx(), MarketJournal::open(&out))
            .expect("a market whose preflight passes constructs");
        let fund = market_schedule()
            .into_iter()
            .find(|s| matches!(s.op, MOp::FundKey { .. }))
            .expect("the graph provisions actors");
        let mut record = MarketCustodyRecord {
            phase: fund.phase,
            kind: fund.kind,
            action: Safe::literal(fund.action),
            network: MarketNetwork::Testnet,
            signer: admin.clone(),
            payload_digest: Sha256Hex::default(),
            target: None,
            artifact_digest: None,
            predicted: None,
            pre_state: vec![Safe::literal(
                "no settled result exists at this coordinate; nothing has been replayed",
            )],
            tool_error: None,
        };
        record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
        // The intent is open — reserved, never settled — exactly as an
        // interrupted first run leaves it.
        MarketJournal::open(&out)
            .begin(fund.phase, fund.slot, &record)
            .expect("reserve the funding intent");
        let d = MarketDeployer::new(
            MarketConfig::load(&config).expect("fixture config reloads"),
            ctx(),
            MarketJournal::open(&out),
        )
        .expect("a resume with an open funding intent constructs");
        assert!(
            d.terminal.is_none(),
            "the open funding intent reconciles from its coordinate: {:?}",
            d.terminal
        );
        assert!(
            MarketJournal::open(&out)
                .result(fund.phase, fund.slot)
                .expect("the journal is readable")
                .is_some(),
            "the reconciled coordinate is settled, not replayed"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The other half of the crash window: the manifest's atomic rename
    /// succeeded and the settlement did not. Resume honors the published file
    /// as the proof it is, and needs no chain observation at all to do it,
    /// because even the ledger stamp comes from the file being honored. The
    /// frozen intent survives byte-for-byte.
    #[test]
    fn an_already_published_manifest_completes_without_any_observation() {
        let run = a_completed_market("publish-settled", 8_675_309);
        run.freeze_publication(&run.admin);
        run.publish(&run.manifest);
        let frozen_intent = run.intent_bytes();
        let d = run.resume("market-admin");
        assert!(
            d.terminal.is_none(),
            "an interrupted settlement resumes from the published file"
        );
        assert!(
            run.publication_settled(),
            "the published file settles the coordinate it was written for"
        );
        assert_eq!(
            d.manifest.expect("the resumed run holds its manifest").pool,
            run.pool,
            "the resumed run completes with the pool this run deployed"
        );
        assert_eq!(
            ledger_reads(&run.calls()),
            0,
            "honoring a published manifest observed nothing, not even the ledger"
        );
        assert!(
            mutating_calls(&run.calls()).is_empty(),
            "completing a settlement submitted nothing: {:?}",
            mutating_calls(&run.calls())
        );
        assert_eq!(
            run.intent_bytes(),
            frozen_intent,
            "completing a publication never rewrites its frozen intent"
        );
        let _ = std::fs::remove_dir_all(&run.out);
    }

    /// A manifest that does not reproduce this run's settled facts is not this
    /// run's proof. It is refused as unresolved custody and left exactly where
    /// it was, never overwritten; and the same directory completes once the
    /// manifest that really belongs to it is in place.
    #[test]
    fn another_runs_published_manifest_is_refused_not_overwritten() {
        let run = a_completed_market("publish-foreign", 5_555_555);
        run.freeze_publication(&run.admin);
        let mut foreign = run.manifest.clone();
        foreign.pool = fixture_contract(0);
        run.publish(&foreign);
        let foreign_bytes = run.published_bytes().expect("the foreign file is planted");
        let d = run.resume("market-admin");
        assert!(
            matches!(
                d.terminal,
                Some(DeployEvent::Custody {
                    unresolved: true,
                    ..
                })
            ),
            "a manifest that does not reproduce this run is unresolved custody"
        );
        assert!(
            !run.publication_settled(),
            "a refused publication stays open for its operator"
        );
        assert_eq!(
            run.published_bytes().expect("still planted"),
            foreign_bytes,
            "a foreign manifest is honored as evidence, never overwritten"
        );
        assert!(
            mutating_calls(&run.calls()).is_empty(),
            "refusing a foreign manifest submitted nothing: {:?}",
            mutating_calls(&run.calls())
        );
        run.publish(&run.manifest);
        let d = run.resume("market-admin");
        assert!(
            d.terminal.is_none(),
            "the same directory completes once this run's manifest is in place"
        );
        assert_eq!(
            d.manifest.expect("the publication completed").pool,
            run.pool,
            "the completed run names the pool this run deployed"
        );
        assert_eq!(
            ledger_reads(&run.calls()),
            0,
            "completing from a published file needed no observation at all"
        );
        let _ = std::fs::remove_dir_all(&run.out);
    }

    /// A resume whose frozen plan does not reproduce is refused, and a refused
    /// run must leave every existing record exactly as it found it. Scanning
    /// can now settle a coordinate and write a manifest naming whoever the
    /// resumed configuration resolved, so the scan must not run at all behind
    /// that refusal: the records belong to the run the plan froze, and the
    /// operator who can reproduce it must still be able to recover them.
    #[test]
    fn a_refused_resume_neither_publishes_nor_settles() {
        let run = a_completed_market("refused", 3_141_592);
        run.freeze_publication(&run.admin);
        // A different but perfectly valid operator: preflight proves it, and
        // the plan the original run froze does not reproduce from it.
        let d = run.resume("someone-else");
        assert!(
            matches!(d.terminal, Some(DeployEvent::Failed(_))),
            "a plan that does not reproduce is refused, never re-planned"
        );
        assert!(d.manifest.is_none(), "a refused run holds no manifest");
        assert!(
            run.published_bytes().is_none(),
            "a refused resume wrote no manifest naming its own operator"
        );
        assert!(
            !run.publication_settled(),
            "a refused resume settled no coordinate"
        );
        assert!(
            mutating_calls(&run.calls()).is_empty(),
            "a refused resume submitted nothing: {:?}",
            mutating_calls(&run.calls())
        );
        let d = run.resume("market-admin");
        assert!(
            d.terminal.is_none(),
            "the operator the plan froze still recovers the same directory"
        );
        assert_eq!(
            d.manifest.expect("the publication completed").pool,
            run.pool,
            "the records a refused resume preserved are the ones the recovery needed"
        );
        let _ = std::fs::remove_dir_all(&run.out);
    }

    // ---- a real market: its graph, its legs, and its reuse ----------------
    //
    // Everything below drives a real market's graph against the same kind of
    // stand-in the lab tests use: a `stellar` script that records every
    // invocation and answers from a tiny ledger, so each test states what a
    // run submitted and what its journal settled rather than reading an
    // internal.

    /// The activation shim's two synthetic classic codes.
    const REAL_BLND_CODE: &str = "SHIMBLND";
    const REAL_USDC_CODE: &str = "SHIMUSDC";

    /// The `stellar` stand-in a real market's graph runs against. Its ledger
    /// is one file per fact under `<run dir>/state`: `key-<name>` is an
    /// identity, `account-<G>` is `open` or `locked`, `trust-<CODE_ISSUER>` is
    /// the balance the operator holds in that classic asset,
    /// `sac-<CODE_ISSUER>` is the asset contract the network's own wrapper
    /// gives it, `owner` is who a shared runtime names itself as owned by, and
    /// `proxy-<asset key>` is the ProxyConfig a runtime reports serving for
    /// one asset. A fact that was never armed reads back as the documented
    /// absence — an empty entry list, a failed simulation — because a run has
    /// to be able to prove something is missing. `fail-payment` and `fail-lock`
    /// make one send report nothing definitive, which is the state a resume
    /// exists for.
    const REAL_STELLAR: &str = r##"#!/bin/sh
LOG='@LOG@'
S='@STATE@'
printf '%s\n' "$*" >> "$LOG"

# The pinned signer of a market send is its --source G-address.
pin=
prev=
for w in "$@"; do
    if [ "$prev" = --source ]; then
        case "$w" in G*) pin=$w ;; esac
    fi
    prev=$w
done

arg() { # $1 = flag, prints the word after it
    flag=$1
    shift
    prev=
    for w in "$@"; do
        [ "$prev" = "$flag" ] && { printf '%s' "$w"; return; }
        prev=$w
    done
}

# The invoked function's own arguments live after the `--` separator. A
# contract call carries `--id <contract>` before it, so a proposal argument
# read with the plain reader would be answered with the contract's address —
# the two `--id`s are not the same thing.
argfn() { # $1 = flag, prints the word after it among the function's arguments
    flag=$1
    shift
    sep=0
    prev=
    for w in "$@"; do
        if [ "$sep" = 1 ]; then
            [ "$prev" = "$flag" ] && { printf '%s' "$w"; return; }
        fi
        [ "$w" != -- ] || sep=1
        prev=$w
    done
}

account_doc() { # $1 = address, $2 = open|locked
    if [ "$2" = locked ]; then
        th=00010101
        sg='[]'
    else
        th=01010101
        sg='[{"key":"'"$1"'","weight":1}]'
    fi
    printf '{"entries":[{"key":{"account":{"account_id":"%s"}},"val":{"account":{"account_id":"%s","balance":"20000000","seq_num":"1","num_sub_entries":1,"inflation_dest":null,"flags":0,"home_domain":"","thresholds":"%s","signers":%s,"ext":"v0"}},"lastModifiedLedgerSeq":1}],"latestLedger":1}\n' "$1" "$1" "$th" "$sg"
}

trust_doc() { # $1 = account, $2 = balance
    printf '{"entries":[{"key":{},"val":{"trustline":{"account_id":"%s","balance":"%s","limit":"9223372036854775807","flags":1}}}],"latestLedger":1}\n' "$1" "$2"
}

nothing() { printf '{"entries":[],"latestLedger":1}\n'; }

# The stand-in has no XDR codec, so a stored entry is kept as the bytes that
# identify it: the symbols and addresses its ScVal names, then its u32 as
# exactly four bytes. Two entries that name different addresses therefore
# differ in their bytes, and two that name the same address and differ only
# in an expiry differ only in those last four — which is exactly the property
# a caller that ignores the expiry relies on. Values are padded to whole
# four-byte words because a real ScVal is.
scval_b64() { # $1 = the ScVal in the CLI's JSON spelling
    text=$(printf '%s' "$1" | grep -o '"\(symbol\|address\)":"[^"]*"' | sed 's/.*:"//; s/"$//')
    num=$(printf '%s' "$1" | grep -o '"u32":[0-9]*' | sed 's/.*://')
    [ -n "$num" ] || num=0
    esc=$(printf '\\%03o\\%03o\\%03o\\%03o' \
        $((num >> 24 & 255)) $((num >> 16 & 255)) $((num >> 8 & 255)) $((num & 255)))
    {
        printf '%s' "$text"
        printf "$esc"
        pad=$(( (4 - $(printf '%s' "$text" | wc -c) % 4) % 4 ))
        i=0
        while [ "$i" -lt "$pad" ]; do printf '\000'; i=$((i + 1)); done
    } | base64 -w0
}

# The encoding of a key given to `contract read --key` by name, which the
# caller checks byte for byte: an ScVal Symbol is the discriminant 15, the
# text's length, and the text padded with zeroes to a whole four-byte word.
symbol_b64() { # $1 = the symbol's text
    len=${#1}
    esc=$(printf '\\%03o\\%03o\\%03o\\%03o' \
        $((len >> 24 & 255)) $((len >> 16 & 255)) $((len >> 8 & 255)) $((len & 255)))
    {
        printf '\000\000\000\017'
        printf "$esc"
        printf '%s' "$1"
        pad=$(( (4 - len % 4) % 4 ))
        i=0
        while [ "$i" -lt "$pad" ]; do printf '\000'; i=$((i + 1)); done
    } | base64 -w0
}

temp_value() { # $1 = contract id; the entry's ScVal JSON, or empty
    f="$S/temp-$1"
    [ -f "$f" ] || return 0
    scval_b64 "$(cat "$f")"
}

# The roles one account holds on one governance. A test arms
# `hasrole-<contract>-<account>` with the role names it holds, space-separated
# (an empty file holds none). Unarmed, the answer is the one a run may assume
# of itself: the operator administers the governance it deployed, and nobody
# else does — which is the state a handover is asked to change.
roles_of() { # $1 = contract, $2 = account
    f="$S/hasrole-$1-$2"
    if [ -f "$f" ]; then cat "$f"; return; fi
    if [ -f "$S/no-admin-role" ]; then return; fi
    if [ -f "$S/key-market-admin" ] && [ "$(cat "$S/key-market-admin")" = "$2" ]; then
        printf 'Admin'
    fi
}

trust_path() { printf '%s/trust-%s' "$S" "$(printf '%s' "$1" | tr ':' '_')"; }

case "$1 $2" in
'network info') printf '{"passphrase":"@PASSPHRASE@"}\n'; exit 0 ;;
'ledger latest') printf '{"sequence":@LEDGER@}\n'; exit 0 ;;
esac
if [ "$1" = --version ]; then printf 'stellar 26.1.0 (1228cff)\n'; exit 0; fi

if [ "$1" = xdr ] && [ "$2" = encode ]; then
    eval "file=\${$#}"
    if [ ! -f "$file" ]; then printf 'no ScVal to encode\n' >&2; exit 1; fi
    scval_b64 "$(cat "$file")"
    printf '\n'
    exit 0
fi

# A temporary entry is the one thing a handoff is proved by, so the stand-in
# keeps them: one per contract, armed as the ScVal JSON the contract would
# have stored (`propose_admin` and `transfer_ownership` write their own). The
# requested key is echoed back, because it is the same encoder's output the
# caller computed, and a value that names another address simply does not
# match.
if [ "$1" = contract ] && [ "$2" = read ]; then
    cid=$(arg --id "$@")
    key=$(arg --key-xdr "$@")
    if [ -z "$key" ]; then key=$(symbol_b64 "$(arg --key "$@")"); fi
    val=$(temp_value "$cid")
    if [ -z "$val" ]; then
        printf 'no matching contract data entries were found for the specified contract id\n' >&2
        exit 1
    fi
    printf '%s,%s,%s\n' "$key" "$val" "$(scval_b64 '{"u32":1}')"
    exit 0
fi

if [ "$1" = keys ] && [ "$2" = public-key ]; then
    f="$S/key-$3"
    if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
    printf 'no identity of this name\n' >&2
    exit 1
fi
if [ "$1" = keys ] && [ "$2" = generate ]; then
    printf 'the stand-in holds no key of this name\n' >&2
    exit 1
fi
if [ "$1" = keys ] && [ "$2" = rm ]; then
    rm -f "$S/key-$3"
    exit 0
fi

if [ "$1" = ledger ] && [ "$2" = entry ] && [ "$3" = fetch ]; then
    acct=$(arg --account "$@")
    asset=$(arg --asset "$@")
    case "$4" in
    account)
        f="$S/account-$acct"
        if [ -f "$f" ]; then account_doc "$acct" "$(cat "$f")"; exit 0; fi
        nothing; exit 0 ;;
    trustline)
        f=$(trust_path "$asset")
        if [ -f "$f" ]; then trust_doc "$acct" "$(cat "$f")"; exit 0; fi
        nothing; exit 0 ;;
    esac
    printf 'no ledger entry of this kind\n' >&2
    exit 1
fi

if [ "$1" = tx ] && [ "$2" = new ]; then
    case "$3" in
    create-account)
        dest=$(arg --destination "$@")
        [ -n "$dest" ] || { printf 'no destination\n' >&2; exit 1; }
        printf open > "$S/account-$dest"
        exit 0 ;;
    change-trust)
        line=$(arg --line "$@")
        [ -n "$line" ] || { printf 'no line\n' >&2; exit 1; }
        if [ -f "$S/fail-change-trust" ]; then printf 'ambiguous submission\n' >&2; exit 1; fi
        printf 0 > "$(trust_path "$line")"
        exit 0 ;;
    payment)
        asset=$(arg --asset "$@")
        amount=$(arg --amount "$@")
        if [ -f "$S/fail-payment" ]; then printf 'ambiguous submission\n' >&2; exit 1; fi
        [ -n "$asset" ] && [ -n "$amount" ] || { printf 'no asset\n' >&2; exit 1; }
        printf '%s' "$amount" > "$(trust_path "$asset")"
        exit 0 ;;
    set-options)
        if [ -f "$S/fail-lock" ]; then printf 'ambiguous submission\n' >&2; exit 1; fi
        [ -n "$pin" ] || { printf 'no pinned signer\n' >&2; exit 1; }
        printf locked > "$S/account-$pin"
        exit 0 ;;
    esac
fi

if [ "$1" = contract ] && [ "$2" = asset ] && [ "$3" = deploy ]; then
    f=$(printf '%s/sac-%s' "$S" "$(printf '%s' "$(arg --asset "$@")" | tr ':' '_')")
    if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
    printf 'no asset contract is armed\n' >&2
    exit 1
fi
if [ "$1" = contract ] && [ "$2" = id ] && [ "$3" = asset ]; then
    f=$(printf '%s/sac-%s' "$S" "$(printf '%s' "$(arg --asset "$@")" | tr ':' '_')")
    if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
    printf 'no asset contract is armed\n' >&2
    exit 1
fi
if [ "$1" = contract ] && [ "$2" = id ] && [ "$3" = wasm ]; then
    # The operator alias lives only in the operator's keystore: a prediction
    # made from the run-local keystore cannot resolve it, as on the network.
    src=$(arg --source "$@")
    cfg=$(arg --config-dir "$@")
    if [ "$src" = market-admin ] && [ "$cfg" != '@ADMIN@' ]; then
        printf 'error: Failed to find config identity for market-admin\n' >&2
        exit 1
    fi
    f="$S/predict-$(arg --salt "$@")"
    if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
    if [ -f "$S/predicted" ]; then cat "$S/predicted"; printf '\n'; exit 0; fi
    printf 'no prediction is armed\n' >&2
    exit 1
fi
if [ "$1" = contract ] && [ "$2" = deploy ] && [ -n "$(arg --wasm-hash "$@")" ]; then
    f="$S/predict-$(arg --salt "$@")"
    if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
    if [ -f "$S/predicted" ]; then cat "$S/predicted"; printf '\n'; exit 0; fi
    printf 'no deployment is armed\n' >&2
    exit 1
fi
if [ "$1" = contract ] && [ "$2" = upload ]; then
    # The chain's answer to an upload is the content address of the bytes it
    # received; a test arms the hash of exactly the bytes it wrote there.
    f="$S/upload-$(arg --wasm "$@" | tr '/.' '__')"
    if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
    printf 'no artifact is armed at this path\n' >&2
    exit 1
fi
if [ "$1" = contract ] && [ "$2" = fetch ]; then
    hash=$(arg --wasm-hash "$@")
    if [ -n "$hash" ]; then
        # Installed code (`contract-hash-<hash>`): a hash the network holds
        # answers, anything else is not live.
        f="$S/contract-hash-$hash"
        if [ ! -f "$f" ]; then printf 'no such wasm\n' >&2; exit 1; fi
        cat "$f" > "$(arg --out-file "$@")"
        exit 0
    fi
    id=$(arg --id "$@")
    [ -n "$id" ] || { printf 'the stand-in fetches armed contracts only\n' >&2; exit 1; }
    f="$S/contract-$id"
    if [ ! -f "$f" ]; then printf 'no contract is armed\n' >&2; exit 1; fi
    cat "$f" > "$(arg --out-file "$@")"
    exit 0
fi

if [ "$1" = contract ] && [ "$2" = invoke ]; then
    fn=
    sep=0
    for w in "$@"; do
        if [ "$sep" = 1 ] && [ -z "$fn" ]; then fn=$w; fi
        [ "$w" != -- ] || sep=1
    done
    # The pinned ABI per contract ROLE (`role-<contract id>`): a function a
    # role's code does not export is refused, as the network refuses it. A
    # union over roles would hide drift (SEP-40 `base` is not runtime ABI).
    cid=$(arg --id "$@")
    role=
    if [ -f "$S/role-$cid" ]; then role=$(cat "$S/role-$cid"); fi
    abi=
    case "$role" in
    governance) abi=' __constructor active_ids cancel_proposal create_proposal execute_proposal extend_ttl get_effective_proposal_ttl get_operation_ttl get_proposal get_roles has_role list_role next_proposal_id proxy_oracle ' ;;
    runtime) abi=' __constructor accept_ownership add_breaker aggregated_history aggregated_latest configure_breakers extend_ttl get_breaker_set_view get_cached get_owner get_proxy rearm refresh registered_assets remove_breaker remove_proxy renounce_ownership set_enforced set_manual_trip set_proxy source_base transfer_ownership upgrade ' ;;
    lazer) abi=' __constructor assets base config decimals extend_ttl lastprice price prices reset_verification_epoch set_freshness set_supported_feed_ids set_verification_config stored_price supported_feed_ids update_price_feeds verification_epoch accept_ownership get_owner renounce_ownership transfer_ownership ' ;;
    esac
    if [ -n "$abi" ]; then
        case "$abi" in
        *" $fn "*) ;;
        *) printf 'error: function %s does not exist in the %s contract\n' "$fn" "$role" >&2; exit 1 ;;
        esac
    fi
    case "$fn" in
    get_operation_ttl|get_effective_proposal_ttl)
        printf '0\n'
        exit 0 ;;
    get_config)
        if [ -f "$S/pool-config" ]; then cat "$S/pool-config"; printf '\n'; exit 0; fi
        printf '{"bstop_rate":0,"max_positions":4,"min_collateral":"10000000000","oracle":"%s","status":6}\n' "$(cat "$S/pool-oracle" 2>/dev/null)"
        exit 0 ;;
    source_base|base|get_owner|get_admin|get_controller)
        # A view answers with a JSON value. Ownership-style answers are
        # per-contract (`owner-`/`admin-`/`controller-<id>`); an unarmed
        # contract has no such answer, exactly as an absent contract has.
        if [ "$fn" = source_base ] || [ "$fn" = base ]; then
            printf '{"Other":"@BASE@"}\n'
            exit 0
        fi
        key=owner
        case "$fn" in
        get_admin) key="admin-$cid" ;;
        get_controller) key="controller-$cid" ;;
        get_owner) key="owner-$cid" ;;
        esac
        f="$S/$key"
        if [ ! -f "$f" ] && [ "$fn" = get_owner ] && [ -f "$S/owner" ]; then f="$S/owner"; fi
        if [ -f "$f" ]; then printf '"%s"\n' "$(cat "$f")"; exit 0; fi
        printf 'no such answer\n' >&2
        exit 1 ;;
    name)
        if [ -f "$S/name-$cid" ]; then printf '"%s"\n' "$(cat "$S/name-$cid")"; exit 0; fi
        printf 'no asset contract answers here\n' >&2
        exit 1 ;;
    balance)
        if [ -f "$S/balance-$cid" ]; then cat "$S/balance-$cid"; printf '\n'; exit 0; fi
        printf 'no balance is armed\n' >&2
        exit 1 ;;
    pool_data)
        if [ -f "$S/pool-data-after-$cid" ]; then
            n=$(cat "$S/pool-data-count-$cid" 2>/dev/null || printf '0')
            n=$((n + 1))
            printf '%s' "$n" > "$S/pool-data-count-$cid"
            if [ "$n" -ge 2 ]; then
                cat "$S/pool-data-after-$cid"
                printf '\n'
                exit 0
            fi
        fi
        if [ -f "$S/pool-data-$cid" ]; then cat "$S/pool-data-$cid"; printf '\n'; exit 0; fi
        printf 'no pool data is armed\n' >&2
        exit 1 ;;
    init)
        # The production Comet's `__constructor` is inert; `init` is the call
        # that sets the controller, and it is open to anyone — first one wins.
        # The stand-in models that: an answer appears only once some run has
        # actually initialized the instance.
        printf '%s' "$(argfn --controller "$@")" > "$S/controller-$cid"
        printf 'true\n'
        exit 0 ;;
    get_tokens)
        if [ -f "$S/tokens-$cid" ]; then cat "$S/tokens-$cid"; printf '\n'; exit 0; fi
        printf 'no tokens are armed\n' >&2
        exit 1 ;;
    proxy_oracle)
        if [ -f "$S/proxy-oracle-$cid" ]; then printf '"%s"\n' "$(cat "$S/proxy-oracle-$cid")"; exit 0; fi
        printf 'no bound proxy oracle\n' >&2
        exit 1 ;;
    deposit)
        if [ -f "$S/fail-deposit" ]; then printf 'error: refused\n' >&2; exit 1; fi
        printf 'true\n'
        exit 0 ;;
    get_proxy)
        # The runtime's `get_proxy` returns Option<ProxyConfig>: an asset it
        # does not serve is answered `null` with exit 0, exactly as the
        # network answers it. A non-zero exit is a failed read — a rate
        # limit, a timeout — which no caller may read as absence.
        if [ -f "$S/fail-get-proxy" ]; then printf 'error: rpc refused\n' >&2; exit 1; fi
        f="$S/proxy-$(printf '%s' "$(arg --asset "$@")" | tr -cd 'A-Za-z0-9')"
        if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
        printf 'null\n'
        exit 0 ;;
    queue_set_reserve)
        printf 'null\n'
        exit 0 ;;
    set_reserve)
        # The index the pool enabled the reserve at, armed by the test.
        cat "$S/set-reserve-index"; printf '\n'
        exit 0 ;;
    get_reserve)
        # The reserve as the pool reads it back, armed per asset id.
        f="$S/reserve-$(printf '%s' "$(arg --asset "$@")" | tr -cd 'A-Za-z0-9')"
        if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
        printf 'no such reserve\n' >&2
        exit 1 ;;
    set_status)
        # A status change is visible to the next `get_config`: the pool is
        # re-read after it is set, and must report what was asked for.
        printf '{"bstop_rate":0,"max_positions":4,"min_collateral":"10000000000","oracle":"%s","status":%s}\n' \
            "$(cat "$S/pool-oracle" 2>/dev/null)" "$(argfn --pool_status "$@")" > "$S/pool-config"
        printf 'null\n'
        exit 0 ;;
    deploy)
        # The factory's `deploy` returns the new pool's address, armed.
        printf '"%s"\n' "$(cat "$S/factory-deploy-result")"
        exit 0 ;;
    is_pool)
        printf 'true\n'
        exit 0 ;;
    set_freeze_status)
        printf 'true\n'
        exit 0 ;;
    propose_admin)
        # The pool's temporary `PropAdmin` entry: the proposed address and the
        # ledger its proposal expires at ten days out. The caller that reads it
        # back ignores the expiry, which is why the stand-in may pick one.
        printf '{"vec":[{"address":"%s"},{"u32":%s}]}' \
            "$(argfn --new_admin "$@")" "$(( @LEDGER@ + 172800 ))" > "$S/temp-$cid"
        printf 'null\n'
        exit 0 ;;
    accept_admin)
        if [ -f "$S/temp-$cid" ]; then
            sed -n 's/.*"address":"\([^"]*\)".*/\1/p' "$S/temp-$cid" > "$S/admin-$cid"
            rm -f "$S/temp-$cid"
        fi
        printf 'null\n'
        exit 0 ;;
    transfer_ownership)
        # The two-step transfer a source's own `accept_ownership` completes.
        printf '{"map":[{"key":{"symbol":"address"},"val":{"address":"%s"}},{"key":{"symbol":"live_until_ledger"},"val":{"u32":%s}}]}' \
            "$(argfn --new_owner "$@")" "$(argfn --live_until_ledger "$@")" > "$S/temp-$cid"
        printf 'null\n'
        exit 0 ;;
    accept_ownership)
        if [ -f "$S/temp-$cid" ]; then
            sed -n 's/.*"address":"\([^"]*\)".*/\1/p' "$S/temp-$cid" > "$S/owner-$cid"
            rm -f "$S/temp-$cid"
        fi
        printf 'null\n'
        exit 0 ;;
    set_controller)
        printf '%s' "$(argfn --manager "$@")" > "$S/controller-$cid"
        printf 'null\n'
        exit 0 ;;
    get_roles)
        # A JSON vector of the role names, which is how the CLI renders the
        # contract's own `Role` enum.
        printf '['
        first=1
        for role in $(roles_of "$cid" "$(argfn --account "$@")"); do
            [ "$first" = 1 ] || printf ','
            printf '"%s"' "$role"
            first=0
        done
        printf ']\n'
        exit 0 ;;
    get_total_supply)
        printf '"1000000000"\n'
        exit 0 ;;
    config)
        # A price adapter's own configuration, as far as a verifier reads it:
        # the runtime it asks, armed per contract id.
        if [ -f "$S/adapter-parent-$cid" ]; then
            printf '{"parent_oracle":"%s"}\n' "$(cat "$S/adapter-parent-$cid")"
            exit 0
        fi
        # A Lazer source's configuration, armed per contract id.
        if [ -f "$S/lazer-config-$cid" ]; then cat "$S/lazer-config-$cid"; printf '\n'; exit 0; fi
        printf 'no adapter configuration is armed\n' >&2
        exit 1 ;;
    decimals|resolution)
        # A source's own metadata: the pinned code reports it and nothing can
        # change it, so the lab graph reports a mismatch with `[oracle]`
        # instead of refusing. Armed per contract id, defaulting to 7 and 1.
        f="$S/$fn-$cid"
        if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
        case "$fn" in
            decimals) printf '7\n' ;;
            *) printf '1\n' ;;
        esac
        exit 0 ;;
    refresh)
        # The refresh call itself is not the proof: a runtime accepts it and
        # still reports a stale or unresolved price, which is exactly the
        # state the coordinate must refuse on.
        if [ -f "$S/fail-refresh" ]; then printf 'error: refused\n' >&2; exit 1; fi
        printf 'true\n'
        exit 0 ;;
    get_cached)
        # The runtime's cached price for one asset. A test arms
        # `cached-<slug>` with the answer the runtime gives; unarmed is a
        # quorum that has never resolved, which is what a refresh refuses on.
        f="$S/cached-$(printf '%s' "$(arg --asset "$@")" | tr -cd 'A-Za-z0-9')"
        if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
        printf '{"status":"Pending"}\n'
        exit 0 ;;
    next_proposal_id)
        if [ -f "$S/next-proposal-id" ]; then cat "$S/next-proposal-id"; else printf '0'; fi
        printf '\n'
        exit 0 ;;
    has_role)
        # One account's membership of one role, from the same state `get_roles`
        # reports: the two are the same question asked two ways, and a
        # stand-in that answered them differently would hide a whole class of
        # handoff bugs.
        if [ -f "$S/fail-has_role" ]; then printf 'error: unreachable\n' >&2; exit 1; fi
        for role in $(roles_of "$cid" "$(argfn --account "$@")"); do
            if [ "$role" = "$(argfn --role "$@")" ]; then printf 'true\n'; exit 0; fi
        done
        printf 'false\n'
        exit 0 ;;
    get_proposal)
        id=$(argfn --id "$@")
        f="$S/proposal-$id"
        if [ -f "$f" ]; then cat "$f"; printf '\n'; exit 0; fi
        printf 'no such proposal\n' >&2
        exit 1 ;;
    create_proposal|execute_proposal)
        if [ -f "$S/fail-$fn" ]; then printf 'error: refused\n' >&2; exit 1; fi
        if [ "$fn" = create_proposal ]; then
            id=$(argfn --id "$@")
            op=$(argfn --operation "$@")
            printf '{"id":%s,"ttl_ns":0,"operation":%s}' "$id" "$op" > "$S/proposal-$id"
        else
            # A proposal that executes changes the state it proposes. Only a
            # `SetRole` is modelled, because it is the only operation this
            # graph hands over; anything else is recorded and reported, which
            # is all its own readback asks of the stand-in.
            id=$(argfn --id "$@")
            op=
            if [ -f "$S/proposal-$id" ]; then op=$(cat "$S/proposal-$id"); fi
            case "$op" in
            *'"SetRole"'*)
                who=$(printf '%s' "$op" | sed -n 's/.*"SetRole":\["\([^"]*\)".*/\1/p')
                role=$(printf '%s' "$op" | sed -n 's/.*"SetRole":\["[^"]*","\([^"]*\)".*/\1/p')
                granted=$(printf '%s' "$op" | sed -n 's/.*,\(true\|false\).*/\1/p')
                now=$(roles_of "$cid" "$who")
                case " $now " in
                *" $role "*) ;;
                *)
                    if [ -n "$now" ]; then now="$now "; fi
                    now="$now$role"
                    ;;
                esac
                if [ "$granted" != true ]; then
                    kept=
                    for held in $now; do
                        if [ "$held" != "$role" ]; then
                            if [ -n "$kept" ]; then kept="$kept "; fi
                            kept="$kept$held"
                        fi
                    done
                    now=$kept
                fi
                printf '%s' "$now" > "$S/hasrole-$cid-$who"
                ;;
            esac
        fi
        printf 'true\n'
        exit 0 ;;
    esac
fi
printf 'the stand-in refuses this command\n' >&2
exit 1
"##;

    /// One real market prepared for a test: its configuration, its artifact
    /// bytes, its operator and issuer identities, and the ledger the stand-in
    /// answers from. `reuse_runtime` and `reuse_adapter` name the shared
    /// contracts a second pool in an existing group points at, which is how a
    /// market comes to deploy no proxy code of its own.
    struct RealMarket {
        out: PathBuf,
        stellar: PathBuf,
        state: PathBuf,
        call_log: PathBuf,
        admin: AccountId,
        issuers: [AccountId; 2],
        assets: [ContractId; 2],
        runtime: Option<ContractId>,
        governance: ContractId,
        sacs: [ContractId; 2],
        artifacts: BTreeMap<String, (PathBuf, Vec<u8>)>,
        cfg: MarketConfig,
        wasm_dir: Option<PathBuf>,
    }

    /// The ledger file the stand-in reads for one classic asset's trustline.
    fn trust_file(asset: &str) -> String {
        format!("trust-{}", asset.replace(':', "_"))
    }

    /// The same slug the stand-in computes for an asset key: its alphanumeric
    /// bytes, which is how a test arms the answer a runtime gives for it.
    fn asset_slug(key: &Value) -> String {
        key.to_string()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect()
    }

    /// The commands this graph sends that `mutating_calls` does not count:
    /// the classic transactions, which are no prefix of its list, and the
    /// network's own asset wrapper.
    const REAL_SENDS: [&str; 2] = ["tx new ", "contract asset deploy"];

    /// Every command the stand-in was asked to run that could have reached
    /// the ledger. A market simulation is `contract invoke --send no`, which
    /// changes nothing, so it is not a send.
    fn real_sends(log: &str) -> Vec<String> {
        log.lines()
            .filter(|line| {
                (mutating_calls(line).len() == 1
                    || REAL_SENDS.iter().any(|prefix| line.starts_with(prefix)))
                    && !(line.starts_with("contract invoke") && line.contains("--send no"))
            })
            .map(str::to_string)
            .collect()
    }

    /// The backing pool's code is the network's own production Wasm, so the
    /// fixture arms no pin for it — and a coordinate that places an instance
    /// needs a hash to place it from. Any armed pin serves: the instance is
    /// found by its salt, never by its bytes.
    fn arm_comet_pin(market: &mut RealMarket) {
        let pin = market
            .cfg
            .artifacts
            .get(&crate::manifest::ArtifactRole::Factory)
            .cloned()
            .expect("the fixture pins the roles this run deploys");
        market
            .cfg
            .artifacts
            .insert(crate::manifest::ArtifactRole::Comet, pin);
    }

    fn a_real_market(tag: &str, reuse_runtime: bool, reuse_adapter: bool) -> RealMarket {
        a_real_market_on(tag, reuse_runtime, reuse_adapter, MarketNetwork::Testnet, None, false)
    }
    fn a_real_market_lazer(tag: &str) -> RealMarket {
        a_real_market_on(tag, false, false, MarketNetwork::Testnet, None, true)
    }

    /// A digest-pinned market's qualified bytes live in a directory the
    /// operator supplied, not in the configuration; see `--wasm-dir`.
    fn a_real_market_with_wasm_dir(
        tag: &str,
        reuse_runtime: bool,
        reuse_adapter: bool,
        dir: &Path,
    ) -> RealMarket {
        a_real_market_on(
            tag,
            reuse_runtime,
            reuse_adapter,
            MarketNetwork::Testnet,
            Some(dir),
            false,
        )
    }

    /// One real market on a chosen network. The testnet form is
    /// `a_real_market`; the mainnet form freezes the public-network
    /// passphrase into both the configuration and the stand-in's own
    /// `network info`, which is the only way a test can prove a run sends
    /// on the network its plan froze.
    fn a_real_market_on(
        tag: &str,
        reuse_runtime: bool,
        reuse_adapter: bool,
        network: MarketNetwork,
        wasm_dir: Option<&Path>,
        lazer: bool,
    ) -> RealMarket {
        let passphrase = match network {
            MarketNetwork::Testnet => MARKET_TESTNET_PASSPHRASE,
            MarketNetwork::Mainnet => crate::manifest::MARKET_MAINNET_PASSPHRASE,
        };
        let out = std::env::temp_dir().join(format!("abtest-real-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let state = out.join("state");
        std::fs::create_dir_all(&state).expect("ledger directory");
        let cfg_dir = out.join("cfg");
        std::fs::create_dir_all(&cfg_dir).expect("config directory");

        let admin = fixture_actor(0);
        let issuers = [fixture_actor(11), fixture_actor(12)];
        let assets = [fixture_contract(40), fixture_contract(41)];
        let sep40 = [fixture_contract(42), fixture_contract(43)];
        let runtime = fixture_contract(44);
        let governance = fixture_contract(45);
        let adapter = fixture_contract(46);
        let sacs = [fixture_contract(47), fixture_contract(48)];

        // The identities a real run signs with exist in its own keystore
        // before the graph asks for them.
        for (name, address) in [
            ("market-admin".to_string(), admin.as_str().to_string()),
            (MarketDeployer::issuer_key(0), issuers[0].as_str().to_string()),
            (MarketDeployer::issuer_key(1), issuers[1].as_str().to_string()),
        ] {
            std::fs::write(state.join(format!("key-{name}")), &address)
                .expect("armed an identity");
        }
        for (leg, code) in [(0usize, REAL_BLND_CODE), (1usize, REAL_USDC_CODE)] {
            let asset = crate::stellar::classic_asset(code, &issuers[leg]);
            std::fs::write(
                state.join(format!("sac-{}", asset.replace(':', "_"))),
                sacs[leg].as_str(),
            )
            .expect("armed an asset contract");
        }
        if reuse_runtime {
            // The shared stack is a runtime and a governance of this exact
            // pinned code: the stand-in refuses any function their ABI does
            // not export, as the network does.
            std::fs::write(state.join(format!("role-{runtime}")), "runtime")
                .expect("armed a runtime role");
            std::fs::write(state.join(format!("role-{governance}")), "governance")
                .expect("armed a governance role");
            std::fs::write(state.join("owner"), governance.as_str())
                .expect("armed a runtime owner");
        }

        // The sep-40 oracles this market quotes through are not this run's
        // to deploy, so they must simply be there: preflight asks for each
        // before any XLM is spent, and an armed contract answers a fetch.
        for oracle in &sep40 {
            std::fs::write(
                state.join(format!("contract-{}", oracle.as_str())),
                b"fixture sep-40 oracle wasm",
            )
            .expect("armed a referenced oracle");
        }

        if lazer {
            // The named verifier is a contract on the network, so the
            // preflight's reference check finds code at its address.
            std::fs::write(
                state.join(format!("contract-{}", fixture_contract(52).as_str())),
                b"fixture lazer verifier wasm",
            )
            .expect("armed the verifier");
        }

        // A market deploys only the code it owns: reusing a runtime drops its
        // runtime and governance from what this run may even upload.
        let mut roles: Vec<&'static str> = crate::manifest::MARKET_REAL_ARTIFACT_ROLES.to_vec();
        if lazer {
            // A market that names Lazer sources owes its source contract's
            // code as a required artifact, exactly as on the network.
            roles.push(crate::manifest::MARKET_LAZER_SOURCE_ROLE);
        }
        if reuse_runtime {
            roles.retain(|role| !matches!(*role, "proxy-runtime" | "proxy-governance"));
        }
        let mut text = String::from(&format!(
            "[network]\npassphrase = \"{passphrase}\"\nlabel = \"{}\"\n\n",
            match network {
                MarketNetwork::Testnet => "testnet",
                MarketNetwork::Mainnet => "mainnet",
            },
        ));
        let mut artifacts = BTreeMap::new();
        for role in &roles {
            let bytes = format!("fixture wasm bytes for {role}").into_bytes();
            // A `--wasm-dir` market is pinned by digest alone: the qualified
            // bytes sit in the operator's directory under the file name the
            // launch profile itself uses, and only the digest is authority.
            let path = match wasm_dir {
                Some(dir) => dir.join(
                    crate::manifest::launch_wasm_file(role)
                        .map_or_else(|| format!("{role}.wasm"), |entry| entry.asset.to_string()),
                ),
                None => cfg_dir.join(format!("{role}.wasm")),
            };
            std::fs::write(&path, &bytes).expect("fixture artifact");
            match wasm_dir {
                Some(_) => text.push_str(&format!(
                    "[artifacts.{role}]\nwasm_hash = \"{}\"\n\n",
                    sha256_hex(&bytes)
                )),
                None => text.push_str(&format!(
                    "[artifacts.{role}]\npath = \"{}\"\nsha256 = \"{}\"\n\n",
                    path.display(),
                    sha256_hex(&bytes)
                )),
            }
            artifacts.insert((*role).to_string(), (path, bytes));
        }
        // An upload answers with its content address. A path-pinned market's
        // code is also already fetchable on the network (a live pin's
        // preflight proof); a `--wasm-dir` market models a first deploy, whose
        // network holds none of it until this run uploads it.
        for (path, bytes) in artifacts.values() {
            let hash = sha256_hex(bytes);
            if wasm_dir.is_none() {
                std::fs::write(state.join(format!("contract-hash-{hash}")), bytes)
                    .expect("armed fetchable code");
            }
            std::fs::write(
                state.join(format!(
                    "upload-{}",
                    path.display().to_string().replace(['/', '.'], "_")
                )),
                &hash,
            )
            .expect("armed an upload answer");
        }
        text.push_str(&format!(
            "[oracle]\nsources = 3\nmin_sources = 1\nmax_age_secs = 300\nmax_clock_drift_secs = 0\nbase = \"USD\"\ndecimals = 7\nresolution = 300\nbase_asset = \"{}\"\n\n[adapter]\ndecimals = 7\nresolution = 1\nbase = \"USD\"\n\n[shim]\nblnd_code = \"{}\"\nusdc_code = \"{}\"\n\n[pool]\nname = \"Abtest Real Market\"\nadmin = \"{}\"\nbackstop_take_rate = 0\nmax_positions = 4\nmin_collateral = \"10000000000\"\ntarget_status = 0\n{}",
            assets[0].as_str(),
            REAL_BLND_CODE,
            REAL_USDC_CODE,
            admin.as_str(),
            if lazer {
                format!(
                    "lazer_verifier = \"{}\"\n",
                    fixture_contract(52).as_str()
                )
            } else {
                String::new()
            },
        ));
        if reuse_runtime {
            text.push_str(&format!("runtime = \"{}\"\n", runtime.as_str()));
        }
        if reuse_adapter {
            text.push_str(&format!("adapter = \"{}\"\n", adapter.as_str()));
        }
        text.push('\n');
        for (index, (asset, label, code)) in [
            (assets[0].as_str(), "usdc", "USDC"),
            (assets[1].as_str(), "xlm", "XLM"),
        ]
        .into_iter()
        .enumerate()
        {
            text.push_str(&format!(
                "[[pool.reserves]]\nlabel = \"{label}\"\nasset = \"{asset}\"\ndecimals = 7\nc_factor = 9000000\nl_factor = 9500000\nutil = 8500000\nmax_util = 9500000\nsupply_cap = \"1000000000000000000\"\nmin_position = \"10000000\"\nr_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\nenabled = true\nmin_sources = 1\nmax_age_secs = 300\n\n{}",
                if index == 0 && lazer {
                    format!(
                        "[[pool.reserves.sources]]\nkind = \"lazer\"\nfeed_id = \"{}\"\n\n",
                        7 + 16 * index
                    )
                } else {
                    String::new()
                },
            ));
            text.push_str(&format!(
                "[[pool.reserves.sources]]\nkind = \"sep40\"\ncontract = \"{}\"\nasset = {{ Other = \"{code}\" }}\n\n",
                sep40[index].as_str(),
            ));
        }
        let config = cfg_dir.join("config.toml");
        std::fs::write(&config, &text).expect("fixture config");
        let cfg = MarketConfig::load(&config)
            .expect("a real market fixture must load through the strict parser");

        let stellar = out.join("stellar");
        let script = REAL_STELLAR
            .replace("@LOG@", &out.join("stellar-calls.log").display().to_string())
            .replace("@STATE@", &state.display().to_string())
            .replace("@PASSPHRASE@", passphrase)
            .replace("@BASE@", "USD")
            .replace("@ADMIN@", &out.join("admin").display().to_string())
            .replace("@LEDGER@", "1234567");
        std::fs::write(&stellar, script).expect("stand-in script");
        std::fs::set_permissions(&stellar, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("the stand-in must be executable");

        RealMarket {
            call_log: out.join("stellar-calls.log"),
            artifacts,
            admin,
            assets,
            cfg,
            wasm_dir: wasm_dir.map(Path::to_path_buf),
            governance,
            issuers,
            out,
            runtime: reuse_runtime.then_some(runtime),
            sacs,
            state,
            stellar,
        }
    }

    impl RealMarket {
        fn calls(&self) -> String {
            std::fs::read_to_string(&self.call_log).unwrap_or_default()
        }

        fn journal(&self) -> MarketJournal {
            MarketJournal::open(&self.out)
        }

        fn context(&self, alias: &str) -> MarketExecutionContext {
            fixture_market_context(&self.out, &self.stellar, alias)
        }

        /// The context an operator on the public network would start this
        /// run with: the same keystore directories, bound to mainnet.
        fn mainnet_context(&self, alias: &str) -> MarketExecutionContext {
            self.context(alias).with_network(MarketNetwork::Mainnet)
        }

        /// Arm one ledger fact the stand-in answers from.
        fn arm(&self, name: &str, text: &str) {
            std::fs::write(self.state.join(name), text).expect("armed the stand-in");
        }

        fn disarm(&self, name: &str) {
            let _ = std::fs::remove_file(self.state.join(name));
        }

        /// Arm the code the chain holds at one contract id. The bytes hash
        /// to whatever digest the test chose; the digest a coordinate
        /// declared is what a reconciliation re-hashes them against.
        fn code_at(&self, id: &ContractId, bytes: &[u8]) {
            std::fs::write(self.state.join(format!("contract-{}", id.as_str())), bytes)
                .expect("armed installed code");
        }

        /// What the backstop reports about one pool: its held `tokens`, the
        /// reserves behind them, and the queued-withdrawal figure. All three
        /// are 7-decimal stroops, exactly as the network's `pool_data`
        /// answers.
        fn backstop_data(&self, backstop: &ContractId, json: &str) {
            self.arm(&format!("pool-data-{}", backstop.as_str()), json);
        }

        /// Arm the id every prediction answers with before a gate opens. A
        /// prediction is made while the gate is being prepared, so a test
        /// that wants one specific id arms it here, then reads back what
        /// the gate bound from the record it frozen.
        fn arm_predict(&self, id: &ContractId) {
            self.arm("predicted", id.as_str());
        }

        /// Say what this run has already deployed. In a real run the map is
        /// rebuilt from settled custody records; a test that starts at a
        /// later coordinate states it directly.
        fn arm_deployed<'a>(
            &self,
            d: &mut MarketDeployer,
            known: impl IntoIterator<Item = (&'a str, ContractId)>,
        ) {
            for (role, id) in known {
                d.deployed.insert(role.to_string(), id);
            }
        }

        /// Open the confirmation gate for one coordinate, exactly as a run
        /// would, and hand back the custody record it froze — without
        /// reserving or sending anything. The caller books it with
        /// `journal.begin` to reproduce a process lost after the send.
        fn open_gate(&self, op: &MOp) -> (MSlot, MarketCustodyRecord, MarketDeployer) {
            self.open_gate_armed(op, [])
        }

        fn open_gate_armed<'a>(
            &self,
            op: &MOp,
            known: impl IntoIterator<Item = (&'a str, ContractId)>,
        ) -> (MSlot, MarketCustodyRecord, MarketDeployer) {
            let slot = self.slot(op);
            let mut d = self.run(false);
            self.arm_deployed(&mut d, known);
            match d
                .prepare_gate(&slot)
                .unwrap_or_else(|e| panic!("the gate for {op:?} would not open: {e:#}\n{}", self.calls()))
            {
                DeployEvent::Confirm { .. } => {}
                other => panic!("opening the gate for {op:?} gave {other:?}"),
            }
            let record = d
                .gate
                .as_ref()
                .unwrap_or_else(|| panic!("no gate is open for {op:?}"))
                .record
                .clone();
            (slot, record, d)
        }

        /// Take a coordinate the whole way a run would: open its gate,
        /// confirm the frozen summary, and execute it once. A coordinate
        /// the truth rejects answers with an error rather than a panic, so
        /// a test can assert on the refusal itself.
        fn confirm(&self, d: &mut MarketDeployer, op: &MOp) -> Result<(), anyhow::Error> {
            let slot = self.slot(op);
            let token = match d.prepare_gate(&slot)? {
                DeployEvent::Confirm { token, .. } => token,
                other => anyhow::bail!("opening the gate for {op:?} gave {other:?}"),
            };
            d.execute_gate(&Gate {
                phase: slot.phase,
                slot: slot.slot,
                token,
                record: d
                    .gate
                    .as_ref()
                    .unwrap_or_else(|| panic!("no gate is open for {op:?}"))
                    .record
                    .clone(),
                summary: Safe::literal("the fixture operator confirmed this coordinate"),
            })
            .map(|_| ())
        }

        /// The graph this market executes under one activation intent, derived
        /// exactly the way a run derives it.
        fn schedule(&self, activate: bool) -> Vec<MSlot> {
            let mut shape = RealShape::from_config(&self.cfg)
                .expect("a real configuration derives its own shape");
            shape.activate = activate;
            shape.handoff = RealHandoff::owed(&self.cfg, Some(self.admin.as_str()));
            real_market_schedule(&self.cfg, &shape)
        }

        fn slot(&self, op: &MOp) -> MSlot {
            self.schedule(false)
                .into_iter()
                .find(|s| &s.op == op)
                .unwrap_or_else(|| panic!("the graph schedules no {op:?} coordinate"))
        }

        /// Start (or resume) this market's run as the operator would.
        fn run(&self, activate: bool) -> MarketDeployer {
            MarketDeployer::with_options(
                self.cfg.clone(),
                self.context("market-admin"),
                self.journal(),
                MarketDeployOptions {
                    activate,
                    wasm_dir: self.wasm_dir.clone(),
                },
            )
            .expect("a real market whose preflight passes constructs")
        }

        /// Execute one scheduled coordinate through the engine's own gate path:
        /// the custody record is frozen and bound before the send, and the
        /// settlement is written by the strict journal writer.
        fn execute(&self, d: &mut MarketDeployer, slot: &MSlot, signer: &AccountId, target: Option<ContractId>) {
            let mut record = MarketCustodyRecord {
                phase: slot.phase,
                kind: slot.kind,
                action: Safe::literal(slot.action),
                network: MarketNetwork::Testnet,
                signer: signer.clone(),
                payload_digest: Sha256Hex::default(),
                target: target.clone(),
                artifact_digest: None,
                predicted: target,
                pre_state: vec![Safe::literal(
                    "no settled result exists at this coordinate; nothing has been replayed",
                )],
                tool_error: None,
            };
            record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
            let gate = Gate {
                phase: slot.phase,
                slot: slot.slot,
                token: String::new(),
                record,
                summary: Safe::literal("the fixture operator confirmed this coordinate"),
            };
            d.execute_gate(&gate)
                .unwrap_or_else(|e| panic!("{} failed: {e:#}", slot.action));
        }

        /// The operator-signed coordinates of the activation shim's legs; the
        /// mint and the lock belong to the leg's issuer and nobody else.
        fn leg_signer(&self, op: &MOp, leg: usize) -> AccountId {
            match op {
                MOp::MintLeg { .. } | MOp::LockIssuer { .. } => self.issuers[leg].clone(),
                _ => self.admin.clone(),
            }
        }

        fn settled(&self, slot: &MSlot) -> bool {
            self.journal()
                .result(slot.phase, slot.slot)
                .expect("the coordinate is readable")
                .is_some()
        }

        fn clean(&self) {
            let _ = std::fs::remove_dir_all(&self.out);
        }
    }

    /// Render one graph the way an operator would read it: coordinate, action,
    /// operation. Two runs of one configuration must render identically.
    fn render_graph(schedule: &[MSlot]) -> Vec<String> {
        schedule
            .iter()
            .map(|s| format!("{:03}-{:02} {} {:?}", s.phase, s.slot, s.action, s.op))
            .collect()
    }

    fn graph_has(schedule: &[MSlot], op: &MOp) -> bool {
        schedule.iter().any(|s| &s.op == op)
    }

    /// One configuration is one graph. Building it twice from the same
    /// configuration yields the same coordinates, the same salt table, and the
    /// same digest — and the graph deploys the proxy code only when the pool
    /// does not already have a runtime to share.
    #[test]
    fn a_real_graph_is_the_same_graph_twice_and_owns_only_the_code_it_deploys() {
        let owned = a_real_market("schedule-determinism", false, false);
        let first = owned.schedule(false);
        let second = owned.schedule(false);
        assert!(!first.is_empty(), "a real market must schedule a graph");
        assert_eq!(
            render_graph(&first),
            render_graph(&second),
            "one configuration produced two graphs"
        );
        let nonce = 0x0123_4567_89ab_cdef;
        assert_eq!(
            market_salt_triples(&first, nonce),
            market_salt_triples(&second, nonce),
            "two builds of one graph would deploy from different salts"
        );
        assert_eq!(
            market_config_digest(&owned.cfg).expect("the configuration canonicalizes"),
            market_config_digest(&owned.cfg).expect("the configuration canonicalizes"),
            "the digest a plan is bound to must not drift between readings"
        );
        let coordinates: std::collections::BTreeSet<(u32, u32)> =
            first.iter().map(|s| (s.phase, s.slot)).collect();
        assert_eq!(
            coordinates.len(),
            first.len(),
            "two mutations share one journal coordinate"
        );
        for slot in &first {
            assert!(
                market_coordinate_is_scanned(slot.phase, slot.slot),
                "coordinate ({}, {}) would be invisible to the no-replay scan",
                slot.phase,
                slot.slot
            );
        }

        // A market that owns its proxy stack deploys it; a market that reuses
        // a runtime deploys no proxy code at all and verifies what it shares.
        assert!(graph_has(&first, &MOp::RealDeployRuntime));
        assert!(graph_has(&first, &MOp::RealDeployGovernance));
        assert!(!graph_has(&first, &MOp::RealVerifyRuntime));
        let uploads = |schedule: &[MSlot]| -> Vec<&'static str> {
            schedule
                .iter()
                .filter_map(|s| match &s.op {
                    MOp::Upload { role } => Some(*role),
                    _ => None,
                })
                .collect()
        };
        assert!(uploads(&first).contains(&"proxy-runtime"));

        let shared = a_real_market("schedule-reuse", true, true);
        let reused = shared.schedule(false);
        assert!(
            !graph_has(&reused, &MOp::RealDeployRuntime),
            "a market that reuses a runtime deploys no runtime of its own"
        );
        assert!(
            !graph_has(&reused, &MOp::RealDeployGovernance),
            "a market that reuses a runtime deploys no governance of its own"
        );
        assert!(
            graph_has(&reused, &MOp::RealVerifyRuntime),
            "the runtime this pool shares is verified instead of deployed"
        );
        assert!(
            !uploads(&reused).iter().any(|role| matches!(*role, "proxy-runtime" | "proxy-governance")),
            "code this run does not deploy is never uploaded"
        );
        owned.clean();
        shared.clean();
    }

    /// `--activate` belongs to the deployment, not to the running of it: it
    /// adds the activation coordinate, it is frozen with the plan, and a
    /// resume under the other intent is a different deployment — refused,
    /// never quietly re-planned.
    #[test]
    fn the_activation_intent_is_part_of_the_frozen_graph() {
        let market = a_real_market("activate-intent", false, false);
        let unactivated = market.schedule(false);
        let activated = market.schedule(true);
        let status = MOp::RealSetStatus { to: 0 };
        assert!(
            !graph_has(&unactivated, &status),
            "a run without --activate leaves the pool in its creation status"
        );
        let activation = activated
            .iter()
            .find(|s| s.op == status)
            .expect("--activate schedules the pool's set_status(0)")
            .clone();
        assert_eq!(
            unactivated.len() + 1,
            activated.len(),
            "the activation intent adds exactly one coordinate"
        );
        let without_activation: Vec<String> = render_graph(&activated)
            .into_iter()
            .zip(activated.iter())
            .filter(|(_, s)| s.op != status)
            .map(|(line, _)| line)
            .collect();
        assert_eq!(
            render_graph(&unactivated),
            without_activation,
            "activating changes nothing but the activation"
        );

        let d = market.run(true);
        assert!(d.terminal.is_none(), "an activated run starts");
        let plan = read_deployment_plan(&market.out)
            .expect("the plan is readable")
            .expect("the first run froze a plan");
        assert!(
            plan.slots.contains(&(activation.phase, activation.slot)),
            "the frozen plan carries the activation coordinate"
        );

        let resumed = market.run(false);
        assert!(
            matches!(resumed.terminal, Some(DeployEvent::Failed(_))),
            "a run resumed under the other activation intent is refused, not re-planned"
        );
        let same_intent = market.run(true);
        assert!(
            same_intent.terminal.is_none(),
            "the intent the plan froze still resumes"
        );
        assert!(
            mutating_calls(&market.calls()).is_empty(),
            "planning and refusing submitted nothing: {:?}",
            mutating_calls(&market.calls())
        );
        market.clean();
    }

    /// The activation shim is one sequence, and it is executed once: issuer,
    /// trustline, asset contract, mint, and lock each submit exactly one send,
    /// each settles in the journal, and a second pump over the same
    /// coordinates needs no send at all. The issuer a resume reports for leg 0
    /// is the account this run created, read from the record of the coordinate
    /// that created it.
    #[test]
    fn the_shim_leg_is_created_trusted_minted_and_locked_exactly_once() {
        let market = a_real_market("leg-once", false, false);
        let leg = 0usize;
        let asset = crate::stellar::classic_asset(REAL_BLND_CODE, &market.issuers[leg]);
        let operations = [
            MOp::CreateIssuer { leg },
            MOp::TrustLeg { leg },
            MOp::DeploySac { leg },
            MOp::MintLeg { leg },
            MOp::LockIssuer { leg },
        ];
        let slots: Vec<MSlot> = operations.iter().map(|op| market.slot(op)).collect();
        let target_for = |op: &MOp| match op {
            MOp::DeploySac { .. } => Some(market.sacs[leg].clone()),
            _ => None,
        };

        let mut d = market.run(false);
        for slot in &slots {
            let signer = market.leg_signer(&slot.op, leg);
            market.execute(&mut d, slot, &signer, target_for(&slot.op));
        }
        for slot in &slots {
            assert!(market.settled(slot), "{} left no settlement", slot.action);
        }

        let sends = real_sends(&market.calls());
        assert_eq!(sends.len(), 5, "one send per coordinate, no more: {sends:?}");
        let issued = |prefix: &str| -> bool { sends.iter().any(|c| c.starts_with(prefix)) };
        assert!(
            sends.iter().any(|c| c.starts_with("tx new create-account")
                && c.contains(market.issuers[leg].as_str())),
            "the leg's issuer was never funded: {sends:?}"
        );
        assert!(issued("tx new change-trust") && sends.iter().any(|c| c.contains(&asset)), "no trustline was extended for the leg: {sends:?}");
        assert!(
            issued("contract asset deploy"),
            "the leg's asset contract was never deployed: {sends:?}"
        );
        let (blnd, usdc) = crate::manifest::market_shim_seed();
        let seed = if leg == 0 { blnd } else { usdc };
        assert!(
            sends.iter().any(|c| c.starts_with("tx new payment")
                && c.contains(&asset)
                && c.contains(&seed.to_string())),
            "the leg's backing was never minted to the operator: {sends:?}"
        );
        assert!(
            sends.iter().any(|c| {
                c.starts_with("tx new set-options")
                    && c.contains("--master-weight")
                    && c.contains(market.issuers[leg].as_str())
            }),
            "the leg's issuer was never locked: {sends:?}"
        );

        // The fact only the journal can carry: the issuer this run created.
        let facts = d
            .settled_facts(slots[0].phase, slots[0].slot)
            .expect("the creating coordinate is readable")
            .expect("the creating coordinate recorded the account it created");
        assert_eq!(
            facts["issuer"].as_str(),
            Some(market.issuers[leg].as_str()),
            "the journal records an issuer this run did not create"
        );

        // A second pump: every coordinate is settled, so nothing is sent.
        let before = real_sends(&market.calls());
        let mut resumed = market.run(false);
        for slot in &slots {
            let signer = market.leg_signer(&slot.op, leg);
            market.execute(&mut resumed, slot, &signer, target_for(&slot.op));
        }
        assert_eq!(
            real_sends(&market.calls()),
            before,
            "a settled leg was submitted a second time"
        );
        assert_eq!(
            resumed
                .issuer_of(leg)
                .expect("a resumed run finds its issuer in the journal"),
            market.issuers[leg],
            "the resumed run's leg 0 issuer is not the account the run created"
        );
        market.clean();
    }

    /// A mint whose submission says nothing is never replayed. The intent
    /// stays open, and the next run settles it from the balance the same
    /// readback asks about — one record, one send.
    #[test]
    fn an_ambiguous_mint_is_reconciled_by_readback_and_never_resent() {
        let market = a_real_market("mint-ambiguous", false, false);
        let leg = 0usize;
        let asset = crate::stellar::classic_asset(REAL_BLND_CODE, &market.issuers[leg]);
        let mut d = market.run(false);
        for op in [MOp::CreateIssuer { leg }, MOp::TrustLeg { leg }] {
            let slot = market.slot(&op);
            market.execute(&mut d, &slot, &market.admin, None);
        }

        // The submission is ambiguous: the network reports nothing definitive
        // and the operator's balance never moved.
        market.arm("fail-payment", "");
        let mint = market.slot(&MOp::MintLeg { leg });
        let gate_error = {
            let mut record = MarketCustodyRecord {
                phase: mint.phase,
                kind: mint.kind,
                action: Safe::literal(mint.action),
                network: MarketNetwork::Testnet,
                signer: market.issuers[leg].clone(),
                payload_digest: Sha256Hex::default(),
                target: None,
                artifact_digest: None,
                predicted: None,
                pre_state: vec![Safe::literal(
                    "no settled result exists at this coordinate; nothing has been replayed",
                )],
                tool_error: None,
            };
            record.payload_digest = custody_payload_digest(&record).expect("fixture digest");
            d.execute_gate(&Gate {
                phase: mint.phase,
                slot: mint.slot,
                token: String::new(),
                record,
                summary: Safe::literal("the mint"),
            })
            .err()
        };
        assert!(gate_error.is_some(), "an ambiguous mint is not a settled coordinate");
        let journal = market.journal();
        assert!(
            journal.read_intent(mint.phase, mint.slot).expect("readable").is_some(),
            "the mint's intent is held open for reconciliation"
        );
        assert!(
            journal.result(mint.phase, mint.slot).expect("readable").is_none(),
            "an unexplained submission never reads as settled"
        );
        let sent = |log: String| -> usize {
            real_sends(&log)
                .into_iter()
                .filter(|c| c.starts_with("tx new payment") && c.contains(&asset))
                .count()
        };
        assert_eq!(sent(market.calls()), 1, "the mint was submitted exactly once");

        // The readback says the mint landed. A resume proves it; it does not
        // ask the network to pay again.
        let (blnd, _) = crate::manifest::market_shim_seed();
        market.arm(&trust_file(&asset), &blnd.to_string());
        market.disarm("fail-payment");
        let resumed = market.run(false);
        assert!(
            resumed.terminal.is_none(),
            "a mint reconciled from the ledger does not abandon the run"
        );
        assert!(
            market.settled(&mint),
            "the open mint settled from the balance its submission alone explains"
        );
        assert_eq!(
            sent(market.calls()),
            1,
            "a reconciled mint was submitted a second time"
        );
        market.clean();
    }

    /// Preflight proves the bytes a run will submit. A pinned artifact whose
    /// bytes are not there is a refusal that reaches no ledger, and the same
    /// run proceeds once the bytes are present.
    #[test]
    fn missing_artifact_bytes_refuse_the_run_before_any_send() {
        let market = a_real_market("artifact-bytes", false, false);
        let (path, bytes) = market.artifacts["pool"].clone();
        std::fs::remove_file(&path).expect("the pinned artifact is removed");

        let refused = MarketDeployer::with_options(
            market.cfg.clone(),
            market.context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        )
        .expect("a refused preflight is a result, not a panic");
        assert!(
            matches!(refused.terminal, Some(DeployEvent::Failed(_))),
            "bytes that are not there are a refusal, not a run"
        );
        assert!(
            read_deployment_plan(&market.out).expect("readable").is_none(),
            "a run that proved nothing binds no plan"
        );
        assert!(
            mutating_calls(&market.calls()).is_empty(),
            "a refused preflight reached the ledger: {:?}",
            mutating_calls(&market.calls())
        );

        std::fs::write(&path, &bytes).expect("the bytes are present again");
        let accepted = MarketDeployer::with_options(
            market.cfg.clone(),
            market.context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        )
        .expect("the same run with its bytes constructs");
        assert!(
            accepted.terminal.is_none(),
            "the same configuration proceeds once its bytes are present"
        );
        assert!(
            mutating_calls(&market.calls()).is_empty(),
            "preflight is read-only: {:?}",
            mutating_calls(&market.calls())
        );
        market.clean();
    }

    /// A handoff target that does not exist on the network is refused at
    /// preflight: the run would only meet it in the handoff phase, by which
    /// point every upload, both locked issuers, the Comet initialization and
    /// the backstop deposit are paid for, and nobody could accept the roles.
    #[test]
    fn a_handoff_target_absent_from_the_network_refuses_the_run_at_preflight() {
        let mut market = a_real_market("pool-admin", false, false);
        let foreign = fixture_actor(9);
        market
            .cfg
            .pool
            .as_mut()
            .expect("a real market has a pool")
            .admin = foreign.as_str().to_string();

        let refused = MarketDeployer::with_options(
            market.cfg.clone(),
            market.context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        )
        .expect("a refused preflight is a result, not a panic");
        let Some(DeployEvent::Failed(note)) = &refused.terminal else {
            market.clean();
            panic!("a pool owned by another key is a refusal, not a run: {:?}", refused.terminal);
        };
        // The configuration's final admin is no longer asked to *be* the
        // operator — a curator deploys a pool it is about to hand over. What
        // preflight still refuses is a handoff to an address that is not on
        // this network, which the run would otherwise discover at phase 11.
        let note = note.to_string();
        assert!(
            note.contains("does not exist on this network")
                && note.contains(foreign.as_str()),
            "the refusal must name the handoff target it could not find: {note}"
        );
        assert!(
            read_deployment_plan(&market.out).expect("readable").is_none(),
            "a run that never reached its operator binds no plan"
        );
        assert!(
            mutating_calls(&market.calls()).is_empty(),
            "a refused pool admin reached the ledger: {:?}",
            mutating_calls(&market.calls())
        );
        market.clean();
    }

    /// A pool whose final admin is another key is a legitimate deployment:
    /// the run deploys it and hands it over at phase 11. Only an admin that
    /// is not on the network at all is refused, and an account is proved by
    /// the ledger entry it has.
    #[test]
    fn a_pool_admin_that_exists_elsewhere_is_deployed_and_handed_over() {
        let mut market = a_real_market("pool-admin-curated", false, false);
        let foreign = fixture_actor(9);
        market.arm(&format!("account-{}", foreign.as_str()), "open");
        market
            .cfg
            .pool
            .as_mut()
            .expect("a real market has a pool")
            .admin = foreign.as_str().to_string();

        let run = market.run(false);
        assert!(
            run.terminal.is_none(),
            "a pool owned by a curator that exists is a run, not a refusal: {:?}",
            run.terminal
        );
        let slots = market.schedule(false);
        assert!(
            slots.iter().any(|slot| slot.phase == 11
                && matches!(&slot.op, MOp::RealHandoffPool)),
            "a pool owed to another key schedules its handoff: {slots:?}"
        );
        assert!(
            slots.iter().any(|slot| slot.phase == 11
                && matches!(&slot.op, MOp::RealHandoffComet)),
            "the backing pool's controller follows the pool's admin: {slots:?}"
        );
        market.clean();
    }

    /// A configuration written before a curator could hold a pool — its admin
    /// is the operator, and it names no oracle admin — is the graph it always
    /// was: no phase-11 coordinate exists anywhere in it.
    #[test]
    fn a_pool_whose_admin_is_the_operator_schedules_no_handoff() {
        let market = a_real_market("pool-admin-self", false, false);
        let slots = market.schedule(false);
        assert!(
            !slots.iter().any(|slot| slot.phase == 11),
            "a run that hands nothing over schedules no handoff: {slots:?}"
        );
        market.clean();
    }

    /// A run that owes a pool a new admin proposes it, and the proposal is
    /// proved by the one thing the pool keeps of it: the temporary entry
    /// naming the proposed address. Nothing settles on the invoke's word.
    #[test]
    fn the_pool_handoff_proposes_its_admin_and_the_readback_proves_the_proposal() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-pool", &foreign);
        let pool = fixture_contract(87);
        let (record, ev) = confirm_through_gate(&market, &MOp::RealHandoffPool, vec![("pool", pool.clone())]);
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "the admin proposal must settle: {ev:?}\n{}",
            market.calls()
        );
        assert_eq!(record.target.as_ref(), Some(&pool));
        let calls = market.calls();
        assert!(
            calls.contains(&format!("--new_admin {}", foreign.as_str())),
            "the pool was never asked to propose this run's admin: {calls}"
        );
        assert!(
            calls.contains("propose_admin") && calls.contains("PropAdmin"),
            "the proposal is proved by the pool's own temporary entry: {calls}"
        );
        // The signature list belongs to a finished run and to no other: a
        // coordinate the graph has not reached yet is not an acceptance the
        // curator can sign, and a partial list printed as if it were the
        // whole one would tell a driver the market is fully handed over when
        // it is not.
        assert!(
            market.run(false).handoff_acceptance().is_none(),
            "an unfinished run has no signature list to print"
        );
        market.clean();
    }

    /// The pool keeps one proposed admin and stamps it with the ledger its
    /// proposal expires at. A live proposal that already names this run's
    /// target is this run's own work: proposing again would move a deadline
    /// nobody asked to move and re-start the curator's ten days.
    #[test]
    fn a_pool_that_already_proposes_this_runs_admin_proposes_it_no_again() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-pool-open", &foreign);
        let pool = fixture_contract(87);
        // The stand-in's latest ledger is 1234567: this proposal is still live.
        market.arm(
            &format!("temp-{}", pool.as_str()),
            &format!("{{\"vec\":[{{\"address\":\"{}\"}},{{\"u32\":9999999}}]}}", foreign.as_str()),
        );
        let (_, ev) = confirm_through_gate(&market, &MOp::RealHandoffPool, vec![("pool", pool.clone())]);
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "an open proposal must settle as the work it is: {ev:?}\n{}",
            market.calls()
        );
        let calls = market.calls();
        assert!(
            !calls.contains("propose_admin"),
            "an open proposal is not a second proposal: {calls}"
        );
        // What the coordinate records is the signature the curator still owes,
        // named by contract, method and signer: the same entry the finished
        // run's list is made of, so a driver that reaches `done` can tell
        // "deployed" from "deployed and waiting for someone else to sign".
        let facts = market
            .run(false)
            .settled_facts(11, 1)
            .expect("the handoff coordinate is settled")
            .expect("a settled coordinate records what it settled");
        assert_eq!(
            facts["accept"],
            json!({"contract": pool.as_str(), "method": "accept_admin",
                   "by": foreign.as_str()}),
            "the readback records whose signature is still owed: {facts}"
        );
        market.clean();
    }

    /// A proposal that names someone else is not this run's, however it
    /// arrived: the handoff this configuration asked for still has to be
    /// made, and the pool's single slot is overwritten by the address it
    /// actually owes.
    #[test]
    fn a_pool_proposing_another_admin_is_still_handed_over() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-pool-other", &foreign);
        let pool = fixture_contract(87);
        market.arm(
            &format!("temp-{}", pool.as_str()),
            &format!(
                "{{\"vec\":[{{\"address\":\"{}\"}},{{\"u32\":0}}]}}",
                fixture_actor(11).as_str()
            ),
        );
        let (_, ev) = confirm_through_gate(&market, &MOp::RealHandoffPool, vec![("pool", pool)]);
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "someone else's proposal cannot settle this run's handoff: {ev:?}\n{}",
            market.calls()
        );
        assert!(
            market.calls().contains(&format!("--new_admin {}", foreign.as_str())),
            "the pool was left with the address this run did not ask for"
        );
        market.clean();
    }

    /// The two governance halves are one transfer asked twice: the oracle
    /// administrator is left holding the administrator role, and the key that
    /// deployed the market is left holding none. Each half is proved by what
    /// governance reports afterwards, not by the proposal being accepted —
    /// and the release is proved separately from the single-role read, as
    /// "nothing is left" rather than "this one role is gone".
    #[test]
    fn the_governance_pair_hands_the_administrator_role_over_and_empties_the_deployer() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-roles", &foreign);
        let governance = fixture_contract(52);
        let mut d = market.run(false);
        market.arm_deployed(&mut d, [("proxy-governance", governance.clone())]);
        for op in [
            MOp::RealProposeRole { grant: true },
            MOp::RealExecuteRole { grant: true },
            MOp::RealProposeRole { grant: false },
            MOp::RealExecuteRole { grant: false },
        ] {
            let slot = market
                .schedule(false)
                .into_iter()
                .find(|s| s.op == op)
                .unwrap_or_else(|| panic!("the handoff graph schedules no {op:?}"));
            d.prepare_gate(&slot).expect("the handoff gate opens");
            let token = d.gate.as_ref().expect("the gate is open").token.clone();
            let ev = d
                .step(DeployAct::Confirm { token })
                .expect("the confirmed step runs");
            assert!(
                matches!(ev, DeployEvent::Phase { .. }),
                "{op:?} must settle: {ev:?}\n{}",
                market.calls()
            );
        }
        // Read what governance *wrote*: an unwritten file is the fixture's
        // default ("the operator administers"), so reading it as empty would
        // pass a run whose release half never executed.
        let roles = |who: &str| {
            std::fs::read_to_string(
                market
                    .state
                    .join(format!("hasrole-{}-{who}", governance.as_str())),
            )
            .unwrap_or_else(|_| panic!("governance never changed the roles of {who}"))
        };
        assert_eq!(
            roles(foreign.as_str()),
            "Admin",
            "the oracle administrator was never granted the role"
        );
        assert_eq!(
            roles(market.admin.as_str()),
            "",
            "the deployer key kept a role the graph exists to take away"
        );
        market.clean();
    }

    /// A role read that fails is an error, never "does not hold". The release
    /// half settles on "the operator holds no Admin", so a failed read taken
    /// for false would finish the run saying the deployer keeps no power
    /// while it still administers the runtime.
    #[test]
    fn a_failed_role_read_never_settles_the_release() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-role-read-fails", &foreign);
        let governance = fixture_contract(52);
        let mut d = market.run(false);
        market.arm_deployed(&mut d, [("proxy-governance", governance)]);
        market.arm("fail-has_role", "1");
        let slot = market.slot(&MOp::RealProposeRole { grant: false });
        let outcome = d.prepare_gate(&slot).and_then(|ev| match ev {
            DeployEvent::Confirm { token, .. } => d.step(DeployAct::Confirm { token }),
            other => Ok(other),
        });
        assert!(
            !matches!(outcome, Ok(DeployEvent::Phase { .. })),
            "a release settled on a read that never answered: {outcome:?}"
        );
        assert!(
            !market.settled(&slot),
            "the release coordinate settled without proof that the operator holds nothing"
        );
        market.clean();
    }

    /// An interrupted role proposal reconciled on resume keeps the proposal
    /// id its gate froze, exactly as an asset proposal does: the execute
    /// behind it reads the id from these facts and refuses without one.
    #[test]
    fn a_reconciled_role_proposal_records_its_proposal_id() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-role-reconcile", &foreign);
        market.arm("next-proposal-id", "4");
        let op = MOp::RealProposeRole { grant: true };
        let (slot, record, d) =
            market.open_gate_armed(&op, [("proxy-governance", fixture_contract(52))]);
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        d.finish_reconciled(slot.phase, slot.slot, &record, Some(&op))
            .expect("a frozen role proposal reconciles");
        let facts = d
            .settled_facts(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled");
        assert_eq!(
            facts.get("proposal").and_then(Value::as_str),
            Some("4"),
            "the execute behind this proposal could never name it: {facts}"
        );
        market.clean();
    }

    /// An open role-proposal intent whose step never needed a proposal — the
    /// oracle administrator already holds `Admin` — reconciles on resume. The
    /// frozen id names no proposal, and waiting for one would strand the run
    /// in the handoff with the deploying key still an administrator.
    #[test]
    fn a_role_proposal_that_needed_no_proposal_reconciles() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-role-already", &foreign);
        let governance = fixture_contract(52);
        let op = MOp::RealProposeRole { grant: true };
        let (_, record, d) = market.open_gate_armed(&op, [("proxy-governance", governance.clone())]);
        assert!(
            !d.real_effect_present(&op, &record).expect("the readback answers"),
            "nothing proposed and nothing granted yet must stay unresolved"
        );
        market.arm(&format!("hasrole-{}-{}", governance.as_str(), foreign.as_str()), "Admin");
        assert!(
            d.real_effect_present(&op, &record).expect("the readback answers"),
            "an oracle administrator already holding Admin settles the grant's proposal"
        );
        market.clean();
    }

    /// The pool forgets a proposal past its expiry even while anyone keeps the
    /// temporary entry alive, so an expired proposal naming this run's target
    /// is not this run's handoff: it is proposed again.
    #[test]
    fn an_expired_proposal_for_the_target_is_proposed_again() {
        let foreign = fixture_actor(9);
        let market = a_market_owed_a_handoff("handoff-pool-expired", &foreign);
        let pool = fixture_contract(87);
        // The stand-in's latest ledger is 1234567; this proposal ended before it.
        market.arm(
            &format!("temp-{}", pool.as_str()),
            &format!("{{\"vec\":[{{\"address\":\"{}\"}},{{\"u32\":1000}}]}}", foreign.as_str()),
        );
        let (_, ev) = confirm_through_gate(&market, &MOp::RealHandoffPool, vec![("pool", pool)]);
        assert!(matches!(ev, DeployEvent::Phase { .. }), "{ev:?}\n{}", market.calls());
        assert!(
            market.calls().contains(&format!("--new_admin {}", foreign.as_str())),
            "an expired proposal was taken for a live one: {}",
            market.calls()
        );
        market.clean();
    }

    /// The backing pool's controller is the one handover with no second half
    /// to wait for, so it is asked last — and it is asked only when the
    /// controller is not already the address the pool belongs to.
    #[test]
    fn the_backing_pool_is_given_the_pools_admin_and_asked_no_again_when_it_already_has_him() {
        let foreign = fixture_actor(9);

        let market = a_market_owed_a_handoff("handoff-comet", &foreign);
        let comet = fixture_contract(88);
        let (_, ev) = confirm_through_gate(&market, &MOp::RealHandoffComet, vec![("comet", comet.clone())]);
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "the controller change must settle: {ev:?}\n{}",
            market.calls()
        );
        let calls = market.calls();
        assert!(
            calls.contains(&format!("--manager {}", foreign.as_str())),
            "the backing pool was never given the pool's admin: {calls}"
        );
        market.clean();

        let market = a_market_owed_a_handoff("handoff-comet-set", &foreign);
        let comet = fixture_contract(88);
        market.arm(
            &format!("controller-{}", comet.as_str()),
            foreign.as_str(),
        );
        let (_, ev) = confirm_through_gate(&market, &MOp::RealHandoffComet, vec![("comet", comet)]);
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "a controller that is already right settles as the state it is: {ev:?}\n{}",
            market.calls()
        );
        assert!(
            !market.calls().contains("set_controller"),
            "a controller nobody needed to change was changed anyway: {}",
            market.calls()
        );
        market.clean();
    }

    /// A price source this run deployed goes with the pool it prices, by the
    /// two-step the source's own `accept_ownership` completes. The deadline
    /// this run proposed with is frozen at the gate, because the entry it is
    /// proved by carries one and a resume has to ask about *that* transfer.
    #[test]
    fn a_price_source_this_run_deployed_is_handed_to_the_oracle_administrator() {
        let foreign = fixture_actor(9);
        let mut market = a_real_market_lazer("handoff-lazer");
        market.arm(&format!("account-{}", foreign.as_str()), "open");
        market
            .cfg
            .pool
            .as_mut()
            .expect("a real market has a pool")
            .admin = foreign.as_str().to_string();
        let source = fixture_contract(90);
        let (record, ev) = confirm_through_gate(
            &market,
            &MOp::RealHandoffLazer { instance: 0 },
            vec![("lazer:0", source.clone())],
        );
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "the source transfer must settle: {ev:?}\n{}",
            market.calls()
        );
        assert_eq!(record.target.as_ref(), Some(&source));
        assert!(
            record
                .pre_state
                .iter()
                .any(|noted| noted.to_string().contains("pending_live_until_ledger")),
            "the transfer's own deadline is not part of its intent: {:?}",
            record.pre_state
        );
        let calls = market.calls();
        assert!(
            calls.contains(&format!("--new_owner {}", foreign.as_str())),
            "the source was never asked to transfer to the oracle administrator: {calls}"
        );
        market.clean();
    }

    /// A market whose pool and oracle admin belong to another key: the only
    /// configuration that owes the phase-11 graph.
    fn a_market_owed_a_handoff(name: &str, curator: &AccountId) -> RealMarket {
        let mut market = a_real_market(name, false, false);
        market.arm(&format!("account-{}", curator.as_str()), "open");
        market
            .cfg
            .pool
            .as_mut()
            .expect("a real market has a pool")
            .admin = curator.as_str().to_string();
        market
    }

    /// The shared runtime's governance is the contract every later coordinate
    /// asks to act, so a governance that does not grant this operator the
    /// Admin role is asked at preflight rather than at the first
    /// create_proposal. Without the refusal the same market constructs, which
    /// is what makes the refusal the role and not an unrelated fault.
    #[test]
    fn a_shared_runtime_whose_governance_refuses_the_operator_refuses_the_run() {
        let market = a_real_market("runtime-no-admin-role", true, true);
        market.arm("no-admin-role", "");
        let refused = MarketDeployer::with_options(
            market.cfg.clone(),
            market.context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        )
        .expect("a refused preflight is a result, not a panic");
        let Some(DeployEvent::Failed(note)) = &refused.terminal else {
            market.clean();
            panic!(
                "a governance that refuses the operator is a refusal, not a run: {:?}",
                refused.terminal
            );
        };
        let note = note.to_string();
        assert!(
            note.contains("does not grant the operator")
                && note.contains(market.governance.as_str()),
            "the refusal must name the governance that refused: {note}"
        );
        let log = market.calls();
        assert!(
            log.contains("has_role"),
            "the refusal must come from the ownership read: {log}"
        );
        assert!(
            real_sends(&log).is_empty(),
            "a refused shared runtime reached the ledger: {:?}",
            real_sends(&log)
        );
        market.clean();

        // The same market whose governance does answer is accepted: the
        // refusal above was the role, not the new read itself.
        let accepted = a_real_market("runtime-with-admin-role", true, true);
        let running = MarketDeployer::with_options(
            accepted.cfg.clone(),
            accepted.context("market-admin"),
            accepted.journal(),
            MarketDeployOptions::default(),
        )
        .expect("a shared runtime the operator admins constructs");
        assert!(
            running.terminal.is_none(),
            "the same market with its role proceeds: {:?}",
            running.terminal
        );
        accepted.clean();
    }

    /// A runtime this pool shares is not this run's to reconfigure. An asset
    /// the runtime already serves with exactly this configuration is never
    /// proposed again — re-registering would clear its cache, its breakers,
    /// and its history — while an asset it does not serve still is.
    #[test]
    fn a_reused_runtime_never_reregisters_an_asset_it_already_serves() {
        let market = a_real_market("runtime-reuse", true, true);
        let mut d = market.run(false);
        let runtime = market.runtime.clone().expect("this market shares a runtime");

        // The shared runtime is verified, which is what makes its governance
        // and its served assets known to the coordinates that follow.
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut d, &verify, &market.admin, Some(runtime.clone()));

        // The runtime serves asset 0 with exactly the configuration this
        // market configures it with. Asset 1 it does not serve.
        let (_, asked, _) = d
            .real_set_proxy_operation(0)
            .expect("the first asset's configuration is well-formed");
        let asset_key = json!({ "Stellar": market.assets[0].as_str() });
        market.arm(
            &format!("proxy-{}", asset_slug(&asset_key)),
            &serde_json::to_string(&asked).expect("the configuration renders"),
        );
        market.arm("next-proposal-id", "1");

        let served = market.slot(&MOp::RealProposeAsset { asset: 0 });
        market.execute(&mut d, &served, &market.admin, Some(market.governance.clone()));
        assert!(market.settled(&served), "the served asset's coordinate settles");

        market
            .confirm(&mut d, &MOp::RealProposeAsset { asset: 1 })
            .expect("an asset the shared runtime does not serve is proposed");

        let proposals: Vec<String> = market
            .calls()
            .lines()
            .filter(|line| {
                line.starts_with("contract invoke")
                    && !line.contains("--send no")
                    && line.contains("create_proposal")
            })
            .map(str::to_string)
            .collect();
        assert_eq!(
            proposals.len(),
            1,
            "only the asset the runtime does not serve was registered: {proposals:?}"
        );
        assert!(
            proposals[0].contains(market.assets[1].as_str())
                && !proposals[0].contains(market.assets[0].as_str()),
            "the one proposal names the wrong asset: {}",
            proposals[0]
        );
        market.clean();
    }

    /// The reuse case the rehearsal met on the network: a shared runtime
    /// already serves *both* of this pool's assets with exactly the
    /// configuration this market would propose. Neither the propose nor the
    /// execute may reach the governance then — not one proposal is created
    /// and nothing is submitted — yet both coordinates still settle, with
    /// the execute's readback made before the propose's (absent) proposal
    /// id is ever read.
    #[test]
    fn a_reuse_run_whose_assets_are_all_served_never_proposes_and_never_sends() {
        let market = a_real_market("runtime-reuse-both-served", true, true);
        let mut d = market.run(false);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut d, &verify, &market.admin, Some(runtime));
        for asset in 0..2 {
            let (_, asked, _) = d
                .real_set_proxy_operation(asset)
                .expect("the asset's configuration is well-formed");
            let asset_key = json!({ "Stellar": market.assets[asset].as_str() });
            market.arm(
                &format!("proxy-{}", asset_slug(&asset_key)),
                &serde_json::to_string(&asked).expect("the configuration renders"),
            );
        }
        market.arm("next-proposal-id", "0");
        for asset in 0..2 {
            let propose = market.slot(&MOp::RealProposeAsset { asset });
            market.execute(&mut d, &propose, &market.admin, Some(market.governance.clone()));
            assert!(
                market.settled(&propose),
                "a served asset's propose must settle with nothing submitted"
            );
            let execute = market.slot(&MOp::RealExecuteAsset { asset });
            market.execute(&mut d, &execute, &market.admin, Some(market.governance.clone()));
            assert!(
                market.settled(&execute),
                "a served asset's execute must settle behind the same truth"
            );
        }
        let log = market.calls();
        assert!(
            !log.contains("create_proposal") && !log.contains("execute_proposal"),
            "a governance proposal was made for an asset the shared runtime already serves: {log}"
        );
        assert!(
            real_sends(&log).is_empty(),
            "a run that submits nothing still reached the ledger: {:?}",
            real_sends(&log)
        );
        market.clean();
    }

    /// A shared runtime that already serves every reserve exactly needs no
    /// proposal, so a governance whose Admin role has moved to the multisig
    /// refuses the run neither at preflight nor at the verify coordinate.
    /// One unserved asset still refuses
    /// (`a_shared_runtime_whose_governance_refuses_the_operator_refuses_the_run`).
    #[test]
    fn a_reuse_run_whose_assets_are_all_served_needs_no_governance_admin() {
        let market = a_real_market("runtime-served-no-admin", true, true);
        let d = market.run(false);
        for asset in 0..2 {
            let (_, asked, _) = d
                .real_set_proxy_operation(asset)
                .expect("the asset's configuration is well-formed");
            let asset_key = json!({ "Stellar": market.assets[asset].as_str() });
            market.arm(
                &format!("proxy-{}", asset_slug(&asset_key)),
                &serde_json::to_string(&asked).expect("the configuration renders"),
            );
        }
        market.arm("no-admin-role", "");
        let mut d = market.run(false);
        assert!(
            d.terminal.is_none(),
            "a run that proposes nothing was refused for a role it never uses: {:?}",
            d.terminal
        );
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut d, &verify, &market.admin, Some(runtime));
        assert!(market.settled(&verify), "the verify coordinate settles without the role");
        assert!(
            real_sends(&market.calls()).is_empty(),
            "verifying a shared runtime reached the ledger"
        );
        market.clean();
    }

    /// The same coordinate with an asset the runtime does not serve at all is
    /// proposed: the readback that skips a registration only ever answers for
    /// a perfect match.
    #[test]
    fn an_asset_the_shared_runtime_does_not_serve_is_proposed() {
        let market = a_real_market("runtime-new-asset", true, true);
        let mut d = market.run(false);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut d, &verify, &market.admin, Some(runtime));
        market.arm("next-proposal-id", "0");

        market
            .confirm(&mut d, &MOp::RealProposeAsset { asset: 0 })
            .expect("an unserved asset is proposed");

        let proposals: Vec<String> = market
            .calls()
            .lines()
            .filter(|line| {
                line.starts_with("contract invoke")
                    && !line.contains("--send no")
                    && line.contains("create_proposal")
            })
            .map(str::to_string)
            .collect();
        assert_eq!(proposals.len(), 1, "an unserved asset was not registered");
        assert!(proposals[0].contains(market.assets[0].as_str()));
        market.clean();
    }

    /// A read the network refused is not an answer about what it serves. A
    /// `get_proxy` that fails (rate limit, timeout, 5xx) must never be read
    /// as "this asset is unserved": the run refuses the propose and submits
    /// nothing, because re-registering an asset the first pool already serves
    /// would clear its breakers and its history.
    #[test]
    fn a_failed_proxy_read_on_a_shared_runtime_refuses_the_run() {
        let market = a_real_market("runtime-proxy-read-failed", true, true);
        let mut d = market.run(false);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut d, &verify, &market.admin, Some(runtime));
        market.arm("next-proposal-id", "0");
        market.arm("fail-get-proxy", "");

        let refused = market.confirm(&mut d, &MOp::RealProposeAsset { asset: 0 });
        let Err(error) = refused else {
            market.clean();
            panic!("a propose whose runtime read failed settled instead of refusing");
        };
        let note = error.to_string();
        assert!(
            note.contains("could not read whether the shared proxy runtime already serves"),
            "the refusal must name the failed read, not an unrelated fault: {note}"
        );
        assert!(
            !market.settled(&market.slot(&MOp::RealProposeAsset { asset: 0 })),
            "a refused propose must not settle"
        );
        let log = market.calls();
        assert!(
            !log.contains("create_proposal"),
            "a proposal was made for an asset whose serving could not be read: {log}"
        );
        market.clean();
    }

    /// An interrupted propose is reconciled against the proposal it named,
    /// never against the governance's counter. The second reserve of any
    /// launch pool — and every pool after the first on a shared governance —
    /// finds that counter already above zero, so "the counter moved" proves
    /// nothing. The one answer that attributes a proposal to this coordinate
    /// is the proposal at the id this coordinate froze before it sent, and
    /// the reconciliation writes that id into the coordinate's own facts so
    /// the execute behind it can still name a proposal.
    #[test]
    fn an_interrupted_propose_reconciles_against_the_proposal_it_froze() {
        let market = a_real_market("propose-reconcile", true, true);
        let mut probe = market.run(false);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut probe, &verify, &market.admin, Some(runtime));
        // The first reserve's proposal is already counted. This is exactly the
        // case a counter cannot answer.
        market.arm("next-proposal-id", "1");

        let (slot, record, d) = market.open_gate(&MOp::RealProposeAsset { asset: 1 });
        let (operation, _, served) = d
            .real_set_proxy_operation(1)
            .expect("the second asset's configuration is well-formed");
        assert!(!served, "the shared runtime must not serve this asset");
        assert_eq!(
            MarketDeployer::frozen_proposal_id(&record),
            Some(1),
            "the gate must freeze the id its send will use"
        );
        market.arm(
            "proposal-1",
            &json!({"id": 1, "ttl_ns": 0, "operation": operation}).to_string(),
        );
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");

        let resumed = market.run(false);
        assert!(
            market.settled(&slot),
            "the proposal this run named was never reconciled: {:?}",
            resumed.unresolved
        );
        let result = market
            .journal()
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled");
        let facts: Value =
            serde_json::from_str(&result.step.command.to_string()).expect("the facts are JSON");
        assert_eq!(
            facts.get("proposal").and_then(Value::as_str),
            Some("1"),
            "a reconciled propose must record the proposal id its execute binds to"
        );

        // The execute behind it proceeds on those facts: it submits exactly
        // the proposal this run created, rather than refusing for lack of an
        // id or guessing one from the counter.
        let execute = market.slot(&MOp::RealExecuteAsset { asset: 1 });
        let mut second = market.run(false);
        market.execute(
            &mut second,
            &execute,
            &market.admin,
            Some(market.governance.clone()),
        );
        let log = market.calls();
        assert!(
            log.lines()
                .any(|line| line.contains("execute_proposal") && line.contains("--id 1")),
            "the execute did not submit the proposal its propose recorded: {log}"
        );
        market.clean();
    }

    /// The same coordinate with a counter that has moved but no proposal of
    /// this asset at the id this run named stays unresolved. A proposal
    /// another client created under that id is not this run's registration,
    /// and settling on the counter would leave the run permanently unable to
    /// explain — or replay — a submission it cannot prove.
    #[test]
    fn a_propose_is_not_reconciled_by_a_proposal_for_another_asset() {
        let market = a_real_market("propose-reconcile-foreign", true, true);
        let mut probe = market.run(false);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut probe, &verify, &market.admin, Some(runtime));
        market.arm("next-proposal-id", "1");

        let (slot, record, d) = market.open_gate(&MOp::RealProposeAsset { asset: 1 });
        let (foreign_operation, _, _) = d
            .real_set_proxy_operation(0)
            .expect("the first asset's configuration is well-formed");
        market.arm(
            "proposal-1",
            &json!({"id": 1, "ttl_ns": 0, "operation": foreign_operation}).to_string(),
        );
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");

        let resumed = market.run(false);
        assert!(
            !market.settled(&slot),
            "a proposal for another asset settled this run's propose"
        );
        assert!(
            resumed.unresolved.is_some(),
            "an unattributable proposal must leave the run unresolved"
        );
        let log = market.calls();
        assert!(
            !log.lines()
                .any(|line| line.contains("create_proposal") && !line.contains("--send no")),
            "an unresolved propose was replayed: {log}"
        );
        market.clean();
    }

    // ---- regressions found by the live testnet rehearsal -----------------
    //
    // Each test below pins one behaviour the rehearsal had to teach the
    // generator the hard way: what the pinned contract ABIs actually export,
    // whose keystore may answer a prediction, what proves a deployment whose
    // process died after the send, and what the backstop's threshold law
    // actually reads. They are written against the stand-in, so each one
    // asks the same question of the same code a real run asks.

    /// A Lazer source's constructor takes `supported_feed_ids` as 1..=N ids
    /// in strictly increasing order: the config's feed list is sorted and
    /// de-duplicated before the constructor sees it, and a value that is not
    /// a feed id at all is refused rather than sent.
    #[test]
    fn the_lazer_feed_registry_is_strictly_increasing_and_refuses_a_non_numeric_feed() {
        assert_eq!(
            lazer_feed_registry(&["23".to_string(), "7".to_string(), "7".to_string()])
                .expect("a duplicated and out-of-order list is a lawful registry"),
            vec![7u32, 23],
            "the constructor's strictly-increasing law must be met before the send"
        );
        assert_eq!(
            lazer_feed_registry(&["1".to_string()]).expect("one feed is a registry"),
            vec![1u32]
        );
        assert!(lazer_feed_registry(&[]).expect("no feeds is an empty registry").is_empty());
        let refused = lazer_feed_registry(&["12".to_string(), "seven".to_string()]);
        assert!(
            refused.is_err(),
            "a feed id that is not a number is refused, not sent: {refused:?}"
        );
        assert!(
            lazer_feed_registry(&["4294967296".to_string()]).is_err(),
            "a feed id outside u32 is refused"
        );
    }

    /// The runtime v0.1.4 ABI has no `base`: a reused runtime is asked for
    /// its base through `source_base`. The stand-in refuses a function the
    /// runtime's pinned export set does not hold — per ROLE, not by the
    /// union of every market contract, which is exactly how the SEP-40
    /// `base` name slipped past a name check in the rehearsal.
    #[test]
    fn a_reused_runtime_is_asked_for_its_base_through_source_base_not_base() {
        let market = a_real_market("runtime-source-base", true, true);
        let mut d = market.run(false);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut d, &verify, &market.admin, Some(runtime));
        let log = market.calls();
        assert!(
            log.contains("source_base"),
            "the shared runtime's base was never read through the export it has: {log}"
        );
        assert!(
            !log.lines().any(|line| {
                line.starts_with("contract invoke") && line.contains(" base ")
            }),
            "the shared runtime was asked for `base`, which runtime v0.1.4 does not export: {log}"
        );
        market.clean();
    }

    /// The same refusal, stated as the ABI law it is: a function the pinned
    /// runtime export set does not hold is refused by the network, and so
    /// may not be called by this tool.
    #[test]
    fn a_function_outside_the_pinned_runtime_abi_is_refused() {
        let market = a_real_market("runtime-abi-refusal", true, true);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let refused = market
            .run(false)
            .reader()
            .view(runtime.as_str(), "market-admin", "refresh_asset", &[]);
        assert!(
            refused.is_err(),
            "runtime v0.1.4 exports `refresh`, never `refresh_asset`"
        );
        // The same call for a function the runtime does export is refused
        // only by the stand-in's own limits, never as an unknown function:
        // the refusal above is the ABI law, not a blanket refusal.
        let uninstrumented = market
            .run(false)
            .reader()
            .view(runtime.as_str(), "market-admin", "rearm", &[]);
        assert!(
            uninstrumented.is_err(),
            "the stand-in would otherwise refuse everything"
        );
        let log = market.calls();
        assert!(
            log.contains("refresh_asset"),
            "the unknown-function call was never asked of the network: {log}"
        );
        market.clean();
    }

    /// An id prediction is not a pure derivation: the CLI resolves the
    /// signing alias in a keystore. Only the operator's own config directory
    /// holds `market-admin`, so every `contract id wasm` for it must be
    /// asked of the operator's keystore, and a run-local keystore must be
    /// refused exactly as the network refuses it.
    #[test]
    fn only_the_operators_keystore_may_predict_an_id_for_the_admin_alias() {
        let market = a_real_market_lazer("predict-keystore");
        let id = fixture_contract(71);
        market.arm_predict(&id);
        market.code_at(&id, &market.artifacts["adapter"].1);
        let (slot, record, _) = market.open_gate(&MOp::RealDeployAdapter);
        assert_eq!(
            record.predicted.as_ref(),
            Some(&id),
            "the gate bound an id other than the one the CLI's derivation answered"
        );
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        let log = market.calls();
        let predictions: Vec<String> = log
            .lines()
            .filter(|line| line.starts_with("contract id wasm"))
            .map(str::to_string)
            .collect();
        assert!(
            !predictions.is_empty(),
            "the gate bound its id without asking the CLI, which owns the derivation law"
        );
        for line in &predictions {
            assert!(
                line.contains("--source market-admin")
                    && line.contains(&format!("--config-dir {}", market.out.join("admin").display())),
                "an id was predicted for the admin alias outside the operator's keystore: {line}"
            );
        }
        market.clean();
    }

    /// The crash the rehearsal actually hit: a real deployment was submitted,
    /// the process died before its settlement, and the run directory was
    /// bricked because nothing could prove the code had landed. Every real
    /// deploy coordinate is now reconciled the way the lab reconciles its
    /// own: the code installed at the predicted id must re-hash to the
    /// digest the coordinate declared, read back through the operator's own
    /// handle.
    #[test]
    fn every_real_deploy_coordinate_is_proven_by_its_code_hash_at_the_predicted_id() {
        let market = a_real_market_lazer("reconcile-all-deploys");
        let operations: Vec<MOp> = [
            MOp::RealDeployRuntime,
            MOp::RealDeployGovernance,
            MOp::RealDeployAdapter,
            MOp::RealDeployBackstop,
            MOp::RealDeployFactory,
        ]
        .into_iter()
        .collect();
        for op in &operations {
            let id = fixture_contract(73);
            market.arm_predict(&id);
            // No role is pre-registered. The matrix once registered a factory
            // here for every coordinate, including the factory's own: its gate
            // then froze a `target` (the pre-registered factory) beside a
            // `predicted` id, two different contracts for one deployment, which
            // no real gate can freeze and which no settlement can satisfy.
            let (slot, record, _) = market.open_gate(op);
            assert_eq!(
                record.target, None,
                "{op:?}: a deployment that has not happened has no target"
            );
            let id = record
                .predicted
                .clone()
                .unwrap_or_else(|| panic!("{op:?} bound no predicted id for its intent"));
            let role = match op {
                MOp::RealDeployRuntime => "proxy-runtime",
                MOp::RealDeployGovernance => "proxy-governance",
                MOp::RealDeployAdapter => "adapter",
                MOp::RealDeployBackstop => "backstop",
                MOp::RealDeployFactory => "factory",
                MOp::RealDeployPool => "pool",
                _ => unreachable!("the matrix holds only these coordinates"),
            };
            market.code_at(&id, &market.artifacts[role].1);
            market
                .journal()
                .begin(slot.phase, slot.slot, &record)
                .unwrap_or_else(|e| panic!("reserving {op:?}: {e:#}"));
            let d = market.run(false);
            assert!(
                d.terminal.is_none(),
                "{op:?} left an unattributable deployment behind: {:?}",
                d.terminal
            );
            assert!(
                market.settled(&slot),
                "{op:?} was never reconciled from the code at its predicted id"
            );
        }
        market.clean();
    }

    /// The matrix's negative half: a contract answering at the predicted
    /// address is not proof of *this* deployment. Code that hashes to
    /// anything other than the declared digest leaves the coordinate
    /// unresolved — the weaker half of the proof settles nothing.
    #[test]
    fn a_real_deploy_is_not_settled_when_the_installed_code_hash_differs() {
        let market = a_real_market("reconcile-wrong-code", false, false);
        let id = fixture_contract(74);
        market.arm_predict(&id);
        let (slot, record, _) = market.open_gate(&MOp::RealDeployFactory);
        assert_eq!(record.predicted.as_ref(), Some(&id));
        market.code_at(&id, b"someone else's qualified bytes");
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        let d = market.run(false);
        assert!(
            !market.settled(&slot),
            "a contract answering at the predicted id is not this coordinate's code"
        );
        assert!(
            d.unresolved.is_some(),
            "a deployment that cannot be attributed stays unresolved"
        );
        assert!(
            !d.deployed.contains_key("factory"),
            "an unattributed deployment registers no role"
        );
        market.clean();
    }

    /// A reconciled coordinate registers its role the moment it settles, so
    /// the next coordinate of the same run binds to it rather than waiting
    /// for a restart that rebuilds the map from the journal.
    #[test]
    fn a_reconciled_deployment_registers_its_role_for_the_next_coordinate() {
        let market = a_real_market("reconcile-binds-next", false, false);
        market.arm_predict(&fixture_contract(76));
        let (slot, record, mut d) = market.open_gate_armed(
            &MOp::RealDeployPool,
            [("factory", fixture_contract(70))],
        );
        let id = record
            .predicted
            .clone()
            .expect("the pool child's id is derived, never guessed");
        market.code_at(&id, &market.artifacts["pool"].1);
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        d.reconcile_pending(slot.phase, slot.slot, &record)
            .expect("the reconciliation itself is not an error");
        assert_eq!(
            d.slot_target(&slot).as_ref(),
            Some(&id),
            "the coordinate that just settled binds nothing"
        );
        market.clean();
    }

    /// A leg's asset contract is the network's own code: it declares no
    /// digest of ours, and the one answer that identifies it is the token's
    /// own `name`. Presence is proved by that answer, not by an address
    /// that happens to hold a contract.
    #[test]
    fn a_legs_asset_contract_is_proven_by_its_token_name() {
        let market = a_real_market("reconcile-sac", false, false);
        let leg = 0usize;
        let mut d0 = market.run(false);
        let cslot = market.slot(&MOp::CreateIssuer { leg });
        let signer = market.leg_signer(&MOp::CreateIssuer { leg }, leg);
        market.execute(&mut d0, &cslot, &signer, None);
        let (slot, record, _) = market.open_gate(&MOp::DeploySac { leg });
        let id = record
            .predicted
            .clone()
            .expect("a leg's asset contract id is derived, never guessed");
        assert!(
            record.artifact_digest.is_none(),
            "the network's own code declares no digest of ours"
        );
        market.arm(&format!("name-{id}"), "SHIMBLND");
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        let d = market.run(false);
        assert!(
            market.settled(&slot),
            "the asset contract the asset itself answers for was never reconciled: {:?}",
            d.unresolved
        );
        let log = market.calls();
        assert!(
            log.lines()
                .any(|line| line.contains(&format!("--id {}", id.as_str()))
                    && line.contains("-- name")),
            "the proof must be the token's own `name`, asked of the asset contract: {log}"
        );
        // The operator's handle, never the run-local keystore: that keystore
        // has no `market-admin` identity, so a readback asking it answered
        // nothing in a real run and left the coordinate unresolved forever.
        assert!(
            !log.contains("get_admin"),
            "a leg's asset contract was proved by a getter it does not have: {log}"
        );
        market.clean();
    }

    /// The same coordinate without that answer stays unresolved: a contract
    /// at the derived address that is not the asset's own token is not this
    /// run's deployment.
    #[test]
    fn a_legs_asset_contract_is_not_settled_when_it_answers_nothing() {
        let market = a_real_market("reconcile-sac-absent", false, false);
        let leg = 0usize;
        let mut d0 = market.run(false);
        let cslot = market.slot(&MOp::CreateIssuer { leg });
        let signer = market.leg_signer(&MOp::CreateIssuer { leg }, leg);
        market.execute(&mut d0, &cslot, &signer, None);
        let (slot, record, _) = market.open_gate(&MOp::DeploySac { leg });
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        let d = market.run(false);
        assert!(
            !market.settled(&slot),
            "an address holding some contract is not the asset's token"
        );
        assert!(d.unresolved.is_some(), "the open intent stays unresolved");
        market.clean();
    }

    /// An issuer account the run funded is its own knowledge: the key was
    /// generated locally before the send, so the address is derived from the
    /// run, not from the settlement that is missing. An intent left open by a
    /// process lost after the funding must therefore be reconciled — the
    /// account either exists at that address or it does not. Reading the
    /// settled facts instead, as this readback once did, asks a coordinate for
    /// the very record its own unfinished settlement is supposed to write, so
    /// the run could never be resolved.
    #[test]
    fn a_funded_issuer_account_reconciles_its_own_open_intent() {
        let market = a_real_market("reconcile-issuer", false, false);
        let leg = 0usize;
        let (slot, record, _) = market.open_gate(&MOp::CreateIssuer { leg });
        assert!(
            !market.settled(&slot),
            "a coordinate only its gate reached is not settled"
        );
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        // The funding landed: the ledger now holds the account at the address
        // the run's own key derives.
        market.arm(&format!("account-{}", market.issuers[leg].as_str()), "open");

        let d = market.run(false);
        assert!(
            market.settled(&slot),
            "an issuer the network holds was left unresolved: {:?}",
            d.unresolved
        );
        let log = market.calls();
        assert!(
            !log.lines().any(|line| line.starts_with("tx new create-account")),
            "a reconciled funding was submitted again: {log}"
        );
        market.clean();
    }

    /// The same coordinate with no account at that address stays unresolved:
    /// a key in the run's own keystore is not evidence that anyone was
    /// funded.
    #[test]
    fn an_unfunded_issuer_account_leaves_its_intent_unresolved() {
        let market = a_real_market("reconcile-issuer-absent", false, false);
        let leg = 0usize;
        let (slot, record, _) = market.open_gate(&MOp::CreateIssuer { leg });
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        let d = market.run(false);
        assert!(
            !market.settled(&slot),
            "a key is not an account; the coordinate settled without one"
        );
        assert!(d.unresolved.is_some(), "the open intent stays unresolved");
        market.clean();
    }

    /// A Lazer instance is numbered by its order among a reserve's Lazer
    /// sources, never by its place in that reserve's whole source list. A
    /// reserve that quotes an existing SEP-40 oracle before its Lazer feed is
    /// the case that tells the two laws apart: numbered by list position, the
    /// shape would schedule one instance and the `SetProxy` ask for another,
    /// and the coordinate that binds them would refuse — or bind a contract no
    /// coordinate of this graph ever deployed.
    #[test]
    fn a_lazer_source_after_an_existing_oracle_binds_its_own_instance() {
        let mut market = a_real_market_lazer("lazer-index");
        market
            .cfg
            .pool
            .as_mut()
            .expect("a real market has a pool")
            .reserves
            .first_mut()
            .expect("a configured reserve")
            .sources
            .reverse();
        assert!(
            matches!(
                market.cfg.pool.as_ref().expect("pool").reserves[0]
                    .sources
                    .first()
                    .map(|source| source.kind),
                Some(crate::manifest::SourceKind::Sep40)
            ),
            "the case must put the existing oracle first"
        );
        let shape =
            RealShape::from_config(&market.cfg).expect("the shape derives from config alone");
        assert_eq!(
            shape.lazer_feeds.as_slice(),
            &[vec!["7".to_string()]],
            "one Lazer source is instance zero, whatever the reserve lists before it"
        );

        let mut d = MarketDeployer::with_options(
            market.cfg.clone(),
            market.context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        )
        .expect("the market constructs");
        let instance = fixture_contract(77);
        d.deployed.insert("lazer:0".to_string(), instance.clone());
        let (operation, _, _) = d
            .real_set_proxy_operation(0)
            .expect("the SetProxy binds the deployed Lazer instance");
        let sources = operation["SetProxy"][1]["sources"]
            .as_array()
            .expect("the configuration names its sources");
        assert_eq!(sources.len(), 2, "the reserve keeps both of its sources");
        assert_eq!(
            sources[1]["oracle"].as_str(),
            Some(instance.as_str()),
            "the Lazer source bound a contract other than its own instance"
        );
        market.clean();
    }

    /// `init` on the production Comet is open to anyone, so a backing pool
    /// this run deployed can already answer another controller by the time
    /// this coordinate runs. That is not a dead end: the run deploys its own
    /// instance under a salt only it could use, initializes that one, and
    /// leaves *it* bound — the backstop, the factory and the pool are built
    /// against the Comet this coordinate ends on, never the one someone else
    /// claimed.
    #[test]
    fn a_front_run_backing_pool_is_replaced_by_one_this_run_initializes() {
        let mut market = a_real_market("comet-front-run", false, false);
        arm_comet_pin(&mut market);
        let mut d = market.run(false);
        let claimed = fixture_contract(60);
        let ours = fixture_contract(61);
        let other = fixture_actor(9);
        market.arm_deployed(
            &mut d,
            [
                ("comet", claimed.clone()),
                ("sac:0", market.sacs[0].clone()),
                ("sac:1", market.sacs[1].clone()),
            ],
        );
        market.arm(&format!("controller-{claimed}"), other.as_str());
        market.arm(&format!("predict-{}", d.comet_claim_salt(1)), ours.as_str());
        market.arm(
            &format!("tokens-{ours}"),
            &format!("[\"{}\",\"{}\"]", market.sacs[0].as_str(), market.sacs[1].as_str()),
        );
        let slot = market.slot(&MOp::RealInitComet);
        market.execute(&mut d, &slot, &market.admin, Some(claimed.clone()));
        assert!(
            market.settled(&slot),
            "a backing pool someone else claimed left the coordinate open"
        );
        assert_eq!(
            d.deployed.get("comet"),
            Some(&ours),
            "the run left the claimed instance bound, not the one it initialized"
        );
        let log = market.calls();
        assert!(
            log.lines()
                .any(|line| line.contains("-- init") && line.contains(&format!("--id {}", ours.as_str()))),
            "the recovery initialized the wrong instance: {log}"
        );
        assert!(
            !log.lines()
                .any(|line| line.contains("-- init") && line.contains(&format!("--id {}", claimed.as_str()))),
            "a pool owned by another controller was initialized: {log}"
        );
        let result = market
            .journal()
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled");
        let facts: Value =
            serde_json::from_str(&result.step.command.to_string()).expect("the facts are JSON");
        assert_eq!(
            facts.get("comet").and_then(Value::as_str),
            Some(ours.as_str()),
            "the recovery must journal the instance it left bound, so a resume hands \
             the backstop and the factory the contract this run actually initialized"
        );
        market.clean();
    }

    /// A restart after the adapter was verified keeps it bound. The pool that
    /// names an existing adapter deploys none, so no prediction carries it and
    /// only the settled coordinate's own record does: a resume that rebuilt
    /// the run from the journal without reading that target lost the adapter
    /// and then refused the pool it is constructed against — a restart after
    /// phase 7 could not continue a reuse run at all.
    #[test]
    fn a_restart_after_the_adapter_is_verified_keeps_it_bound() {
        let market = a_real_market("restart-adapter", true, true);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let adapter = market
            .cfg
            .pool
            .as_ref()
            .and_then(|pool| pool.adapter.as_deref())
            .map(|id| id.parse::<ContractId>().expect("the configured adapter id"))
            .expect("this market names an adapter");
        market.arm(&format!("adapter-parent-{adapter}"), runtime.as_str());

        let (verify_runtime, ev) = confirm_through_gate(&market, &MOp::RealVerifyRuntime, vec![]);
        assert_eq!(verify_runtime.target.as_ref(), Some(&runtime));
        assert!(matches!(ev, DeployEvent::Phase { .. }), "{ev:?}");

        let (verify_adapter, ev) = confirm_through_gate(
            &market,
            &MOp::RealVerifyAdapter,
            vec![("proxy-runtime", runtime.clone())],
        );
        assert_eq!(verify_adapter.target.as_ref(), Some(&adapter));
        assert!(matches!(ev, DeployEvent::Phase { .. }), "{ev:?}");

        // A fresh deployer on the same run directory: this is the restart.
        let restarted = market.run(false);
        assert_eq!(
            restarted.deployed.get("proxy-runtime"),
            Some(&runtime),
            "the verified runtime is rebuilt from its own coordinate"
        );
        assert_eq!(
            restarted.deployed.get("adapter"),
            Some(&adapter),
            "a reuse run's adapter must survive the restart the pool is built after"
        );
        market.clean();
    }

    /// The whole confirmed path for the backing pool's initialization: the
    /// gate freezes the intent, the token releases it, and the settlement the
    /// step returns must agree with what the gate froze. The hand-built
    /// records the neighbouring tests execute carry `predicted: target`,
    /// which is not the shape this coordinate's gate freezes — it freezes a
    /// `target` (a config of a contract that already exists) and no
    /// `predicted` — so only this route sees a settlement that names the
    /// wrong field. That mismatch settled nothing: the init had already landed
    /// on the network and the run reported unresolved custody afterwards.
    fn init_comet_through_its_gate(
        tag: &str,
        arm: impl FnOnce(&RealMarket, &mut MarketDeployer, &ContractId),
    ) -> (RealMarket, MarketDeployer, MSlot, ContractId, DeployEvent) {
        let mut market = a_real_market(tag, false, false);
        arm_comet_pin(&mut market);
        let comet = fixture_contract(62);
        let (slot, record, mut d) = market.open_gate_armed(
            &MOp::RealInitComet,
            [
                ("comet", comet.clone()),
                ("sac:0", market.sacs[0].clone()),
                ("sac:1", market.sacs[1].clone()),
            ],
        );
        assert_eq!(
            (record.target.as_ref(), record.predicted.as_ref()),
            (Some(&comet), None),
            "the gate freezes a config of an existing contract: a target and no prediction"
        );
        arm(&market, &mut d, &comet);
        let token = d.gate.as_ref().expect("the gate is open").token.clone();
        let ev = d
            .step(DeployAct::Confirm { token })
            .expect("the confirmed step runs");
        (market, d, slot, comet, ev)
    }

    fn arm_comet_tokens(market: &RealMarket, id: &ContractId) {
        market.arm(
            &format!("tokens-{id}"),
            &format!(
                "[\"{}\",\"{}\"]",
                market.sacs[0].as_str(),
                market.sacs[1].as_str()
            ),
        );
    }

    #[test]
    fn a_backing_pool_initialization_settles_against_the_target_its_gate_froze() {
        let (market, _d, slot, comet, ev) =
            init_comet_through_its_gate("comet-gate-init", |market, _, comet| {
                arm_comet_tokens(market, comet)
            });
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "an init that landed must settle, not strand the run in custody: {ev:?}\n{}",
            market.calls()
        );
        let result = market
            .journal()
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled");
        assert_eq!(
            result.step.target.as_ref(),
            Some(&comet),
            "the settlement binds the contract the gate bound"
        );
        assert!(
            market
                .calls()
                .lines()
                .any(|line| line.contains("-- init") && line.contains(&format!("--id {comet}"))),
            "the init was actually sent: {}",
            market.calls()
        );
        market.clean();
    }

    /// The read-only return of the same arm: a pool this admin already
    /// initialized (a resumed run, or a landed init) settles with nothing
    /// submitted, against the same frozen shape.
    #[test]
    fn an_already_initialized_backing_pool_settles_against_the_target_its_gate_froze() {
        let (market, _d, slot, comet, ev) =
            init_comet_through_its_gate("comet-gate-already", |market, _, comet| {
                market.arm(&format!("controller-{comet}"), market.admin.as_str());
            });
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "an already-initialized pool must settle: {ev:?}\n{}",
            market.calls()
        );
        let result = market
            .journal()
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled");
        assert_eq!(result.step.target.as_ref(), Some(&comet));
        assert!(
            !market.calls().lines().any(|line| line.contains("-- init")),
            "nothing is submitted for a pool this admin already controls: {}",
            market.calls()
        );
        market.clean();
    }

    /// The front-run recovery leaves a fresh instance bound and journals it in
    /// its facts, but the settlement still names what its gate froze: a
    /// settlement that disagreed with its own intent would be refused.
    #[test]
    fn a_recovered_backing_pool_still_settles_against_the_target_its_gate_froze() {
        let other = fixture_actor(9);
        let ours = fixture_contract(61);
        let (market, d, slot, comet, ev) =
            init_comet_through_its_gate("comet-gate-recovery", |market, d, comet| {
                market.arm(&format!("controller-{comet}"), other.as_str());
                market.arm(&format!("predict-{}", d.comet_claim_salt(1)), ours.as_str());
                arm_comet_tokens(market, &ours);
            });
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "a recovered pool must settle: {ev:?}\n{}",
            market.calls()
        );
        let result = market
            .journal()
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled");
        assert_eq!(
            result.step.target.as_ref(),
            Some(&comet),
            "the settlement names the gate's contract, not the recovery's instance"
        );
        assert_eq!(d.deployed.get("comet"), Some(&ours), "the recovery stays bound");
        market.clean();
    }

    /// One coordinate taken the whole confirmed way: the gate freezes the
    /// intent, the token releases it, and the event the step returns says
    /// whether the settlement it wrote agrees with what the gate froze. The
    /// neighbouring `RealMarket::execute` helper hand-builds a record whose
    /// `predicted` copies its `target`, a shape no real gate freezes, so only
    /// this route can tell a settlement that names the wrong field.
    fn confirm_through_gate(
        market: &RealMarket,
        op: &MOp,
        known: Vec<(&'static str, ContractId)>,
    ) -> (MarketCustodyRecord, DeployEvent) {
        confirm_through_gate_armed(market, op, known, |_, _| {})
    }

    /// The same, arming the stand-in once the gate has frozen its intent: a
    /// fact the readback must agree with (the id a factory returns, a reserve
    /// as the pool reads it) is a function of what the gate froze.
    fn confirm_through_gate_armed(
        market: &RealMarket,
        op: &MOp,
        known: Vec<(&'static str, ContractId)>,
        arm: impl FnOnce(&MarketCustodyRecord, &MarketDeployer),
    ) -> (MarketCustodyRecord, DeployEvent) {
        let (_, record, mut d) = market.open_gate_armed(op, known);
        arm(&record, &d);
        let token = d.gate.as_ref().expect("the gate is open").token.clone();
        let ev = d
            .step(DeployAct::Confirm { token })
            .expect("the confirmed step runs");
        (record, ev)
    }

    /// The deterministic deployments freeze a `predicted` id and no `target`;
    /// the step names the id it deployed, which the arm already requires to be
    /// that prediction. Each settles through its own gate.
    #[test]
    fn every_real_deployment_settles_against_the_prediction_its_gate_froze() {
        let ops = [
            (MOp::RealDeployRuntime, "proxy-runtime"),
            (MOp::RealDeployGovernance, "proxy-governance"),
            (MOp::RealDeployAdapter, "adapter"),
            (MOp::RealDeployComet, "comet"),
            (MOp::RealDeployEmitter, "emitter"),
            (MOp::RealDeployBackstop, "backstop"),
            (MOp::RealDeployFactory, "factory"),
        ];
        for (op, own) in ops {
            let mut market = a_real_market(&format!("gate-deploy-{own}"), false, false);
            arm_comet_pin(&mut market);
            // The emitter, like the Comet, is the network's own code with no
            // pin in this fixture; any armed pin serves, the instance is found
            // by its salt and never by its bytes.
            let factory_pin = market
                .cfg
                .artifacts
                .get(&crate::manifest::ArtifactRole::Factory)
                .cloned()
                .expect("the fixture pins the roles this run deploys");
            market
                .cfg
                .artifacts
                .insert(crate::manifest::ArtifactRole::Emitter, factory_pin);
            let predicted = fixture_contract(99);
            market.arm_predict(&predicted);
            // The runtime reads its owner back, and it must be the governance
            // the constructor named; the stand-in predicts every id alike.
            market.arm(&format!("owner-{predicted}"), predicted.as_str());
            // Governance reads back the runtime it was bound to.
            market.arm(
                &format!("proxy-oracle-{predicted}"),
                fixture_contract(80).as_str(),
            );
            // An adapter reads back the runtime it asks.
            market.arm(
                &format!("adapter-parent-{predicted}"),
                fixture_contract(80).as_str(),
            );
            // Everything this coordinate depends on is already deployed; the
            // coordinate's own role is not, or it would not be deploying it.
            let known: Vec<(&'static str, ContractId)> = [
                ("proxy-runtime", 80u8),
                ("proxy-governance", 81),
                ("adapter", 82),
                ("comet", 83),
                ("emitter", 84),
                ("backstop", 85),
                ("factory", 86),
            ]
            .into_iter()
            .filter(|(role, _)| *role != own)
            .map(|(role, n)| (role, fixture_contract(n)))
            .chain([
                ("sac:0", market.sacs[0].clone()),
                ("sac:1", market.sacs[1].clone()),
            ])
            .collect();
            let (record, ev) = confirm_through_gate(&market, &op, known);
            assert_eq!(
                (record.target.as_ref(), record.predicted.as_ref()),
                (None, Some(&predicted)),
                "{op:?}: a deployment freezes a prediction and no target"
            );
            assert!(
                matches!(ev, DeployEvent::Phase { .. }),
                "{op:?}: a deployment that landed must settle against its own intent: {ev:?}\n{}",
                market.calls()
            );
            market.clean();
        }
    }

    /// A config of a contract that already exists freezes that contract as its
    /// `target` and predicts nothing; the settlement names the same contract.
    #[test]
    fn the_real_config_coordinates_settle_against_the_target_their_gate_froze() {
        let runtime = fixture_contract(80);
        let comet = fixture_contract(83);
        let mut market = a_real_market("gate-config", false, false);
        arm_comet_pin(&mut market);
        let asset = market
            .cfg
            .pool
            .as_ref()
            .expect("a real config has a pool")
            .reserves[0]
            .asset
            .clone();
        market.arm(
            &format!("cached-{}", asset_slug(&json!({ "Stellar": asset }))),
            r#"{"status":{"Accepted":0}}"#,
        );
        let cases: Vec<(MOp, Vec<(&'static str, ContractId)>)> = vec![
            (
                MOp::RealRefreshAsset { asset: 0 },
                vec![("proxy-runtime", runtime.clone())],
            ),
            (MOp::RealFreezeComet, vec![("comet", comet.clone())]),
        ];
        for (op, known) in cases {
            let want = known[0].1.clone();
            let (record, ev) = confirm_through_gate(&market, &op, known);
            assert_eq!(
                (record.target.as_ref(), record.predicted.as_ref()),
                (Some(&want), None),
                "{op:?}: a config freezes the contract it acts on and no prediction"
            );
            assert!(
                matches!(ev, DeployEvent::Phase { .. }),
                "{op:?}: a landed config must settle against its own intent: {ev:?}\n{}",
                market.calls()
            );
        }
        market.clean();
    }

    /// The pool's own coordinates, taken through their gates: the factory's
    /// child deployment (a predicted id derived from the factory, the salt and
    /// the admin), the reserve queue and enablement it reads back, and the
    /// Lazer source a Lazer market deploys. Each freezes the shape the settlement
    /// must agree with.
    #[test]
    fn the_pool_and_source_coordinates_settle_against_the_shape_their_gate_froze() {
        // The pool child.
        let market = a_real_market("gate-pool-deploy", false, false);
        let adapter = fixture_contract(82);
        market.arm("pool-oracle", adapter.as_str());
        let (record, ev) = confirm_through_gate_armed(
            &market,
            &MOp::RealDeployPool,
            vec![("factory", fixture_contract(86)), ("adapter", adapter.clone())],
            |record, _| {
                let predicted = record.predicted.as_ref().expect("a pool child is predicted");
                market.arm("factory-deploy-result", predicted.as_str());
            },
        );
        assert_eq!(record.target, None, "a pool that does not exist yet has no target");
        assert!(record.predicted.is_some());
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "a deployed pool must settle against its prediction: {ev:?}\n{}",
            market.calls()
        );
        market.clean();

        // Reserve queue and enablement, on a deployed pool in setup status.
        let mut market = a_real_market("gate-reserves", false, false);
        arm_comet_pin(&mut market);
        let pool = fixture_contract(87);
        market.arm("pool-oracle", fixture_contract(82).as_str());
        for asset in 0..2usize {
            let (record, ev) = confirm_through_gate(
                &market,
                &MOp::RealQueueReserve { asset },
                vec![("pool", pool.clone())],
            );
            assert_eq!(
                (record.target.as_ref(), record.predicted.as_ref()),
                (Some(&pool), None),
                "queue reserve {asset}"
            );
            assert!(
                matches!(ev, DeployEvent::Phase { .. }),
                "queue reserve {asset} must settle: {ev:?}\n{}",
                market.calls()
            );
            market.arm("set-reserve-index", &asset.to_string());
            let (record, ev) = confirm_through_gate_armed(
                &market,
                &MOp::RealSetReserve { asset },
                vec![("pool", pool.clone())],
                |_, d| {
                    let reserve = d.reserve(asset).expect("a configured reserve").clone();
                    let want = d
                        .real_reserve_config(&reserve, asset)
                        .expect("the reserve's reviewed configuration");
                    market.arm(
                        &format!("reserve-{}", reserve.asset),
                        &json!({ "config": want }).to_string(),
                    );
                },
            );
            assert_eq!(
                (record.target.as_ref(), record.predicted.as_ref()),
                (Some(&pool), None),
                "set reserve {asset}"
            );
            assert!(
                matches!(ev, DeployEvent::Phase { .. }),
                "set reserve {asset} must settle: {ev:?}\n{}",
                market.calls()
            );
        }
        market.clean();

        // Activation: the pool re-reads its status after the send.
        let market = a_real_market("gate-set-status", false, false);
        let pool = fixture_contract(87);
        market.arm("pool-oracle", fixture_contract(82).as_str());
        let slot = market
            .schedule(true)
            .into_iter()
            .find(|s| s.op == MOp::RealSetStatus { to: 0 })
            .expect("an activating run schedules the status coordinate");
        let mut d = market.run(true);
        market.arm_deployed(&mut d, [("pool", pool.clone())]);
        d.prepare_gate(&slot).expect("the activation gate opens");
        let frozen = d.gate.as_ref().expect("the gate is open");
        assert_eq!(
            (frozen.record.target.as_ref(), frozen.record.predicted.as_ref()),
            (Some(&pool), None)
        );
        let token = frozen.token.clone();
        let ev = d
            .step(DeployAct::Confirm { token })
            .expect("the confirmed step runs");
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "an activated pool must settle: {ev:?}\n{}",
            market.calls()
        );
        market.clean();

        // A Lazer source instance.
        let market = a_real_market_lazer("gate-lazer");
        let verifier = fixture_contract(52);
        // A deterministic deployment predicts its id while the gate opens.
        market.arm_predict(&fixture_contract(88));
        let (record, ev) = confirm_through_gate_armed(
            &market,
            &MOp::RealDeployLazer { instance: 0 },
            vec![],
            |record, d| {
                let predicted = record.predicted.as_ref().expect("a source is predicted");
                market.arm(
                    &format!("lazer-config-{predicted}"),
                    &json!({"verifier": verifier.as_str(), "base": d.price_base()}).to_string(),
                );
            },
        );
        assert_eq!(record.target, None, "a source that does not exist yet has no target");
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "a deployed source must settle against its prediction: {ev:?}\n{}",
            market.calls()
        );
        market.clean();
    }

    /// The read-only verifications of reused contracts. Neither a shared
    /// runtime nor a pool's named adapter is deployed by this run, so no
    /// prediction names it: the gate must bind the configured address itself,
    /// and the settlement names that address. The adapter's gate used to find
    /// nothing to bind and refuse to open at all.
    #[test]
    fn the_read_only_verifications_bind_the_configured_contract_at_their_gate() {
        let market = a_real_market("gate-verify", true, true);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let (record, ev) = confirm_through_gate(&market, &MOp::RealVerifyRuntime, vec![]);
        assert_eq!(
            (record.target.as_ref(), record.predicted.as_ref()),
            (Some(&runtime), None)
        );
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "a verified shared runtime must settle: {ev:?}\n{}",
            market.calls()
        );
        market.clean();

        let market = a_real_market("gate-verify-adapter", true, true);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let adapter = market
            .cfg
            .pool
            .as_ref()
            .and_then(|pool| pool.adapter.as_deref())
            .map(|id| id.parse::<ContractId>().expect("the configured adapter id"))
            .expect("this market names an adapter");
        market.arm(&format!("adapter-parent-{adapter}"), runtime.as_str());
        let (record, ev) = confirm_through_gate(
            &market,
            &MOp::RealVerifyAdapter,
            vec![("proxy-runtime", runtime)],
        );
        assert_eq!(
            (record.target.as_ref(), record.predicted.as_ref()),
            (Some(&adapter), None),
            "the gate binds the adapter the configuration names"
        );
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "a verified adapter must settle: {ev:?}\n{}",
            market.calls()
        );
        market.clean();
    }

    /// The instance nobody claimed is initialized where it lies: the recovery
    /// is a response to a front-run, not a second deployment every run pays
    /// for.
    #[test]
    fn an_unclaimed_backing_pool_is_initialized_where_it_lies() {
        let mut market = a_real_market("comet-unclaimed", false, false);
        arm_comet_pin(&mut market);
        let mut d = market.run(false);
        let comet = fixture_contract(62);
        market.arm_deployed(
            &mut d,
            [
                ("comet", comet.clone()),
                ("sac:0", market.sacs[0].clone()),
                ("sac:1", market.sacs[1].clone()),
            ],
        );
        market.arm(
            &format!("tokens-{comet}"),
            &format!("[\"{}\",\"{}\"]", market.sacs[0].as_str(), market.sacs[1].as_str()),
        );
        let slot = market.slot(&MOp::RealInitComet);
        market.execute(&mut d, &slot, &market.admin, Some(comet.clone()));
        assert!(market.settled(&slot), "the backing pool was not initialized");
        let log = market.calls();
        assert!(
            log.lines()
                .any(|line| line.contains("-- init") && line.contains(&format!("--id {}", comet.as_str()))),
            "the pool this run owns was not initialized in place: {log}"
        );
        assert!(
            !real_sends(&log)
                .iter()
                .any(|line| line.starts_with("contract deploy")),
            "an unclaimed pool cost the run a second deployment: {log}"
        );
        market.clean();
    }

    /// A read-only verification is not a mutation: an intent left open at
    /// one is never settled by a guess, it is archived as never-applied and
    /// re-run, so its coordinate still records the shared addresses the
    /// following coordinates bind to.
    #[test]
    fn an_open_read_only_verify_is_replayed_and_records_its_facts() {
        let market = a_real_market("verify-replay", true, true);
        let runtime = market.runtime.clone().expect("this market shares a runtime");
        let (slot, record, _) = market.open_gate(&MOp::RealVerifyRuntime);
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        let mut d = market.run(false);
        assert!(
            d.terminal.is_none(),
            "a never-applied read-only check does not abandon the run: {:?}",
            d.terminal
        );
        assert!(
            market
                .journal()
                .read_intent(slot.phase, slot.slot)
                .expect("the journal is readable")
                .is_none(),
            "the intent proven never to have applied is archived, not carried"
        );
        // The coordinate is free of its archived intent: the resumed run
        // re-executes it, and its facts are recorded normally.
        market.execute(&mut d, &slot, &market.admin, Some(runtime.clone()));

        let result = market
            .journal()
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate was re-executed, not reconciled");
        assert_eq!(
            result.step.target.as_ref(),
            Some(&runtime),
            "the replayed check must name the runtime it verified"
        );
        let fresh = market.run(false);
        let propose = market.slot(&MOp::RealProposeAsset { asset: 0 });
        assert_eq!(
            fresh.slot_target(&propose).as_ref(),
            Some(&market.governance),
            "the coordinate after a replayed check could not bind the governance it acts on"
        );
        let log = market.calls();
        assert!(
            !log.lines().any(|line| line.contains("--send yes")),
            "a replayed read-only check submitted something: {log}"
        );
        market.clean();
    }

    /// The backstop's threshold is the runbook's law on the reserves it
    /// reports: seven-decimal stroops floored to whole tokens, the smaller
    /// leg to the fourth power times the larger, at or above 1e25 — plus a
    /// full LP held and nothing queued. It is never read from the always-
    /// zero `q4w_pct` field.
    #[test]
    fn the_backstop_deposit_readback_proves_the_threshold_from_the_reserves() {
        let minimum = {
            let (blnd, usdc) = crate::manifest::market_shim_seed();
            (blnd, usdc)
        };
        let blnd_s = minimum.0.to_string();
        let usdc_ok = minimum.1.to_string();
        // The law is floor(blnd)^4 * floor(usdc) >= 1e25 on the 7-decimal
        // stroop figures. Its boundary is computed here, so the row one
        // whole token below it must be refused by the law itself.
        let b4 = (minimum.0 / 10_000_000) as u128;
        let b4 = b4.pow(4);
        let floor_min = 10u128.pow(25).div_ceil(b4);
        let usdc_short = ((floor_min - 1) * 10_000_000).to_string();
        for (label, blnd, usdc, q4w, held, expect_settled) in [
            (
                "the exact minimum seed",
                blnd_s.clone(),
                usdc_ok.clone(),
                "0",
                "1000000000",
                true,
            ),
            (
                "one whole token below the threshold boundary",
                blnd_s.clone(),
                usdc_short.clone(),
                "0",
                "1000000000",
                false,
            ),
            (
                "a queued withdrawal figure that is not zero",
                blnd_s.clone(),
                usdc_ok.clone(),
                "1",
                "1000000000",
                false,
            ),
            (
                "less than the backing pool's LP",
                blnd_s.clone(),
                usdc_ok.clone(),
                "0",
                "999999999",
                false,
            ),
        ] {
            // The stand-in CLI is a shell script that cannot take a run
            // directory with spaces or quotes in it; the label is prose, the path not.
            let dir = label.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
            let market = a_real_market(&format!("deposit-{dir}"), false, false);
            let mut d = market.run(false);
            let backstop = fixture_contract(80);
            let pool = fixture_contract(81);
            let comet = fixture_contract(82);
            market.arm_deployed(
                &mut d,
                [
                    ("backstop", backstop.clone()),
                    ("pool", pool.clone()),
                    ("comet", comet.clone()),
                ],
            );
            market.arm(&format!("balance-{comet}"), "1000000000");
            market.backstop_data(
                &backstop,
                "{\"tokens\":\"0\",\"blnd\":\"0\",\"usdc\":\"0\",\"q4w_pct\":\"0\"}",
            );
            market.arm(
                &format!("pool-data-after-{}", backstop.as_str()),
                &format!(
                    "{{\"tokens\":\"{held}\",\"blnd\":\"{blnd}\",\"usdc\":\"{usdc}\",\"q4w_pct\":\"{q4w}\"}}"
                ),
            );
            let slot = market.slot(&MOp::RealBackstopDeposit);
            let settled = market
                .confirm(&mut d, &MOp::RealBackstopDeposit)
                .map(|()| market.settled(&slot))
                .unwrap_or(false);
            assert_eq!(
                settled,
                expect_settled,
                "{label}: the deposit coordinate answered the wrong way"
            );
            let sent = market
                .calls()
                .lines()
                .filter(|line| {
                    line.starts_with("contract invoke")
                        && line.contains(" deposit ")
                        && line.contains(&format!("--id {}", backstop.as_str()))
                })
                .count();
            assert!(
                sent <= 1,
                "{label}: the runback submitted more than one deposit"
            );
            if expect_settled {
                assert_eq!(
                    sent,
                    1usize,
                    "{label}: the accepted deposit was never submitted"
                );
            }
            let result = market
                .journal()
                .result(slot.phase, slot.slot)
                .expect("the journal is readable");
            assert_eq!(
                result.map(|r| r.step.target),
                expect_settled.then_some(Some(backstop.clone())),
                "{label}: the forward path must journal a settled result bound to the backstop it funded"
            );
            market.clean();
        }
    }

    /// A lab market prepared for a test: the shipped lab stack's shape, with
    /// every artifact pinned to deterministic fixture bytes and matching
    /// digests so preflight passes offline, and the same shell stand-in the
    /// real-market fixtures use.
    fn a_lab_market(tag: &str) -> (PathBuf, MarketConfig, PathBuf) {
        let out = std::env::temp_dir().join(format!("abtest-lab-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).expect("scratch directory");
        let cfg_dir = out.join("cfg");
        std::fs::create_dir_all(&cfg_dir).expect("config directory");
        let state = out.join("state");
        std::fs::create_dir_all(&state).expect("ledger directory");
        std::fs::write(
            state.join(format!("key-{}", "market-admin")),
            fixture_actor(0).as_str(),
        )
        .expect("armed the operator identity");
        std::fs::write(out.join("admin-key.txt"), fixture_actor(0).as_str())
            .expect("armed the operator file");

        let mut text = String::from(
            "[network]\npassphrase = \"Test SDF Network ; September 2015\"\nlabel = \"testnet\"\n\n",
        );
        for role in crate::manifest::MARKET_ARTIFACT_ROLES {
            let bytes = format!("fixture lab bytes for {role}").into_bytes();
            let path = cfg_dir.join(format!("{role}.wasm"));
            std::fs::write(&path, &bytes).expect("fixture artifact");
            text.push_str(&format!(
                "[artifacts.{role}]\npath = \"{}\"\nsha256 = \"{}\"\n\n",
                path.display(),
                sha256_hex(&bytes)
            ));
        }
        text.push_str(
            "[oracle]\nsources = 3\nmin_sources = 3\nmax_age_secs = 3600\n\
             max_clock_drift_secs = 60\nbase = \"USD\"\ndecimals = 7\nresolution = 300\n\n\
             [adapter]\ndecimals = 7\nresolution = 1\nbase = \"USD\"\n\n\
             [reserves.stable]\nc_factor = 9000000\nl_factor = 9500000\nutil = 8500000\n\
             max_util = 9500000\nsupply_cap = \"1000000000000000000\"\nr_base = 100000\n\
             r_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
             [reserves.xlm]\nc_factor = 7500000\nl_factor = 7500000\nutil = 5000000\n\
             max_util = 9500000\nsupply_cap = \"1000000000000000000\"\nr_base = 100000\n\
             r_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
             [reserves.weth]\nc_factor = 8000000\nl_factor = 8000000\nutil = 7000000\n\
             max_util = 9500000\nsupply_cap = \"170141183460469231731687303715884105727\"\n\
             r_base = 100000\nr_one = 500000\nr_two = 5000000\nr_three = 15000000\nreactivity = 20\n\n\
             [[balances]]\nwho = \"frodo\"\ntoken = \"stable\"\namount = 100000000000\n\n\
             [[balances]]\nwho = \"sam\"\ntoken = \"stable\"\namount = 100000000000\n\n\
             [[balances]]\nwho = \"user\"\ntoken = \"stable\"\namount = 100000000000\n",
        );
        let config = cfg_dir.join("config.toml");
        std::fs::write(&config, &text).expect("fixture config");
        let cfg = MarketConfig::load(&config)
            .expect("a lab market fixture must load through the strict parser");

        let stellar = out.join("stellar");
        let script = REAL_STELLAR
            .replace("@LOG@", &out.join("stellar-calls.log").display().to_string())
            .replace("@STATE@", &state.display().to_string())
            .replace("@PASSPHRASE@", MARKET_TESTNET_PASSPHRASE)
            .replace("@BASE@", "USD")
            .replace("@ADMIN@", &out.join("cfg").display().to_string())
            .replace("@LEDGER@", "1234567");
        std::fs::write(&stellar, script).expect("stand-in script");
        std::fs::set_permissions(&stellar, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("the stand-in must be executable");
        (out, cfg, stellar)
    }

    /// The lab graph reports a source-metadata mismatch and walks on. A
    /// resolution the pinned code reports and nothing can change is
    /// information, not a refusal: gating on it would stop a lab run at a
    /// coordinate whose "violation" no operator action could ever clear.
    #[test]
    fn the_lab_init_source_coordinate_reports_a_resolution_mismatch_and_walks_on() {
        let (out, cfg, stellar) = a_lab_market("init-source");
        let mut d = MarketDeployer::new(
            cfg,
            fixture_market_context(&out, &stellar, "market-admin"),
            MarketJournal::open(&out),
        )
        .expect("the lab market constructs");
        let slot = d
            .schedule_owned()
            .into_iter()
            .find(|s| matches!(s.op, MOp::InitSource { index: 0 }))
            .expect("the lab graph configures its sources");
        let source = fixture_contract(70);
        d.deployed.insert("source:0".to_string(), source.clone());
        let token = match d.prepare_gate(&slot) {
            Ok(DeployEvent::Confirm { token, .. }) => token,
            other => panic!(
                "the source gate would not open: {other:?} (terminal {:?})",
                d.terminal
            ),
        };
        let ev = d
            .step(DeployAct::Confirm { token })
            .expect("the confirmed step runs");
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "a reported mismatch must not stop the coordinate: {ev:?}"
        );
        let step = d
            .journal
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled")
            .step;
        assert!(
            step.violations.is_empty(),
            "a metadata mismatch is reported, never a refusal: {:?}",
            step.violations
        );
        let outcome = step.outcome.to_string();
        assert!(
            outcome.contains("resolution") && outcome.contains("latest-only feed"),
            "the mismatch is still reported in the step's own outcome: {outcome}"
        );
        assert_eq!(step.target, Some(source));
        // The graph proceeds: the next coordinate is offered, not a stop.
        let next = d
            .step(DeployAct::Continue)
            .expect("the run continues past the coordinate");
        assert!(
            matches!(next, DeployEvent::Confirm { .. }),
            "the lab graph must walk past a reported mismatch: {next:?}"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// One release-form artifact the fake catalog publishes: the catalog row
    /// text and the bytes that row pins, spelled exactly as contracts CI
    /// writes them.
    fn release_row(key: &str, package: &str, version: &str, bytes: &[u8]) -> (String, String) {
        let tag = format!("{package}-v{version}");
        let asset = format!("{}-{version}.wasm", package.replace('-', "_"));
        (
            format!("{key}@{version}.tsv"),
            format!(
                "{key}\t{version}\t{tag}\t{asset}\t{}\t{}",
                sha256_hex(bytes),
                bytes.len()
            ),
        )
    }

    /// The catalog seam, faked: rows and bytes per artifact, plus a count of
    /// how often it was consulted at all. A resume under a frozen plan must
    /// leave that count exactly where it was.
    struct FakeCatalog {
        rows: BTreeMap<String, Vec<(String, String)>>,
        /// Release tag to the bytes that tag's asset holds.
        assets: BTreeMap<String, Vec<u8>>,
        reads: std::cell::Cell<usize>,
    }

    impl FakeCatalog {
        fn new() -> Self {
            Self {
                rows: BTreeMap::new(),
                assets: BTreeMap::new(),
                reads: std::cell::Cell::new(0),
            }
        }

        fn publish(&mut self, key: &str, package: &str, version: &str, bytes: &[u8]) {
            self.rows
                .entry(key.to_string())
                .or_default()
                .push(release_row(key, package, version, bytes));
            self.assets
                .insert(format!("{package}-v{version}"), bytes.to_vec());
        }

        fn reads(&self) -> usize {
            self.reads.get()
        }
    }

    impl crate::release::ReleaseSource for FakeCatalog {
        fn catalog_commit(&self) -> anyhow::Result<String> {
            self.reads.set(self.reads.get() + 1);
            Ok("0123456789abcdef0123456789abcdef01234567".to_string())
        }

        fn catalog_rows(
            &self,
            _commit: &str,
            artifact: &str,
        ) -> anyhow::Result<Vec<(String, String)>> {
            Ok(self.rows.get(artifact).cloned().unwrap_or_default())
        }

        fn download(&self, tag: &str, asset: &str, max_len: u64) -> anyhow::Result<Vec<u8>> {
            let _ = asset;
            let bytes = self
                .assets
                .get(tag)
                .cloned()
                .ok_or_else(|| anyhow!("the fake catalog publishes no bytes for {tag}"))?;
            anyhow::ensure!(bytes.len() as u64 <= max_len, "release asset over its limit");
            Ok(bytes)
        }
    }

    /// A resumed real deploy keeps the release provenance it froze. The
    /// deployer materializes a release pin whose digest the network already
    /// holds into a live hash pin, which carries no provenance, so a plan
    /// frozen from the post-materialization configuration held none — and the
    /// next `--resume` refused with "the run's frozen plan has no release pin
    /// for role …". The plan must hold both roles, the resumed run must reach
    /// the same confirmation from the same bytes, and it must consult the
    /// catalog for nothing.
    #[test]
    fn a_resumed_real_deploy_keeps_the_release_pins_it_froze() {
        const RUNTIME: &str = "soroban-proxy-oracle";
        const GOVERNANCE: &str = "soroban-proxy-governance";
        const RUNTIME_PACKAGE: &str = "templar-proxy-oracle-soroban-contract";
        const GOVERNANCE_PACKAGE: &str = "templar-proxy-oracle-soroban-governance-contract";

        let market = a_real_market("release-resume", false, false);
        let cache = market.out.join("release-cache");
        let mut catalog = FakeCatalog::new();
        let runtime_bytes = b"fixture proxy runtime release bytes";
        let governance_bytes = b"fixture proxy governance release bytes";
        catalog.publish(RUNTIME, RUNTIME_PACKAGE, "0.1.4", runtime_bytes);
        catalog.publish(GOVERNANCE, GOVERNANCE_PACKAGE, "0.1.5", governance_bytes);
        // The stand-in network already holds both releases' digests. That is
        // the state this case is about: a release pin whose code is live is
        // materialized into a live hash pin, which carries no provenance.
        for bytes in [runtime_bytes.as_slice(), governance_bytes.as_slice()] {
            std::fs::write(
                market
                    .state
                    .join(format!("contract-hash-{}", sha256_hex(bytes))),
                "release code this network already holds",
            )
            .expect("armed release code the network already holds");
        }
        // Both proxy roles as release pins, unresolved, exactly as a pool
        // config that borrows the published stack declares them.
        let mut cfg = market.cfg.clone();
        for (role, key) in [
            (crate::manifest::ArtifactRole::ProxyRuntime, RUNTIME),
            (crate::manifest::ArtifactRole::ProxyGovernance, GOVERNANCE),
        ] {
            cfg.artifacts.insert(
                role,
                crate::manifest::ArtifactPin::Release(crate::manifest::ReleasePin {
                    release: key.to_string(),
                    version: None,
                    resolved: None,
                    path: None,
                }),
            );
        }
        // The first run resolves through the catalog, as `market deploy` does.
        crate::release::resolve_market(&mut cfg, &market.out, &[], &catalog, &cache)
            .expect("a first run resolves its release pins through the catalog");
        let first_reads = catalog.reads();
        assert!(first_reads > 0, "the first run consults the catalog");

        // Freeze the plan: construction runs materialize_missing_code, which
        // turns each resolved release pin into a live hash pin because this
        // stand-in network already holds the code.
        let summary_text = {
            let mut d = MarketDeployer::with_options(
                cfg.clone(),
                market.context("market-admin"),
                market.journal(),
                MarketDeployOptions::default(),
            )
            .expect("the first run constructs");
            // The precondition, stated so this test cannot go vacuous: the
            // network held both digests, so both pins are live now and the
            // configuration no longer carries their provenance.
            for role in [
                crate::manifest::ArtifactRole::ProxyRuntime,
                crate::manifest::ArtifactRole::ProxyGovernance,
            ] {
                assert!(
                    matches!(
                        d.cfg.artifacts.get(&role),
                        Some(crate::manifest::ArtifactPin::Live(_))
                    ),
                    "{role} must be materialized into a live pin for this case to mean anything"
                );
            }
            assert!(
                d.cfg.releases().is_empty(),
                "the materialized configuration holds no release provenance"
            );
            let ev = d.step(DeployAct::Init).expect("the first run advances");
            let DeployEvent::Confirm { summary, .. } = ev else {
                panic!("the first run must reach a confirmation, not {ev:?}");
            };
            summary.to_string()
        };
        let plan_path = market.out.join(MARKET_PLAN_FILE);
        let plan_bytes = std::fs::read(&plan_path).expect("the first run froze a plan");
        let plan = read_deployment_plan(&market.out)
            .expect("the frozen plan is readable")
            .expect("the frozen plan exists");
        let mut roles: Vec<&str> = plan.releases.keys().map(String::as_str).collect();
        roles.sort_unstable();
        assert_eq!(
            roles,
            vec!["proxy-governance", "proxy-runtime"],
            "the frozen plan must carry both release-form roles' provenance"
        );
        assert_eq!(plan.releases["proxy-runtime"].version, "0.1.4");
        assert_eq!(plan.releases["proxy-governance"].version, "0.1.5");

        // The resume: the same configuration as it is written on disk (pins
        // unresolved), resolved against the frozen plan — the catalog must not
        // be consulted at all.
        let mut resumed_cfg = cfg.clone();
        for role in [
            crate::manifest::ArtifactRole::ProxyRuntime,
            crate::manifest::ArtifactRole::ProxyGovernance,
        ] {
            resumed_cfg.artifacts.insert(
                role,
                crate::manifest::ArtifactPin::Release(crate::manifest::ReleasePin {
                    release: match role {
                        crate::manifest::ArtifactRole::ProxyRuntime => RUNTIME,
                        _ => GOVERNANCE,
                    }
                    .to_string(),
                    version: None,
                    resolved: None,
                    path: None,
                }),
            );
        }
        crate::release::resolve_market(&mut resumed_cfg, &market.out, &[], &catalog, &cache)
            .expect("a resume re-resolves on the frozen plan, never the catalog");
        assert_eq!(
            catalog.reads(),
            first_reads,
            "a resumed run must not consult the release catalog"
        );
        let mut resumed = MarketDeployer::with_options(
            resumed_cfg,
            market.context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        )
        .expect("the resumed run constructs on its frozen plan");
        assert!(
            !matches!(resumed.terminal, Some(DeployEvent::Failed(_))),
            "a resumed run is not refused: {:?}",
            resumed.terminal
        );
        let ev = resumed.step(DeployAct::Init).expect("the resumed run advances");
        let DeployEvent::Confirm { summary, .. } = ev else {
            panic!("the resumed run must reach the same confirmation, not {ev:?}");
        };
        assert_eq!(
            summary.to_string(),
            summary_text,
            "the resumed run must confirm the same mutation"
        );
        assert_eq!(
            std::fs::read(&plan_path).expect("the plan is still there"),
            plan_bytes,
            "a resume never rewrites the frozen plan"
        );
        market.clean();
    }

    /// A real refresh is the one coordinate whose proof is a price. A step
    /// that reports no Accepted price is a definite refusal, not a settled
    /// mutation: the run stops there, nothing after it is attempted, and the
    /// intent is archived so a resume asks again rather than reconciling it
    /// as done.
    #[test]
    fn a_refresh_without_an_accepted_price_stops_the_run_and_is_not_settled() {
        let market = a_real_market("refresh-refused", false, false);
        let runtime = fixture_contract(44);
        let op = MOp::RealRefreshAsset { asset: 0 };
        let (slot, _, mut d) = market.open_gate_armed(&op, [("proxy-runtime", runtime)]);
        let token = d
            .gate
            .as_ref()
            .unwrap_or_else(|| panic!("the refresh gate did not open:\n{}", market.calls()))
            .token
            .clone();
        let ev = d
            .step(DeployAct::Confirm { token })
            .expect("the confirmed step runs");
        let DeployEvent::Failed(reason) = &ev else {
            market.clean();
            panic!("a refresh with no Accepted price must stop the run: {ev:?}");
        };
        assert!(
            reason.to_string().contains("violation"),
            "the failure names the refusal: {reason}"
        );
        assert_eq!(d.stopped_violations().len(), 1);
        assert!(
            d.stopped_violations()[0]
                .to_string()
                .contains("no Accepted price"),
            "the violation is the step's own: {:?}",
            d.stopped_violations()
        );
        assert!(
            !market.settled(&slot),
            "a refusal is never a settled mutation"
        );
        // The intent keeps its bytes and its proof, archived for replay.
        let archive = market
            .out
            .join("market-unapplied")
            .join(format!("{:03}-{:02}", slot.phase, slot.slot));
        assert!(
            archive.join("intent.json").is_file(),
            "the intent bytes survive the refusal"
        );
        assert!(
            archive.join("unapplied.json").is_file(),
            "the proof is filed with the intent"
        );
        // Nothing later in the graph was attempted, and the refusal is
        // terminal for this process.
        assert!(!market.settled(&market.slot(&MOp::RealDeployAdapter)));
        assert!(matches!(
            d.step(DeployAct::Continue).expect("a terminal run answers"),
            DeployEvent::Failed(_)
        ));
        assert_eq!(
            real_sends(&market.calls())
                .into_iter()
                .filter(|line| line.contains("refresh"))
                .count(),
            1,
            "one attempt, no replay"
        );
        market.clean();
    }

    /// The resume half of the same law: an open refresh intent is attributable
    /// only when the runtime caches an Accepted price for the asset. A pending
    /// price is unresolved custody, and the same intent settles once the price
    /// it exists to make servable is actually served.
    #[test]
    fn a_resumed_refresh_settles_only_on_an_accepted_price() {
        let market = a_real_market("refresh-resume", true, false);
        let runtime = fixture_contract(44);
        // The runtime is this run's own settled deployment, as it is by the
        // time a real graph reaches its refresh: the run-local map is rebuilt
        // from that custody record, which is what the proof reads.
        let mut d = market.run(false);
        let verify = market.slot(&MOp::RealVerifyRuntime);
        market.execute(&mut d, &verify, &market.admin.clone(), Some(runtime.clone()));
        assert!(market.settled(&verify), "the runtime coordinate settles first");
        let op = MOp::RealRefreshAsset { asset: 0 };
        let (slot, record, _) = market.open_gate_armed(&op, [("proxy-runtime", runtime)]);
        let asset = market
            .cfg
            .pool
            .as_ref()
            .expect("a real config has a pool")
            .reserves[0]
            .asset
            .clone();
        let cached = format!("cached-{}", asset_slug(&json!({ "Stellar": asset })));
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("an intent is open");
        // A quorum that has not resolved: the intent is not attributable.
        let mut pending = market.run(false);
        let ev = pending.step(DeployAct::Init).expect("a resume scans its journal");
        assert!(
            matches!(ev, DeployEvent::Custody { unresolved: true, .. }),
            "a pending price is unresolved custody, not a settled step: {ev:?}"
        );
        assert!(
            !market.settled(&slot),
            "a pending price never settles a refresh"
        );
        // The same open intent reconciles once the price is Accepted.
        market.arm(&cached, r#"{"status":{"Accepted":0}}"#);
        let mut accepted = market.run(false);
        let _ = accepted.step(DeployAct::Init).expect("a resume scans its journal");
        assert!(
            market.settled(&slot),
            "an Accepted price settles the open refresh intent:\n{}",
            market.calls()
        );
        let result = market
            .journal()
            .result(slot.phase, slot.slot)
            .expect("the journal is readable")
            .expect("the coordinate settled");
        assert!(
            result.step.outcome.to_string().contains("reconciled"),
            "settled by reconciliation, not by replay: {}",
            result.step.outcome
        );
        market.clean();
    }

    /// A refusal is re-askable: once the condition the step reported on holds,
    /// the same coordinate is gated again and settles, and the refresh was
    /// sent exactly once per attempt.
    #[test]
    fn a_refused_refresh_is_replayed_once_its_price_is_accepted() {
        let market = a_real_market("refresh-retry", false, false);
        let runtime = fixture_contract(44);
        let op = MOp::RealRefreshAsset { asset: 0 };
        let asset = market
            .cfg
            .pool
            .as_ref()
            .expect("a real config has a pool")
            .reserves[0]
            .asset
            .clone();
        let (slot, _, mut d) = market.open_gate_armed(&op, [("proxy-runtime", runtime.clone())]);
        let token = d
            .gate
            .as_ref()
            .expect("a gate is open")
            .token
            .clone();
        assert!(matches!(
            d.step(DeployAct::Confirm { token })
                .expect("the confirmed step runs"),
            DeployEvent::Failed(_)
        ));
        assert!(!market.settled(&slot));
        market.arm(
            &format!("cached-{}", asset_slug(&json!({ "Stellar": asset }))),
            r#"{"status":{"Accepted":0}}"#,
        );
        // A fresh run finds the coordinate unsettled and gates it again.
        let (again, _, mut resumed) = market.open_gate_armed(&op, [("proxy-runtime", runtime)]);
        assert_eq!(
            (again.phase, again.slot),
            (slot.phase, slot.slot),
            "the refusal leaves the same coordinate to be asked again"
        );
        let token = resumed
            .gate
            .as_ref()
            .expect("a gate is open")
            .token
            .clone();
        let ev = resumed
            .step(DeployAct::Confirm { token })
            .expect("the confirmed step runs");
        assert!(
            matches!(ev, DeployEvent::Phase { .. }),
            "an Accepted price settles the retry: {ev:?}"
        );
        assert!(market.settled(&slot));
        assert_eq!(
            real_sends(&market.calls())
                .into_iter()
                .filter(|line| line.contains("refresh"))
                .count(),
            2,
            "one send per attempt, and the retry is the second"
        );
        market.clean();
    }

    /// The manifest's backstop figures come from the chain, and are i64 so
    /// the TOML the operator keeps can encode them. A deposit coordinate
    /// settled by reconciliation carries no step facts at all, which is
    /// precisely why the manifest may not read them from the journal.
    #[test]
    fn a_reconciled_deposit_still_publishes_its_backstop_figures_from_the_chain() {
        let market = a_real_market("deposit-manifest", false, false);
        let mut d = market.run(false);
        let backstop = fixture_contract(83);
        let pool = fixture_contract(84);
        let comet = fixture_contract(85);
        for leg in 0..2usize {
            for op in [
                MOp::CreateIssuer { leg },
                MOp::TrustLeg { leg },
                MOp::DeploySac { leg },
                MOp::MintLeg { leg },
                MOp::LockIssuer { leg },
            ] {
            let slot = market.slot(&op);
            let signer = market.leg_signer(&op, leg);
            let target = match &op {
                MOp::DeploySac { leg } => Some(market.sacs[*leg].clone()),
                _ => None,
            };
            market.execute(&mut d, &slot, &signer, target);
        }
        }
        let mut d = market.run(false);
        market.arm_deployed(
            &mut d,
            [
                ("backstop", backstop.clone()),
                ("pool", pool.clone()),
                ("comet", comet.clone()),
            ],
        );
        market.arm(&format!("balance-{comet}"), "1000000000");
        let (blnd, usdc) = crate::manifest::market_shim_seed();
        market.backstop_data(
            &backstop,
            &format!(
                "{{\"tokens\":\"1000000000\",\"blnd\":\"{blnd}\",\"usdc\":\"{usdc}\",\"q4w_pct\":\"0\"}}"
            ),
        );
        let slot = market.slot(&MOp::RealBackstopDeposit);
        let (_, record, _) = market.open_gate_armed(
            &MOp::RealBackstopDeposit,
            [
                ("backstop", backstop.clone()),
                ("pool", pool.clone()),
                ("comet", comet.clone()),
            ],
        );
        market
            .journal()
            .begin(slot.phase, slot.slot, &record)
            .expect("the coordinate was reserved");
        d.reconcile_pending(slot.phase, slot.slot, &record)
            .expect("the deposit reconciles from the backstop's own answer");
        let facts = d
            .settled_facts(slot.phase, slot.slot)
            .expect("the coordinate is readable");
        assert!(
            facts.is_none()
                || facts
                    .as_ref()
                    .and_then(|v| v.get("tokens"))
                    .is_none_or(|v| v.is_null()),
            "a reconciled coordinate fabricates the facts it never recorded: {facts:?}"
        );
        market.arm_deployed(
            &mut d,
            [
                ("proxy-runtime", fixture_contract(90)),
                ("proxy-governance", fixture_contract(91)),
                ("adapter", fixture_contract(92)),
                ("factory", fixture_contract(93)),
                ("emitter", fixture_contract(94)),
                ("lazer:0", fixture_contract(91)),
            ],
        );
        let manifest = d
            .assemble_real_manifest(1234567)
            .expect("the manifest assembles from the chain's own figures");
        let real = manifest
            .real
            .as_ref()
            .expect("a real market names its real shape");
        assert_eq!(real.backstop_deposit, 1_000_000_000);
        assert_eq!(real.q4w_pct, 0);
        let rendered = toml::to_string_pretty(&manifest).expect("the manifest renders as TOML");
        assert!(
            rendered.contains(&format!("backstop_deposit = {}", 1_000_000_000i64)),
            "the published figures must be TOML-encodable integers: {rendered}"
        );
        market.clean();
    }

    /// A run frozen to mainnet is a mainnet run: every gate compares the
    /// context against the network the configuration names, and a context
    /// pointed at testnet is refused before anything is spent.
    #[test]
    fn a_run_frozen_to_mainnet_is_refused_against_a_testnet_context() {
        let market = a_real_market_on(
            "mainnet-refused",
            false,
            false,
            MarketNetwork::Mainnet,
            None,
            false,
        );
        let refused = MarketDeployer::with_options(
            market.cfg.clone(),
            market.context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        );
        match refused {
            Err(e) => {
                let note = format!("{e:#}");
                assert!(
                    note.contains("frozen to mainnet"),
                    "refused for some other reason: {note}"
                );
            }
            Ok(d) => panic!(
                "a mainnet run against a testnet context was accepted: {:?}",
                d.terminal
            ),
        }
        assert!(
            real_sends(&market.calls()).is_empty(),
            "a refused network binding reached the ledger: {:?}",
            real_sends(&market.calls())
        );
        market.clean();
    }

    /// The same market on the network it was configured for constructs and
    /// binds its gate: mainnet is lawful, and the only thing that decides
    /// it is the frozen configuration.
    #[test]
    fn a_run_frozen_to_mainnet_binds_its_gate_on_a_mainnet_context() {
        let market = a_real_market_on(
            "mainnet-bound",
            false,
            false,
            MarketNetwork::Mainnet,
            None,
            false,
        );
        market.arm_predict(&fixture_contract(95));
        let mut d = MarketDeployer::with_options(
            market.cfg.clone(),
            market.mainnet_context("market-admin"),
            market.journal(),
            MarketDeployOptions::default(),
        )
        .expect("a mainnet market whose preflight passes constructs");
        assert!(d.terminal.is_none(), "the mainnet run starts");
        let slot = market.slot(&MOp::RealDeployAdapter);
        match d
            .prepare_gate(&slot)
            .expect("the first gate of a mainnet run opens")
        {
            DeployEvent::Confirm { summary, .. } => {
                let rendered = summary.to_string();
                assert!(
                    rendered.contains("mainnet") || rendered.contains("Mainnet"),
                    "the confirmation a mainnet operator approves must name the frozen network: {rendered}"
                );
            }
            other => panic!("a mainnet gate gave {other:?}"),
        }
        market.clean();
    }

    /// A `--wasm-dir` directory holds the qualified bytes under the file
    /// names the launch profile itself uses (`pool_factory.wasm`, not the
    /// role slug), and a digest-pinned market the network does not hold
    /// finds its reviewed bytes there and plans their upload. The digest
    /// stays the only authority over what is sent. And the decision is the
    /// plan's: once the first run uploaded, a resume that re-asked the chain
    /// would plan no uploads and refuse its own plan, so it must not ask.
    #[test]
    fn a_wasm_dir_market_resolves_the_profile_file_names() {
        let dir = std::env::temp_dir().join(format!(
            "abtest-real-wasm-dir-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("qualified bytes directory");
        let market = a_real_market_with_wasm_dir("wasm-dir-names", false, false, &dir);
        let role = "factory";
        let asset = crate::manifest::launch_wasm_file(role)
            .expect("the launch profile names the factory's file")
            .asset;
        assert_ne!(
            asset,
            format!("{role}.wasm"),
            "the test needs a name that differs from the role slug"
        );
        assert!(
            dir.join(asset).is_file(),
            "the fixture wrote the qualified bytes under the profile's own name"
        );
        assert!(
            !dir.join(format!("{role}.wasm")).is_file(),
            "no role-slug copy may exist: only the profile name may resolve"
        );
        let d = market.run(false);
        assert!(
            d.terminal.is_none(),
            "a digest-pinned market whose qualified bytes sit under the \
             profile's file name must pass preflight: {:?}",
            d.terminal
        );
        let plan = read_deployment_plan(&market.out)
            .expect("the plan is readable")
            .expect("the first run froze a plan");
        assert!(
            plan.uploads.iter().any(|r| r == role),
            "the network did not hold the factory, so the plan uploads it: {:?}",
            plan.uploads
        );
        // The chain now holds every artifact (the first run uploaded them).
        // A resume must still reproduce the frozen plan, uploads included.
        for (_, bytes) in market.artifacts.values() {
            std::fs::write(
                market.state.join(format!("contract-hash-{}", sha256_hex(bytes))),
                bytes,
            )
            .expect("armed live code");
        }
        let resumed = market.run(false);
        assert!(
            !matches!(&resumed.terminal, Some(DeployEvent::Failed(_))),
            "a resume after the uploads landed must reproduce the plan, not \
             re-plan from what the chain holds now: {:?}",
            resumed.terminal
        );
        assert_eq!(
            read_deployment_plan(&market.out).expect("readable").as_ref(),
            Some(&plan),
            "the frozen plan is untouched by the resume"
        );
        let _ = std::fs::remove_dir_all(&dir);
        market.clean();
    }
}
