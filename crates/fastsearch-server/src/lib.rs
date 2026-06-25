//! # fastsearch-server
//!
//! REST 服务：API-Key 认证 + **逐文档 ACL 不可绕过**（服务端注入）+ 基础可观测。
//! 详见 [spec](../../docs/specs/19-server.md)。
//!
//! 安全核心（需求 F44）：ACL 只来自认证身份，客户端无法在请求体里传 ACL 或越权。

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use fastsearch_core::{AclFilter, Chunk, SearchRequest};
use fastsearch_engine::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// 调用者身份（由 API Key 解析）。
#[derive(Debug, Clone)]
pub struct Principal {
    pub tenant: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    searches: AtomicU64,
    indexed: AtomicU64,
}

/// 服务状态（可 Clone：内部 Arc 共享）。
#[derive(Clone)]
pub struct ServerState {
    engine: Arc<Mutex<Engine>>,
    keys: Arc<HashMap<String, Principal>>,
    metrics: Arc<Metrics>,
}

impl ServerState {
    pub fn new(engine: Engine, keys: HashMap<String, Principal>) -> Self {
        ServerState {
            engine: Arc::new(Mutex::new(engine)),
            keys: Arc::new(keys),
            metrics: Arc::new(Metrics::default()),
        }
    }
}

/// 从请求头解析 Principal：`Authorization: Bearer <k>` 或 `X-API-Key: <k>`。纯函数。
pub fn principal_from_headers(
    headers: &HeaderMap,
    keys: &HashMap<String, Principal>,
) -> Option<Principal> {
    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })?;
    keys.get(key.trim()).cloned()
}

/// Principal → 强制 ACL 过滤（服务端注入，客户端不可绕过）。纯函数。
pub fn acl_for(p: &Principal) -> AclFilter {
    AclFilter {
        tenant: p.tenant.clone(),
        allowed_tags: p.tags.clone(),
    }
}

/// 构建 router。
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ready" }))
        .route("/metrics", get(metrics))
        .route("/v1/search", post(search))
        .route("/v1/index", post(index))
        .with_state(state)
}

async fn metrics(State(s): State<ServerState>) -> String {
    let m = &s.metrics;
    format!(
        "fastsearch_requests_total {}\nfastsearch_searches_total {}\nfastsearch_indexed_total {}\n",
        m.requests.load(Ordering::Relaxed),
        m.searches.load(Ordering::Relaxed),
        m.indexed.load(Ordering::Relaxed),
    )
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

async fn search(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<SearchRequest>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let principal = principal_from_headers(&headers, &s.keys).ok_or((
        StatusCode::UNAUTHORIZED,
        "missing or invalid API key".into(),
    ))?;
    let acl = acl_for(&principal);

    let engine = s.engine.lock().await;
    let hits = engine
        .search(&req, Some(&acl)) // ACL 强制注入，客户端不可绕过
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    s.metrics.searches.fetch_add(1, Ordering::Relaxed);

    let arr: Vec<Value> = hits
        .iter()
        .map(|h| {
            json!({
                "citation_id": h.citation.citation_id(),
                "score": h.score,
                "bm25": h.bm25,
                "vector": h.vector,
                "doc_id": h.id.doc_id,
                "chunk_id": h.id.chunk_id,
                "page": h.citation.page,
                "bbox": h.citation.bbox,
                "heading_path": h.citation.heading_path,
                "section_id": h.citation.section_id,
                "highlight": h.highlight,
            })
        })
        .collect();
    Ok(Json(json!({ "hits": arr })))
}

#[derive(Deserialize)]
struct IndexBody {
    collection: String,
    doc_id: String,
    chunks: Vec<Chunk>,
}

async fn index(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<IndexBody>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    principal_from_headers(&headers, &s.keys).ok_or((
        StatusCode::UNAUTHORIZED,
        "missing or invalid API key".into(),
    ))?;

    let mut engine = s.engine.lock().await;
    engine
        .remove_doc(&body.collection, &body.doc_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    for c in &body.chunks {
        engine
            .ingest(&body.collection, c)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    engine
        .commit()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let n = body.chunks.len();
    s.metrics.indexed.fetch_add(n as u64, Ordering::Relaxed);
    Ok(Json(json!({ "indexed": n })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use fastsearch_core::{BBox, ChunkKind};
    use fastsearch_text::TextIndexConfig;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn keys() -> HashMap<String, Principal> {
        let mut m = HashMap::new();
        m.insert(
            "k-team-a".into(),
            Principal {
                tenant: Some("acme".into()),
                tags: vec!["team-a".into()],
            },
        );
        m.insert(
            "k-team-b".into(),
            Principal {
                tenant: Some("acme".into()),
                tags: vec!["team-b".into()],
            },
        );
        m
    }

    fn chunk(id: u64, text: &str, acl: Vec<&str>) -> Chunk {
        Chunk {
            doc_id: "rep.pdf".into(),
            chunk_id: id,
            kind: ChunkKind::Paragraph,
            text: text.into(),
            page: id as u32,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec![],
            section_id: 0,
            char_len: text.len() as u32,
            image_meta: None,
            tenant: Some("acme".into()),
            acl: acl.into_iter().map(String::from).collect(),
        }
    }

    async fn app_with_data() -> Router {
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine
            .ingest("kb", &chunk(1, "secret alpha", vec!["team-a"]))
            .unwrap();
        engine
            .ingest("kb", &chunk(2, "secret beta", vec!["team-b"]))
            .unwrap();
        engine.commit().unwrap();
        router(ServerState::new(engine, keys()))
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_no_auth() {
        let app = app_with_data().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn search_requires_auth() {
        let app = app_with_data().await;
        // 无 key
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // 错 key
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "bogus")
                    .body(Body::from(r#"{"query":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn acl_not_bypassable() {
        let app = app_with_data().await;
        // team-a 的 key 搜 "secret" → 只能看到 chunk 1（team-a），看不到 chunk 2（team-b）
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer k-team-a")
                    .body(Body::from(r#"{"query":"secret","top_k":10}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["chunk_id"], 1);
    }

    #[tokio::test]
    async fn index_then_search() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        // index
        let body = r#"{"collection":"kb","doc_id":"d.pdf","chunks":[
            {"doc_id":"d.pdf","chunk_id":1,"kind":"paragraph","text":"hello world","page":7,
             "bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0},"char_len":11,"acl":["team-a"],"tenant":"acme"}]}"#;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["indexed"], 1);
        // search
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"query":"hello","top_k":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["page"], 7);
        assert_eq!(hits[0]["citation_id"], "kb:d.pdf:1");
    }

    #[tokio::test]
    async fn bad_body_400() {
        let app = app_with_data().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from("{not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn pure_auth_and_acl() {
        let ks = keys();
        let mut h = HeaderMap::new();
        h.insert("x-api-key", "k-team-a".parse().unwrap());
        let p = principal_from_headers(&h, &ks).unwrap();
        assert_eq!(p.tenant.as_deref(), Some("acme"));
        let acl = acl_for(&p);
        assert_eq!(acl.allowed_tags, vec!["team-a".to_string()]);
        // 无 header → None
        assert!(principal_from_headers(&HeaderMap::new(), &ks).is_none());
    }
}
