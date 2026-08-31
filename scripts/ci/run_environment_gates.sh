#!/usr/bin/env bash
# 跑 workspace 全量测试，并把 env-gated 用例的真实执行/跳过数统一记账。
set -euo pipefail

require_pg=false
if [[ "${1:-}" == "--require-pg" ]]; then
  require_pg=true
elif [[ $# -gt 0 ]]; then
  echo "usage: $0 [--require-pg]" >&2
  exit 2
fi

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
log_file=$(mktemp "${TMPDIR:-/tmp}/fastsearch-environment-gates.XXXXXX")
trap 'rm -f "$log_file"' EXIT

cd "$repo_root"
set +e
cargo test --workspace -- --nocapture 2>&1 | tee "$log_file"
test_status=${PIPESTATUS[0]}
set -e

pg_gates=(
  ensure_schema_concurrent_no_race
  ensure_schema_rejects_existing_vector_dimension_mismatch
  integration_roundtrip
  integration_chunk_management_lifecycle
  integration_schema_upgrade_adds_metadata_and_searchable
  integration_media_bytes_roundtrip
  integration_time_filter_null_column_superset
  b6_set_embedding_idempotent_guard
  integration_pgvector_search
  fs201_signal_crud_and_invalidation
  fs201_signal_reconciliation_and_acl_deletes
  fs201_signal_schema_publication_and_cdc_contract
  fs202_signal_vector_search_filters_and_orders
  pgvector_backend_via_engine
  fs202_engine_fuses_three_real_recall_paths
  b6_cdc_write_through_to_pg_embedding
  mm6_inline_serves_bytes_from_source_pg
  cdc_closed_loop_pg_to_search
  cdc_chunk_replay_converges_signal_invalidation
  cdc_consume_persist_crashsafe
  cdc_concurrent_slot_creation_is_idempotent
  cdc_peek_exposes_commit_lsn_lag_and_dead_letters
  cdc_failed_write_through_marks_recovery_until_replay_completes
  cdc_crash_at_peek_persist_and_advance_recovers_without_loss_or_duplicates
  cdc_initial_snapshot_bootstrap
  cdc_batch_embedding_does_not_hold_engine_lock
  cdc_crash_after_apply_before_persist_retries_without_half_state
  cdc_pk_update_and_truncate_converge
  cdc_pg_write_failure_rolls_back_and_retry_converges
  cdc_unchanged_toast_update_does_not_stall
  searchable_false_is_stored_in_pg_but_not_searchable
  index_writes_embedding_through_to_pg_in_pgvector_mode
  chunk_management_routes_enforce_acl_tenant_and_idempotency
  asset_inline_bytes_e2e
)
model_gates=(
  live_embed_gated
  cdc_embed_hybrid_full_loop
  semantic_hybrid_via_server_gated
)

count_gates() {
  local executed=0 skipped=0 missing=0 name
  for name in "$@"; do
    if grep -Fq "skip $name" "$log_file"; then
      skipped=$((skipped + 1))
    elif grep -Eq "test ([[:alnum:]_]+::)*${name} \.\.\. ok" "$log_file"; then
      executed=$((executed + 1))
    else
      missing=$((missing + 1))
      echo "environment-gates: missing result for $name" >&2
    fi
  done
  printf '%s %s %s\n' "$executed" "$skipped" "$missing"
}

read -r pg_executed pg_skipped pg_missing < <(count_gates "${pg_gates[@]}")
read -r model_executed model_skipped model_missing < <(count_gates "${model_gates[@]}")
echo "environment-gates: pg executed=$pg_executed skipped=$pg_skipped missing=$pg_missing; model executed=$model_executed skipped=$model_skipped missing=$model_missing"

if [[ $test_status -ne 0 ]]; then
  echo "environment-gates: cargo test failed with status $test_status" >&2
  exit "$test_status"
fi
if [[ $pg_missing -ne 0 || $model_missing -ne 0 ]]; then
  exit 1
fi
if $require_pg && [[ $pg_executed -ne ${#pg_gates[@]} || $pg_skipped -ne 0 ]]; then
  echo "environment-gates: --require-pg expected ${#pg_gates[@]} executed and 0 skipped" >&2
  exit 1
fi
