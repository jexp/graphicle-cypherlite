// GUAC (Graph for Understanding Artifact Composition) as a Cypher 25
// graph type, derived 1:1 from guac.graphspec.yaml (Ontology Graph Spec
// 1.0.0, pinned by CypherLite's cypherlite-ontology crate).
//
// Style follows cypherlite-migration.md §2.6 and its spike-verified
// quirks: ids/keys are STRING (never UUID), no DEFAULT clauses, and the
// type is promoted with ALTER CURRENT GRAPH TYPE SET GRAPH TYPE so it
// actually enforces. This script is byte-for-byte equivalent to what
// `CALL db.ontology.apply()` compiles the graphspec document into
// (verified on CypherLite v0.13.0: same 29 node types, 56 edge types,
// 12 key constraints).
//
// GUAC modeling notes:
//   - The package/source/vulnerability tries become one node type per
//     trie level; trie path components are denormalized onto child
//     levels so every node carries a node-local key.
//   - Evidence (attestations) are NODES carrying their own provenance
//     (origin, collector, documentRef); subject links run software ->
//     evidence, object links run evidence -> software.
//   - GUAC enum fields (dependencyType, scoreType, VEX status/justification)
//     are STRING here; the graph type cannot express one_of.
//   - No relationship carries properties: all attestation payload lives
//     on the evidence nodes, so every edge type below is bare.
//
// Load into CypherLite (or Neo4j 2026.06+):
//   cypherlite query --database graph.cl "$(cat guac.graphtype.cypher)"
// or store the source ontology instead and let the engine compile it:
//   CALL db.ontology.set($yaml); CALL db.ontology.apply();

CREATE GRAPH TYPE guac {

  // ---- software tree: package trie (purl) ------------------------------

  (:PackageType => {type :: STRING NOT NULL IS KEY}),
  (:PackageNamespace => {type :: STRING NOT NULL, namespace :: STRING NOT NULL}),
  CONSTRAINT package_namespace_key FOR (n:PackageNamespace) REQUIRE (n.type, n.namespace) IS KEY,
  (:PackageName => {type :: STRING NOT NULL, namespace :: STRING NOT NULL, name :: STRING NOT NULL}),
  CONSTRAINT package_name_key FOR (n:PackageName) REQUIRE (n.type, n.namespace, n.name) IS KEY,
  (:PackageVersion => {purl :: STRING NOT NULL IS KEY, version :: STRING NOT NULL,
                       qualifiers :: LIST<STRING NOT NULL>, subpath :: STRING NOT NULL}),

  // ---- software tree: source trie ---------------------------------------

  (:SourceType => {type :: STRING NOT NULL IS KEY}),
  (:SourceNamespace => {type :: STRING NOT NULL, namespace :: STRING NOT NULL}),
  CONSTRAINT source_namespace_key FOR (n:SourceNamespace) REQUIRE (n.type, n.namespace) IS KEY,
  (:SourceName => {type :: STRING NOT NULL, namespace :: STRING NOT NULL, name :: STRING NOT NULL,
                   tag :: STRING, commit :: STRING}),
  CONSTRAINT source_name_key FOR (n:SourceName) REQUIRE (n.type, n.namespace, n.name) IS KEY,

  // ---- software tree: leaf identities ------------------------------------

  (:Artifact => {algorithm :: STRING NOT NULL, digest :: STRING NOT NULL}),
  CONSTRAINT artifact_key FOR (n:Artifact) REQUIRE (n.algorithm, n.digest) IS KEY,
  (:Builder => {uri :: STRING NOT NULL IS KEY}),

  // ---- software tree: vulnerability trie ----------------------------------
  // NoVuln is VulnerabilityID {type: 'novuln', vulnerabilityId: ''}.

  (:VulnerabilityType => {type :: STRING NOT NULL IS KEY}),
  (:VulnerabilityID => {type :: STRING NOT NULL, vulnerabilityId :: STRING NOT NULL}),
  CONSTRAINT vulnerability_id_key FOR (n:VulnerabilityID) REQUIRE (n.type, n.vulnerabilityId) IS KEY,

  (:License => {name :: STRING NOT NULL, inline :: STRING, listVersion :: STRING NOT NULL}),
  CONSTRAINT license_key FOR (n:License) REQUIRE (n.name, n.listVersion) IS KEY,

  // ---- evidence tree ------------------------------------------------------
  // Every evidence node carries GUAC's provenance triple.

  (:IsDependency => {dependencyType :: STRING NOT NULL, justification :: STRING NOT NULL,
                     origin :: STRING NOT NULL, collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:IsOccurrence => {justification :: STRING NOT NULL, origin :: STRING NOT NULL,
                     collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:HasSourceAt => {knownSince :: ZONED DATETIME NOT NULL, justification :: STRING NOT NULL,
                    origin :: STRING NOT NULL, collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:HashEqual => {justification :: STRING NOT NULL, origin :: STRING NOT NULL,
                  collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:PkgEqual => {justification :: STRING NOT NULL, origin :: STRING NOT NULL,
                 collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:VulnEqual => {justification :: STRING NOT NULL, origin :: STRING NOT NULL,
                  collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:HasSBOM => {uri :: STRING NOT NULL, algorithm :: STRING NOT NULL, digest :: STRING NOT NULL,
                downloadLocation :: STRING NOT NULL, knownSince :: ZONED DATETIME NOT NULL,
                origin :: STRING NOT NULL, collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:HasSLSA => {buildType :: STRING NOT NULL, slsaVersion :: STRING NOT NULL,
                startedOn :: ZONED DATETIME, finishedOn :: ZONED DATETIME,
                slsaPredicates :: LIST<STRING NOT NULL>,
                origin :: STRING NOT NULL, collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:HasMetadata => {key :: STRING NOT NULL, value :: STRING NOT NULL, timestamp :: ZONED DATETIME NOT NULL,
                    justification :: STRING NOT NULL, origin :: STRING NOT NULL,
                    collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:PointOfContact => {email :: STRING NOT NULL, info :: STRING NOT NULL, since :: ZONED DATETIME NOT NULL,
                       justification :: STRING NOT NULL, origin :: STRING NOT NULL,
                       collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:CertifyVuln => {timeScanned :: ZONED DATETIME NOT NULL, dbUri :: STRING NOT NULL,
                    dbVersion :: STRING NOT NULL, scannerUri :: STRING NOT NULL,
                    scannerVersion :: STRING NOT NULL, origin :: STRING NOT NULL,
                    collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:VulnerabilityMetadata => {scoreType :: STRING NOT NULL, scoreValue :: FLOAT NOT NULL,
                              timestamp :: ZONED DATETIME NOT NULL, origin :: STRING NOT NULL,
                              collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:CertifyVEXStatement => {status :: STRING NOT NULL, vexJustification :: STRING NOT NULL,
                            statement :: STRING NOT NULL, statusNotes :: STRING NOT NULL,
                            knownSince :: ZONED DATETIME NOT NULL, origin :: STRING NOT NULL,
                            collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:CertifyGood => {justification :: STRING NOT NULL, knownSince :: ZONED DATETIME NOT NULL,
                    origin :: STRING NOT NULL, collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:CertifyBad => {justification :: STRING NOT NULL, knownSince :: ZONED DATETIME NOT NULL,
                   origin :: STRING NOT NULL, collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:CertifyLegal => {declaredLicense :: STRING NOT NULL, discoveredLicense :: STRING NOT NULL,
                     attribution :: STRING NOT NULL, justification :: STRING NOT NULL,
                     timeScanned :: ZONED DATETIME NOT NULL, origin :: STRING NOT NULL,
                     collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),
  (:CertifyScorecard => {aggregateScore :: FLOAT NOT NULL, checks :: LIST<STRING NOT NULL>,
                         timeScanned :: ZONED DATETIME NOT NULL, scorecardVersion :: STRING NOT NULL,
                         scorecardCommit :: STRING NOT NULL, origin :: STRING NOT NULL,
                         collector :: STRING NOT NULL, documentRef :: STRING NOT NULL}),

  // ---- trie structure (parent -> child) -----------------------------------

  (:PackageType)-[:PKG_HAS_NAMESPACE =>]->(:PackageNamespace),
  (:PackageNamespace)-[:PKG_HAS_NAME =>]->(:PackageName),
  (:PackageName)-[:PKG_HAS_VERSION =>]->(:PackageVersion),
  (:SourceType)-[:SRC_HAS_NAMESPACE =>]->(:SourceNamespace),
  (:SourceNamespace)-[:SRC_HAS_NAME =>]->(:SourceName),
  (:VulnerabilityType)-[:VULN_HAS_VULNERABILITY_ID =>]->(:VulnerabilityID),

  // ---- evidence subject links (software -> evidence) ------------------------
  // Where GUAC's PkgMatchFlag allows the name level (ALL_VERSIONS) and the
  // version level (SPECIFIC_VERSION), the link gets two distinct types,
  // mirroring GUAC's separate ArangoDB edge collections.

  (:PackageVersion)-[:PACKAGE_IS_DEPENDENCY =>]->(:IsDependency),
  (:PackageVersion)-[:PACKAGE_IS_OCCURRENCE =>]->(:IsOccurrence),
  (:SourceName)-[:SOURCE_IS_OCCURRENCE =>]->(:IsOccurrence),
  (:PackageName)-[:PACKAGE_NAME_HAS_SOURCE_AT =>]->(:HasSourceAt),
  (:PackageVersion)-[:PACKAGE_VERSION_HAS_SOURCE_AT =>]->(:HasSourceAt),
  (:PackageVersion)-[:PACKAGE_HAS_SBOM =>]->(:HasSBOM),
  (:Artifact)-[:ARTIFACT_HAS_SBOM =>]->(:HasSBOM),
  (:Artifact)-[:ARTIFACT_HAS_SLSA =>]->(:HasSLSA),
  (:PackageVersion)-[:PACKAGE_CERTIFY_VULN =>]->(:CertifyVuln),
  (:PackageVersion)-[:PACKAGE_CERTIFY_VEX_STATEMENT =>]->(:CertifyVEXStatement),
  (:Artifact)-[:ARTIFACT_CERTIFY_VEX_STATEMENT =>]->(:CertifyVEXStatement),
  (:PackageVersion)-[:PACKAGE_CERTIFY_LEGAL =>]->(:CertifyLegal),
  (:SourceName)-[:SOURCE_CERTIFY_LEGAL =>]->(:CertifyLegal),
  (:SourceName)-[:SOURCE_CERTIFY_SCORECARD =>]->(:CertifyScorecard),
  (:PackageName)-[:PACKAGE_NAME_CERTIFY_GOOD =>]->(:CertifyGood),
  (:PackageVersion)-[:PACKAGE_VERSION_CERTIFY_GOOD =>]->(:CertifyGood),
  (:SourceName)-[:SOURCE_CERTIFY_GOOD =>]->(:CertifyGood),
  (:Artifact)-[:ARTIFACT_CERTIFY_GOOD =>]->(:CertifyGood),
  (:PackageName)-[:PACKAGE_NAME_CERTIFY_BAD =>]->(:CertifyBad),
  (:PackageVersion)-[:PACKAGE_VERSION_CERTIFY_BAD =>]->(:CertifyBad),
  (:SourceName)-[:SOURCE_CERTIFY_BAD =>]->(:CertifyBad),
  (:Artifact)-[:ARTIFACT_CERTIFY_BAD =>]->(:CertifyBad),
  (:PackageName)-[:PACKAGE_NAME_HAS_METADATA =>]->(:HasMetadata),
  (:PackageVersion)-[:PACKAGE_VERSION_HAS_METADATA =>]->(:HasMetadata),
  (:SourceName)-[:SOURCE_HAS_METADATA =>]->(:HasMetadata),
  (:Artifact)-[:ARTIFACT_HAS_METADATA =>]->(:HasMetadata),
  (:PackageName)-[:PACKAGE_NAME_POINT_OF_CONTACT =>]->(:PointOfContact),
  (:PackageVersion)-[:PACKAGE_VERSION_POINT_OF_CONTACT =>]->(:PointOfContact),
  (:SourceName)-[:SOURCE_POINT_OF_CONTACT =>]->(:PointOfContact),
  (:Artifact)-[:ARTIFACT_POINT_OF_CONTACT =>]->(:PointOfContact),
  (:PackageVersion)-[:PKG_EQUAL =>]->(:PkgEqual),
  (:VulnerabilityID)-[:VULN_EQUAL =>]->(:VulnEqual),
  (:Artifact)-[:HASH_EQUAL =>]->(:HashEqual),
  (:VulnerabilityID)-[:VULNERABILITY_VULN_METADATA =>]->(:VulnerabilityMetadata),

  // ---- evidence object links (evidence -> software) --------------------------

  (:IsDependency)-[:IS_DEPENDENCY_PACKAGE =>]->(:PackageVersion),
  (:IsOccurrence)-[:IS_OCCURRENCE_ARTIFACT =>]->(:Artifact),
  (:HasSourceAt)-[:HAS_SOURCE_AT_SOURCE =>]->(:SourceName),
  (:HasSBOM)-[:HAS_SBOM_INCLUDED_SOFTWARE_PACKAGE =>]->(:PackageVersion),
  (:HasSBOM)-[:HAS_SBOM_INCLUDED_SOFTWARE_ARTIFACT =>]->(:Artifact),
  (:HasSBOM)-[:HAS_SBOM_INCLUDED_DEPENDENCIES =>]->(:IsDependency),
  (:HasSBOM)-[:HAS_SBOM_INCLUDED_OCCURRENCES =>]->(:IsOccurrence),
  (:HasSLSA)-[:HAS_SLSA_BUILT_BY =>]->(:Builder),
  (:HasSLSA)-[:HAS_SLSA_MATERIALS =>]->(:Artifact),
  (:CertifyVuln)-[:CERTIFY_VULN_VULNERABILITY =>]->(:VulnerabilityID),
  (:CertifyVEXStatement)-[:CERTIFY_VEX_STATEMENT_VULNERABILITY =>]->(:VulnerabilityID),
  (:CertifyLegal)-[:CERTIFY_LEGAL_DECLARED_LICENSE =>]->(:License),
  (:CertifyLegal)-[:CERTIFY_LEGAL_DISCOVERED_LICENSE =>]->(:License),
  (:PkgEqual)-[:PKG_EQUAL_PACKAGE =>]->(:PackageVersion),
  (:VulnEqual)-[:VULN_EQUAL_VULNERABILITY =>]->(:VulnerabilityID),
  (:HashEqual)-[:HASH_EQUAL_ARTIFACT =>]->(:Artifact)
};

// Promote: without this the type exists but enforces nothing
// (cypherlite-migration.md §2.6).
ALTER CURRENT GRAPH TYPE SET GRAPH TYPE guac;
