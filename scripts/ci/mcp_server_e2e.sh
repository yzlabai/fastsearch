#!/usr/bin/env bash
# 真二进制：server + 两个远端 MCP 进程，覆盖探测/search/resolve/index/ACL 隔离。
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

port=${FASTSEARCH_E2E_PORT:-18643}
base="http://127.0.0.1:${port}"
data_dir=$(mktemp -d "${TMPDIR:-/tmp}/fastsearch-mcp-e2e-data.XXXXXX")
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/fastsearch-mcp-e2e-work.XXXXXX")
server_log="$work_dir/server.log"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$data_dir" "$work_dir"
}
trap cleanup EXIT

env FASTSEARCH_DATA="$data_dir" FASTSEARCH_PORT="$port" \
  FASTSEARCH_KEYS='a=acme:team-a;b=acme:team-b' \
  ./target/debug/fastsearch-server >"$server_log" 2>&1 &
server_pid=$!
for _ in $(seq 1 60); do
  if curl -fsS "$base/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done
curl -fsS "$base/healthz" >/dev/null || {
  echo "MCP E2E: server did not become healthy" >&2
  cat "$server_log" >&2
  exit 1
}
curl -fsS "$base/readyz" \
  | jq -e '.ready == true and .scope == "process" and .dependencies_checked == false' \
  >/dev/null

run_mcp() {
  local key=$1 input=$2 output=$3
  printf '%s\n' "$input" | env FASTSEARCH_SERVER="$base" FASTSEARCH_KEY="$key" \
    ./target/debug/fastsearch-mcp >"$output" 2>"$output.stderr"
}

chunk_a='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"index_chunks","arguments":{"collection":"kb","doc_id":"a.txt","chunks":[{"chunk_id":0,"kind":"image","text":"sharedtoken alpha-private","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":25,"media":{"asset":{"kind":"doc_region","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1}},"media_type":"image/png"}}]}}}'
chunk_b='{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"index_chunks","arguments":{"collection":"kb","doc_id":"b.txt","chunks":[{"chunk_id":0,"kind":"paragraph","text":"sharedtoken beta-private","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":24}]}}}'
run_mcp a "$chunk_a" "$work_dir/index-a.jsonl"
run_mcp b "$chunk_b" "$work_dir/index-b.jsonl"
jq -e 'select(.id == 1) | .result.isError == false and (.result.content[0].text | fromjson | .indexed == 1)' "$work_dir/index-a.jsonl" >/dev/null
jq -e 'select(.id == 2) | .result.isError == false and (.result.content[0].text | fromjson | .indexed == 1)' "$work_dir/index-b.jsonl" >/dev/null

requests_a=$(printf '%s\n%s\n%s\n%s\n%s' \
  '{"jsonrpc":"2.0","id":10,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":11,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"search","arguments":{"query":"sharedtoken","mode":"keyword","top_k":10,"filter":{"eq":["collection","kb"]}}}}' \
  '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"resolve_citation","arguments":{"citation_id":"kb:b.txt:0"}}}' \
  '{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"resolve_citation","arguments":{"citation_id":"kb:a.txt:0"}}}')
requests_b=$(printf '%s\n%s' \
  '{"jsonrpc":"2.0","id":20,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"search","arguments":{"query":"sharedtoken","mode":"keyword","top_k":10,"filter":{"eq":["collection","kb"]}}}}')
run_mcp a "$requests_a" "$work_dir/a.jsonl"
run_mcp b "$requests_b" "$work_dir/b.jsonl"

jq -e 'select(.id == 10) | .result.serverInfo.name == "fastsearch-mcp"' "$work_dir/a.jsonl" >/dev/null
jq -e 'select(.id == 11) | [.result.tools[] | select(.name == "search") | .inputSchema.properties.mode.enum] == [["keyword"]]' "$work_dir/a.jsonl" >/dev/null
jq -e 'select(.id == 20) | [.result.tools[].name] | index("index_chunks") != null' "$work_dir/b.jsonl" >/dev/null

a_ids=$(jq -r 'select(.id == 12) | .result.content[0].text | fromjson | .hits[].citation_id' "$work_dir/a.jsonl")
b_ids=$(jq -r 'select(.id == 21) | .result.content[0].text | fromjson | .hits[].citation_id' "$work_dir/b.jsonl")
[[ "$a_ids" == "kb:a.txt:0" ]] || { echo "MCP E2E: key a saw unexpected ids: $a_ids" >&2; exit 1; }
[[ "$b_ids" == "kb:b.txt:0" ]] || { echo "MCP E2E: key b saw unexpected ids: $b_ids" >&2; exit 1; }
jq -e 'select(.id == 13) | .result.content[0].text | fromjson | .found == false' "$work_dir/a.jsonl" >/dev/null
jq -e 'select(.id == 14) | .result.isError == false and (.result.content[0].text | fromjson | .found == true and .media_type == "image/png" and .fetch.kind == "doc_render" and .fetch.doc_id == "a.txt" and .fetch.page == 1 and .fetch.bbox == {"x0":0,"y0":0,"x1":1,"y1":1})' "$work_dir/a.jsonl" >/dev/null

rest_ids=$(curl -fsS -X POST "$base/v1/search" \
  -H 'authorization: Bearer a' -H 'content-type: application/json' \
  --data-binary '{"query":"sharedtoken","mode":"keyword","top_k":10,"filter":{"eq":["collection","kb"]}}' \
  | jq -r '.hits[].citation_id')
[[ "$rest_ids" == "$a_ids" ]] || { echo "MCP E2E: MCP/REST result mismatch" >&2; exit 1; }

echo "mcp-server-e2e: PASS (probe=1 index=2 search=2 resolve=2 acl_keys=2 rest_parity=1)"
