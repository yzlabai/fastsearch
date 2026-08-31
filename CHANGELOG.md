# Changelog

All notable changes to fastsearch are documented here. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); the repository is
currently preparing a release candidate rather than a general-availability
release.

## [Unreleased]

- Multi-representation chunk signals, asynchronous ingestion jobs, and
  deployment-gated release validation remain planned in the active iteration.

## [0.2.0-rc.1] - 2026-08-31

### Breaking and operational changes

- The CLI is now a pure REST client; embedded local indexing/search commands
  were replaced by server-backed `index`, `index-dir`, `ingest`, `search`, and
  `eval` flows.
- Server writes require an identity configured through `FASTSEARCH_KEYS`.
  Missing or unauthorized identities are rejected instead of receiving an
  implicit public write path.
- Search response bodies omit chunk text by default. Callers that need raw text
  must request `include_text`; Agent-facing helpers prefer highlighted context.
- MCP local mode requires an explicit ACL/tenant identity. Remote mode inherits
  the server API-key boundary.

### Added

- Postgres source-of-truth storage, logical-replication CDC with resumable LSN
  checkpoints, derived-index persistence, and rebuild paths.
- Deterministic brute-force, HNSW, binary rotated, and TurboQuant vector indexes,
  plus filter-aware pgvector direct search.
- Stable hybrid-search contracts, named-source fusion contributions, optional
  explain output, citations, assets, facets, pagination, and reranking.
- REST server, MCP server, Rust CLI, TypeScript and Python SDKs, multi-format
  docparse ingestion, and a runnable knowledge-base Agent example.
- CI gates for formatting, clippy, unit/integration/e2e tests, parsing feature
  tiers, Docker builds, environment coverage, and dependency licenses.

### Compatibility

- Existing two-source fusion request variants remain accepted; explain and new
  source details are additive and opt-in.
- TypeScript SDK 0.3.0 and Python SDK 0.2.0 describe their independent package
  publication state in their READMEs; repository examples use the local current
  TypeScript SDK rather than assuming an unpublished registry version.

## [0.1.0] - 2026-06-25

- Initial workspace skeleton and in-memory hybrid-search baseline.
