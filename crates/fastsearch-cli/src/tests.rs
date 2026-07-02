//! CLI 单测：纯函数（分块/解析/过滤）+ 客户端层对 mock HTTP server 的请求/解析。

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

/// 完整读取一个 HTTP 请求（headers + Content-Length 指定的 body）再返回——必须在响应前排空，
/// 否则提前关连接会让客户端写 body 时收到 RST、读响应报 "Invalid argument"（竞态/flaky）。
fn drain_request(stream: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
            let cl = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() - (pos + 4) >= cl {
                break;
            }
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
            let cl = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() - (pos + 4) >= cl {
                break;
            }
        }
    }
    buf
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

// ---------- 纯函数 ----------

#[test]
fn parse_chunks_array_and_ndjson() {
    let arr = br#"[{"id":0,"kind":"paragraph","text":"hello","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":5}]"#;
    let cs = parse_chunks(arr, "d.pdf").unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].doc_id, "d.pdf");
    assert_eq!(cs[0].text, "hello");
    let img = br#"[{"id":2,"kind":"image","text":"chart","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":5,"image":{"data_base64":"cG5n","media_type":"image/png"}}]"#;
    let cs = parse_chunks(img, "img.pdf").unwrap();
    assert_eq!(cs[0].media_bytes, Some(b"png".to_vec()));
    assert_eq!(cs[0].kind, ChunkKind::Image);
    // NDJSON
    let nd = b"{\"id\":0,\"kind\":\"heading\",\"text\":\"H\",\"page\":1,\"bbox\":{\"x0\":0,\"y0\":0,\"x1\":1,\"y1\":1},\"char_len\":1}\n{\"id\":1,\"kind\":\"paragraph\",\"text\":\"p\",\"page\":1,\"bbox\":{\"x0\":0,\"y0\":0,\"x1\":1,\"y1\":1},\"char_len\":1}";
    assert_eq!(parse_chunks(nd, "d").unwrap().len(), 2);
}

#[test]
fn chunk_text_markdown_headings_and_paras() {
    let md = "# Title\n\nfirst para line\nsecond line\n\n## Sub\n\nbody";
    let cs = chunk_text(md, "doc.md");
    // 至少：Title(heading) + para + Sub(heading) + body(para)
    assert!(cs
        .iter()
        .any(|c| c.kind == ChunkKind::Heading && c.text == "Title"));
    assert!(cs
        .iter()
        .any(|c| c.kind == ChunkKind::Heading && c.text == "Sub"));
    assert!(cs
        .iter()
        .any(|c| c.kind == ChunkKind::Paragraph && c.text.contains("first para")));
    // heading_path 累积
    let body = cs.iter().find(|c| c.text == "body").unwrap();
    assert_eq!(body.heading_path, vec!["Title", "Sub"]);
}

#[test]
fn build_filter_always_scopes_collection() {
    // 仅 collection → 单 Eq
    match build_filter("kb", None, None, None, None) {
        Filter::Eq(f, FieldValue::Str(v)) => {
            assert_eq!(f, "collection");
            assert_eq!(v, "kb");
        }
        other => panic!("expected Eq(collection), got {other:?}"),
    }
    // collection + kind + page → And 含 collection
    match build_filter("kb", Some("table"), None, Some(2), None) {
        Filter::And(cl) => {
            assert_eq!(cl.len(), 3);
            assert!(
                matches!(&cl[0], Filter::Eq(f, FieldValue::Str(v)) if f=="collection" && v=="kb")
            );
        }
        other => panic!("expected And, got {other:?}"),
    }
    match build_filter("kb", None, Some("image"), None, None) {
        Filter::And(cl) => {
            assert!(
                matches!(&cl[1], Filter::Eq(f, FieldValue::Str(v)) if f=="modality" && v=="image")
            );
        }
        other => panic!("expected And, got {other:?}"),
    }
}

// ---------- mock HTTP server ----------

/// 起一个返回固定 JSON 的 mock server，返回其 base URL（每连接一条响应，无限服务）。
fn spawn_mock(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            drain_request(&mut stream);
            write_response(&mut stream, "200 OK", body);
        }
    });
    format!("http://{addr}")
}

fn spawn_capture(body: &'static str) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let req = read_request(&mut stream);
            let _ = tx.send(req);
            write_response(&mut stream, "200 OK", body);
        }
    });
    (format!("http://{addr}"), rx)
}

#[test]
fn cmd_search_parses_hits_from_server() {
    let url = spawn_mock(
        r#"{"hits":[{"citation_id":"kb:a.pdf:1","score":0.91,"page":3,"heading_path":["H"]}],"facets":[]}"#,
    );
    let opts = SearchOpts {
        server: Some(url),
        key: Some("k".into()),
        collection: "kb".into(),
        query: "毛利率".into(),
        mode: SearchMode::Hybrid,
        top_k: 10,
        kind: None,
        modality: None,
        image: None,
        page_min: None,
        page_max: None,
    };
    let hits = cmd_search(&opts).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["citation_id"], "kb:a.pdf:1");
}

#[test]
fn cmd_index_reports_indexed_count() {
    let url = spawn_mock(r#"{"indexed":2}"#);
    let opts = IndexOpts {
        server: Some(url),
        key: Some("k".into()),
        collection: "kb".into(),
        doc_id: "d.pdf".into(),
        store_media: None,
    };
    let input = br#"[{"id":0,"kind":"paragraph","text":"a","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":1},{"id":1,"kind":"paragraph","text":"b","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":1}]"#;
    assert_eq!(cmd_index(&opts, input).unwrap(), 2);
}

#[test]
fn cmd_upload_image_posts_multipart_to_images_endpoint() {
    let (url, rx) = spawn_capture(r#"{"indexed":1}"#);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("img.png");
    std::fs::write(&path, b"png-bytes").unwrap();
    let opts = ImageUploadOpts {
        server: Some(url),
        key: Some("k".into()),
        collection: "kb".into(),
        doc_id: "img-1".into(),
        text: Some("chart caption".into()),
        page: 2,
        store_media: Some(StoreMedia::Object),
    };
    let v = cmd_upload_image(&opts, &path).unwrap();
    assert_eq!(v["indexed"], 1);
    let req = String::from_utf8_lossy(&rx.recv().unwrap()).to_string();
    assert!(req.starts_with("POST /v1/images "), "request: {req}");
    assert!(req.contains("multipart/form-data"));
    assert!(req.contains("name=\"collection\"\r\n\r\nkb"));
    assert!(req.contains("name=\"doc_id\"\r\n\r\nimg-1"));
    assert!(req.contains("name=\"text\"\r\n\r\nchart caption"));
    assert!(req.contains("name=\"store_media\"\r\n\r\nobject"));
    assert!(req.contains("Content-Type: image/png"));
    assert!(req.contains("png-bytes"));
}

#[test]
fn cmd_index_dir_feeds_folder_to_server() {
    let url = spawn_mock(r#"{"indexed":3}"#);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# A\n\nbody a").unwrap();
    std::fs::write(dir.path().join("b.txt"), "plain b").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/c.md"), "# C\n\nbody c").unwrap();
    let opts = IndexDirOpts {
        server: Some(url),
        key: Some("k".into()),
        collection: "kb".into(),
        concurrency: 1,
    };
    let (ok, failed, total) = cmd_index_dir(&opts, dir.path()).unwrap();
    assert_eq!(ok, 3, "三个文本文件都应上传");
    assert_eq!(failed, 0);
    assert_eq!(total, 9, "每文件 mock 报 3 chunk → 3×3");
}

#[test]
fn cmd_index_dir_concurrent_uploads_all() {
    // 并发档：20 个文件、concurrency=6 → 全部上传，计数确定（原子聚合不丢/不重）。
    let url = spawn_mock(r#"{"indexed":1}"#);
    let dir = tempfile::tempdir().unwrap();
    for i in 0..20 {
        std::fs::write(
            dir.path().join(format!("f{i}.md")),
            format!("# T{i}\n\nbody {i}"),
        )
        .unwrap();
    }
    let opts = IndexDirOpts {
        server: Some(url),
        key: Some("k".into()),
        collection: "kb".into(),
        concurrency: 6,
    };
    let (ok, failed, total) = cmd_index_dir(&opts, dir.path()).unwrap();
    assert_eq!(ok, 20, "20 文件应全部上传（并发不丢）");
    assert_eq!(failed, 0);
    assert_eq!(total, 20, "每文件 mock 报 1 → 共 20");
}

#[test]
fn server_error_surfaced() {
    // mock 返回 500 → cmd_search 应报错（含状态码）。
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            drain_request(&mut stream);
            write_response(&mut stream, "500 Internal Server Error", "boom");
        }
    });
    let opts = SearchOpts {
        server: Some(format!("http://{addr}")),
        key: Some("k".into()),
        collection: "kb".into(),
        query: "q".into(),
        mode: SearchMode::Keyword,
        top_k: 5,
        kind: None,
        modality: None,
        image: None,
        page_min: None,
        page_max: None,
    };
    let err = cmd_search(&opts).unwrap_err().to_string();
    assert!(err.contains("500"), "应含状态码: {err}");
}

fn index_opts(url: String) -> IndexOpts {
    IndexOpts {
        server: Some(url),
        key: Some("k".into()),
        collection: "kb".into(),
        doc_id: "d".into(),
        store_media: None,
    }
}
const ONE_CHUNK: &[u8] = br#"[{"id":0,"kind":"paragraph","text":"a","page":1,"bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":1}]"#;

#[test]
fn index_status_error_does_not_retry() {
    // 500 是确定性拒绝 → post_retry 不应重试（恰好 1 次请求）。
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = count.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            c2.fetch_add(1, Ordering::SeqCst);
            drain_request(&mut s);
            write_response(&mut s, "500 Internal Server Error", "boom");
        }
    });
    let err = cmd_index(&index_opts(format!("http://{addr}")), ONE_CHUNK).unwrap_err();
    assert!(err.to_string().contains("500"));
    assert_eq!(count.load(Ordering::SeqCst), 1, "Status 拒绝不应重试");
}

#[test]
fn index_transport_failure_retries_then_succeeds() {
    // 首次连接不响应即关（传输失败）→ 应重试；第二次成功。
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = count.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
            drain_request(&mut s);
            if n == 1 {
                drop(s); // 不响应 → 客户端读响应得传输错误
            } else {
                write_response(&mut s, "200 OK", r#"{"indexed":1}"#);
            }
        }
    });
    let n = cmd_index(&index_opts(format!("http://{addr}")), ONE_CHUNK).unwrap();
    assert_eq!(n, 1);
    assert!(count.load(Ordering::SeqCst) >= 2, "传输失败后应重试");
}
