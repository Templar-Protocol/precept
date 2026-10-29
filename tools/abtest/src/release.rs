//! Resolve contract WASMs from published `Templar-Protocol/contracts` releases.
//!
//! A release is only usable once CI has recorded its catalog row on the
//! contracts `dev` branch (`contract/artifacts/releases/<artifact>@<version>.tsv`,
//! one tab-separated row `artifact version tag asset sha256 length`). The row is
//! the reviewed digest: an uncatalogued release, or bytes that disagree with the
//! row in length or SHA-256, are refused. There is no fallback to a local build
//! or to an unverified file.
//!
//! The module is deliberately free of deploy-tool state (`Artifact` is plain
//! data, the network sits behind `ReleaseSource`) so the mainnet deploy tool
//! can reuse it unchanged. The testnet-specific glue is `resolve_pins`/
//! `resolve_market`, which turn release-form pins into verified cache files.

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context};
use serde::{Deserialize, Serialize};

use crate::manifest::{ArtifactPin, ArtifactRole, MarketConfig, Sha256Hex, MARKET_ARTIFACT_ROLES};
use crate::market::sha256_hex;

pub const CONTRACTS_REPO: &str = "Templar-Protocol/contracts";
/// The branch whose catalog rows define what has been released and reviewed.
pub const CATALOG_BRANCH: &str = "dev";
const CATALOG_DIR: &str = "contract/artifacts/releases";
/// A catalog row is one short line; anything larger is not a row.
const ROW_LIMIT: u64 = 4096;
const LISTING_LIMIT: u64 = 4 << 20;

/// One catalogued contract artifact: its catalog key and its cargo package,
/// from which the release tag and asset names are derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub key: &'static str,
    pub package: &'static str,
}

/// The Soroban proxy-oracle stack as catalogued by contracts CI.
pub const SOROBAN_PROXY_STACK: [Artifact; 5] = [
    Artifact {
        key: "soroban-proxy-oracle",
        package: "templar-proxy-oracle-soroban-contract",
    },
    Artifact {
        key: "soroban-proxy-governance",
        package: "templar-proxy-oracle-soroban-governance-contract",
    },
    Artifact {
        key: "soroban-sep40-adapter",
        package: "templar-proxy-oracle-soroban-sep40-adapter-contract",
    },
    Artifact {
        key: "soroban-lazer-source",
        package: "templar-proxy-oracle-soroban-pyth-lazer-source-contract",
    },
    Artifact {
        key: "soroban-batcher",
        package: "templar-proxy-oracle-soroban-batcher-contract",
    },
];

pub fn soroban_artifact(key: &str) -> Option<Artifact> {
    SOROBAN_PROXY_STACK.iter().copied().find(|a| a.key == key)
}

impl Artifact {
    /// The release-plz tag: `{package}-v{version}`.
    fn tag(&self, version: &Version) -> String {
        format!("{}-v{version}", self.package)
    }

    /// The optimized WASM asset: `{package with - replaced by _}-{version}.wasm`.
    fn asset(&self, version: &Version) -> String {
        format!("{}-{version}.wasm", self.package.replace('-', "_"))
    }
}

/// Strict `MAJOR.MINOR.PATCH[-PRERELEASE]` (semver without build metadata).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: (u64, u64, u64),
    pre: Option<String>,
}

impl Version {
    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }
}

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let number = |part: &str| -> anyhow::Result<u64> {
            ensure!(
                !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && (part == "0" || !part.starts_with('0')),
                "not a semantic version"
            );
            part.parse::<u64>()
                .map_err(|_| anyhow!("not a semantic version"))
        };
        let mut parts = core.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            bail!("version {s:?} is not MAJOR.MINOR.PATCH");
        };
        if let Some(pre) = pre {
            for id in pre.split('.') {
                ensure!(
                    !id.is_empty()
                        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && (id.bytes().any(|b| !b.is_ascii_digit())
                            || id == "0"
                            || !id.starts_with('0')),
                    "version {s:?} has a malformed pre-release"
                );
            }
        }
        Ok(Self {
            core: (
                number(major).with_context(|| format!("version {s:?}"))?,
                number(minor).with_context(|| format!("version {s:?}"))?,
                number(patch).with_context(|| format!("version {s:?}"))?,
            ),
            pre: pre.map(str::to_string),
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (a, b, c) = self.core;
        write!(f, "{a}.{b}.{c}")?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

// Wire form is the version string, exactly as a catalog row and a market
// config spell it.
impl serde::Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Ord for Version {
    // ponytail: pre-release identifiers compare as plain strings. They only
    // order the "available versions" listing; `latest` never looks at them.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.core, self.pre.is_none(), &self.pre).cmp(&(
            other.core,
            other.pre.is_none(),
            &other.pre,
        ))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Which release to take: the newest catalogued stable one, or exactly one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionSpec {
    Latest,
    Exact(Version),
}

impl FromStr for VersionSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        if s == "latest" {
            return Ok(Self::Latest);
        }
        Ok(Self::Exact(s.parse()?))
    }
}

/// `--artifact-version <role>=<version|latest>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionOverride {
    pub role: String,
    pub spec: VersionSpec,
}

impl FromStr for VersionOverride {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (role, spec) = s
            .split_once('=')
            .context("expected <role>=<version|latest>")?;
        ensure!(
            MARKET_ARTIFACT_ROLES.contains(&role),
            "unknown artifact role {role:?}"
        );
        Ok(Self {
            role: role.to_string(),
            spec: spec.parse()?,
        })
    }
}

/// What a deploy ran against: the exact catalog row plus the catalog commit it
/// was read at. Frozen into the run's deployment plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub artifact: String,
    pub version: String,
    pub tag: String,
    pub asset: String,
    pub sha256: Sha256Hex,
    pub length: u64,
    pub catalog_commit: String,
}

/// The network seam. Tests supply a fake; `HttpSource` talks to GitHub.
pub trait ReleaseSource {
    /// Resolve the catalog branch to a commit so every later read is pinned.
    fn catalog_commit(&self) -> anyhow::Result<String>;
    /// `(filename, contents)` of every catalog row named `<artifact>@*` at `commit`.
    fn catalog_rows(&self, commit: &str, artifact: &str) -> anyhow::Result<Vec<(String, String)>>;
    fn download(&self, tag: &str, asset: &str, max_len: u64) -> anyhow::Result<Vec<u8>>;
}

/// A release-form pin after resolution: the frozen row and where the verified
/// bytes were written. `path` is a runtime fact — the caller's cache
/// directory plus the digest — and is carried onto the pin, not persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRelease {
    pub provenance: Provenance,
    pub path: PathBuf,
}

/// Naming law shared by catalog rows and frozen provenance: the tag and asset
/// must be exactly what the package and version dictate.
fn check_naming(
    artifact: &Artifact,
    version: &str,
    tag: &str,
    asset: &str,
    length: u64,
) -> anyhow::Result<Version> {
    let v: Version = version.parse()?;
    ensure!(v.to_string() == version, "version is not canonical");
    ensure!(
        tag == artifact.tag(&v),
        "tag must be {}, not {tag:?}",
        artifact.tag(&v)
    );
    ensure!(
        asset == artifact.asset(&v),
        "asset must be {}, not {asset:?}",
        artifact.asset(&v)
    );
    ensure!(length > 0, "length must be positive");
    Ok(v)
}

fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Provenance {
    fn validate(&self, artifact: &Artifact) -> anyhow::Result<Version> {
        ensure!(
            self.artifact == artifact.key,
            "provenance is for {:?}, not {}",
            self.artifact,
            artifact.key
        );
        ensure!(
            is_commit(&self.catalog_commit),
            "catalog commit is malformed"
        );
        check_naming(artifact, &self.version, &self.tag, &self.asset, self.length)
    }
}

/// Parse and fully validate one catalog row. The file is self-describing, so
/// its name is only cross-checked, never trusted.
fn parse_row(
    artifact: &Artifact,
    filename: &str,
    text: &str,
    catalog_commit: &str,
) -> anyhow::Result<Provenance> {
    let line = text.strip_suffix('\n').unwrap_or(text);
    let fields: Vec<&str> = line.split('\t').collect();
    let [key, version, tag, asset, sha256, length] = fields[..] else {
        bail!("expected 6 tab-separated fields, found {}", fields.len());
    };
    ensure!(key == artifact.key, "row is for artifact {key:?}");
    ensure!(
        filename == format!("{}@{version}.tsv", artifact.key),
        "row version {version:?} does not match its file name"
    );
    ensure!(
        length.bytes().all(|b| b.is_ascii_digit()),
        "length is not a decimal integer"
    );
    let length: u64 = length.parse().context("length is out of range")?;
    let prov = Provenance {
        artifact: key.to_string(),
        version: version.to_string(),
        tag: tag.to_string(),
        asset: asset.to_string(),
        sha256: Sha256Hex::try_from(sha256.to_string())?,
        length,
        catalog_commit: catalog_commit.to_string(),
    };
    check_naming(artifact, version, tag, asset, length)?;
    Ok(prov)
}

/// Read the catalog at one commit and pick a row. Never touches the network
/// for bytes.
pub fn resolve_row(
    src: &dyn ReleaseSource,
    artifact: &Artifact,
    spec: &VersionSpec,
) -> anyhow::Result<Provenance> {
    let commit = src.catalog_commit()?;
    ensure!(is_commit(&commit), "catalog commit is malformed");
    let mut rows = Vec::new();
    for (name, text) in src.catalog_rows(&commit, artifact.key)? {
        let row = parse_row(artifact, &name, &text, &commit)
            .with_context(|| format!("invalid catalog row {name}"))?;
        rows.push((row.version.parse::<Version>()?, row));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    ensure!(
        !rows.is_empty(),
        "no catalogued release for {} in {CONTRACTS_REPO} {CATALOG_BRANCH} ({commit})",
        artifact.key
    );
    let pick = match spec {
        VersionSpec::Latest => rows.iter().rev().find(|(v, _)| !v.is_prerelease()),
        VersionSpec::Exact(want) => rows.iter().find(|(v, _)| v == want),
    };
    match pick {
        Some((_, row)) => Ok(row.clone()),
        None => {
            let listed: Vec<String> = rows.iter().map(|(v, _)| v.to_string()).collect();
            match spec {
                VersionSpec::Latest => bail!(
                    "no stable catalogued release for {}; catalogued: {}",
                    artifact.key,
                    listed.join(", ")
                ),
                VersionSpec::Exact(want) => bail!(
                    "no catalogued release {} {want}; catalogued: {}",
                    artifact.key,
                    listed.join(", ")
                ),
            }
        }
    }
}

fn digest_matches(bytes: &[u8], length: u64, sha256: &Sha256Hex) -> bool {
    bytes.len() as u64 == length && sha256_hex(bytes) == sha256.as_str()
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// `$XDG_CACHE_HOME/precept-abtest/artifacts`, else `$HOME/.cache/...`.
/// A relative base is refused rather than taken against the process cwd, so
/// the cache never depends on where the tool happened to be invoked.
pub fn default_cache_dir() -> anyhow::Result<PathBuf> {
    cache_dir_from(env_path("XDG_CACHE_HOME"), env_path("HOME"))
}

/// Only an absolute base is honored: a relative one would resolve against the
/// process cwd, making the cache depend on where the tool was invoked.
fn cache_dir_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let absolute = |base: &Path| base.is_absolute().then(|| base.to_path_buf());
    let base = match (
        xdg.as_deref().and_then(absolute),
        home.as_deref().and_then(absolute),
    ) {
        (Some(xdg), _) => xdg,
        (None, Some(home)) => home.join(".cache"),
        (None, None) => {
            bail!("neither XDG_CACHE_HOME nor HOME is set to an absolute path; no artifact cache")
        }
    };
    Ok(base.join("precept-abtest").join("artifacts"))
}

/// Verified bytes for a row, as a cache file named by its digest. A cache hit
/// is re-hashed; a corrupt entry is replaced, never trusted. Nothing is
/// cached unless length and SHA-256 match the row.
pub fn fetch_verified(
    src: &dyn ReleaseSource,
    cache: &Path,
    prov: &Provenance,
) -> anyhow::Result<PathBuf> {
    let path = cache.join(format!("{}.wasm", prov.sha256.as_str()));
    match std::fs::read(&path) {
        Ok(bytes) if digest_matches(&bytes, prov.length, &prov.sha256) => return Ok(path),
        Ok(_) => std::fs::remove_file(&path).context("removing a corrupt cached artifact")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::new(e).context("reading the artifact cache")),
    }
    let bytes = src
        .download(&prov.tag, &prov.asset, prov.length)
        .with_context(|| format!("downloading release asset {}", prov.asset))?;
    ensure!(
        bytes.len() as u64 == prov.length,
        "release asset {} is {} bytes; the catalog row says {}",
        prov.asset,
        bytes.len(),
        prov.length
    );
    ensure!(
        sha256_hex(&bytes) == prov.sha256.as_str(),
        "release asset {} does not match the catalog digest {}",
        prov.asset,
        prov.sha256.as_str()
    );
    std::fs::create_dir_all(cache).context("creating the artifact cache")?;
    let tmp = cache.join(format!(
        "{}.{}.tmp",
        prov.sha256.as_str(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context("writing the artifact cache"));
    }
    Ok(path)
}

/// Catalog row plus verified bytes.
pub fn resolve(
    src: &dyn ReleaseSource,
    cache: &Path,
    artifact: &Artifact,
    spec: &VersionSpec,
) -> anyhow::Result<ResolvedRelease> {
    let provenance = resolve_row(src, artifact, spec)?;
    let path = fetch_verified(src, cache, &provenance)?;
    Ok(ResolvedRelease { provenance, path })
}

/// Re-materialize previously frozen provenance. The catalog is not consulted:
/// the frozen row is the authority, and the bytes must still match it.
pub fn resolve_frozen(
    src: &dyn ReleaseSource,
    cache: &Path,
    artifact: &Artifact,
    frozen: &Provenance,
) -> anyhow::Result<ResolvedRelease> {
    frozen
        .validate(artifact)
        .context("frozen release provenance")?;
    let path = fetch_verified(src, cache, frozen)?;
    Ok(ResolvedRelease {
        provenance: frozen.clone(),
        path,
    })
}

/// Turn every release-form pin into verified cache bytes, in place.
///
/// Precedence for the wanted version: `--artifact-version` override, then the
/// pin's `version`, then `latest`. A role with frozen provenance (a resumed
/// run) always uses the frozen exact version; a request for a different exact
/// version is refused rather than silently ignored. Overrides may only name
/// release-form roles.
pub fn resolve_pins(
    pins: &mut BTreeMap<ArtifactRole, ArtifactPin>,
    frozen: Option<&BTreeMap<String, Provenance>>,
    overrides: &[VersionOverride],
    src: &dyn ReleaseSource,
    cache: &Path,
) -> anyhow::Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for o in overrides {
        ensure!(
            seen.insert(o.role.as_str()),
            "--artifact-version names role {} twice",
            o.role
        );
        let is_release = pins.iter().any(|(role, pin)| {
            role.to_string() == o.role && matches!(pin, ArtifactPin::Release(_))
        });
        ensure!(
            is_release,
            "--artifact-version applies only to release-form roles; {} is not one",
            o.role
        );
    }
    for (role, pin) in pins.iter_mut() {
        let ArtifactPin::Release(rp) = pin else {
            continue;
        };
        let name = role.to_string();
        let artifact = soroban_artifact(&rp.release)
            .ok_or_else(|| anyhow!("role {name} names an unknown release artifact"))?;
        let wanted = match overrides.iter().find(|o| o.role == name) {
            Some(o) => o.spec.clone(),
            None => match &rp.version {
                Some(v) => VersionSpec::Exact(v.clone()),
                None => VersionSpec::Latest,
            },
        };
        let resolved = match frozen {
            Some(frozen) => {
                let prov = frozen.get(&name).ok_or_else(|| {
                    anyhow!("the run's frozen plan has no release pin for role {name}")
                })?;
                if let VersionSpec::Exact(want) = &wanted {
                    ensure!(
                        want.to_string() == prov.version,
                        "role {name} is frozen on {} in this run; {want} was requested. Start a new run directory to change versions",
                        prov.version
                    );
                }
                resolve_frozen(src, cache, &artifact, prov)
            }
            None => resolve(src, cache, &artifact, &wanted),
        }
        .with_context(|| format!("resolving artifact role {name}"))?;
        rp.version = Some(
            resolved
                .provenance
                .version
                .parse()
                .context("catalogued version is not a semantic version")?,
        );
        rp.path = Some(resolved.path);
        rp.resolved = Some(resolved.provenance);
    }
    Ok(())
}

/// Resolve a market config's release pins at deploy start. A run directory
/// that already holds a frozen plan resumes on the plan's exact versions and
/// never consults the catalog.
pub fn resolve_market(
    cfg: &mut MarketConfig,
    out: &Path,
    overrides: &[VersionOverride],
    src: &dyn ReleaseSource,
    cache: &Path,
) -> anyhow::Result<()> {
    let plan = crate::deploy::read_deployment_plan(out)?;
    let frozen = plan.map(|p| p.releases);
    resolve_pins(&mut cfg.artifacts, frozen.as_ref(), overrides, src, cache)
}

/// GitHub-backed source. Reads go through the API at a pinned commit; the
/// token (public repo, so optional) is only ever sent to `api.github.com`.
pub struct HttpSource {
    agent: ureq::Agent,
    token: OnceCell<Option<String>>,
}

impl HttpSource {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout(Duration::from_secs(180))
                .user_agent("precept-abtest")
                .build(),
            token: OnceCell::new(),
        }
    }

    fn token(&self) -> Option<&str> {
        self.token
            .get_or_init(|| {
                for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
                    if let Ok(t) = std::env::var(var) {
                        if !t.trim().is_empty() {
                            return Some(t.trim().to_string());
                        }
                    }
                }
                let out = std::process::Command::new("gh")
                    .args(["auth", "token"])
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()?;
                let t = String::from_utf8(out.stdout).ok()?;
                (out.status.success() && !t.trim().is_empty()).then(|| t.trim().to_string())
            })
            .as_deref()
    }

    fn get(
        &self,
        url: &str,
        accept: &str,
        token: Option<&str>,
        limit: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let mut req = self.agent.get(url).set("Accept", accept);
        if let Some(t) = token {
            req = req
                .set("Authorization", &format!("Bearer {t}"))
                .set("X-GitHub-Api-Version", "2022-11-28");
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => {
                let hint = if matches!(code, 401 | 403 | 429) {
                    " (GitHub auth or rate limit; set GH_TOKEN or run `gh auth login`)"
                } else {
                    ""
                };
                bail!("GET {url}: HTTP {code}{hint}")
            }
            Err(e) => bail!("GET {url}: {e}"),
        };
        let mut buf = Vec::new();
        resp.into_reader()
            .take(limit + 1)
            .read_to_end(&mut buf)
            .with_context(|| format!("reading {url}"))?;
        ensure!(
            buf.len() as u64 <= limit,
            "GET {url}: response exceeds {limit} bytes"
        );
        Ok(buf)
    }

    /// API read. The repository is public, so a rejected token is retried
    /// anonymously rather than failing on a stale credential.
    fn api(&self, path_and_query: &str, accept: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
        let url = format!("https://api.github.com/repos/{CONTRACTS_REPO}/{path_and_query}");
        match self.get(&url, accept, self.token(), limit) {
            Err(e) if self.token().is_some() && e.to_string().contains("HTTP 401") => {
                self.get(&url, accept, None, limit)
            }
            other => other,
        }
    }
}

impl Default for HttpSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseSource for HttpSource {
    fn catalog_commit(&self) -> anyhow::Result<String> {
        let body = self.api(
            &format!("commits/{CATALOG_BRANCH}"),
            "application/vnd.github.sha",
            256,
        )?;
        Ok(String::from_utf8(body)
            .context("catalog commit is not UTF-8")?
            .trim()
            .to_string())
    }

    fn catalog_rows(&self, commit: &str, artifact: &str) -> anyhow::Result<Vec<(String, String)>> {
        ensure!(is_commit(commit), "catalog commit is malformed");
        // ponytail: the contents API lists at most 1000 entries per directory;
        // the catalog is ~30. Past that, list via the git trees API instead.
        let listing = self.api(
            &format!("contents/{CATALOG_DIR}?ref={commit}"),
            "application/vnd.github+json",
            LISTING_LIMIT,
        )?;
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            r#type: String,
        }
        let entries: Vec<Entry> =
            serde_json::from_slice(&listing).context("catalog listing is malformed")?;
        let prefix = format!("{artifact}@");
        let mut rows = Vec::new();
        for e in entries {
            // The name goes into a URL path: only the catalog's own alphabet.
            let plain = e
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'));
            if e.r#type != "file" || !e.name.starts_with(&prefix) || !e.name.ends_with(".tsv") {
                continue;
            }
            ensure!(plain, "catalog file name is malformed");
            let body = self.api(
                &format!("contents/{CATALOG_DIR}/{}?ref={commit}", e.name),
                "application/vnd.github.raw+json",
                ROW_LIMIT,
            )?;
            rows.push((
                e.name,
                String::from_utf8(body).context("catalog row is not UTF-8")?,
            ));
        }
        Ok(rows)
    }

    fn download(&self, tag: &str, asset: &str, max_len: u64) -> anyhow::Result<Vec<u8>> {
        let url = format!("https://github.com/{CONTRACTS_REPO}/releases/download/{tag}/{asset}");
        self.get(&url, "application/octet-stream", None, max_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ReleasePin;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const KEY: &str = "soroban-proxy-oracle";
    const PKG: &str = "templar-proxy-oracle-soroban-contract";

    #[derive(Default)]
    struct Fake {
        rows: RefCell<Vec<(String, String)>>,
        blobs: RefCell<HashMap<String, Vec<u8>>>,
        catalog_reads: Cell<u32>,
        downloads: Cell<u32>,
    }

    fn wasm(version: &str) -> Vec<u8> {
        format!("wasm bytes for {version}").into_bytes()
    }

    impl Fake {
        fn row_text(version: &str, bytes: &[u8]) -> String {
            let asset = format!("{}-{version}.wasm", PKG.replace('-', "_"));
            format!(
                "{KEY}\t{version}\t{PKG}-v{version}\t{asset}\t{}\t{}\n",
                sha256_hex(bytes),
                bytes.len()
            )
        }

        fn with(versions: &[&str]) -> Self {
            let fake = Self::default();
            for v in versions {
                fake.publish(v);
            }
            fake
        }

        fn publish(&self, version: &str) {
            let bytes = wasm(version);
            self.rows.borrow_mut().push((
                format!("{KEY}@{version}.tsv"),
                Self::row_text(version, &bytes),
            ));
            self.blobs
                .borrow_mut()
                .insert(format!("{PKG}-v{version}"), bytes);
        }

        /// Rewrite the only row's text through `f` (a tampering hook).
        fn tamper(&self, f: impl Fn(&str) -> String) {
            let mut rows = self.rows.borrow_mut();
            rows[0].1 = f(&rows[0].1);
        }
    }

    impl ReleaseSource for Fake {
        fn catalog_commit(&self) -> anyhow::Result<String> {
            Ok(COMMIT.to_string())
        }

        fn catalog_rows(
            &self,
            commit: &str,
            artifact: &str,
        ) -> anyhow::Result<Vec<(String, String)>> {
            assert_eq!(commit, COMMIT, "rows must be read at the resolved commit");
            self.catalog_reads.set(self.catalog_reads.get() + 1);
            Ok(self
                .rows
                .borrow()
                .iter()
                .filter(|(n, _)| n.starts_with(&format!("{artifact}@")))
                .cloned()
                .collect())
        }

        fn download(&self, tag: &str, _asset: &str, _max: u64) -> anyhow::Result<Vec<u8>> {
            self.downloads.set(self.downloads.get() + 1);
            self.blobs
                .borrow()
                .get(tag)
                .cloned()
                .ok_or_else(|| anyhow!("404"))
        }
    }

    fn artifact() -> Artifact {
        soroban_artifact(KEY).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abtest-release-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec(s: &str) -> VersionSpec {
        s.parse().unwrap()
    }

    fn row(fake: &Fake, s: &str) -> anyhow::Result<Provenance> {
        resolve_row(fake, &artifact(), &spec(s))
    }

    #[test]
    fn latest_picks_highest_stable_numerically() {
        let fake = Fake::with(&["0.1.2", "0.1.10", "0.1.9", "0.2.0-rc.1"]);
        let got = row(&fake, "latest").unwrap();
        assert_eq!(got.version, "0.1.10", "0.1.10 outranks 0.1.9 and the rc");
        assert_eq!(got.catalog_commit, COMMIT);
        assert_eq!(got.tag, format!("{PKG}-v0.1.10"));
        assert_eq!(got.asset, format!("{}-0.1.10.wasm", PKG.replace('-', "_")));
    }

    #[test]
    fn prerelease_is_chosen_only_when_named_exactly() {
        let fake = Fake::with(&["0.1.2", "0.2.0-rc.1"]);
        assert_eq!(row(&fake, "latest").unwrap().version, "0.1.2");
        assert_eq!(row(&fake, "0.2.0-rc.1").unwrap().version, "0.2.0-rc.1");
        let only_rc = Fake::with(&["0.2.0-rc.1"]);
        let err = row(&only_rc, "latest").unwrap_err().to_string();
        assert!(err.contains("no stable catalogued release"), "{err}");
    }

    #[test]
    fn exact_version_hit_and_miss() {
        let fake = Fake::with(&["0.1.2", "0.1.3"]);
        assert_eq!(row(&fake, "0.1.2").unwrap().version, "0.1.2");
        let err = row(&fake, "0.1.9").unwrap_err().to_string();
        assert!(
            err.contains("no catalogued release soroban-proxy-oracle 0.1.9"),
            "{err}"
        );
        assert!(
            err.contains("0.1.2, 0.1.3"),
            "the miss must list what exists: {err}"
        );
    }

    #[test]
    fn empty_catalog_is_a_clean_refusal() {
        let err = row(&Fake::default(), "latest").unwrap_err().to_string();
        assert!(
            err.starts_with("no catalogued release for soroban-proxy-oracle"),
            "{err}"
        );
    }

    #[test]
    fn rows_for_other_artifacts_are_ignored() {
        let fake = Fake::with(&["0.1.2"]);
        fake.rows.borrow_mut().push((
            "soroban-proxy-governance@9.9.9.tsv".into(),
            "garbage".into(),
        ));
        assert_eq!(row(&fake, "latest").unwrap().version, "0.1.2");
    }

    #[test]
    fn tampered_rows_are_rejected() {
        let good = Fake::row_text("0.1.2", &wasm("0.1.2"));
        let sha = sha256_hex(&wasm("0.1.2"));
        let cases: Vec<(&str, String)> = vec![
            ("artifact key", good.replacen(KEY, "soroban-batcher", 1)),
            ("version", good.replacen("\t0.1.2\t", "\t0.1.3\t", 1)),
            ("non-semver", good.replacen("\t0.1.2\t", "\t01.2\t", 1)),
            ("tag", good.replace(&format!("{PKG}-v0.1.2"), "evil-v0.1.2")),
            (
                "asset",
                good.replace("_contract-0.1.2.wasm", "_contract-0.1.3.wasm"),
            ),
            ("short sha", good.replace(&sha, &sha[..63])),
            ("upper sha", good.replace(&sha, &sha.to_uppercase())),
            (
                "zero length",
                good.replace(&format!("\t{}\n", wasm("0.1.2").len()), "\t0\n"),
            ),
            (
                "signed length",
                good.replace(&format!("\t{}\n", wasm("0.1.2").len()), "\t+5\n"),
            ),
            ("extra field", good.replace('\n', "\textra\n")),
            (
                "missing field",
                good.replace('\n', "")
                    .rsplit_once('\t')
                    .unwrap()
                    .0
                    .to_string(),
            ),
        ];
        for (what, text) in cases {
            let fake = Fake::with(&["0.1.2"]);
            fake.tamper(|_| text.clone());
            assert!(
                row(&fake, "latest").is_err(),
                "a tampered {what} must be refused"
            );
        }
        // A file name that disagrees with its row is refused too.
        let fake = Fake::with(&["0.1.2"]);
        fake.rows.borrow_mut()[0].0 = format!("{KEY}@0.1.3.tsv");
        assert!(
            row(&fake, "latest").is_err(),
            "a renamed row must be refused"
        );
    }

    #[test]
    fn version_parser_is_strict() {
        for ok in ["0.0.0", "1.2.3", "1.2.3-rc.1", "1.2.3-0", "1.2.3-alpha-1"] {
            assert_eq!(ok.parse::<Version>().unwrap().to_string(), ok);
        }
        for bad in [
            "1.2", "1.2.3.4", "01.2.3", "1.2.3-", "1.2.3-01", "1.2.3+b", "v1.2.3", "1.2.-3", "",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn downloads_are_verified_then_cached_and_reverified() {
        let fake = Fake::with(&["0.1.2"]);
        let cache = scratch("cache");
        let got = resolve(&fake, &cache, &artifact(), &spec("latest")).unwrap();
        assert_eq!(std::fs::read(&got.path).unwrap(), wasm("0.1.2"));
        assert_eq!(
            got.path,
            cache.join(format!("{}.wasm", got.provenance.sha256.as_str()))
        );
        assert_eq!(fake.downloads.get(), 1);

        // A second resolve is a cache hit: no further download.
        resolve(&fake, &cache, &artifact(), &spec("latest")).unwrap();
        assert_eq!(
            fake.downloads.get(),
            1,
            "a verified cache hit must not download"
        );

        // A corrupted cache entry is detected and replaced, never trusted.
        std::fs::write(&got.path, b"poisoned").unwrap();
        let again = resolve(&fake, &cache, &artifact(), &spec("latest")).unwrap();
        assert_eq!(fake.downloads.get(), 2);
        assert_eq!(std::fs::read(&again.path).unwrap(), wasm("0.1.2"));
        let leftovers: Vec<_> = std::fs::read_dir(&cache).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp file may survive");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn length_or_digest_mismatch_refuses_and_caches_nothing() {
        let cache = scratch("mismatch");
        // Right length, wrong bytes.
        let fake = Fake::with(&["0.1.2"]);
        let mut evil = wasm("0.1.2");
        evil[0] ^= 1;
        fake.blobs
            .borrow_mut()
            .insert(format!("{PKG}-v0.1.2"), evil);
        let err = resolve(&fake, &cache, &artifact(), &spec("latest")).unwrap_err();
        assert!(
            format!("{err:#}").contains("does not match the catalog digest"),
            "{err:#}"
        );
        // Wrong length.
        let fake = Fake::with(&["0.1.2"]);
        fake.blobs
            .borrow_mut()
            .insert(format!("{PKG}-v0.1.2"), b"short".to_vec());
        let err = resolve(&fake, &cache, &artifact(), &spec("latest")).unwrap_err();
        assert!(
            format!("{err:#}").contains("the catalog row says"),
            "{err:#}"
        );
        // Missing release asset.
        let fake = Fake::with(&["0.1.2"]);
        fake.blobs.borrow_mut().clear();
        assert!(resolve(&fake, &cache, &artifact(), &spec("latest")).is_err());
        let cached = std::fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0);
        assert_eq!(cached, 0, "a refused download must leave the cache empty");
        let _ = std::fs::remove_dir_all(&cache);
    }

    fn release_pin(version: Option<&str>) -> ArtifactPin {
        ArtifactPin::Release(ReleasePin {
            release: KEY.to_string(),
            version: version.map(|v| v.parse().unwrap()),
            resolved: None,
            path: None,
        })
    }

    fn pins(version: Option<&str>) -> BTreeMap<ArtifactRole, ArtifactPin> {
        BTreeMap::from([(ArtifactRole::ProxyRuntime, release_pin(version))])
    }

    fn resolved(pins: &BTreeMap<ArtifactRole, ArtifactPin>) -> Provenance {
        match &pins[&ArtifactRole::ProxyRuntime] {
            ArtifactPin::Release(rp) => rp.resolved.clone().expect("resolved"),
            ArtifactPin::File(_) | ArtifactPin::Live(_) => panic!("not a release pin"),
        }
    }

    fn freeze(pins: &BTreeMap<ArtifactRole, ArtifactPin>) -> BTreeMap<String, Provenance> {
        BTreeMap::from([("proxy-runtime".to_string(), resolved(pins))])
    }

    #[test]
    fn frozen_run_resumes_on_its_version_when_the_catalog_moves_on() {
        let fake = Fake::with(&["0.1.2"]);
        let cache = scratch("frozen");
        let mut first = pins(None);
        resolve_pins(&mut first, None, &[], &fake, &cache).unwrap();
        let frozen = freeze(&first);
        assert_eq!(frozen["proxy-runtime"].version, "0.1.2");

        fake.publish("0.2.0");
        let reads = fake.catalog_reads.get();
        let mut resumed = pins(None);
        resolve_pins(&mut resumed, Some(&frozen), &[], &fake, &cache).unwrap();
        assert_eq!(resolved(&resumed), frozen["proxy-runtime"]);
        assert_eq!(
            fake.catalog_reads.get(),
            reads,
            "resume must not read the catalog"
        );
        // A fresh run, by contrast, picks the new release.
        let mut fresh = pins(None);
        resolve_pins(&mut fresh, None, &[], &fake, &cache).unwrap();
        assert_eq!(resolved(&fresh).version, "0.2.0");
        // `latest` and the frozen exact version are both acceptable requests; a
        // different exact version is refused.
        let latest = [VersionOverride {
            role: "proxy-runtime".into(),
            spec: VersionSpec::Latest,
        }];
        resolve_pins(&mut pins(None), Some(&frozen), &latest, &fake, &cache).unwrap();
        let other = [VersionOverride {
            role: "proxy-runtime".into(),
            spec: spec("0.2.0"),
        }];
        let err = resolve_pins(&mut pins(None), Some(&frozen), &other, &fake, &cache).unwrap_err();
        assert!(format!("{err:#}").contains("frozen on 0.1.2"), "{err:#}");
        let err =
            resolve_pins(&mut pins(Some("0.2.0")), Some(&frozen), &[], &fake, &cache).unwrap_err();
        assert!(format!("{err:#}").contains("frozen on 0.1.2"), "{err:#}");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn frozen_provenance_is_reverified_against_its_digest() {
        let fake = Fake::with(&["0.1.2"]);
        let cache = scratch("frozen-verify");
        let mut first = pins(None);
        resolve_pins(&mut first, None, &[], &fake, &cache).unwrap();
        let mut frozen = freeze(&first);
        // A forged frozen digest cannot be satisfied by the real bytes.
        let forged = Sha256Hex::try_from("a".repeat(64)).unwrap();
        frozen.get_mut("proxy-runtime").unwrap().sha256 = forged;
        assert!(resolve_pins(&mut pins(None), Some(&frozen), &[], &fake, &cache).is_err());
        // Forged naming is refused before any download.
        let mut frozen = freeze(&first);
        frozen.get_mut("proxy-runtime").unwrap().tag = "evil-v0.1.2".into();
        assert!(resolve_pins(&mut pins(None), Some(&frozen), &[], &fake, &cache).is_err());
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// The deploy path reads exactly what the resolver verified: a resolved
    /// pin names the bytes in the cache directory it was resolved into.
    #[test]
    fn a_resolved_pin_reads_the_bytes_the_resolver_verified() {
        let fake = Fake::with(&["0.1.2"]);
        let cache = scratch("pin-path");
        let mut pins = pins(None);
        resolve_pins(&mut pins, None, &[], &fake, &cache).unwrap();
        let (path, sha256) = pins[&ArtifactRole::ProxyRuntime].file_location().unwrap();
        assert_eq!(
            path,
            cache.join(format!("{}.wasm", resolved(&pins).sha256.as_str()))
        );
        assert_eq!(
            path.parent(),
            Some(cache.as_path()),
            "the pin names the cache it was resolved into"
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(sha256_hex(&bytes), sha256.as_str());
        // An unresolved release pin has no bytes to read.
        assert!(release_pin(None).file_location().is_err());
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// What a run is frozen to is the catalog row, not where its bytes were
    /// written: the same release resolved into two different caches
    /// serializes to the same bytes, with the version still spelled as a
    /// string and no cache path anywhere.
    #[test]
    fn the_frozen_config_names_the_row_not_the_cache_directory() {
        let fake = Fake::with(&["0.1.2"]);
        let a = scratch("digest-a");
        let b = scratch("digest-b");
        let mut first = pins(None);
        let mut second = pins(None);
        resolve_pins(&mut first, None, &[], &fake, &a).unwrap();
        resolve_pins(&mut second, None, &[], &fake, &b).unwrap();
        let rendered = toml::to_string(&first).unwrap();
        assert_eq!(
            rendered,
            toml::to_string(&second).unwrap(),
            "a different cache directory must not change what the config is frozen to"
        );
        assert!(
            rendered.contains("version = \"0.1.2\""),
            "the version keeps its wire string: {rendered}"
        );
        assert!(
            !rendered.contains(a.to_str().unwrap()) && !rendered.contains(b.to_str().unwrap()),
            "the cache directory is never frozen: {rendered}"
        );
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn override_precedence_is_cli_then_config_then_latest() {
        let fake = Fake::with(&["0.1.2", "0.1.3", "0.1.4"]);
        let cache = scratch("precedence");
        let version = |pins: &BTreeMap<ArtifactRole, ArtifactPin>| resolved(pins).version;
        let ov = |s: &str| {
            [VersionOverride {
                role: "proxy-runtime".into(),
                spec: spec(s),
            }]
        };

        let mut p = pins(None);
        resolve_pins(&mut p, None, &[], &fake, &cache).unwrap();
        assert_eq!(version(&p), "0.1.4", "absent version means latest");
        let mut p = pins(Some("0.1.2"));
        resolve_pins(&mut p, None, &[], &fake, &cache).unwrap();
        assert_eq!(version(&p), "0.1.2", "the config version pins");
        let mut p = pins(Some("0.1.2"));
        resolve_pins(&mut p, None, &ov("0.1.3"), &fake, &cache).unwrap();
        assert_eq!(
            version(&p),
            "0.1.3",
            "the CLI override beats the config version"
        );
        let mut p = pins(Some("0.1.2"));
        resolve_pins(&mut p, None, &ov("latest"), &fake, &cache).unwrap();
        assert_eq!(
            version(&p),
            "0.1.4",
            "the CLI can ask for latest over a pinned config"
        );
        let mut p = pins(None);
        assert!(resolve_pins(&mut p, None, &ov("0.9.9"), &fake, &cache).is_err());
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn overrides_apply_only_to_release_form_roles() {
        let fake = Fake::with(&["0.1.2"]);
        let cache = scratch("override-scope");
        let mut map = pins(None);
        map.insert(
            ArtifactRole::Pool,
            ArtifactPin::File(crate::manifest::FilePin {
                path: "/nonexistent/pool.wasm".into(),
                sha256: Sha256Hex::default(),
            }),
        );
        let on_pool = [VersionOverride {
            role: "pool".into(),
            spec: spec("1.0.0"),
        }];
        let err = resolve_pins(&mut map, None, &on_pool, &fake, &cache).unwrap_err();
        assert!(
            err.to_string().contains("only to release-form roles"),
            "{err}"
        );
        assert_eq!(
            fake.catalog_reads.get(),
            0,
            "refused before any network read"
        );
        let twice = [
            VersionOverride {
                role: "proxy-runtime".into(),
                spec: spec("0.1.2"),
            },
            VersionOverride {
                role: "proxy-runtime".into(),
                spec: spec("latest"),
            },
        ];
        assert!(resolve_pins(&mut map, None, &twice, &fake, &cache).is_err());
        assert!("nonsense".parse::<VersionOverride>().is_err());
        assert!("not-a-role=1.0.0".parse::<VersionOverride>().is_err());
        assert_eq!(
            "proxy-governance=latest"
                .parse::<VersionOverride>()
                .unwrap(),
            VersionOverride {
                role: "proxy-governance".into(),
                spec: VersionSpec::Latest
            }
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn path_form_roles_are_untouched_and_never_hit_the_network() {
        let fake = Fake::default();
        let cache = scratch("path-form");
        let file = crate::manifest::FilePin {
            path: "/some/pool.wasm".into(),
            sha256: Sha256Hex::default(),
        };
        let mut map = BTreeMap::from([(ArtifactRole::Pool, ArtifactPin::File(file.clone()))]);
        resolve_pins(&mut map, None, &[], &fake, &cache).unwrap();
        assert!(matches!(&map[&ArtifactRole::Pool], ArtifactPin::File(f) if f.path == file.path));
        assert_eq!(fake.catalog_reads.get() + fake.downloads.get(), 0);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn cache_dir_refuses_a_relative_base() {
        let absolute = PathBuf::from("/abs");
        assert_eq!(
            cache_dir_from(Some(absolute.clone()), None).unwrap(),
            absolute.join("precept-abtest/artifacts")
        );
        assert_eq!(
            cache_dir_from(None, Some(absolute.clone())).unwrap(),
            absolute.join(".cache/precept-abtest/artifacts")
        );
        // A relative XDG falls through to HOME; a relative HOME is refused.
        assert_eq!(
            cache_dir_from(Some("rel".into()), Some(absolute.clone())).unwrap(),
            absolute.join(".cache/precept-abtest/artifacts")
        );
        let err = cache_dir_from(None, Some("rel".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute path"), "{err}");
        assert!(cache_dir_from(None, None).is_err());
    }
}
