//! # fastsearch-cli (lib)
//!
//! CLI 的可测逻辑：docparse chunks 解析 + 落盘 index/search。命令行壳在 `main.rs`。
//! 详见 [spec](../../docs/specs/17-cli.md)。

#[cfg(feature = "parse")]
pub mod ingest;

use anyhow::{anyhow, Context, Result};
use fastsearch_core::{
    BBox, Chunk, ChunkKind, FieldValue, Filter, ImageMeta, SearchMode, SearchRequest,
};
use fastsearch_engine::{Engine, SearchHit};
use fastsearch_text::{TextIndexConfig, TokenizerKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// docparse `-f chunks` 的单个 chunk（字段 `id`，无 doc_id/acl）。
#[derive(Debug, Deserialize)]
struct DocparseChunk {
    id: u64,
    kind: ChunkKind,
    text: String,
    page: u32,
    bbox: BBox,
    #[serde(default)]
    heading_path: Vec<String>,
    #[serde(default)]
    section_id: u64,
    char_len: u32,
    #[serde(default)]
    image: Option<ImageMeta>,
}

fn to_core(dc: DocparseChunk, doc_id: &str) -> Chunk {
    Chunk {
        doc_id: doc_id.to_string(),
        chunk_id: dc.id,
        kind: dc.kind,
        text: dc.text,
        page: dc.page,
        bbox: dc.bbox,
        heading_path: dc.heading_path,
        section_id: dc.section_id,
        char_len: dc.char_len,
        // 遗留 docparse `image` 字段迁移到统一 media（file→Object，否则 DocRegion）。
        media: dc.image.as_ref().map(|im| im.to_media(dc.page, dc.bbox)),
        tenant: None,
        acl: vec!["public".to_string()],
    }
}

/// 解析 docparse chunks（JSON 数组 或 NDJSON），注入 doc_id → core::Chunk。
pub fn parse_chunks(bytes: &[u8], doc_id: &str) -> Result<Vec<Chunk>> {
    let s = std::str::from_utf8(bytes).context("input is not valid UTF-8")?;
    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    if trimmed.starts_with('[') {
        let arr: Vec<DocparseChunk> =
            serde_json::from_str(trimmed).context("parsing JSON array of chunks")?;
        Ok(arr.into_iter().map(|c| to_core(c, doc_id)).collect())
    } else {
        // NDJSON：逐行
        let mut out = Vec::new();
        for (i, line) in s.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let c: DocparseChunk = serde_json::from_str(line)
                .map_err(|e| anyhow!("parse error on line {}: {e}", i + 1))?;
            out.push(to_core(c, doc_id));
        }
        Ok(out)
    }
}

/// 解析 markdown 标题行 → `(层级, 标题)`；非标题返回 None。
fn parse_md_heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    let level = t.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let title = t[level..].trim();
    // 需 `#` 后有空白（避免把 `#tag` 误判为标题）。
    if title.is_empty() || !t[level..].starts_with(char::is_whitespace) {
        return None;
    }
    Some((level, title.to_string()))
}

fn heading_titles(path: &[(usize, String)]) -> Vec<String> {
    path.iter().map(|(_, t)| t.clone()).collect()
}

fn mk_text_chunk(
    doc_id: &str,
    id: &mut u64,
    kind: ChunkKind,
    text: String,
    hp: Vec<String>,
) -> Chunk {
    let c = Chunk {
        doc_id: doc_id.to_string(),
        chunk_id: *id,
        kind,
        char_len: text.chars().count() as u32,
        text,
        page: 1, // 纯文本/markdown 无页码概念
        bbox: BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 0.0,
            y1: 0.0,
        },
        heading_path: hp,
        section_id: 0,
        media: None,
        tenant: None,
        acl: vec!["public".to_string()],
    };
    *id += 1;
    c
}

/// flush 待定正文段为一个 Paragraph chunk（非空才推）。
fn flush_para(
    buf: &mut Vec<&str>,
    chunks: &mut Vec<Chunk>,
    next: &mut u64,
    doc_id: &str,
    path: &[(usize, String)],
) {
    if buf.is_empty() {
        return;
    }
    let text = buf.join(" ").trim().to_string();
    buf.clear();
    if !text.is_empty() {
        chunks.push(mk_text_chunk(
            doc_id,
            next,
            ChunkKind::Paragraph,
            text,
            heading_titles(path),
        ));
    }
}

/// 把纯文本 / markdown 内容切成 chunk：**空行分段**；markdown 标题（`# …`）更新 `heading_path`
/// 并自成一个 `Heading` chunk，正文段为 `Paragraph`。供"喂一个文件夹"的端到端检索（无需 PDF/docparse）。
pub fn chunk_text(content: &str, doc_id: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut next = 0u64;
    for line in content.lines() {
        if let Some((level, title)) = parse_md_heading(line) {
            flush_para(&mut buf, &mut chunks, &mut next, doc_id, &path);
            while path.last().map(|(l, _)| *l >= level).unwrap_or(false) {
                path.pop();
            }
            path.push((level, title.clone()));
            chunks.push(mk_text_chunk(
                doc_id,
                &mut next,
                ChunkKind::Heading,
                title,
                heading_titles(&path),
            ));
        } else if line.trim().is_empty() {
            flush_para(&mut buf, &mut chunks, &mut next, doc_id, &path);
        } else {
            buf.push(line);
        }
    }
    flush_para(&mut buf, &mut chunks, &mut next, doc_id, &path);
    chunks
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    tokenizer: String,
}

fn tokenizer_from_name(s: &str) -> TokenizerKind {
    match s {
        "jieba" => TokenizerKind::Jieba,
        _ => TokenizerKind::Default,
    }
}

fn text_dir(data: &Path) -> PathBuf {
    data.join("text")
}

fn load_or_init_meta(data: &Path, tokenizer: TokenizerKind) -> Result<TokenizerKind> {
    let meta_path = data.join("meta.json");
    if meta_path.exists() {
        let m: Meta =
            serde_json::from_slice(&std::fs::read(&meta_path)?).context("reading meta.json")?;
        Ok(tokenizer_from_name(&m.tokenizer))
    } else {
        let m = Meta {
            tokenizer: tokenizer.name().to_string(),
        };
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&m)?)?;
        Ok(tokenizer)
    }
}

fn open_engine(data: &Path, tokenizer: TokenizerKind) -> Result<Engine> {
    std::fs::create_dir_all(text_dir(data))?;
    let cfg = TextIndexConfig {
        tokenizer,
        ..Default::default()
    };
    Ok(Engine::open_or_create(&text_dir(data), cfg)?)
}

/// index 选项。
pub struct IndexOpts {
    pub data: PathBuf,
    pub collection: String,
    pub doc_id: String,
    pub tokenizer: TokenizerKind,
}

/// 灌入一个 doc 的 chunks（doc 级替换）。返回灌入条数。
pub fn cmd_index(opts: &IndexOpts, input: &[u8]) -> Result<usize> {
    std::fs::create_dir_all(&opts.data)?;
    let tokenizer = load_or_init_meta(&opts.data, opts.tokenizer)?;
    let mut engine = open_engine(&opts.data, tokenizer)?;
    let chunks = parse_chunks(input, &opts.doc_id)?;
    engine.remove_doc(&opts.collection, &opts.doc_id)?; // 替换语义：先删旧
    for c in &chunks {
        engine.ingest(&opts.collection, c)?;
    }
    engine.commit()?;
    Ok(chunks.len())
}

/// index-dir 选项（喂整个文件夹）。
pub struct IndexDirOpts {
    pub data: PathBuf,
    pub collection: String,
    pub tokenizer: TokenizerKind,
}

/// 文本类文件后缀（其余忽略——PDF 等需 `parse` feature 的 `ingest`）。
fn is_text_file(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown") | Some("txt") | Some("text")
    )
}

/// 递归收集文件夹下的文本类文件（确定性：调用方排序）。
fn collect_text_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))? {
        let p = entry?.path();
        if p.is_dir() {
            collect_text_files(&p, out)?;
        } else if is_text_file(&p) {
            out.push(p);
        }
    }
    Ok(())
}

/// **喂一个文件夹**：递归遍历 `root` 下的 .md/.txt（确定性排序），对每个文件 `chunk_text` 切块、
/// 按**文件**做 doc 级灌入（`doc_id` = 相对路径，再灌即替换）。返回 `(文件数, chunk 数)`。
/// 之后即可 `cmd_search` 对这批内容检索——一个不依赖 PDF/docparse 的端到端检索闭环。
pub fn cmd_index_dir(opts: &IndexDirOpts, root: &Path) -> Result<(usize, usize)> {
    std::fs::create_dir_all(&opts.data)?;
    let tokenizer = load_or_init_meta(&opts.data, opts.tokenizer)?;
    let mut engine = open_engine(&opts.data, tokenizer)?;
    let mut files = Vec::new();
    collect_text_files(root, &mut files)?;
    files.sort(); // 确定性：稳定的 chunk_id/doc 顺序
    let mut total = 0usize;
    for path in &files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let chunks = chunk_text(&content, &rel);
        engine.remove_doc(&opts.collection, &rel)?; // 替换语义
        for c in &chunks {
            engine.ingest(&opts.collection, c)?;
        }
        total += chunks.len();
    }
    engine.commit()?;
    Ok((files.len(), total))
}

/// search 选项。
pub struct SearchOpts {
    pub data: PathBuf,
    pub collection: String,
    pub query: String,
    pub top_k: usize,
    pub kind: Option<String>,
    pub page_min: Option<u32>,
    pub page_max: Option<u32>,
}

/// 由简单标志构造过滤（kind + page 范围）。
pub fn build_filter(
    kind: Option<&str>,
    page_min: Option<u32>,
    page_max: Option<u32>,
) -> Option<Filter> {
    let mut clauses = Vec::new();
    if let Some(k) = kind {
        clauses.push(Filter::Eq("kind".into(), FieldValue::Str(k.to_string())));
    }
    if let Some(lo) = page_min {
        clauses.push(Filter::Gte("page".into(), FieldValue::Int(lo as i64)));
    }
    if let Some(hi) = page_max {
        clauses.push(Filter::Lte("page".into(), FieldValue::Int(hi as i64)));
    }
    match clauses.len() {
        0 => None,
        1 => clauses.pop(),
        _ => Some(Filter::And(clauses)),
    }
}

/// 落盘 keyword 检索（向量未落盘，故为 keyword 模式）。
pub fn cmd_search(opts: &SearchOpts) -> Result<Vec<SearchHit>> {
    let meta_path = opts.data.join("meta.json");
    let tokenizer = if meta_path.exists() {
        let m: Meta = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
        tokenizer_from_name(&m.tokenizer)
    } else {
        TokenizerKind::Default
    };
    let engine = open_engine(&opts.data, tokenizer)?;
    let req = SearchRequest {
        query: opts.query.clone(),
        mode: SearchMode::Keyword,
        top_k: opts.top_k,
        candidates: opts.top_k.max(150),
        filter: build_filter(opts.kind.as_deref(), opts.page_min, opts.page_max),
        ..Default::default()
    };
    Ok(engine.search(&req, None)?)
}

/// eval 选项。
pub struct EvalOpts {
    /// golden 集 JSON 路径（`GoldenSet` 格式）。
    pub golden: PathBuf,
    /// 可选 baseline 指标 JSON（`Metrics` 格式）；给定则做回归门禁。
    pub baseline: Option<PathBuf>,
    /// 容差（任一指标比 baseline 掉超过此值 → 回归）。
    pub tol: f64,
    /// @k。
    pub k: usize,
    /// 索引分词器（中文 golden 用 jieba）。
    pub tokenizer: TokenizerKind,
    /// 检索模式（默认 keyword，确定性、无需嵌入）。
    pub mode: SearchMode,
}

/// 对 golden 集跑真实检索、算相关性指标；给定 baseline 时做回归门禁。
///
/// 返回 `(Metrics, gate)`，`gate=Some(Err)` 表示掉点超容差（调用方据此置退出码）。
pub fn cmd_eval(opts: &EvalOpts) -> Result<(fastsearch_eval::Metrics, Option<Result<(), String>>)> {
    let json = std::fs::read_to_string(&opts.golden)
        .with_context(|| format!("reading golden {}", opts.golden.display()))?;
    let set = fastsearch_eval::GoldenSet::from_json(&json)
        .with_context(|| format!("parsing golden {}", opts.golden.display()))?;
    let cfg = TextIndexConfig {
        tokenizer: opts.tokenizer,
        ..Default::default()
    };
    let metrics = fastsearch_engine::golden::run(&set, cfg, opts.mode, opts.k)?;
    let gate = match &opts.baseline {
        Some(p) => {
            let b = std::fs::read_to_string(p)
                .with_context(|| format!("reading baseline {}", p.display()))?;
            let baseline: fastsearch_eval::Metrics = serde_json::from_str(&b)
                .with_context(|| format!("parsing baseline {}", p.display()))?;
            Some(fastsearch_eval::assert_no_regression(
                &baseline, &metrics, opts.tol,
            ))
        }
        None => None,
    };
    Ok((metrics, gate))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARR: &str = r#"[
      {"id":1,"kind":"table","text":"毛利率 下降 数据","page":23,
       "bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0},"heading_path":["第3章","财务"],
       "section_id":7,"char_len":9},
      {"id":2,"kind":"paragraph","text":"公司 发布 新 产品","page":3,
       "bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":7}
    ]"#;

    #[test]
    fn parse_array_and_ndjson() {
        let a = parse_chunks(ARR.as_bytes(), "rep.pdf").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].doc_id, "rep.pdf");
        assert_eq!(a[0].chunk_id, 1); // id→chunk_id
        assert_eq!(a[0].acl, vec!["public".to_string()]);
        assert_eq!(a[0].heading_path, vec!["第3章", "财务"]);

        let nd = "{\"id\":5,\"kind\":\"code\",\"text\":\"x\",\"page\":1,\"bbox\":{\"x0\":0.0,\"y0\":0.0,\"x1\":1.0,\"y1\":1.0},\"char_len\":1}";
        let b = parse_chunks(nd.as_bytes(), "d").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].chunk_id, 5);
    }

    #[test]
    fn empty_and_bad_input() {
        assert_eq!(parse_chunks(b"", "d").unwrap().len(), 0);
        assert_eq!(parse_chunks(b"   \n  ", "d").unwrap().len(), 0);
        assert!(parse_chunks(b"{not json}", "d").is_err());
    }

    #[test]
    fn index_then_search_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let iopts = IndexOpts {
            data: dir.path().to_path_buf(),
            collection: "kb".into(),
            doc_id: "rep.pdf".into(),
            tokenizer: TokenizerKind::Jieba,
        };
        let n = cmd_index(&iopts, ARR.as_bytes()).unwrap();
        assert_eq!(n, 2);

        // 新开一次 search（跨"调用"持久化）
        let sopts = SearchOpts {
            data: dir.path().to_path_buf(),
            collection: "kb".into(),
            query: "毛利率".into(),
            top_k: 10,
            kind: None,
            page_min: None,
            page_max: None,
        };
        let hits = cmd_search(&sopts).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
        assert_eq!(hits[0].citation.page, 23);
        assert_eq!(hits[0].citation.heading_path, vec!["第3章", "财务"]);
    }

    #[test]
    fn chunk_text_markdown_headings_and_paras() {
        let md = "# 财报\n\n毛利率 提升 至 42%。\n\n## 风险\n\n汇率 波动 影响 海外 收入。\n\n#notag 不是标题\n";
        let cs = chunk_text(md, "report.md");
        // Heading(财报) + Para + Heading(风险) + Para + Para(#notag 行)
        let kinds: Vec<_> = cs.iter().map(|c| c.kind).collect();
        assert_eq!(cs[0].kind, ChunkKind::Heading);
        assert_eq!(cs[0].text, "财报");
        // "毛利率" 段在 财报 标题下
        let mao = cs.iter().find(|c| c.text.contains("毛利率")).unwrap();
        assert_eq!(mao.heading_path, vec!["财报"]);
        // "汇率" 段在 财报>风险 下
        let fx = cs.iter().find(|c| c.text.contains("汇率")).unwrap();
        assert_eq!(fx.heading_path, vec!["财报", "风险"]);
        // chunk_id 连续从 0
        assert_eq!(cs[0].chunk_id, 0);
        assert!(cs.windows(2).all(|w| w[1].chunk_id == w[0].chunk_id + 1));
        // `#notag` 无空白 → 不是标题，作正文
        assert!(cs.iter().any(|c| c.text.contains("#notag")));
        assert!(kinds.contains(&ChunkKind::Paragraph));
    }

    #[test]
    fn index_dir_then_search() {
        let src = tempfile::tempdir().unwrap();
        // 模拟"一个资料文件夹"：两篇 markdown + 一个子目录 + 一个被忽略的非文本文件。
        std::fs::write(
            src.path().join("finance.md"),
            "# 财务\n\n公司 毛利率 提升 至 42%。\n\n营业 收入 同比 增长。\n",
        )
        .unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(
            src.path().join("sub").join("product.txt"),
            "新一代 旗舰 手机 搭载 自研 芯片。",
        )
        .unwrap();
        std::fs::write(src.path().join("ignore.bin"), b"\x00\x01binary").unwrap();

        let data = tempfile::tempdir().unwrap();
        let iopts = IndexDirOpts {
            data: data.path().to_path_buf(),
            collection: "kb".into(),
            tokenizer: TokenizerKind::Jieba,
        };
        let (files, chunks) = cmd_index_dir(&iopts, src.path()).unwrap();
        assert_eq!(files, 2, "只灌 .md/.txt（忽略 .bin）");
        assert!(chunks >= 3);

        let search = |q: &str| {
            cmd_search(&SearchOpts {
                data: data.path().to_path_buf(),
                collection: "kb".into(),
                query: q.into(),
                top_k: 10,
                kind: None,
                page_min: None,
                page_max: None,
            })
            .unwrap()
        };
        // 跨文件检索：命中来自不同文件，doc_id = 相对路径。
        let mao = search("毛利率");
        assert!(!mao.is_empty());
        assert_eq!(mao[0].id.doc_id, "finance.md");
        assert_eq!(mao[0].citation.heading_path, vec!["财务"]);

        let chip = search("自研 芯片");
        assert!(!chip.is_empty());
        assert_eq!(chip[0].id.doc_id, "sub/product.txt");

        // 子目录文件确实被收录、检索得到。
        assert!(search("旗舰 手机")
            .iter()
            .any(|h| h.id.doc_id == "sub/product.txt"));
    }

    #[test]
    fn filter_kind_and_page() {
        assert!(build_filter(None, None, None).is_none());
        let f = build_filter(Some("table"), Some(10), None).unwrap();
        assert!(matches!(f, Filter::And(ref v) if v.len() == 2));

        let dir = tempfile::tempdir().unwrap();
        let iopts = IndexOpts {
            data: dir.path().to_path_buf(),
            collection: "kb".into(),
            doc_id: "rep.pdf".into(),
            tokenizer: TokenizerKind::Jieba,
        };
        cmd_index(&iopts, ARR.as_bytes()).unwrap();
        // 查 "数据"/"产品" 都在，但限制 kind=table → 只剩 chunk 1
        let sopts = SearchOpts {
            data: dir.path().to_path_buf(),
            collection: "kb".into(),
            query: "数据 产品".into(),
            top_k: 10,
            kind: Some("table".into()),
            page_min: None,
            page_max: None,
        };
        let hits = cmd_search(&sopts).unwrap();
        assert!(hits.iter().all(|h| h.id.chunk_id == 1));
    }

    #[test]
    fn doc_replace_on_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |doc: &str| IndexOpts {
            data: dir.path().to_path_buf(),
            collection: "kb".into(),
            doc_id: doc.into(),
            tokenizer: TokenizerKind::Default,
        };
        // 初次：含 "oldword"
        cmd_index(
            &mk("rep.pdf"),
            br#"[{"id":1,"kind":"paragraph","text":"oldword","page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":7}]"#,
        )
        .unwrap();
        // 再次同 doc：含 "newword"（替换）
        cmd_index(
            &mk("rep.pdf"),
            br#"[{"id":1,"kind":"paragraph","text":"newword","page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":7}]"#,
        )
        .unwrap();
        let s = |q: &str| SearchOpts {
            data: dir.path().to_path_buf(),
            collection: "kb".into(),
            query: q.into(),
            top_k: 10,
            kind: None,
            page_min: None,
            page_max: None,
        };
        assert_eq!(cmd_search(&s("oldword")).unwrap().len(), 0);
        assert_eq!(cmd_search(&s("newword")).unwrap().len(), 1);
    }

    #[test]
    fn eval_runs_and_gates() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("g.json");
        std::fs::write(
            &golden,
            r#"{
              "collection":"kb",
              "corpus":[
                {"doc_id":"d","chunk_id":0,"kind":"paragraph","text":"毛利率 提升 至 42%",
                 "page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":10},
                {"doc_id":"d","chunk_id":1,"kind":"paragraph","text":"员工 休假 政策",
                 "page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":7}
              ],
              "queries":[{"query":"毛利率","relevant":{"kb:d:0":3}}]
            }"#,
        )
        .unwrap();
        let opts = EvalOpts {
            golden: golden.clone(),
            baseline: None,
            tol: 0.02,
            k: 5,
            tokenizer: TokenizerKind::Jieba,
            mode: SearchMode::Keyword,
        };
        let (m, gate) = cmd_eval(&opts).unwrap();
        assert!(gate.is_none());
        // 唯一相关项在 top-1 → 各指标满分。
        assert!((m.mrr - 1.0).abs() < 1e-9);
        assert!((m.ndcg - 1.0).abs() < 1e-9);

        // baseline 比当前高 → 门禁失败。
        let base = dir.path().join("b.json");
        std::fs::write(
            &base,
            r#"{"ndcg":1.0,"recall":1.0,"mrr":1.0,"precision":1.0}"#,
        )
        .unwrap();
        let opts2 = EvalOpts {
            baseline: Some(base),
            ..opts
        };
        let (_m, gate2) = cmd_eval(&opts2).unwrap();
        // precision@5 = 1/5 < baseline 1.0 → 回归。
        assert!(gate2.unwrap().is_err());
    }
}
