//! The native deps.dev v3 REST client (supply-chain plan §1), and the purl
//! decomposition its keys come from (plan §2).
//!
//! Endpoints, base [`BASE_URL`], no auth. The field names are the live v3
//! keys (checked by anonymous curl, 2026-11-01); a renamed key fails decode
//! rather than silently defaulting:
//! - [`Client::package`]: `GET /systems/{system}/packages/{name}`, every version
//!   with `isDefault` and `isDeprecated`.
//! - [`Client::version`]: `GET .../versions/{version}`, with `licenses`,
//!   `advisoryKeys` (for that exact version), `links` and SLSA provenance.
//! - [`Client::requirements`]: `GET .../versions/{version}:requirements`, the
//!   manifest constraints.
//! - [`Client::dependencies`]: `GET .../versions/{version}:dependencies`, the
//!   resolved graph (SELF, DIRECT, INDIRECT nodes and requirement edges).
//! - [`Client::project`]: `GET /projects/{project-id}`, stars, license and the
//!   OpenSSF Scorecard.
//! - [`Client::advisory`]: `GET /advisories/{osv-id}`, title, aliases and CVSS3.
//!
//! Every path segment is percent-encoded as one segment, so the Go module
//! `github.com/google/uuid` becomes `github.com%2Fgoogle%2Fuuid`. Each fetch
//! goes through the [`HttpCache`], so repeated calls within the hour are served
//! from memory or disk, and network calls are self-throttled at
//! [`MIN_INTERVAL`]. A 404 is [`Error::NotFound`].
//!
//! The transport is injected ([`Transport`]). Production passes
//! [`ReqwestTransport`]; the tests pass a fake that answers from the golden
//! fixtures under `tests/fixtures/supply_chain/deps_dev/`, so CI never touches
//! the network. Those fixtures match the live key structure; their values are
//! trimmed or synthetic.
//!
//! purl decomposition ([`version_key`]), following GUAC's rules: `golang` maps
//! to `GO`; every other type is upper-cased (`cargo` to `CARGO`, `npm` to
//! `NPM`, `pypi` to `PYPI`); a maven purl's name is `groupId:artifactId`;
//! qualifiers and subpaths are dropped. A purl with no version is rejected,
//! because deps.dev keys are versioned.
use crate::supply_chain::http_cache::{FetchError, HttpCache, Transport};
use chrono::{DateTime, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::time::Duration;

/// The deps.dev v3 API root.
pub const BASE_URL: &str = "https://api.deps.dev/v3";

/// The [`HttpCache`] namespace for this client's entries.
pub const NAMESPACE: &str = "deps-dev";

/// The gap between deps.dev requests: four a second. The API publishes no
/// limit, so this is a conservative posture and not a measured one.
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// A versioned deps.dev key: the `system`, `name` and `version` that the
/// endpoints take in their paths and that every response repeats as `versionKey`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VersionKey {
    pub system: String,
    pub name: String,
    pub version: String,
}

/// Why a purl has no deps.dev key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurlError {
    /// Not a `pkg:type/...@version` purl.
    Malformed { purl: String },
    /// A well-formed purl without a version.
    Versionless { purl: String },
}

impl fmt::Display for PurlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { purl } => write!(f, "`{purl}` is not a pkg:type/name@version purl"),
            Self::Versionless { purl } => {
                write!(f, "`{purl}` has no version; deps.dev keys are versioned")
            }
        }
    }
}

impl std::error::Error for PurlError {}

/// Decomposes a purl into its deps.dev key.
pub fn version_key(purl: &str) -> Result<VersionKey, PurlError> {
    let malformed = || PurlError::Malformed {
        purl: purl.to_owned(),
    };
    let rest = purl.strip_prefix("pkg:").ok_or_else(malformed)?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (path, version) = rest
        .rsplit_once('@')
        .ok_or_else(|| PurlError::Versionless {
            purl: purl.to_owned(),
        })?;
    let version = decode(version).ok_or_else(|| PurlError::Versionless {
        purl: purl.to_owned(),
    })?;
    let (kind, path) = path.split_once('/').ok_or_else(malformed)?;
    let segments = path
        .split('/')
        .map(decode)
        .collect::<Option<Vec<String>>>()
        .ok_or_else(malformed)?;
    let name = if kind.eq_ignore_ascii_case("maven") {
        match segments.as_slice() {
            [group, artifact] => format!("{group}:{artifact}"),
            _ => return Err(malformed()),
        }
    } else {
        segments.join("/")
    };
    let system = if kind.eq_ignore_ascii_case("golang") {
        "GO".to_owned()
    } else {
        kind.to_ascii_uppercase()
    };
    Ok(VersionKey {
        system,
        name,
        version,
    })
}

/// One percent-decoded purl component, or `None` when it decodes to nothing.
fn decode(component: &str) -> Option<String> {
    percent_decode_str(component)
        .decode_utf8()
        .ok()
        .filter(|text| !text.is_empty())
        .map(|text| text.into_owned())
}

/// Path segments are encoded strictly: only unreserved characters pass.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn segment(value: &str) -> String {
    utf8_percent_encode(value, SEGMENT).to_string()
}

/// Why a deps.dev call produced no typed response.
#[derive(Debug)]
pub enum Error {
    /// deps.dev answered 404 for this path.
    NotFound { url: String },
    /// The request failed, or the cache could not serve it.
    Fetch(FetchError),
    /// A 2xx body that does not match the typed response.
    Decode {
        url: String,
        source: serde_json::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { url } => write!(f, "deps.dev has no entry at {url}"),
            Self::Fetch(error) => write!(f, "{error}"),
            Self::Decode { url, source } => {
                write!(f, "unexpected deps.dev response from {url}: {source}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<FetchError> for Error {
    fn from(error: FetchError) -> Self {
        Self::Fetch(error)
    }
}

/// The deps.dev client, over an injected transport and its read-through cache.
pub struct Client<T: Transport> {
    transport: T,
    cache: HttpCache,
    base: String,
}

impl<T: Transport> Client<T> {
    /// A client against [`BASE_URL`].
    pub fn new(transport: T, cache: HttpCache) -> Self {
        Self::with_base_url(transport, cache, BASE_URL)
    }

    /// A client against another API root, such as a mirror or a local fixture server.
    pub fn with_base_url(transport: T, cache: HttpCache, base: &str) -> Self {
        Self {
            transport,
            cache,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// Every version of a package, with its default and deprecation flags.
    pub async fn package(&self, system: &str, name: &str) -> Result<Package, Error> {
        self.fetch(&format!(
            "/systems/{}/packages/{}",
            segment(system),
            segment(name)
        ))
        .await
    }

    /// One version: licenses, advisory keys, links and SLSA provenance.
    pub async fn version(&self, key: &VersionKey) -> Result<Version, Error> {
        self.fetch(&version_path(key, "")).await
    }

    /// One version's manifest constraints.
    pub async fn requirements(&self, key: &VersionKey) -> Result<Requirements, Error> {
        self.fetch(&version_path(key, ":requirements")).await
    }

    /// One version's resolved dependency graph.
    pub async fn dependencies(&self, key: &VersionKey) -> Result<DependencyGraph, Error> {
        self.fetch(&version_path(key, ":dependencies")).await
    }

    /// A project by its repository path, such as `github.com/time-rs/time`.
    pub async fn project(&self, project_id: &str) -> Result<Project, Error> {
        self.fetch(&format!("/projects/{}", segment(project_id)))
            .await
    }

    /// An advisory by its OSV id, such as `RUSTSEC-2020-0071`.
    pub async fn advisory(&self, osv_id: &str) -> Result<Advisory, Error> {
        self.fetch(&format!("/advisories/{}", segment(osv_id)))
            .await
    }

    async fn fetch<R: DeserializeOwned>(&self, path: &str) -> Result<R, Error> {
        let url = format!("{}{path}", self.base);
        let body = match self.cache.get(&self.transport, &url).await {
            Ok(body) => body,
            Err(FetchError::Status { status: 404, .. }) => return Err(Error::NotFound { url }),
            Err(error) => return Err(Error::Fetch(error)),
        };
        serde_json::from_slice(&body).map_err(|source| Error::Decode { url, source })
    }
}

fn version_path(key: &VersionKey, suffix: &str) -> String {
    format!(
        "/systems/{}/packages/{}/versions/{}{suffix}",
        segment(&key.system),
        segment(&key.name),
        segment(&key.version)
    )
}

/// `GET /systems/{system}/packages/{name}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub package_key: PackageKey,
    #[serde(default)]
    pub versions: Vec<PackageVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageKey {
    pub system: String,
    pub name: String,
}

/// One entry of a package's version list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageVersion {
    pub version_key: VersionKey,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub is_deprecated: bool,
    #[serde(default)]
    pub deprecation_reason: Option<String>,
}

/// `GET .../versions/{version}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub version_key: VersionKey,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub is_deprecated: bool,
    /// SPDX expressions as deps.dev reports them.
    #[serde(default)]
    pub licenses: Vec<String>,
    /// OSV ids that affect this exact version.
    #[serde(default)]
    pub advisory_keys: Vec<AdvisoryKey>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub slsa_provenances: Vec<SlsaProvenance>,
    #[serde(default)]
    pub attestations: Vec<Attestation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdvisoryKey {
    pub id: String,
}

/// A labelled URL, such as `SOURCE_REPO`, `HOMEPAGE` or `DOCUMENTATION`.
#[derive(Debug, Clone, Deserialize)]
pub struct Link {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlsaProvenance {
    #[serde(default)]
    pub build_type: Option<String>,
    #[serde(default)]
    pub builder_id: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub invocation_uri: Option<String>,
    #[serde(default)]
    pub verified: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attestation {
    #[serde(default)]
    pub predicate_type: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// `GET .../versions/{version}:requirements`, keyed by ecosystem. Only the
/// Cargo shape is typed here; the other ecosystems parse to `None`.
#[derive(Debug, Clone, Deserialize)]
pub struct Requirements {
    #[serde(default)]
    pub cargo: Option<CargoRequirements>,
}

/// The Cargo manifest: `dependencies` (the `[dependencies]`, `[dev-dependencies]`
/// and `[build-dependencies]` tables, flattened) and the `[features]` table.
/// Both lists are required, so a renamed key is a decode error, not an empty list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CargoRequirements {
    pub dependencies: Vec<CargoRequirement>,
    pub features: Vec<CargoFeature>,
}

/// One manifest dependency. `requirement` is the version req, `kind` the table
/// (`normal`, `dev`, `build`, lowercase as the API sends it), `target` the
/// platform cfg (empty when there is none), and `package_alias` the rename
/// from `package =`, empty when there is none.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CargoRequirement {
    pub name: String,
    pub requirement: String,
    pub kind: String,
    pub optional: bool,
    pub uses_default_features: bool,
    pub package_alias: String,
    pub features: Vec<String>,
    pub target: String,
}

/// One `[features]` entry: the features it turns on.
#[derive(Debug, Clone, Deserialize)]
pub struct CargoFeature {
    pub name: String,
    pub implies: Vec<String>,
}

/// `GET .../versions/{version}:dependencies`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyGraph {
    #[serde(default)]
    pub nodes: Vec<Node>,
    /// Edges index into `nodes`.
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub version_key: VersionKey,
    #[serde(default)]
    pub relation: Relation,
    #[serde(default)]
    pub bundled: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// How a node relates to the queried version. `SELF` is the queried version
/// itself, `DIRECT` a declared dependency, `INDIRECT` a transitive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum Relation {
    #[serde(rename = "SELF")]
    SelfNode,
    #[serde(rename = "DIRECT")]
    Direct,
    #[serde(rename = "INDIRECT")]
    #[default]
    Indirect,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub from_node: usize,
    pub to_node: usize,
    #[serde(default)]
    pub requirement: String,
}

/// `GET /projects/{project-id}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub project_key: ProjectKey,
    #[serde(default)]
    pub stars_count: u64,
    #[serde(default)]
    pub forks_count: u64,
    #[serde(default)]
    pub open_issues_count: u64,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub scorecard: Option<Scorecard>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectKey {
    pub id: String,
}

/// The OpenSSF Scorecard for a project: the scan date, the repository and
/// Scorecard tool versions it ran against, an overall score and one entry per check.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scorecard {
    #[serde(default)]
    pub date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub repository: Option<ScorecardRepository>,
    #[serde(default)]
    pub scorecard: Option<ScorecardTool>,
    #[serde(default)]
    pub overall_score: f64,
    #[serde(default)]
    pub checks: Vec<ScorecardCheck>,
}

/// The repository a Scorecard scan read, at the commit it read.
#[derive(Debug, Clone, Deserialize)]
pub struct ScorecardRepository {
    pub name: String,
    pub commit: String,
}

/// The Scorecard tool that produced the scan.
#[derive(Debug, Clone, Deserialize)]
pub struct ScorecardTool {
    pub version: String,
    pub commit: String,
}

/// One Scorecard check; `score` is -1 when the check does not apply.
#[derive(Debug, Clone, Deserialize)]
pub struct ScorecardCheck {
    pub name: String,
    pub score: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub documentation: Option<CheckDocumentation>,
    #[serde(default)]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckDocumentation {
    pub short_description: String,
    pub url: String,
}

/// `GET /advisories/{osv-id}`. deps.dev carries no affected or fixed ranges;
/// those come from OSV.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Advisory {
    pub advisory_key: AdvisoryKey,
    #[serde(default)]
    pub url: Option<String>,
    pub title: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub cvss3_score: Option<f64>,
    #[serde(default)]
    pub cvss3_vector: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supply_chain::http_cache::test_transport::{FixtureTransport, json};
    use std::sync::Arc;
    use tempfile::TempDir;

    const PACKAGE: &str =
        include_str!("../../tests/fixtures/supply_chain/deps_dev/package_cargo_time.json");
    const VERSION: &str =
        include_str!("../../tests/fixtures/supply_chain/deps_dev/version_cargo_time_0_2_22.json");
    const REQUIREMENTS: &str = include_str!(
        "../../tests/fixtures/supply_chain/deps_dev/requirements_cargo_time_0_2_22.json"
    );
    const DEPENDENCIES: &str = include_str!(
        "../../tests/fixtures/supply_chain/deps_dev/dependencies_cargo_time_0_2_22.json"
    );
    const PROJECT: &str =
        include_str!("../../tests/fixtures/supply_chain/deps_dev/project_github_time.json");
    const ADVISORY: &str =
        include_str!("../../tests/fixtures/supply_chain/deps_dev/advisory_rustsec_2020_0071.json");

    fn time_key() -> VersionKey {
        version_key("pkg:cargo/time@0.2.22").unwrap()
    }

    /// A client over the golden fixtures, with an unthrottled cache in a fresh
    /// store. The returned transport counts the requests that reached it.
    fn client(
        routes: Vec<(String, &str)>,
    ) -> (
        Client<Arc<FixtureTransport>>,
        Arc<FixtureTransport>,
        TempDir,
    ) {
        let store = TempDir::new().unwrap();
        let transport = Arc::new(FixtureTransport::new(
            routes.into_iter().map(|(url, body)| (url, json(body))),
        ));
        let cache = HttpCache::for_store(store.path(), NAMESPACE, Duration::ZERO);
        (Client::new(transport.clone(), cache), transport, store)
    }

    fn golden_routes() -> Vec<(String, &'static str)> {
        let base = BASE_URL;
        let version = format!("{base}/systems/CARGO/packages/time/versions/0.2.22");
        vec![
            (format!("{base}/systems/CARGO/packages/time"), PACKAGE),
            (version.clone(), VERSION),
            (format!("{version}:requirements"), REQUIREMENTS),
            (format!("{version}:dependencies"), DEPENDENCIES),
            (
                format!("{base}/projects/github.com%2Ftime-rs%2Ftime"),
                PROJECT,
            ),
            (format!("{base}/advisories/RUSTSEC-2020-0071"), ADVISORY),
        ]
    }

    #[test]
    fn golang_purls_map_to_go_and_keep_the_module_path() {
        assert_eq!(
            version_key("pkg:golang/github.com/google/uuid@v1.6.0").unwrap(),
            VersionKey {
                system: "GO".into(),
                name: "github.com/google/uuid".into(),
                version: "v1.6.0".into(),
            }
        );
    }

    #[test]
    fn other_purl_types_upper_case_into_the_system() {
        let cargo = version_key("pkg:cargo/serde@1.0.200").unwrap();
        assert_eq!(
            (cargo.system.as_str(), cargo.name.as_str()),
            ("CARGO", "serde")
        );
        let pypi = version_key("pkg:pypi/requests@2.32.3").unwrap();
        assert_eq!(
            (pypi.system.as_str(), pypi.name.as_str()),
            ("PYPI", "requests")
        );
        let npm = version_key("pkg:npm/%40babel/core@7.24.0").unwrap();
        assert_eq!(
            (npm.system.as_str(), npm.name.as_str()),
            ("NPM", "@babel/core")
        );
    }

    #[test]
    fn maven_names_are_group_and_artifact() {
        assert_eq!(
            version_key("pkg:maven/org.apache.commons/commons-lang3@3.12.0").unwrap(),
            VersionKey {
                system: "MAVEN".into(),
                name: "org.apache.commons:commons-lang3".into(),
                version: "3.12.0".into(),
            }
        );
        assert!(matches!(
            version_key("pkg:maven/commons-lang3@3.12.0"),
            Err(PurlError::Malformed { .. })
        ));
    }

    #[test]
    fn qualifiers_and_subpaths_are_not_part_of_the_key() {
        let key = version_key("pkg:cargo/serde@1.0.200?repository_url=x#src").unwrap();
        assert_eq!(key.version, "1.0.200");
        assert_eq!(key.name, "serde");
    }

    #[test]
    fn versionless_purls_are_rejected_with_a_named_error() {
        let versionless = PurlError::Versionless {
            purl: "pkg:cargo/serde".into(),
        };
        assert_eq!(version_key("pkg:cargo/serde"), Err(versionless));
        assert!(matches!(
            version_key("pkg:cargo/serde@"),
            Err(PurlError::Versionless { .. })
        ));
        assert!(matches!(
            version_key("serde@1.0.200"),
            Err(PurlError::Malformed { .. })
        ));
    }

    #[tokio::test]
    async fn package_lists_every_version_with_default_and_deprecation() {
        let (client, transport, _store) = client(golden_routes());
        let package = client.package("CARGO", "time").await.unwrap();
        assert_eq!(transport.calls(), 1);
        assert_eq!(package.package_key.name, "time");
        assert_eq!(package.versions.len(), 3);
        let default = package.versions.iter().find(|v| v.is_default).unwrap();
        assert_eq!(default.version_key.version, "0.3.36");
        let deprecated = package.versions.iter().find(|v| v.is_deprecated).unwrap();
        assert_eq!(deprecated.version_key.version, "0.1.45");
        assert!(deprecated.deprecation_reason.is_some());
    }

    #[tokio::test]
    async fn version_carries_licenses_advisory_keys_and_links() {
        let (client, transport, _store) = client(golden_routes());
        let version = client.version(&time_key()).await.unwrap();
        assert_eq!(transport.calls(), 1);
        assert_eq!(version.licenses, ["MIT OR Apache-2.0"]);
        assert_eq!(version.advisory_keys[0].id, "RUSTSEC-2020-0071");
        assert!(version.links.iter().any(|l| l.label == "SOURCE_REPO"));
        assert!(version.slsa_provenances.is_empty());
    }

    #[tokio::test]
    async fn requirements_list_manifest_constraints() {
        let (client, transport, _store) = client(golden_routes());
        let requirements = client.requirements(&time_key()).await.unwrap();
        assert_eq!(transport.calls(), 1);
        let cargo = requirements.cargo.unwrap();
        assert_eq!(cargo.dependencies.len(), 4);
        let windows = cargo
            .dependencies
            .iter()
            .find(|r| r.name == "winapi")
            .unwrap();
        assert_eq!(windows.kind, "normal");
        assert!(windows.optional);
        assert_eq!(windows.target, "cfg(windows)");
        assert_eq!(windows.features, ["minwinbase", "minwindef", "timezoneapi"]);
        let dev = cargo
            .dependencies
            .iter()
            .find(|r| r.name == "rand")
            .unwrap();
        assert_eq!(dev.kind, "dev");
        assert!(!dev.uses_default_features);
        assert_eq!(cargo.features.len(), 3);
        assert_eq!(cargo.features[2].implies, ["libc", "winapi"]);
    }

    #[tokio::test]
    async fn dependencies_resolve_self_direct_and_indirect_nodes() {
        let (client, transport, _store) = client(golden_routes());
        let graph = client.dependencies(&time_key()).await.unwrap();
        assert_eq!(transport.calls(), 1);
        assert_eq!(graph.nodes[0].relation, Relation::SelfNode);
        assert_eq!(graph.nodes[1].relation, Relation::Direct);
        assert_eq!(graph.nodes[3].relation, Relation::Indirect);
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(graph.edges[0].from_node, 0);
        assert_eq!(graph.edges[0].to_node, 1);
        assert_eq!(graph.edges[0].requirement, "^0.4.2");
        assert_eq!(graph.edges[1].requirement, "^0.2");
        assert!(graph.error.is_empty());
    }

    #[tokio::test]
    async fn project_carries_stars_license_and_scorecard() {
        let (client, transport, _store) = client(golden_routes());
        let project = client.project("github.com/time-rs/time").await.unwrap();
        assert_eq!(transport.calls(), 1);
        assert_eq!(project.stars_count, 1347);
        assert_eq!(project.license.as_deref(), Some("Apache-2.0"));
        let scorecard = project.scorecard.unwrap();
        assert!((scorecard.overall_score - 4.6).abs() < f64::EPSILON);
        let repository = scorecard.repository.unwrap();
        assert_eq!(repository.name, "github.com/time-rs/time");
        let tool = scorecard.scorecard.unwrap();
        assert_eq!(tool.version, "v5.5.1-0.20260815060127-d1fab88f5463");
        let maintained = scorecard
            .checks
            .iter()
            .find(|c| c.name == "Maintained")
            .unwrap();
        assert_eq!(maintained.score, 10);
        assert!(
            maintained
                .documentation
                .as_ref()
                .unwrap()
                .url
                .ends_with("#maintained")
        );
    }

    #[tokio::test]
    async fn advisory_carries_aliases_and_cvss() {
        let (client, transport, _store) = client(golden_routes());
        let advisory = client.advisory("RUSTSEC-2020-0071").await.unwrap();
        assert_eq!(transport.calls(), 1);
        assert_eq!(advisory.advisory_key.id, "RUSTSEC-2020-0071");
        assert_eq!(advisory.title, "Potential segfault in the time crate");
        assert!(advisory.aliases.contains(&"CVE-2020-26235".to_owned()));
        assert!(advisory.cvss3_score.is_some());
    }

    #[tokio::test]
    async fn repeated_calls_are_served_from_cache() {
        let (client, transport, _store) = client(golden_routes());
        client.package("CARGO", "time").await.unwrap();
        client.package("CARGO", "time").await.unwrap();
        assert_eq!(transport.calls(), 1);
        client.version(&time_key()).await.unwrap();
        client.version(&time_key()).await.unwrap();
        assert_eq!(transport.calls(), 2);
    }

    #[tokio::test]
    async fn unknown_package_is_not_found() {
        let (client, transport, _store) = client(golden_routes());
        let error = client.package("CARGO", "no-such-crate").await.unwrap_err();
        assert!(matches!(error, Error::NotFound { .. }), "{error}");
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn names_are_percent_encoded_as_one_path_segment() {
        let url = format!("{BASE_URL}/systems/GO/packages/github.com%2Fgoogle%2Fuuid");
        let (client, transport, _store) = client(vec![(url, PACKAGE)]);
        client
            .package("GO", "github.com/google/uuid")
            .await
            .unwrap();
        assert_eq!(transport.calls(), 1);
    }
}
