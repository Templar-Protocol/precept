//! Manifest-bound contract-role registry, explicit related-contract
//! attachment, and live-ABI inspection for one deployed market (plan §1).
//!
//! Authority is the completed `MarketManifest` plus the live contract specs
//! read through the pinned native CLI seam (`Stellar::contract_interface`);
//! there is no static function whitelist and no Rust-side ABI/XDR parser
//! here. Roles attached by an operator are persisted only in a separate
//! attachment store — the frozen manifest is never rewritten — and every
//! attachment carries the exact testnet binding and the SHA-256 digest of
//! the live spec captured at attach time, both rechecked on every use.
//! No raw CLI bytes enter this module; failure messages are input-free.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{MarketManifest, MarketNetwork, MARKET_TESTNET_PASSPHRASE};
use crate::market::sha256_hex;
use crate::stellar::{AccountId, ContractId, Executor, Policy, Safe, Stellar, WasmHash};

/// Namespace prefixes owned by the frozen manifest registry; an operator
/// attachment may never shadow or overload one of them.
const RESERVED_ROLE_PREFIXES: [&str; 3] = ["token.", "source.", "proxy-"];

/// Directory holding operator attachments. Separate from `market-manifest.toml`:
/// the frozen manifest is never rewritten by an attach.
const ATTACHMENT_DIR: &str = "market-attachments";

/// Length ceiling for rendered spec JSON. The raw-spec ceiling is owned by
/// the CLI seam (`contract_interface` rejects over-cap specs itself); this
/// ceiling bounds only what `inspect` renders into a `Safe`.
const SPEC_RENDER_CAP: usize = 65_536;
/// A checksum-validated public Stellar address. This type deliberately has
/// no key lookup or signer authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublicAddress {
    Account(AccountId),
    Contract(ContractId),
}

impl PublicAddress {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Account(id) => id.as_str(),
            Self::Contract(id) => id.as_str(),
        }
    }
}

impl FromStr for PublicAddress {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.as_bytes().first() {
            Some(b'G') => AccountId::from_str(value)
                .map(Self::Account)
                .map_err(|_| anyhow!("{}", Safe::literal("public address is invalid"))),
            Some(b'C') => ContractId::from_str(value)
                .map(Self::Contract)
                .map_err(|_| anyhow!("{}", Safe::literal("public address is invalid"))),
            _ => Err(anyhow!(
                "{}",
                Safe::literal("public address must be a checksum-valid G or C address")
            )),
        }
    }
}

/// A validated, public autocomplete label and its concrete Stellar address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedAddress {
    pub name: String,
    pub address: PublicAddress,
}

const NAMED_ADDRESS_DIR: &str = "market-addresses";
const NAMED_ADDRESS_RECORD_CAP: usize = 4096;

fn named_address_error(message: &'static str) -> anyhow::Error {
    anyhow!("{}", Safe::literal(message))
}

fn safe_named_name(name: &str) -> bool {
    safe_registry_name(name) && name.as_bytes()[0].is_ascii_lowercase()
}

fn normalize_named_name(name: &str) -> anyhow::Result<String> {
    let normalized = name.to_ascii_lowercase();
    if safe_named_name(&normalized) {
        Ok(normalized)
    } else {
        Err(named_address_error("named address label is invalid"))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedAddressRecord {
    name: String,
    address: String,
}

/// The six tagged arms `SCSpecEntry` serializes in CLI26 JSON output
/// (stellar-xdr `ScSpecEntry`/`ScSpecEntryKind` schema: exactly one of
/// function, UDT struct/union/enum/error-enum, or event per entry). Every
/// legitimate interface entry carries exactly one of these as its sole key.
pub(crate) const SPEC_ENTRY_TAGS: [&str; 6] = [
    "function_v0",
    "udt_struct_v0",
    "udt_union_v0",
    "udt_enum_v0",
    "udt_error_enum_v0",
    "event_v0",
];

/// Enumerates every contract role frozen in the market manifest. This is the
/// whole registry: pool, backstop, factory, comet, emitter, the proxy-oracle
/// stack (`proxy-runtime`, `adapter`, plus `proxy-governance` only where the
/// manifest recorded one), each deployed source as `source.<n>`, and each
/// market token as `token.<ticker>`. Resolution and attach re-enforce
/// checksum validity and collision rules; this listing only mirrors what the
/// manifest already binds.
pub fn roles(manifest: &MarketManifest) -> Vec<(String, ContractId)> {
    let named = [
        ("pool", &manifest.pool),
        ("backstop", &manifest.backstop),
        ("factory", &manifest.factory),
        ("comet", &manifest.comet),
        ("emitter", &manifest.emitter),
        ("proxy-runtime", &manifest.oracle.runtime),
        ("adapter", &manifest.oracle.adapter),
    ];
    let mut out = Vec::with_capacity(
        named.len()
            + usize::from(manifest.oracle.governance.is_some())
            + manifest.oracle.sources.len()
            + manifest.tokens.len(),
    );
    for (role, id) in named.into_iter().chain(
        manifest
            .oracle
            .governance
            .as_ref()
            .map(|id| ("proxy-governance", id)),
    ) {
        out.push((role.to_string(), id.clone()));
    }
    for (n, id) in manifest.oracle.sources.iter().enumerate() {
        out.push((format!("source.{n}"), id.clone()));
    }
    for (tok, id) in &manifest.tokens {
        out.push((format!("token.{}", tok.as_str()), id.clone()));
    }
    out
}

fn safe_registry_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn add_manifest_name(
    entries: &mut std::collections::BTreeMap<String, NamedAddress>,
    name: String,
    address: PublicAddress,
) -> anyhow::Result<()> {
    if !safe_registry_name(&name) {
        return Err(named_address_error("market address label is malformed"));
    }
    let key = name.to_ascii_lowercase();
    if let Some(existing) = entries.get(&key) {
        if existing.address == address {
            return Ok(());
        }
        return Err(named_address_error(
            "market address label collides with a different public address",
        ));
    }
    entries.insert(key, NamedAddress { name, address });
    Ok(())
}

/// Merges manifest labels, validated attached roles, and run-local public
/// aliases. Stored records are bounded, descriptor-confined, and re-parsed.
pub fn named_addresses(manifest: &MarketManifest, out: &Path) -> anyhow::Result<Vec<NamedAddress>> {
    let mut entries = std::collections::BTreeMap::new();
    add_manifest_name(
        &mut entries,
        "admin".to_string(),
        PublicAddress::Account(manifest.admin.clone()),
    )?;
    for who in crate::manifest::Who::ALL {
        if let Some(id) = manifest.identities.get(&who) {
            add_manifest_name(
                &mut entries,
                who.as_str().to_string(),
                PublicAddress::Account(id.clone()),
            )?;
        }
    }
    for (role, id) in roles(manifest) {
        add_manifest_name(&mut entries, role, PublicAddress::Contract(id))?;
    }
    let oracle_labels = [
        ("oracle.runtime", &manifest.oracle.runtime),
        ("oracle.adapter", &manifest.oracle.adapter),
    ];
    for (name, id) in oracle_labels.into_iter().chain(
        manifest
            .oracle
            .governance
            .as_ref()
            .map(|id| ("oracle.governance", id)),
    ) {
        add_manifest_name(
            &mut entries,
            name.to_string(),
            PublicAddress::Contract(id.clone()),
        )?;
    }
    for (index, id) in manifest.oracle.sources.iter().enumerate() {
        add_manifest_name(
            &mut entries,
            format!("oracle.source.{index}"),
            PublicAddress::Contract(id.clone()),
        )?;
    }

    for (role, bytes) in read_store_records(out, ATTACHMENT_DIR, NAMED_ADDRESS_RECORD_CAP)? {
        if !safe_role(&role) {
            return Err(named_address_error("market role attachment is malformed"));
        }
        let binding: AttachmentBinding = serde_json::from_slice(&bytes)
            .map_err(|_| named_address_error("market role attachment is malformed"))?;
        let (contract, _) = parse_binding(&binding, &role)?;
        let key = role.to_ascii_lowercase();
        if entries.contains_key(&key) {
            return Err(named_address_error(
                "market address label collides with an existing label",
            ));
        }
        add_manifest_name(&mut entries, role, PublicAddress::Contract(contract))?;
    }

    for (filename_name, bytes) in
        read_store_records(out, NAMED_ADDRESS_DIR, NAMED_ADDRESS_RECORD_CAP)?
    {
        let record: NamedAddressRecord = serde_json::from_slice(&bytes)
            .map_err(|_| named_address_error("market address record is malformed"))?;
        if !safe_named_name(&filename_name) || record.name != filename_name {
            return Err(named_address_error("market address record is malformed"));
        }
        let key = record.name.to_ascii_lowercase();
        if entries.contains_key(&key) {
            return Err(named_address_error(
                "market address label collides with an existing label",
            ));
        }
        let address = PublicAddress::from_str(&record.address)
            .map_err(|_| named_address_error("market address record is malformed"))?;
        add_manifest_name(&mut entries, record.name, address)?;
    }

    Ok(entries.into_values().collect())
}

/// Saves a public alias without shadowing any manifest/attachment/saved label.
/// The create-new file is confined to market-addresses and never edits the
/// frozen manifest or transaction journal.
pub fn save_named_address(
    manifest: &MarketManifest,
    out: &Path,
    name: &str,
    address: PublicAddress,
) -> anyhow::Result<Vec<NamedAddress>> {
    let name = normalize_named_name(name)?;
    let entries = named_addresses(manifest, out)?;
    if entries
        .iter()
        .any(|entry| entry.name.eq_ignore_ascii_case(&name))
    {
        return Err(named_address_error(
            "named address label already exists or is reserved",
        ));
    }
    store_named_address(out, &name, &address)?;
    named_addresses(manifest, out)
}

/// One operator attachment record as persisted. Every field is re-parsed at
/// the trust boundary on read; nothing in this struct is trusted as-is.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentBinding {
    role: String,
    contract: String,
    spec_sha256: String,
    network: String,
}

/// Whether `role` is a safe attachment name: lowercase ASCII letters,
/// digits, `.`, `_`, `-`, never a manifest-namespace prefix, never able to
/// steer a store path (`.`-edge components excluded), and length-bounded.
fn safe_role(role: &str) -> bool {
    !role.is_empty()
        && role.len() <= 64
        && !role.starts_with('.')
        && !role.ends_with('.')
        && !role.contains("..")
        && role.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
        && !RESERVED_ROLE_PREFIXES.iter().any(|p| role.starts_with(p))
}

fn attachment_dir(out: &Path) -> PathBuf {
    out.join(ATTACHMENT_DIR)
}

fn attachment_path(out: &Path, role: &str) -> PathBuf {
    attachment_dir(out).join(format!("{role}.json"))
}

fn attachment_confinement_error() -> anyhow::Error {
    anyhow!(
        "{}",
        Safe::literal("market attachment store escapes the selected run directory")
    )
}

fn attachment_read_confined(out: &Path, role: &str) -> anyhow::Result<()> {
    let dir = attachment_dir(out);
    let run_canonical = out
        .canonicalize()
        .map_err(|_| attachment_confinement_error())?;
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(attachment_confinement_error()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(attachment_confinement_error())
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(attachment_confinement_error())
        }
        Ok(_) => {}
    }
    match dir.canonicalize() {
        Ok(dir_canonical) if dir_canonical == run_canonical.join(ATTACHMENT_DIR) => {}
        _ => return Err(attachment_confinement_error()),
    }
    match std::fs::symlink_metadata(attachment_path(out, role)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(attachment_confinement_error()),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(attachment_confinement_error()),
        Ok(_) => Ok(()),
    }
}

fn attachment_write_confined(out: &Path, role: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        safe_role(role),
        "{}",
        Safe::literal("market attachment role name is invalid")
    );
    attachment_read_confined(out, role)
}

/// Store access is descriptor-relative on Linux so symlink swaps cannot
/// redirect reads or exclusive writes outside the selected run.
fn open_market_run_dir(out: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(out)
        .map_err(|_| named_address_error("market address run directory is unavailable"))
}

fn proc_child_dir(parent: &std::fs::File, name: &str) -> PathBuf {
    use std::os::fd::AsRawFd;

    PathBuf::from(format!("/proc/self/fd/{}/{}", parent.as_raw_fd(), name))
}

fn verify_pinned_dir(path: &Path, held: &std::fs::File) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let path_meta = std::fs::symlink_metadata(path)
        .map_err(|_| named_address_error("market address store changed during access"))?;
    let held_meta = held
        .metadata()
        .map_err(|_| named_address_error("market address store changed during access"))?;
    if !path_meta.is_dir()
        || path_meta.file_type().is_symlink()
        || path_meta.dev() != held_meta.dev()
        || path_meta.ino() != held_meta.ino()
    {
        return Err(named_address_error(
            "market address store changed during access",
        ));
    }
    Ok(())
}

fn verify_missing_store_dir(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(named_address_error(
            "market address store changed during access",
        )),
    }
}

fn open_store_dir(
    run: &std::fs::File,
    name: &str,
    create: bool,
) -> anyhow::Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;

    let path = proc_child_dir(run, name);
    if create {
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => {
                return Err(named_address_error("market address store is unavailable"));
            }
        }
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
    {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(named_address_error(
            "market address store is unavailable or unsafe",
        )),
    }
}

fn read_store_record(dir: &std::fs::File, filename: &str, cap: usize) -> anyhow::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let path = format!("/proc/self/fd/{}/{filename}", dir.as_raw_fd());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| named_address_error("market address record is unavailable or unsafe"))?;
    let metadata = file
        .metadata()
        .map_err(|_| named_address_error("market address record is malformed"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > cap as u64 {
        return Err(named_address_error("market address record is malformed"));
    }
    let expected = metadata.len();
    let mut bytes = Vec::with_capacity(expected as usize);
    file.take((cap + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| named_address_error("market address record is malformed"))?;
    if bytes.len() as u64 != expected || bytes.len() > cap {
        return Err(named_address_error(
            "market address record changed while reading",
        ));
    }
    Ok(bytes)
}

fn read_store_records(
    out: &Path,
    store_name: &str,
    cap: usize,
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    use std::os::fd::AsRawFd;

    let run = open_market_run_dir(out)?;
    verify_pinned_dir(out, &run)?;
    let store_path = out.join(store_name);
    let Some(dir) = open_store_dir(&run, store_name, false)? else {
        verify_missing_store_dir(&store_path)?;
        verify_pinned_dir(out, &run)?;
        return Ok(Vec::new());
    };
    verify_pinned_dir(&store_path, &dir)?;
    let records_path = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let entries = std::fs::read_dir(records_path)
        .map_err(|_| named_address_error("market address store is unavailable"))?;
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| named_address_error("market address store is unreadable"))?;
        let filename = entry
            .file_name()
            .into_string()
            .map_err(|_| named_address_error("market address record name is invalid"))?;
        let name = filename
            .strip_suffix(".json")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| named_address_error("market address record name is invalid"))?;
        records.push((name.to_string(), read_store_record(&dir, &filename, cap)?));
    }
    verify_pinned_dir(&store_path, &dir)?;
    verify_pinned_dir(out, &run)?;
    Ok(records)
}

fn store_named_address(out: &Path, name: &str, address: &PublicAddress) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let run = open_market_run_dir(out)?;
    verify_pinned_dir(out, &run)?;
    let store_path = out.join(NAMED_ADDRESS_DIR);
    let dir = open_store_dir(&run, NAMED_ADDRESS_DIR, true)?
        .ok_or_else(|| named_address_error("market address store is unavailable"))?;
    verify_pinned_dir(&store_path, &dir)?;

    let record = NamedAddressRecord {
        name: name.to_string(),
        address: address.as_str().to_string(),
    };
    let bytes = serde_json::to_vec(&record)
        .map_err(|_| named_address_error("market address record could not be encoded"))?;
    let filename = format!("{name}.json");
    let path = format!("/proc/self/fd/{}/{filename}", dir.as_raw_fd());
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(named_address_error("named address label already exists"));
        }
        Err(_) => {
            return Err(named_address_error(
                "market address record could not be created",
            ))
        }
    };
    file.write_all(&bytes)
        .map_err(|_| named_address_error("market address record could not be written"))?;
    file.sync_all()
        .map_err(|_| named_address_error("market address record could not be synced"))?;
    dir.sync_all()
        .map_err(|_| named_address_error("market address store could not be synced"))?;
    run.sync_all()
        .map_err(|_| named_address_error("market address run directory could not be synced"))?;

    let record_path = PathBuf::from(format!("/proc/self/fd/{}/{filename}", dir.as_raw_fd()));
    let record_path_meta = std::fs::symlink_metadata(record_path)
        .map_err(|_| named_address_error("market address record changed during save"))?;
    let record_meta = file
        .metadata()
        .map_err(|_| named_address_error("market address record changed during save"))?;
    if !record_path_meta.is_file()
        || record_path_meta.file_type().is_symlink()
        || record_path_meta.dev() != record_meta.dev()
        || record_path_meta.ino() != record_meta.ino()
    {
        return Err(named_address_error(
            "market address record changed during save",
        ));
    }
    verify_pinned_dir(&store_path, &dir)?;
    verify_pinned_dir(out, &run)
}

/// Re-parses one attachment record: role agreement, checksum-validated
/// contract id, lowercase 64-hex spec digest, and the exact-testnet binding.
/// Rejection text is input-free — stored bytes never render.
fn parse_binding(
    binding: &AttachmentBinding,
    role: &str,
) -> anyhow::Result<(ContractId, WasmHash)> {
    anyhow::ensure!(
        binding.role == role && safe_role(role),
        "{}",
        Safe::literal("market role attachment is malformed")
    );
    anyhow::ensure!(
        binding.network == MARKET_TESTNET_PASSPHRASE,
        "{}",
        Safe::literal("market role attachment is not bound to the exact testnet")
    );
    let contract = ContractId::from_str(&binding.contract).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market role attachment contract id failed checksum")
        )
    });
    let digest = WasmHash::from_str(&binding.spec_sha256).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market role attachment spec digest is malformed")
        )
    });
    Ok((contract?, digest?))
}

/// SHA-256 of the canonical serialization of a live spec value obtained
/// through the CLI seam. The same deterministic function is used at attach
/// time and at every read-time recheck, so any spec change breaks binding.
fn spec_digest(spec: &Value) -> anyhow::Result<WasmHash> {
    let bytes = serde_json::to_vec(spec).context("canonicalizing market contract spec failed")?;
    WasmHash::from_str(&sha256_hex(&bytes)).map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market contract spec digest is malformed")
        )
    })
}

/// Durably reserves one attachment record. The attachment store is checked
/// before and after directory creation, so a symlink cannot move it outside
/// the selected run. `create_new` makes a second attach for the same role fail
/// before anything else can happen, and the record file lives only under the
/// attachment directory — the frozen manifest is never touched.
fn store_attachment(
    out: &Path,
    role: &str,
    contract: &ContractId,
    spec_sha256: &WasmHash,
) -> anyhow::Result<()> {
    attachment_write_confined(out, role)?;
    let dir = out.join(ATTACHMENT_DIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating attachment directory {}", dir.display()))?;
    attachment_write_confined(out, role)?;
    let path = attachment_path(out, role);
    let record = AttachmentBinding {
        role: role.to_string(),
        contract: contract.as_str().to_string(),
        spec_sha256: spec_sha256.as_str().to_string(),
        network: MARKET_TESTNET_PASSPHRASE.to_string(),
    };
    let bytes = serde_json::to_vec(&record).context("serializing market role attachment failed")?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("reserving market role attachment {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing market role attachment {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing market role attachment {}", path.display()))?;
    drop(file);
    let dir_file = std::fs::File::open(&dir)
        .with_context(|| format!("opening attachment directory {} for sync", dir.display()))?;
    dir_file
        .sync_all()
        .with_context(|| format!("syncing attachment directory {}", dir.display()))
}

/// Loads and re-parses the attachment for `role`, if one exists. Symlinked
/// attachment directories and records are rejected before any read. A stored
/// record that fails any integrity check is a hard failure, never skipped.
fn attached_binding(out: &Path, role: &str) -> anyhow::Result<Option<(ContractId, WasmHash)>> {
    if !safe_role(role) {
        return Ok(None);
    }
    attachment_read_confined(out, role)?;
    let path = attachment_path(out, role);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading market role attachment {}", path.display()))?;
    let binding: AttachmentBinding =
        serde_json::from_str(&text).context("parsing market role attachment failed")?;
    parse_binding(&binding, role).map(Some)
}

/// Resolves `role` to its bound contract and, for attachments only, the
/// recorded spec digest. A role present in both the frozen manifest and the
/// attachment store is refused rather than silently preferring either side.
fn bound_target(
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
) -> anyhow::Result<(ContractId, Option<WasmHash>)> {
    let frozen = roles(manifest)
        .into_iter()
        .find(|(r, _)| r == role)
        .map(|(_, id)| id);
    let attached = attached_binding(out, role)?;
    match (frozen, attached) {
        (Some(id), None) => Ok((id, None)),
        (None, Some(binding)) => Ok((binding.0, Some(binding.1))),
        (Some(_), Some(_)) => Err(anyhow!(
            "{}",
            Safe::literal(
                "market role is bound in both the frozen manifest and an operator attachment"
            )
        )),
        (None, None) => Err(anyhow!("{}", Safe::literal("unknown market role"))),
    }
}

/// Rejects any market context whose manifest network is not one of the two
/// lawful market networks, or whose execution network is not the very
/// network the run froze. Pure-local: runs before any executor call.
fn ensure_manifest_network(
    manifest: &MarketManifest,
    st: &Stellar<'_, impl Executor>,
) -> anyhow::Result<()> {
    let declared = MarketNetwork::from_pair(
        &manifest.network.passphrase.to_string(),
        &manifest.network.label.to_string(),
    )
    .map_err(|_| {
        anyhow!(
            "{}",
            Safe::literal("market manifest network is not a known market network")
        )
    })?;
    anyhow::ensure!(
        st.net.passphrase == declared.passphrase(),
        "{}",
        Safe::literal("market execution network is not the network the run froze")
    );
    Ok(())
}

/// Explicitly binds `role` to `id` after rechecking the manifest and
/// execution networks and capturing the live ABI through the CLI seam. The
/// captured spec must be a well-formed CLI26 tagged entry array (the same
/// validation `inspect` applies before any render), so a forged interface can
/// never bind an attachment. The recorded digest binds the attachment to that
/// exact spec; any later spec change fails every read-time recheck.
pub fn attach<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
    id: ContractId,
) -> anyhow::Result<()> {
    ensure_manifest_network(manifest, st)?;
    anyhow::ensure!(
        safe_role(role),
        "{}",
        Safe::literal("market attachment role name is invalid")
    );
    anyhow::ensure!(
        !named_addresses(manifest, out)?
            .iter()
            .any(|entry| entry.name.eq_ignore_ascii_case(role)),
        "{}",
        Safe::literal("market attachment role collides with an existing market address label")
    );
    // A long-lived explorer re-proves the endpoint's identity after local
    // role/store checks and before the first live spec read or write.
    st.assert_rpc_network(st.market_network())?;
    let spec = st.contract_interface(&id)?;
    anyhow::ensure!(
        spec_is_wellformed(&spec),
        "{}",
        Safe::literal("contract interface spec is not a well-formed tagged entry array")
    );
    let digest = spec_digest(&spec)?;
    store_attachment(out, role, &id, &digest)
}

/// A callable live function bound to its target and whole-interface digest.
pub struct LiveFunction {
    pub target: ContractId,
    pub spec_hash: WasmHash,
    pub inputs: Vec<String>,
}

fn checked_spec<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
) -> anyhow::Result<(ContractId, WasmHash, Value)> {
    ensure_manifest_network(manifest, st)?;
    let (target, recorded) = bound_target(out, manifest, role)?;
    // Local role/store refusals precede network calls. Re-prove the
    // endpoint's identity on each use of this live ABI boundary.
    st.assert_rpc_network(st.market_network())?;
    let spec = st.contract_interface(&target)?;
    anyhow::ensure!(
        spec_is_wellformed(&spec),
        "{}",
        Safe::literal("contract interface spec is not a well-formed tagged entry array")
    );
    let digest = spec_digest(&spec)?;
    if let Some(recorded) = recorded {
        anyhow::ensure!(
            digest == recorded,
            "{}",
            Safe::literal("attached market contract spec no longer matches its recorded binding")
        );
    }
    Ok((target, digest, spec))
}

/// Resolves a role after a single live ABI and attachment-binding check.
pub fn validated_target<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
) -> anyhow::Result<ContractId> {
    checked_spec(st, out, manifest, role).map(|(target, _, _)| target)
}

fn function_entry<'a>(spec: &'a Value, method: &str) -> anyhow::Result<&'a Value> {
    spec.as_array()
        .and_then(|entries| {
            entries.iter().find(|entry| {
                entry
                    .get("function_v0")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    == Some(method)
            })
        })
        .ok_or_else(|| {
            anyhow!(
                "{}",
                Safe::literal("requested function is not present in the live contract interface")
            )
        })
}

/// Whether `value` is a usable ABI name: a non-empty string with no
/// control characters. Used for every function and input name check.
fn valid_name(value: Option<&Value>) -> Option<&str> {
    value?
        .as_str()
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
}

/// Whether a `function_v0` payload is well-formed: the function `name` is
/// valid, `inputs` is an array, and every input is an object whose `name`
/// is valid and unique within this function. A function with no inputs is
/// legal and yields an empty name list.
fn function_inputs_are_wellformed(payload: &Value) -> Option<Vec<&str>> {
    valid_name(payload.get("name"))?;
    let inputs = payload.get("inputs")?.as_array()?;
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::with_capacity(inputs.len());
    for input in inputs {
        let name = valid_name(input.get("name"))?;
        if !seen.insert(name) {
            return None;
        }
        names.push(name);
    }
    Some(names)
}

/// Resolves any exported function using the native CLI's live tagged ABI.
/// Malformed or duplicate input names, and malformed function payloads,
/// fail closed with input-free errors; no index or `expect` is reachable
/// from untrusted spec bytes.
pub fn resolve_function<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
    method: &str,
) -> anyhow::Result<LiveFunction> {
    let (target, spec_hash, spec) = checked_spec(st, out, manifest, role)?;
    let entry = function_entry(&spec, method)?;
    let inputs = entry
        .get("function_v0")
        .and_then(function_inputs_are_wellformed)
        .ok_or_else(|| {
            anyhow!(
                "{}",
                Safe::literal("live contract function has malformed or duplicate input names")
            )
        })?
        .into_iter()
        .map(str::to_owned)
        .collect();
    Ok(LiveFunction {
        target,
        spec_hash,
        inputs,
    })
}

/// Inspects the live ABI of the contract bound to `role` through the pinned
/// native CLI seam. Without `method`, returns the bounded, sanitized render
/// of the complete live entry array — no whitelist, every exported entry is
/// visible. With `method`, returns that exact entry only, and an absent
/// function is a visible error, never a silent empty result. Every entry is
/// first revalidated as a well-formed CLI26 tagged interface entry (exactly
/// one recognized `SCSpecEntry` arm; `function_v0` requires a string `name`
/// and an array `inputs`), so untagged, unknown, duplicate-arm, or malformed
/// objects fail closed before any render. Absent, malformed, non-entry, or
/// over-cap specs fail closed through the CLI seam's input-free errors, and
/// an attachment whose live spec no longer matches its recorded digest fails
/// before any result is rendered.
pub fn inspect<E: Executor>(
    st: &Stellar<'_, E>,
    out: &Path,
    manifest: &MarketManifest,
    role: &str,
    method: Option<&str>,
) -> anyhow::Result<Safe> {
    let (_, _, spec) = checked_spec(st, out, manifest, role)?;
    match method {
        None => Ok(Safe::render_parsed(Policy::Market, &spec, SPEC_RENDER_CAP)),
        Some(f) => {
            let entry = function_entry(&spec, f)?;
            Ok(Safe::render_parsed(Policy::Market, entry, SPEC_RENDER_CAP))
        }
    }
}

/// Whether `spec` is a CLI26 interface document: an array in which every
/// element is a single-key object naming exactly one recognized
/// `SCSpecEntry` arm with an object payload, every `function_v0` arm
/// carries a valid (non-empty, control-free) `name`, valid and unique
/// `inputs` names, and function names are unique across the whole entry
/// array. Untagged/flat/unknown objects, multi-arm objects, empty,
/// control-char, or duplicate names, and malformed function payloads all
/// fail.
fn spec_is_wellformed(spec: &Value) -> bool {
    let Some(entries) = spec.as_array() else {
        return false;
    };
    if !entries.iter().all(entry_is_wellformed) {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    entries.iter().all(|entry| {
        match entry
            .get("function_v0")
            .and_then(|function| valid_name(function.get("name")))
        {
            Some(name) => seen.insert(name),
            None => true,
        }
    })
}

fn entry_is_wellformed(entry: &Value) -> bool {
    let Some(arms) = entry.as_object() else {
        return false;
    };
    let Some(tag) = SPEC_ENTRY_TAGS
        .iter()
        .find(|tag| arms.contains_key(**tag))
        .copied()
    else {
        return false;
    };
    arms.len() == 1
        && arms
            .get(tag)
            .is_some_and(|payload| payload.is_object() && arm_payload_is_wellformed(tag, payload))
}

fn arm_payload_is_wellformed(tag: &str, payload: &Value) -> bool {
    if tag != "function_v0" {
        return true;
    }
    function_inputs_are_wellformed(payload).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stellar::fake::FakeExecutor;
    use crate::stellar::Net;
    use std::path::PathBuf;

    const V26: &str = "stellar 26.1.0 (1228cff)";
    const SPEC: &str = r#"[{"function_v0":{"doc":"","name":"set_data","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"price","type_":"i128"},{"doc":"","name":"timestamp","type_":"u64"}],"outputs":[{"result":{"ok_type":"void","error_type":{"udt":{"name":"ContractError"}}}}]}},{"function_v0":{"doc":"","name":"base","inputs":[],"outputs":[{"udt":{"name":"Asset"}}]}},{"function_v0":{"doc":"","name":"assets","inputs":[],"outputs":[{"vec":{"element_type":{"udt":{"name":"Asset"}}}}]}},{"function_v0":{"doc":"","name":"decimals","inputs":[],"outputs":["u32"]}},{"function_v0":{"doc":"","name":"resolution","inputs":[],"outputs":["u32"]}},{"function_v0":{"doc":"","name":"price","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"timestamp","type_":"u64"}],"outputs":[{"option":{"value_type":{"udt":{"name":"PriceData"}}}}]}},{"function_v0":{"doc":"","name":"prices","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}},{"doc":"","name":"records","type_":"u32"}],"outputs":[{"option":{"value_type":{"vec":{"element_type":{"udt":{"name":"PriceData"}}}}}}]}},{"function_v0":{"doc":"","name":"lastprice","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}}],"outputs":[{"option":{"value_type":{"udt":{"name":"PriceData"}}}}]}},{"function_v0":{"doc":"","name":"extend_ttl","inputs":[{"doc":"","name":"asset","type_":{"udt":{"name":"Asset"}}}],"outputs":[]}},{"udt_struct_v0":{"doc":"","lib":"","fields":[{"doc":"","name":"price","type_":"i128"},{"doc":"","name":"timestamp","type_":"u64"}],"name":"PriceData"}},{"udt_union_v0":{"doc":"","lib":"","name":"Asset","cases":[{"tuple_v0":{"doc":"","name":"Stellar","type_":["address"]}},{"tuple_v0":{"doc":"","name":"Other","type_":["symbol"]}}]}}]"#;
    const ID_A: &str = "CBEF4DWEOEK75LWNGJNROJXVMNBC2DISKHGG63T4NYHAE53OTZRGF3AT";
    const ID_B: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
    const ID_C: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
    const ID_D: &str = "CBBTYOXBO7SO3JJ5EM5OBV4WTV5TML4G2K327TYSX422WL47B6RVMJRI";
    const ID_E: &str = "CBRVKZLFWFURNSJOU5MI6EQJNK72STF5CV4IMJXB7Y7V63BVHMEAJKCY";
    const G_ADMIN: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abtest-market-explorer-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn fresh_scratch(tag: &str) -> PathBuf {
        let dir = scratch(tag);
        std::fs::remove_dir_all(&dir).expect("clear named-address scratch");
        std::fs::create_dir_all(&dir).expect("create named-address scratch");
        dir
    }

    fn fixture_manifest(passphrase: &str) -> MarketManifest {
        serde_json::from_value(serde_json::json!({
            "network": {"label": "testnet", "passphrase": passphrase},
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

    fn market_st<'a>(net: &'a Net, fx: &'a FakeExecutor) -> Stellar<'a, FakeExecutor> {
        Stellar::new(net, fx, Policy::Market)
    }

    fn market_net(out: &Path) -> Net {
        Net {
            rpc_url: "http://rpc".into(),
            passphrase: MARKET_TESTNET_PASSPHRASE.into(),
            config_dir: out.join("cfg"),
            headers: vec![],
        }
    }

    /// Exact-testnet `network info` response required fresh by every
    /// inspection and attachment.
    fn network_info_testnet() -> String {
        serde_json::json!({"passphrase": MARKET_TESTNET_PASSPHRASE}).to_string()
    }

    /// Public-network response representing an endpoint whose identity
    /// changed after startup.
    fn network_info_mainnet() -> String {
        serde_json::json!({
            "passphrase": "Public Global Stellar Network ; September 2015"
        })
        .to_string()
    }

    /// Resolves one bounded role through the same re-parsing rules as the
    /// public API while retaining the recorded attachment digest.
    fn resolve_binding(
        out: &Path,
        manifest: &MarketManifest,
        role: &str,
    ) -> anyhow::Result<ContractId> {
        bound_target(out, manifest, role).map(|(contract, _)| contract)
    }

    fn id(s: &str) -> ContractId {
        ContractId::from_str(s).expect("fixture contract id")
    }

    #[test]
    fn registry_is_manifest_bound_and_unknown_roles_are_refused() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = scratch("registry");
        let listed = roles(&m);
        let mut names: Vec<&str> = listed.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(
            names.len(),
            12,
            "manifest roles plus three sources plus one token"
        );
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 12, "role names must be unique");
        for want in [
            "pool",
            "backstop",
            "factory",
            "comet",
            "emitter",
            "proxy-runtime",
            "proxy-governance",
            "adapter",
            "source.0",
            "source.1",
            "source.2",
            "token.blnd",
        ] {
            assert!(listed.iter().any(|(r, _)| r == want), "missing role {want}");
        }
        assert_eq!(resolve_binding(&out, &m, "pool").unwrap(), m.pool);
        assert_eq!(resolve_binding(&out, &m, "backstop").unwrap(), m.backstop);
        assert_eq!(
            resolve_binding(&out, &m, "proxy-runtime").unwrap(),
            m.oracle.runtime
        );
        assert_eq!(
            resolve_binding(&out, &m, "proxy-governance").unwrap(),
            *m.oracle.governance.as_ref().unwrap()
        );
        assert_eq!(
            resolve_binding(&out, &m, "adapter").unwrap(),
            m.oracle.adapter
        );
        for (n, bound) in m.oracle.sources.iter().enumerate() {
            assert_eq!(
                resolve_binding(&out, &m, &format!("source.{n}")).unwrap(),
                *bound
            );
        }
        assert_eq!(
            resolve_binding(&out, &m, "token.blnd").unwrap(),
            m.tokens.values().next().unwrap().clone()
        );
        assert!(resolve_binding(&out, &m, "nope").is_err());
        assert!(resolve_binding(&out, &m, "token.evil").is_err());
        assert!(
            !out.join(ATTACHMENT_DIR).exists(),
            "pure resolution writes nothing"
        );
    }

    #[test]
    fn an_omitted_governance_contract_has_no_role_or_label() {
        let mut m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        m.oracle.governance = None;
        let out = scratch("gov-absent");
        assert!(
            !roles(&m).iter().any(|(r, _)| r == "proxy-governance"),
            "a manifest with no governance contract lists no governance role"
        );
        assert!(
            resolve_binding(&out, &m, "proxy-governance").is_err(),
            "an unbound governance role is refused, never pointed at a stand-in"
        );
        let names = named_addresses(&m, &out).expect("manifest labels");
        assert!(
            names
                .iter()
                .all(|entry| entry.name != "proxy-governance" && entry.name != "oracle.governance"),
            "the governance label is omitted with its contract"
        );
        assert_eq!(
            resolve_binding(&out, &m, "proxy-runtime").unwrap(),
            m.oracle.runtime,
            "the remaining oracle roles stay bound"
        );
    }

    #[test]
    fn foreign_network_is_refused_before_any_executor_call() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = scratch("net-gate");
        let dir = out.join(ATTACHMENT_DIR);
        std::fs::create_dir_all(&dir).expect("attachment dir");
        std::fs::write(
            attachment_path(&out, "watcher"),
            serde_json::to_vec(&AttachmentBinding {
                role: "watcher".into(),
                contract: ID_B.into(),
                spec_sha256: "a".repeat(64),
                network: "Main Net ; November 2015".into(),
            })
            .unwrap(),
        )
        .expect("planted attachment");
        let net = market_net(&out);
        let fx = FakeExecutor::new(vec![]);
        let st = market_st(&net, &fx);
        assert!(inspect(&st, &out, &m, "watcher", Some("set_data")).is_err());
        assert!(resolve_binding(&out, &m, "watcher").is_err());
        let tampered = fixture_manifest("Main Net ; November 2015");
        assert!(inspect(&st, &out, &tampered, "pool", None).is_err());
        assert_eq!(
            fx.calls.borrow().len(),
            0,
            "no executor call before network checks pass"
        );
    }

    /// SEC boundary: a long-lived explorer may not authorize later
    /// inspection or attachment with its startup proof. The queue stays
    /// armed with a live interface response, so any stale identity reuse
    /// would record the interface fetch and fail the checks below.
    #[test]
    fn inspect_and_attach_refuse_when_rpc_identity_changes_after_startup() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = scratch("stale-identity");
        let net = market_net(&out);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_mainnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_mainnet(), ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let st = market_st(&net, &fx);
        st.assert_rpc_testnet()
            .expect("startup proves the exact testnet identity");

        let err = inspect(&st, &out, &m, "pool", None)
            .expect_err("a changed network must refuse live inspection");
        assert_eq!(
            err.to_string(),
            "market RPC did not prove the exact testnet passphrase"
        );
        let err = attach(&st, &out, &m, "watcher", id(ID_B))
            .expect_err("a changed network must refuse attachment");
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
            6,
            "only version gates and identity probes may run, leaving the queued interface unfetched"
        );
        assert!(
            calls
                .iter()
                .all(|argv| argv.first().map(String::as_str) != Some("contract")),
            "no contract interface fetch may follow a failed identity check: {calls:?}"
        );
        assert!(
            calls.iter().all(|argv| !argv.iter().any(|a| a == ID_B)),
            "the selected target never reaches a changed endpoint"
        );
        for proof in calls.as_chunks::<2>().0 {
            assert_eq!(proof[0], ["--version"]);
            assert!(
                proof[1].len() >= 2 && proof[1][0] == "network" && proof[1][1] == "info",
                "every boundary must collect its own fresh identity proof: {calls:?}"
            );
        }
        assert!(
            !out.join(ATTACHMENT_DIR).exists(),
            "a refused attachment leaves no attachment store behind"
        );
    }

    #[test]
    fn attached_spec_digest_tampering_is_caught_on_live_recheck() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = scratch("digest-tamper");
        let spec: Value = serde_json::from_str(SPEC).unwrap();
        let recorded = spec_digest(&spec).unwrap();
        store_attachment(&out, "watcher", &id(ID_B), &recorded).expect("store attachment");
        assert_eq!(resolve_binding(&out, &m, "watcher").unwrap(), id(ID_B));

        let net = market_net(&out);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let st = market_st(&net, &fx);
        let shown = inspect(&st, &out, &m, "watcher", Some("set_data")).expect("bound fn visible");
        assert!(
            shown.to_string().contains("set_data"),
            "live entry must render"
        );
        assert_eq!(
            fx.calls.borrow().len(),
            4,
            "version gate, fresh identity proof, version gate, then read-only interface fetch"
        );

        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let st = market_st(&net, &fx);
        let err = inspect(&st, &out, &m, "watcher", Some("missing_fn")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "requested function is not present in the live contract interface"
        );

        // Tamper the recorded digest (still well-formed hex, so the offline
        // shape check alone cannot see it): the live recheck must refuse.
        let tampered = {
            let mut d = recorded.as_str().to_string();
            let flip = if d.starts_with('0') { '1' } else { '0' };
            d.replace_range(0..1, &flip.to_string());
            d
        };
        std::fs::write(
            attachment_path(&out, "watcher"),
            serde_json::to_vec(&AttachmentBinding {
                role: "watcher".into(),
                contract: ID_B.into(),
                spec_sha256: tampered,
                network: MARKET_TESTNET_PASSPHRASE.into(),
            })
            .unwrap(),
        )
        .expect("tamper attachment");
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let st = market_st(&net, &fx);
        let err = inspect(&st, &out, &m, "watcher", Some("set_data")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "attached market contract spec no longer matches its recorded binding",
            "digest tampering must fail the live recheck before any result renders"
        );
        assert_eq!(fx.calls.borrow().len(), 4);
    }

    #[test]
    fn attach_binds_live_spec_and_refuses_collisions_without_network() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = scratch("attach");
        let net = market_net(&out);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let st = market_st(&net, &fx);
        attach(&st, &out, &m, "watcher", id(ID_B)).expect("attach live-bound contract");
        assert_eq!(resolve_binding(&out, &m, "watcher").unwrap(), id(ID_B));
        let stored = std::fs::read_to_string(attachment_path(&out, "watcher")).unwrap();
        assert!(stored.contains(ID_B) && stored.contains(MARKET_TESTNET_PASSPHRASE));
        let calls = fx.calls.borrow();
        assert_eq!(
            calls.len(),
            4,
            "version gate, fresh identity proof, version gate, then read-only interface fetch"
        );
        assert!(
            calls[1].iter().all(|a| a != "invoke" && a != "--send"),
            "attach never mutates the chain"
        );
        drop(calls);

        let fx = FakeExecutor::new(vec![]);
        let st = market_st(&net, &fx);
        assert!(
            attach(&st, &out, &m, "pool", id(ID_C)).is_err(),
            "no overloading a frozen role"
        );
        assert!(
            attach(&st, &out, &m, "token.evil", id(ID_C)).is_err(),
            "no shadowing token namespace"
        );
        assert!(
            attach(&st, &out, &m, "watcher", id(ID_C)).is_err(),
            "roles bind exactly once"
        );
        assert!(
            attach(&st, &out, &m, "../escape", id(ID_C)).is_err(),
            "role names cannot steer paths"
        );
        assert_eq!(
            fx.calls.borrow().len(),
            0,
            "collision refusals precede any executor call"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_from_attachment_store_are_refused() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = fresh_scratch("symlink-escape");
        let external = out.join("external");
        std::fs::create_dir_all(&external).expect("external attachment directory");
        let planted = serde_json::to_vec(&AttachmentBinding {
            role: "watcher".into(),
            contract: ID_B.into(),
            spec_sha256: "a".repeat(64),
            network: MARKET_TESTNET_PASSPHRASE.into(),
        })
        .expect("planted attachment encoding");
        std::fs::write(external.join("watcher.json"), &planted)
            .expect("external attachment target");
        let run = out.join("run");
        std::fs::create_dir_all(&run).expect("escape run");
        let frozen_manifest = run.join("market-manifest.toml");
        std::fs::write(&frozen_manifest, b"FROZEN-MANIFEST").expect("frozen manifest");
        std::os::unix::fs::symlink(&external, run.join(ATTACHMENT_DIR))
            .expect("symlinked attachment directory");

        let net = market_net(&out);
        let fx = FakeExecutor::new(vec![]);
        let st = market_st(&net, &fx);
        assert!(
            attach(&st, &run, &m, "watcher", id(ID_C)).is_err(),
            "unsafe attachment store must be refused"
        );
        assert!(resolve_binding(&run, &m, "watcher").is_err());
        assert!(inspect(&st, &run, &m, "watcher", Some("set_data")).is_err());
        assert!(
            fx.calls.borrow().is_empty(),
            "unsafe store refusals precede RPC"
        );
        assert_eq!(
            std::fs::read(&frozen_manifest).expect("frozen manifest unreadable"),
            b"FROZEN-MANIFEST"
        );
        assert_eq!(
            std::fs::read(external.join("watcher.json")).expect("external attachment unreadable"),
            planted
        );

        let second = scratch("symlink-record-escape");
        let second_run = second.join("run");
        std::fs::create_dir_all(&second_run).expect("symlink-record escape run");
        let external_record = second.join("external-record.json");
        std::fs::write(&external_record, b"EXTERNAL-RECORD").expect("external record");
        std::fs::create_dir_all(second_run.join(ATTACHMENT_DIR))
            .expect("confined attachment directory");
        std::os::unix::fs::symlink(&external_record, attachment_path(&second_run, "watcher"))
            .expect("symlinked attachment record");
        let fx = FakeExecutor::new(vec![]);
        let st = market_st(&net, &fx);
        assert!(
            attach(&st, &second_run, &m, "watcher", id(ID_C)).is_err(),
            "symlinked attachment record must be refused"
        );
        assert!(resolve_binding(&second_run, &m, "watcher").is_err());
        assert!(store_attachment(
            &second_run,
            "watcher",
            &id(ID_C),
            &spec_digest(&serde_json::from_str(SPEC).expect("spec fixture")).expect("spec digest"),
        )
        .is_err());
        assert_eq!(
            std::fs::read(&external_record).expect("external record unreadable"),
            b"EXTERNAL-RECORD"
        );
        assert!(fx.calls.borrow().is_empty());

        let dangling = second.join("dangling-record.json");
        let record_path = attachment_path(&second_run, "watcher");
        std::fs::remove_file(&record_path).expect("remove record symlink");
        std::os::unix::fs::symlink(&dangling, &record_path).expect("dangling record symlink");
        assert!(resolve_binding(&second_run, &m, "watcher").is_err());
        assert!(attach(&st, &second_run, &m, "watcher", id(ID_C)).is_err());
        assert!(fx.calls.borrow().is_empty());
        std::fs::remove_file(&record_path).expect("remove dangling record symlink");
    }

    #[test]
    fn malformed_specs_are_refused_by_both_inspection_modes_without_leaking() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = scratch("malformed-spec");
        for bad_spec in [
            r#"[{"name":"evil","doc":"SECRET-ABI-FRAGMENT"}]"#,
            r#"[{"function_v0":"SECRET-ABI-FRAGMENT scalar arm"}]"#,
            r#"[{"function_v0":{"doc":"SECRET-ABI-FRAGMENT","name":7,"inputs":[]}}]"#,
            r#"[{"function_v0":{"doc":"SECRET-ABI-FRAGMENT","name":"ok"}}]"#,
            r#"[{"function_v0":{"doc":"SECRET-ABI-FRAGMENT","name":"ok","inputs":[]}},{"function_v0":{"doc":"SECRET-ABI-FRAGMENT","name":"also_ok","inputs":[]},"event_v0":{"doc":"","topics":[],"data_type":"u32"}}]"#,
        ] {
            for method in [None, Some("evil")] {
                let fx = FakeExecutor::new(vec![
                    FakeExecutor::ok(V26, ""),
                    FakeExecutor::ok(&network_info_testnet(), ""),
                    FakeExecutor::ok(V26, ""),
                    FakeExecutor::ok(bad_spec, ""),
                ]);
                let net = market_net(&out);
                let st = market_st(&net, &fx);
                let err = inspect(&st, &out, &m, "pool", method).unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "contract interface spec is not a well-formed tagged entry array",
                    "no raw CLI bytes may reach the operator"
                );
                assert_eq!(
                    fx.calls.borrow().len(),
                    4,
                    "version gate, fresh identity proof, version gate, then interface fetch"
                );
            }
        }

        let dir = out.join(ATTACHMENT_DIR);
        std::fs::create_dir_all(&dir).expect("attachment dir");
        std::fs::write(
            attachment_path(&out, "watcher"),
            serde_json::to_vec(&AttachmentBinding {
                role: "watcher".into(),
                contract: ID_B.into(),
                spec_sha256: "0".repeat(64),
                network: MARKET_TESTNET_PASSPHRASE.into(),
            })
            .expect("planted attachment encoding"),
        )
        .expect("planted attachment");
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(
                r#"[{"name":"SECRET-ABI-FRAGMENT untagged entry","doc":""}]"#,
                "",
            ),
        ]);
        let net = market_net(&out);
        let st = market_st(&net, &fx);
        let err = inspect(&st, &out, &m, "watcher", Some("set_data")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "contract interface spec is not a well-formed tagged entry array",
            "malformed live specs fail before any recorded-binding path"
        );
        assert_eq!(
            fx.calls.borrow().len(),
            4,
            "version gate, fresh identity proof, version gate, then interface fetch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn malformed_specs_cannot_persist_attachments_and_never_touch_manifest() {
        let m = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = fresh_scratch("malformed-attach");
        let attachment_dir = out.join(ATTACHMENT_DIR);
        std::fs::create_dir_all(&attachment_dir).expect("attachment dir");
        let frozen_manifest = out.join("frozen-manifest");
        std::fs::write(&frozen_manifest, b"FROZEN-MANIFEST").expect("frozen manifest");
        for (role, bad_spec) in [
            (
                "untagged",
                r#"[{"kind":"function","name":"get_data","inputs":[],"outputs":[]},{"name":"evil","doc":"SECRET-ABI-FRAGMENT"}]"#,
            ),
            (
                "scalar-arm",
                r#"[{"function_v0":"SECRET-ABI-FRAGMENT scalar arm"}]"#,
            ),
            (
                "bad-name",
                r#"[{"function_v0":{"doc":"SECRET-ABI-FRAGMENT","name":7,"inputs":[]}}]"#,
            ),
            (
                "missing-inputs",
                r#"[{"function_v0":{"doc":"SECRET-ABI-FRAGMENT","name":"ok"}}]"#,
            ),
            (
                "duplicate-arms",
                r#"[{"function_v0":{"doc":"SECRET-ABI-FRAGMENT","name":"ok","inputs":[]},"event_v0":{"doc":"","topics":[],"data_type":"u32"}}]"#,
            ),
        ] {
            let fx = FakeExecutor::new(vec![
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(&network_info_testnet(), ""),
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(bad_spec, ""),
            ]);
            let net = market_net(&out);
            let st = market_st(&net, &fx);
            let err = attach(&st, &out, &m, role, id(ID_C)).unwrap_err();
            assert!(
                !err.to_string().contains("SECRET-ABI-FRAGMENT"),
                "malformed-spec refusals are input-free"
            );
            assert!(
                !attachment_path(&out, role).exists(),
                "malformed specs MUST NOT persist an attachment record"
            );
            assert!(
                std::fs::read_dir(&attachment_dir)
                    .expect("attachment listing")
                    .next()
                    .is_none(),
                "refused attachment attempts MUST leave the attachment store empty"
            );
            assert_eq!(
                std::fs::read(&frozen_manifest).expect("frozen manifest unreadable"),
                b"FROZEN-MANIFEST",
                "refused attachment attempts MUST NOT touch the frozen manifest"
            );
        }

        assert!(
            resolve_binding(&out, &m, "untagged").is_err(),
            "a refused attachment must not resolve"
        );
    }

    #[test]
    fn named_addresses_merge_manifest_attachments_and_public_aliases() {
        let mut manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        const FRODO: &str = "GAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHV4";
        manifest.identities.insert(
            crate::manifest::Who::Frodo,
            AccountId::from_str(FRODO).expect("valid actor address"),
        );
        let out = fresh_scratch("named-merge");
        let frozen_manifest = out.join("market-manifest.toml");
        std::fs::write(&frozen_manifest, b"frozen market manifest")
            .expect("frozen manifest fixture");

        let attachment = AttachmentBinding {
            role: "watcher".into(),
            contract: ID_A.into(),
            spec_sha256: "a".repeat(64),
            network: MARKET_TESTNET_PASSPHRASE.into(),
        };
        std::fs::create_dir_all(attachment_dir(&out)).expect("attachment dir");
        std::fs::write(
            attachment_path(&out, "watcher"),
            serde_json::to_vec(&attachment).expect("attachment JSON"),
        )
        .expect("attachment record");

        let initial = named_addresses(&manifest, &out).expect("manifest and attachment labels");
        for name in [
            "admin",
            "frodo",
            "pool",
            "backstop",
            "factory",
            "comet",
            "emitter",
            "proxy-runtime",
            "proxy-governance",
            "adapter",
            "source.0",
            "token.blnd",
            "oracle.runtime",
            "oracle.governance",
            "oracle.adapter",
            "oracle.source.0",
            "watcher",
        ] {
            assert!(
                initial.iter().any(|entry| entry.name == name),
                "missing named address {name}"
            );
        }

        let account = PublicAddress::from_str(FRODO).expect("valid public account");
        let saved = save_named_address(&manifest, &out, "Fresh-G", account.clone())
            .expect("save account alias");
        assert!(saved.iter().any(|entry| entry.name == "fresh-g"));
        save_named_address(
            &manifest,
            &out,
            "fresh-c",
            PublicAddress::from_str(ID_D).expect("valid public contract"),
        )
        .expect("save contract alias");
        let alias = save_named_address(
            &manifest,
            &out,
            "pool-copy",
            PublicAddress::Contract(manifest.pool.clone()),
        )
        .expect("same address may have multiple labels");
        assert!(alias.iter().any(|entry| {
            entry.name == "pool" && entry.address == PublicAddress::Contract(manifest.pool.clone())
        }));
        assert!(alias.iter().any(|entry| {
            entry.name == "pool-copy"
                && entry.address == PublicAddress::Contract(manifest.pool.clone())
        }));
        assert!(alias
            .iter()
            .any(|entry| entry.name == "fresh-g" && entry.address == account));
        assert!(alias.iter().any(|entry| {
            entry.name == "fresh-c"
                && entry.address == PublicAddress::from_str(ID_D).expect("public contract")
        }));
        assert!(
            alias
                .windows(2)
                .all(|pair| pair[0].name.to_ascii_lowercase() <= pair[1].name.to_ascii_lowercase()),
            "choices are case-insensitively sorted"
        );
        assert_eq!(
            std::fs::read(out.join(NAMED_ADDRESS_DIR).join("fresh-g.json")).expect("saved record"),
            serde_json::to_vec(&NamedAddressRecord {
                name: "fresh-g".into(),
                address: FRODO.into(),
            })
            .expect("serialized record")
        );
        assert_eq!(
            std::fs::read(&frozen_manifest).expect("frozen manifest remains readable"),
            b"frozen market manifest"
        );
        assert!(
            !out.join("market-pending").exists(),
            "address persistence never touches the transaction journal"
        );
    }

    #[test]
    fn public_address_parser_rejects_secrets_controls_and_bad_checksums_safely() {
        const FORGED: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHG";
        for invalid in [
            FORGED,
            "SDO-NOT-LEAK",
            "not-a-stellar-address",
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF\u{1b}",
        ] {
            let error = PublicAddress::from_str(invalid).expect_err("reject non-public address");
            assert!(
                !error.to_string().contains(invalid),
                "address error must not echo input: {error}"
            );
        }
        assert_eq!(
            PublicAddress::from_str(G_ADMIN)
                .expect("valid account")
                .as_str(),
            G_ADMIN
        );
        assert_eq!(
            PublicAddress::from_str(ID_B)
                .expect("valid contract")
                .as_str(),
            ID_B
        );
    }

    #[test]
    fn named_address_casefold_and_reserved_collisions_never_overwrite() {
        let manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = fresh_scratch("named-collision");
        let reserved = save_named_address(
            &manifest,
            &out,
            "POOL",
            PublicAddress::from_str(G_ADMIN).expect("valid public account"),
        )
        .expect_err("a casefolded manifest role is reserved");
        assert!(!reserved.to_string().contains(G_ADMIN));
        assert!(!out.join(NAMED_ADDRESS_DIR).join("pool.json").exists());
        assert!(save_named_address(
            &manifest,
            &out,
            "Token.BLND",
            PublicAddress::from_str(G_ADMIN).expect("valid public account"),
        )
        .is_err());

        save_named_address(
            &manifest,
            &out,
            "my-address",
            PublicAddress::from_str(G_ADMIN).expect("valid public account"),
        )
        .expect("first label");
        let path = out.join(NAMED_ADDRESS_DIR).join("my-address.json");
        let original = std::fs::read(&path).expect("first record bytes");
        let duplicate = save_named_address(
            &manifest,
            &out,
            "MY-ADDRESS",
            PublicAddress::from_str(ID_B).expect("valid contract"),
        )
        .expect_err("casefolded duplicate label is refused");
        assert!(!duplicate.to_string().contains(ID_B));
        assert_eq!(std::fs::read(path).expect("record remains"), original);
        assert!(save_named_address(
            &manifest,
            &out,
            "../escape",
            PublicAddress::from_str(G_ADMIN).expect("valid account"),
        )
        .is_err());
    }

    #[test]
    fn attachment_refuses_saved_alias_before_persisting() {
        let manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = fresh_scratch("attach-after-alias");
        let original = save_named_address(
            &manifest,
            &out,
            "watcher",
            PublicAddress::from_str(G_ADMIN).unwrap(),
        )
        .unwrap();
        let network = market_net(&out);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        let result = attach(
            &market_st(&network, &fx),
            &out,
            &manifest,
            "watcher",
            id(ID_B),
        );
        assert!(
            !attachment_path(&out, "watcher").exists(),
            "collision must refuse before saving"
        );
        assert!(result.is_err(), "saved public alias reserves its label");
        assert_eq!(named_addresses(&manifest, &out).unwrap(), original);
        assert!(fx.calls.borrow().is_empty(), "collision is a local refusal");
    }

    #[test]
    fn saved_alias_refuses_attached_role_without_changing_either_store() {
        let manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = fresh_scratch("alias-after-attach");
        let network = market_net(&out);
        let fx = FakeExecutor::new(vec![
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(&network_info_testnet(), ""),
            FakeExecutor::ok(V26, ""),
            FakeExecutor::ok(SPEC, ""),
        ]);
        attach(
            &market_st(&network, &fx),
            &out,
            &manifest,
            "watcher",
            id(ID_B),
        )
        .unwrap();
        let original = std::fs::read(attachment_path(&out, "watcher")).unwrap();
        assert!(save_named_address(
            &manifest,
            &out,
            "WATCHER",
            PublicAddress::from_str(G_ADMIN).unwrap(),
        )
        .is_err());
        assert!(!out.join(NAMED_ADDRESS_DIR).join("watcher.json").exists());
        assert_eq!(
            std::fs::read(attachment_path(&out, "watcher")).unwrap(),
            original
        );
        assert!(named_addresses(&manifest, &out)
            .unwrap()
            .iter()
            .any(|entry| entry.name == "watcher" && entry.address.as_str() == ID_B));
    }

    #[test]
    fn attachment_refuses_identity_and_oracle_labels_before_persisting() {
        let mut manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        for who in [
            crate::manifest::Who::Frodo,
            crate::manifest::Who::Sam,
            crate::manifest::Who::User,
        ] {
            manifest.identities.insert(who, G_ADMIN.parse().unwrap());
        }
        let names = std::iter::once("admin")
            .chain(crate::manifest::Who::ALL.iter().map(|who| who.as_str()))
            .chain([
                "oracle.runtime",
                "oracle.governance",
                "oracle.adapter",
                "oracle.source.0",
            ]);
        for name in names {
            let out = fresh_scratch(&format!("attach-reserved-{name}"));
            let before = named_addresses(&manifest, &out).unwrap();
            let network = market_net(&out);
            let fx = FakeExecutor::new(vec![
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(&network_info_testnet(), ""),
                FakeExecutor::ok(V26, ""),
                FakeExecutor::ok(SPEC, ""),
            ]);
            let result = attach(&market_st(&network, &fx), &out, &manifest, name, id(ID_B));
            assert!(
                !attachment_path(&out, name).exists(),
                "{name} collision persisted"
            );
            assert!(result.is_err(), "{name} is reserved");
            assert_eq!(named_addresses(&manifest, &out).unwrap(), before);
            assert!(fx.calls.borrow().is_empty());
            assert!(save_named_address(
                &manifest,
                &out,
                name,
                PublicAddress::from_str(ID_B).unwrap(),
            )
            .is_err());
            assert!(!out
                .join(NAMED_ADDRESS_DIR)
                .join(format!("{name}.json"))
                .exists());
        }
    }

    #[test]
    fn malformed_named_address_records_fail_closed_without_echoing_payloads() {
        let records = [
            (
                "bad-json",
                br#"{"name":"bad-json","address":"SECRET-DO-NOT-LEAK""#.to_vec(),
            ),
            (
                "extra-field",
                serde_json::to_vec(&serde_json::json!({
                    "name":"extra-field",
                    "address":G_ADMIN,
                    "private_key":"SECRET-DO-NOT-LEAK"
                }))
                .expect("extra-field JSON"),
            ),
            (
                "secret",
                serde_json::to_vec(&serde_json::json!({
                    "name":"secret",
                    "address":"SDO-NOT-LEAK"
                }))
                .expect("secret JSON"),
            ),
            (
                "checksum",
                serde_json::to_vec(&serde_json::json!({
                    "name":"checksum",
                    "address":"GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHG"
                }))
                .expect("checksum JSON"),
            ),
            (
                "control-name",
                serde_json::to_vec(&serde_json::json!({
                    "name":"bad\u{1b}name",
                    "address":G_ADMIN
                }))
                .expect("control name JSON"),
            ),
            (
                "control-address",
                serde_json::to_vec(&serde_json::json!({
                    "name":"control-address",
                    "address":format!("{G_ADMIN}\u{1b}")
                }))
                .expect("control address JSON"),
            ),
            (
                "mismatched",
                serde_json::to_vec(&serde_json::json!({
                    "name":"different-name",
                    "address":G_ADMIN
                }))
                .expect("mismatched label JSON"),
            ),
        ];
        for (index, (filename, bytes)) in records.into_iter().enumerate() {
            let manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
            let out = fresh_scratch(&format!("named-malformed-{index}"));
            let dir = out.join(NAMED_ADDRESS_DIR);
            std::fs::create_dir_all(&dir).expect("named address dir");
            std::fs::write(dir.join(format!("{filename}.json")), bytes)
                .expect("malformed named record");
            let error = named_addresses(&manifest, &out).expect_err("malformed data is refused");
            assert!(
                !error.to_string().contains("SECRET-DO-NOT-LEAK")
                    && !error.to_string().contains("SDO-NOT-LEAK")
                    && !error.to_string().contains(G_ADMIN),
                "stored address data must never appear in errors: {error}"
            );
        }
    }

    #[test]
    fn malformed_attached_address_metadata_fails_closed_without_echoing() {
        const FORGED: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KN";
        let cases = [
            (
                "watcher",
                serde_json::json!({
                    "role":"watcher",
                    "contract":ID_B,
                    "spec_sha256":"a".repeat(64),
                    "network":MARKET_TESTNET_PASSPHRASE,
                    "private_key":"SECRET-DO-NOT-LEAK"
                }),
                "SECRET-DO-NOT-LEAK",
            ),
            (
                "watcher",
                serde_json::json!({
                    "role":"watcher",
                    "contract":FORGED,
                    "spec_sha256":"a".repeat(64),
                    "network":MARKET_TESTNET_PASSPHRASE
                }),
                FORGED,
            ),
            (
                "watcher\u{1b}",
                serde_json::json!({
                    "role":"watcher\u{1b}",
                    "contract":ID_B,
                    "spec_sha256":"a".repeat(64),
                    "network":MARKET_TESTNET_PASSPHRASE
                }),
                "watcher\u{1b}",
            ),
        ];
        for (index, (role, record, secret)) in cases.into_iter().enumerate() {
            let manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
            let out = fresh_scratch(&format!("named-attachment-bad-{index}"));
            let dir = attachment_dir(&out);
            std::fs::create_dir_all(&dir).expect("attachment store");
            std::fs::write(
                dir.join(format!("{role}.json")),
                serde_json::to_vec(&record).expect("attachment JSON"),
            )
            .expect("attachment record");
            let error = named_addresses(&manifest, &out).expect_err("invalid attachment refused");
            assert!(
                !error.to_string().contains(secret),
                "attachment errors cannot echo stored data: {error}"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn named_address_and_attachment_store_symlinks_cannot_escape_the_run() {
        use std::os::unix::fs::symlink;

        let manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let outside = fresh_scratch("named-outside");
        std::fs::write(
            outside.join("outside.json"),
            serde_json::to_vec(&NamedAddressRecord {
                name: "outside".into(),
                address: G_ADMIN.into(),
            })
            .expect("outside record"),
        )
        .expect("outside file");

        let linked_dir = fresh_scratch("named-linked-dir");
        symlink(&outside, linked_dir.join(NAMED_ADDRESS_DIR)).expect("named store symlink");
        let error = named_addresses(&manifest, &linked_dir).expect_err("store symlink refused");
        assert!(!error.to_string().contains(outside.to_str().unwrap()));
        assert!(save_named_address(
            &manifest,
            &linked_dir,
            "escape",
            PublicAddress::from_str(G_ADMIN).expect("valid account"),
        )
        .is_err());
        assert!(!outside.join("escape.json").exists());

        let linked_record = fresh_scratch("named-linked-record");
        let named_dir = linked_record.join(NAMED_ADDRESS_DIR);
        std::fs::create_dir_all(&named_dir).expect("named store dir");
        symlink(
            outside.join("outside.json"),
            named_dir.join("record-link.json"),
        )
        .expect("record symlink");
        assert!(named_addresses(&manifest, &linked_record).is_err());

        let linked_attachments = fresh_scratch("named-linked-attachments");
        symlink(&outside, linked_attachments.join(ATTACHMENT_DIR))
            .expect("attachment store symlink");
        assert!(named_addresses(&manifest, &linked_attachments).is_err());
        assert_eq!(
            std::fs::read(outside.join("outside.json")).expect("outside record preserved"),
            serde_json::to_vec(&NamedAddressRecord {
                name: "outside".into(),
                address: G_ADMIN.into(),
            })
            .expect("outside fixture encoding")
        );
    }

    #[test]
    fn concurrent_same_name_saves_have_one_winner_and_never_clobber() {
        use std::sync::{Arc, Barrier};

        let manifest = fixture_manifest(MARKET_TESTNET_PASSPHRASE);
        let out = fresh_scratch("named-concurrent");
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let manifest = manifest.clone();
            let out = out.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                save_named_address(
                    &manifest,
                    &out,
                    "racer",
                    PublicAddress::from_str(G_ADMIN).expect("valid account"),
                )
            }));
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("save worker"))
            .collect();
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "exclusive creation permits exactly one same-name save"
        );
        let bytes =
            std::fs::read(out.join(NAMED_ADDRESS_DIR).join("racer.json")).expect("winning record");
        let parsed: NamedAddressRecord =
            serde_json::from_slice(&bytes).expect("complete winning record");
        assert_eq!(parsed.name, "racer");
        assert_eq!(parsed.address, G_ADMIN);
        let choices = named_addresses(&manifest, &out).expect("reloaded choices");
        assert_eq!(
            choices.iter().filter(|entry| entry.name == "racer").count(),
            1
        );
    }
}
