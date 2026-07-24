# Generic Chunk Metadata and Management API

> Status: complete. All five phases are implemented and verified. This plan adds generic engine capabilities only; it contains no caller-specific schema or behavior.

## Goal

Extend FastSearch so callers can round-trip opaque chunk metadata, retain non-searchable context chunks, retrieve and mutate chunks by existing `GlobalId`, list a document's chunks, and delete a collection.

The engine continues to accept pre-chunked input. Parsing and domain hierarchy remain caller responsibilities.

## Users and Scenarios

- Search platform callers that already own chunking and need to recover application context from search hits.
- Administration tools that need idempotent chunk, document, and collection lifecycle operations.
- RAG applications that store context-only chunks without making them searchable.

Frequent paths are document replacement and search. Chunk-level mutation and collection deletion are lower-frequency management paths.

## Scope

### In Scope

- Opaque JSON object `metadata` on `Chunk`.
- Boolean `searchable`, defaulting to `true`.
- Opt-in full `text` and `metadata` on search hits.
- Batch get/upsert/delete using `(collection, doc_id, chunk_id)`.
- Paginated document chunk listing.
- Idempotent collection deletion across PG truth, derived indexes, registry, and managed media.
- PG DDL/row mapping, CDC, text/vector behavior, REST/OpenAPI, TypeScript/Python SDKs, tests, and live PostgreSQL verification.

### Out of Scope

- Caller-specific string IDs.
- Interpreting or indexing metadata fields.
- Parsing full documents or splitting text in the server.
- Parent/child expansion, QA semantics, or caller-specific collection naming.
- New PostgreSQL extensions or shared-preload requirements.
- Changing embedding, rerank, fusion, or auto-merge defaults.

## Key Decisions

1. **Identity remains `GlobalId`.** No second external identity is added. Callers may place their original ID in metadata.
2. **Metadata is opaque.** It is a JSON object with generic byte/depth/key limits, never a source of ACL or system fields.
3. **PG remains truth.** Metadata and searchable state are persisted in PG and decoded by CDC. Derived indexes can be rebuilt.
4. **Non-searchable rows remain manageable.** They stay in PG but are absent from text/vector indexes.
5. **Management APIs enforce server identity.** New read/list/delete routes apply the same tenant and ACL non-disclosure rules as existing document deletion.
6. **Search payload expansion is opt-in.** Existing clients do not receive larger hits unless requested.

## API Semantics

- Batch get returns one result per requested GlobalId in request order, with an explicit missing representation.
- Batch upsert is transactional in PG. Repeating the same request leaves the same final rows.
- Batch delete is idempotent and reports deleted/missing without exposing unauthorized existence.
- Document list is stable by `chunk_id` and uses an opaque or numeric continuation cursor.
- Collection deletion is idempotent. Structured data deletion and managed-object cleanup are reported separately.
- Existing document replacement/deletion routes remain supported.

Exact request and response types live in code and OpenAPI; this plan defines semantics rather than duplicating implementation structs.

## Implementation Phases

1. [done] Core model, limits, PG DDL/row mapping and CDC decoding.
2. [done] Text/vector indexing behavior and opt-in Search Hit content.
3. [done] Batch chunk and document-list engine/server APIs with ACL tests.
4. [done] Collection deletion including managed media cleanup.
5. [done] OpenAPI, TypeScript/Python SDKs, documentation and real-PostgreSQL route verification.

## Test Cases

1. Metadata round-trips nested Unicode JSON without affecting filter, score, ACL, or citation fields.
2. Oversized, over-deep, or over-keyed metadata is rejected before persistence.
3. `searchable=false` persists and can be read/listed but never appears in keyword, vector, hybrid, similar, facet, or rerank candidates.
4. CDC update toggling searchable removes/adds the row in all derived indexes.
5. Batch get preserves order and handles missing/unauthorized IDs without leakage.
6. Repeated batch upsert/delete and collection delete are idempotent.
7. Document list pagination is deterministic and complete.
8. Existing clients and old JSON without new fields retain current behavior.
9. PG integration uses only pgvector and logical replication.

## Verification

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
DATABASE_URL=postgres://... cargo test -p fastsearch-pg
```

Verification completed against a real pgvector PostgreSQL 18 container through the Axum REST router:
index/search, batch get/upsert/delete, document pagination, searchable toggling, ACL non-disclosure,
cross-tenant conflict, and idempotent collection deletion. Full workspace fmt/clippy/tests and both SDK suites pass.
