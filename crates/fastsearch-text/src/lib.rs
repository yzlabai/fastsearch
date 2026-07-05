//! # fastsearch-text
//!
//! 引擎侧派生全文索引（Tantivy/BM25）。schema 构建、CJK 分词、upsert/delete、
//! 带过滤与 ACL 的检索，返回带引用（page+bbox+heading_path）的命中。
//! 详见 [spec](../../docs/specs/11-text.md)。

mod bm25;
mod error;
mod query_build;
mod schema;
mod tokenizer;

pub use error::{Result, TextError};
pub use query_build::StoredRow;
pub use schema::{TextIndexConfig, TokenizerKind};

use fastsearch_core::{AclFilter, Chunk, ChunkKind, Citation, Filter, GlobalId};
use query_build::{acl_query, stored_row, translate};
use schema::{build_schema, Fields};
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, RegexQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};
use tokenizer::JiebaTokenizer;

/// 一条全文命中。
#[derive(Debug, Clone)]
pub struct TextHit {
    pub id: GlobalId,
    pub score: f32,
    pub citation: Citation,
    /// chunk 类型（用于分面）。
    pub kind: String,
    /// 命中正文（用于 rerank / 上层展示）。
    pub text: String,
    /// 高亮片段（HTML，命中词包 `<b>`）；未请求高亮或无命中词时为 None。
    pub highlight: Option<String>,
}

/// 派生全文索引。
pub struct TextIndex {
    index: Index,
    fields: Fields,
    cfg: TextIndexConfig,
    writer: IndexWriter,
    reader: IndexReader,
}

/// 转义正则元字符（用于把用户前缀安全嵌入 `RegexQuery` 模式）。
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".+*?()[]{}^$|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn kind_str(k: ChunkKind) -> &'static str {
    match k {
        ChunkKind::Heading => "heading",
        ChunkKind::Paragraph => "paragraph",
        ChunkKind::Table => "table",
        ChunkKind::Code => "code",
        ChunkKind::ListItem => "list_item",
        ChunkKind::Image => "image",
        ChunkKind::Audio => "audio",
        ChunkKind::Video => "video",
    }
}

impl TextIndex {
    /// 内存索引（测试/瞬态用）。
    pub fn create_in_ram(cfg: TextIndexConfig) -> Result<Self> {
        let (schema, fields) = build_schema(cfg.tokenizer);
        let index = Index::create_in_ram(schema);
        Self::finish(index, fields, cfg)
    }

    /// 打开或在目录创建（mmap，落盘）。
    pub fn open_or_create(dir: &std::path::Path, cfg: TextIndexConfig) -> Result<Self> {
        let (schema, fields) = build_schema(cfg.tokenizer);
        let mmap = tantivy::directory::MmapDirectory::open(dir)
            .map_err(|e| TextError::QueryParse(format!("open dir: {e}")))?;
        let index = Index::open_or_create(mmap, schema)?;
        Self::finish(index, fields, cfg)
    }

    fn finish(index: Index, fields: Fields, cfg: TextIndexConfig) -> Result<Self> {
        index.tokenizers().register("jieba", JiebaTokenizer::new());
        let writer = index.writer(50_000_000)?;
        let reader = index.reader()?;
        Ok(TextIndex {
            index,
            fields,
            cfg,
            writer,
            reader,
        })
    }

    /// upsert：同 gid 覆盖（先按 gid 删，再加）。
    pub fn upsert(&mut self, collection: &str, chunk: &Chunk) -> Result<()> {
        let gid = chunk.global_id(collection).to_citation_id();
        self.writer
            .delete_term(Term::from_field_text(self.fields.gid, &gid));
        let f = &self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_text(f.gid, &gid);
        doc.add_text(f.collection, collection);
        doc.add_text(f.doc_id, &chunk.doc_id);
        doc.add_text(f.text, &chunk.text);
        doc.add_text(f.heading, chunk.heading_path.join(" "));
        doc.add_text(f.kind, kind_str(chunk.kind));
        doc.add_text(f.modality, chunk.kind.modality().as_str());
        doc.add_u64(f.page, chunk.page as u64);
        doc.add_u64(f.section_id, chunk.section_id);
        doc.add_u64(f.chunk_id, chunk.chunk_id);
        if let Some(t) = &chunk.tenant {
            doc.add_text(f.tenant, t);
        }
        for a in &chunk.acl {
            doc.add_text(f.acl, a);
        }
        doc.add_text(f.heading_path, serde_json::to_string(&chunk.heading_path)?);
        doc.add_text(f.bbox, serde_json::to_string(&chunk.bbox)?);
        if let Some(m) = &chunk.media {
            doc.add_text(f.media, serde_json::to_string(m)?);
        }
        self.writer.add_document(doc)?;
        Ok(())
    }

    /// 按 global_id 删除。
    pub fn delete_by_global_id(&mut self, gid: &GlobalId) -> Result<()> {
        self.writer.delete_term(Term::from_field_text(
            self.fields.gid,
            &gid.to_citation_id(),
        ));
        Ok(())
    }

    /// 删除某 `(collection, doc_id)` 的全部 chunk（doc_id 级替换）。
    ///
    /// Tantivy 仅支持按 term 删除，故先检索（已提交部分）出该 doc 的全部 gid 再
    /// 逐个删除，避免跨集合误删同名 doc_id。
    pub fn delete_by_doc(&mut self, collection: &str, doc_id: &str) -> Result<()> {
        let searcher = self.reader.searcher();
        let q = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.collection, collection),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.doc_id, doc_id),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let top = searcher.search(&q, &TopDocs::with_limit(100_000).order_by_score())?;
        for (_score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(gid) = doc
                .get_first(self.fields.gid)
                .and_then(|v| tantivy::schema::Value::as_str(&v))
            {
                self.writer
                    .delete_term(Term::from_field_text(self.fields.gid, gid));
            }
        }
        Ok(())
    }

    /// 按 `(collection, doc_id)` 取已提交行。用于删除前收集媒资引用。
    pub fn stored_rows_by_doc(
        &self,
        collection: &str,
        doc_id: &str,
    ) -> Result<Vec<query_build::StoredRow>> {
        let searcher = self.reader.searcher();
        let q = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.collection, collection),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.doc_id, doc_id),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let top = searcher.search(&q, &TopDocs::with_limit(100_000).order_by_score())?;
        let mut out = Vec::with_capacity(top.len());
        for (_score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            out.push(query_build::stored_row(&doc, &self.fields));
        }
        Ok(out)
    }

    /// 提交并刷新 reader（提交后立即可见）。
    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// 清空全部文档（保持同 schema/分词器），供单集合**原地重建**（坏索引→从真源重灌）。
    /// 不提交——由调用方在重灌后统一 `commit`，使"清空+重灌"成一次可见切换。
    pub fn clear(&mut self) -> Result<()> {
        self.writer.delete_all_documents()?;
        Ok(())
    }

    /// 按 global_id 取已索引的正文（STORED 字段）；不存在返回 None。
    /// 供 more_like_this 等需要"种子文本"的能力使用。
    pub fn stored_text(&self, gid: &GlobalId) -> Result<Option<String>> {
        let searcher = self.reader.searcher();
        let q = TermQuery::new(
            Term::from_field_text(self.fields.gid, &gid.to_citation_id()),
            IndexRecordOption::Basic,
        );
        let top = searcher.search(&q, &TopDocs::with_limit(1).order_by_score())?;
        match top.first() {
            Some((_s, addr)) => {
                let doc: TantivyDocument = searcher.doc(*addr)?;
                Ok(doc
                    .get_first(self.fields.text)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()))
            }
            None => Ok(None),
        }
    }

    /// 按 global_id 取已索引行的完整字段视图（acl/tenant/media/引用所需），供
    /// `resolve_citation` 做 ACL 校验 + 媒资解析。不存在返回 None。
    pub fn stored_row_by_gid(&self, gid: &GlobalId) -> Result<Option<StoredRow>> {
        let searcher = self.reader.searcher();
        let q = TermQuery::new(
            Term::from_field_text(self.fields.gid, &gid.to_citation_id()),
            IndexRecordOption::Basic,
        );
        let top = searcher.search(&q, &TopDocs::with_limit(1).order_by_score())?;
        match top.first() {
            Some((_s, addr)) => {
                let doc: TantivyDocument = searcher.doc(*addr)?;
                Ok(Some(query_build::stored_row(&doc, &self.fields)))
            }
            None => Ok(None),
        }
    }

    /// 构造正文查询：短语 `"a b"`、邻近 `"a b"~N`、布尔走 Tantivy `QueryParser`；
    /// **末词带 `*` 的前缀查询**（search-as-you-type，如 `数据 检索*`）由本方法识别并
    /// 用 `RegexQuery` 在 text 字段做前缀匹配（前词照常 parse），两路 Should 合并。
    /// 含引号（短语）时不启用前缀逻辑，整串交给 parser。
    fn build_text_query(&self, query: &str) -> Result<Box<dyn Query>> {
        let parser = |q: &str| -> Result<Box<dyn Query>> {
            let mut qp =
                QueryParser::for_index(&self.index, vec![self.fields.text, self.fields.heading]);
            qp.set_field_boost(self.fields.heading, self.cfg.heading_boost);
            qp.parse_query(q)
                .map(|b| b as Box<dyn Query>)
                .map_err(|e| TextError::QueryParse(e.to_string()))
        };

        let trimmed = query.trim();
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        let last = tokens.last().copied().unwrap_or("");
        let is_prefix = !trimmed.contains('"')
            && last.len() > 1
            && last.ends_with('*')
            && !last[..last.len() - 1].contains('*');
        if !is_prefix {
            return parser(trimmed);
        }

        // 末词前缀（小写化以对齐 default 分词器；正则元字符转义）。
        let prefix = last[..last.len() - 1].to_lowercase();
        let pat = format!("{}.*", regex_escape(&prefix));
        let prefix_q: Box<dyn Query> = Box::new(
            RegexQuery::from_pattern(&pat, self.fields.text)
                .map_err(|e| TextError::QueryParse(format!("prefix regex: {e}")))?,
        );
        let head = &tokens[..tokens.len() - 1];
        if head.is_empty() {
            Ok(prefix_q)
        } else {
            // 前词正常解析 + 末词前缀，Should 合并（与 parser 默认 OR 语义一致）。
            Ok(Box::new(BooleanQuery::new(vec![
                (Occur::Should, parser(&head.join(" "))?),
                (Occur::Should, prefix_q),
            ])))
        }
    }

    /// 检索：BM25 + 可选过滤 + 可选 ACL，返回带引用的 top-k。
    ///
    /// 预过滤（SUPERSET 翻译）缩小候选，post-filter（core eval + ACL visible）保证
    /// 精确与不越权。over-fetch 抵消后过滤截断。
    pub fn search(
        &self,
        query: &str,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
        k: usize,
        highlight: bool,
    ) -> Result<Vec<TextHit>> {
        let searcher = self.reader.searcher();
        let empty_query = query.trim().is_empty();
        let text_q: Box<dyn Query> = if empty_query {
            Box::new(AllQuery)
        } else {
            self.build_text_query(query)?
        };

        // 高亮：从 text 查询构造 SnippetGenerator（空查询无命中词，不高亮）。
        let snippet_gen: Option<SnippetGenerator> = if highlight && !empty_query {
            SnippetGenerator::create(&searcher, &*text_q, self.fields.text).ok()
        } else {
            None
        };

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_q)];
        if let Some(f) = filter {
            clauses.push((Occur::Must, translate(f, &self.fields)));
        }
        if let Some(a) = acl {
            clauses.push((Occur::Must, acl_query(a, &self.fields)));
        }
        let q = BooleanQuery::new(clauses);

        // 自定义 k1/b 生效时放宽候选窗口（重排会改变 top-k 归属，减小窗口偏差）。
        let custom_bm25 = bm25::custom_params_active(self.cfg.k1, self.cfg.b);
        let overfetch = if custom_bm25 {
            k.saturating_mul(8).max(k.saturating_add(64))
        } else {
            k.saturating_mul(4).max(k.saturating_add(16))
        };
        let mut top = searcher.search(&q, &TopDocs::with_limit(overfetch).order_by_score())?;

        // 自定义 BM25 重排：用配置 k1/b 自算分覆盖 Tantivy 原生分，再按 (分降序, DocAddress 升序)
        // 确定性重排。无可计分词的候选（纯前缀命中）保留原生分。matching 不变、仅排序变。
        if custom_bm25 {
            let addrs: Vec<_> = top.iter().map(|(_, a)| *a).collect();
            let scores = bm25::score_candidates(
                &searcher,
                &q,
                self.fields.text,
                self.fields.heading,
                self.cfg.heading_boost,
                self.cfg.k1,
                self.cfg.b,
                &addrs,
            )?;
            for (score, addr) in top.iter_mut() {
                if let Some(s) = scores.get(addr) {
                    *score = *s;
                }
            }
            top.sort_by(|(sa, aa), (sb, ab)| {
                sb.partial_cmp(sa)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| (aa.segment_ord, aa.doc_id).cmp(&(ab.segment_ord, ab.doc_id)))
            });
        }

        let mut hits = Vec::with_capacity(k);
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let row = stored_row(&doc, &self.fields);
            if let Some(f) = filter {
                if !f.eval(&row) {
                    continue;
                }
            }
            if let Some(a) = acl {
                if !a.visible(&row) {
                    continue;
                }
            }
            let text = doc
                .get_first(self.fields.text)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // 高亮片段：从存储的 text 生成（命中词包 <b>）；无命中词 → None。
            let highlight = snippet_gen.as_ref().and_then(|g| {
                let snip = g.snippet(&text);
                if snip.fragment().is_empty() {
                    None
                } else {
                    Some(snip.to_html())
                }
            });
            let citation = Citation {
                collection: row.collection.clone(),
                doc_id: row.doc_id.clone(),
                chunk_id: row.chunk_id,
                page: row.page as u32,
                bbox: row.bbox,
                heading_path: row.heading.clone(),
                section_id: row.section_id,
                // 时间区间从媒资引用透出（音视频深链）；媒资引用供答案层渲染/取字节。
                time: row.media.as_ref().and_then(|m| m.time),
                media: row.media.clone(),
            };
            hits.push(TextHit {
                id: GlobalId {
                    collection: row.collection,
                    doc_id: row.doc_id,
                    chunk_id: row.chunk_id,
                },
                score,
                citation,
                kind: row.kind,
                text,
                highlight,
            });
            if hits.len() >= k {
                break;
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::BBox;

    fn chunk(doc: &str, id: u64, kind: ChunkKind, text: &str, page: u32) -> Chunk {
        Chunk {
            doc_id: doc.into(),
            chunk_id: id,
            kind,
            text: text.into(),
            page,
            bbox: BBox {
                x0: 1.0,
                y0: 2.0,
                x1: 3.0,
                y1: 4.0,
            },
            heading_path: vec!["第3章".into(), "财务".into()],
            section_id: 7,
            char_len: text.chars().count() as u32,
            media: None,
            media_bytes: None,
            image_vector_status: None,
            tenant: None,
            acl: vec!["public".into()],
        }
    }

    fn ram(tok: TokenizerKind) -> TextIndex {
        TextIndex::create_in_ram(TextIndexConfig {
            tokenizer: tok,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn index_search_returns_citation() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha beta gamma", 5),
        )
        .unwrap();
        idx.upsert(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta delta", 6),
        )
        .unwrap();
        idx.commit().unwrap();
        let hits = idx.search("alpha", None, None, 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
        assert_eq!(hits[0].citation.page, 5);
        assert_eq!(hits[0].citation.bbox.x1, 3.0);
        assert_eq!(hits[0].citation.heading_path, vec!["第3章", "财务"]);
    }

    #[test]
    fn phrase_slop_prefix_queries() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "quick brown fox", 1),
        )
        .unwrap();
        idx.upsert(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "brown quick fox", 2),
        )
        .unwrap();
        idx.upsert(
            "kb",
            &chunk("a.pdf", 3, ChunkKind::Paragraph, "browser engine", 3),
        )
        .unwrap();
        idx.commit().unwrap();
        let n = |q: &str| idx.search(q, None, None, 10, false).unwrap().len();

        // 短语：仅匹配连续 "quick brown"（chunk 1）
        assert_eq!(n("\"quick brown\""), 1);
        // 邻近 slop=2：chunk 1（quick _ fox）与 chunk 2（quick fox）都在 2 词内
        assert_eq!(n("\"quick fox\"~2"), 2);
        // 前缀 brow*：brown(1,2) + browser(3) → 3 条
        assert_eq!(n("brow*"), 3);
        // 多词 + 末词前缀：engine 命中 3，brow* 命中 1/2/3 → 仍 3（Should 合并）
        assert_eq!(n("engine brow*"), 3);
        // 前词 + 前缀缩小：'fox brow*' → fox(1,2) ∪ brow*(1,2,3) = 3
        assert_eq!(n("fox brow*"), 3);
        // 无匹配前缀
        assert_eq!(n("zzz*"), 0);
    }

    #[test]
    fn bm25_orders_more_relevant_first() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "beta", 1))
            .unwrap();
        idx.upsert(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta beta beta", 1),
        )
        .unwrap();
        idx.commit().unwrap();
        let hits = idx.search("beta", None, None, 10, false).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id.chunk_id, 2); // 词频高的在前
    }

    fn ram_kb(k1: f32, b: f32) -> TextIndex {
        TextIndex::create_in_ram(TextIndexConfig {
            k1,
            b,
            ..Default::default()
        })
        .unwrap()
    }

    // A11：`b`（长度归一）真生效——短文 vs 长文高 tf，b=0/b=1 排序翻转。
    #[test]
    fn bm25_b_length_norm_takes_effect() {
        let short = chunk("a.pdf", 1, ChunkKind::Paragraph, "target", 1);
        let mut long_text = String::from("target target target");
        for i in 0..30 {
            long_text.push_str(&format!(" filler{i}"));
        }
        let longd = chunk("b.pdf", 2, ChunkKind::Paragraph, &long_text, 2);
        let order = |k1: f32, b: f32| {
            let mut idx = ram_kb(k1, b);
            idx.upsert("kb", &short).unwrap();
            idx.upsert("kb", &longd).unwrap();
            idx.commit().unwrap();
            idx.search("target", None, None, 10, false)
                .unwrap()
                .into_iter()
                .map(|h| h.id.chunk_id)
                .collect::<Vec<_>>()
        };
        // b=0：无长度归一 → 长文高 tf(3) 占优
        assert_eq!(order(1.2, 0.0)[0], 2, "b=0 应偏向高 tf 长文");
        // b=1：满长度归一 → 短文(tf=1, dl=1)反超
        assert_eq!(order(1.2, 1.0)[0], 1, "b=1 应偏向短文");
    }

    // A11：`k1`（词频饱和）真生效——同一单文档，k1 改变实际 BM25 分。
    #[test]
    fn bm25_k1_changes_score() {
        let doc = chunk("a.pdf", 1, ChunkKind::Paragraph, "term term term", 1);
        let score = |k1: f32| {
            let mut idx = ram_kb(k1, 0.75);
            idx.upsert("kb", &doc).unwrap();
            idx.commit().unwrap();
            idx.search("term", None, None, 10, false).unwrap()[0].score
        };
        let (lo, hi) = (score(0.5), score(2.5));
        assert!(
            (hi - lo).abs() > 1e-3,
            "k1 应改变打分：k1=0.5→{lo}, k1=2.5→{hi}"
        );
    }

    // A11：自算公式与 Tantivy 对齐——≈默认参数(触发自算路径)的排序 == 原生默认路径排序。
    #[test]
    fn custom_bm25_near_default_matches_native_order() {
        let docs = [
            chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha beta alpha", 1),
            chunk(
                "a.pdf",
                2,
                ChunkKind::Paragraph,
                "alpha gamma delta epsilon",
                2,
            ),
            chunk(
                "a.pdf",
                3,
                ChunkKind::Paragraph,
                "beta alpha alpha alpha",
                3,
            ),
        ];
        let order = |k1: f32, b: f32| {
            let mut idx = ram_kb(k1, b);
            for d in &docs {
                idx.upsert("kb", d).unwrap();
            }
            idx.commit().unwrap();
            idx.search("alpha", None, None, 10, false)
                .unwrap()
                .into_iter()
                .map(|h| h.id.chunk_id)
                .collect::<Vec<_>>()
        };
        // 原生路径(精确默认) vs 自算路径(≈默认，偏移 >EPSILON 触发重排)：排序一致。
        assert_eq!(order(1.2, 0.75), order(1.2 + 1e-3, 0.75));
    }

    #[test]
    fn highlight_wraps_match() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert(
            "kb",
            &chunk(
                "a.pdf",
                1,
                ChunkKind::Paragraph,
                "the quick brown fox jumps",
                1,
            ),
        )
        .unwrap();
        idx.commit().unwrap();
        // 不要高亮 → None
        let no = idx.search("brown", None, None, 10, false).unwrap();
        assert!(no[0].highlight.is_none());
        // 要高亮 → 命中词包 <b>
        let hi = idx.search("brown", None, None, 10, true).unwrap();
        let h = hi[0].highlight.as_ref().unwrap();
        assert!(h.contains("<b>brown</b>"), "highlight was: {h}");
    }

    #[test]
    fn chinese_jieba_search() {
        let mut idx = ram(TokenizerKind::Jieba);
        idx.upsert(
            "kb",
            &chunk(
                "a.pdf",
                1,
                ChunkKind::Table,
                "本季度毛利率因成本上升而下降",
                23,
            ),
        )
        .unwrap();
        idx.upsert(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "公司发布新产品", 3),
        )
        .unwrap();
        idx.commit().unwrap();
        let hits = idx.search("毛利率", None, None, 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn filter_kind_and_page() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert("kb", &chunk("a.pdf", 1, ChunkKind::Table, "data here", 12))
            .unwrap();
        idx.upsert(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "data here", 12),
        )
        .unwrap();
        idx.upsert("kb", &chunk("a.pdf", 3, ChunkKind::Table, "data here", 5))
            .unwrap();
        idx.commit().unwrap();
        // kind=table AND page>=10  → 只剩 chunk 1
        let f = Filter::And(vec![
            Filter::Eq(
                "kind".into(),
                fastsearch_core::FieldValue::Str("table".into()),
            ),
            Filter::Gte("page".into(), fastsearch_core::FieldValue::Int(10)),
        ]);
        let hits = idx.search("data", Some(&f), None, 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn filter_ne_and_not_translate_to_complement() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert("kb", &chunk("a.pdf", 1, ChunkKind::Table, "data here", 1))
            .unwrap();
        idx.upsert(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "data here", 1),
        )
        .unwrap();
        idx.upsert("kb", &chunk("a.pdf", 3, ChunkKind::Code, "data here", 1))
            .unwrap();
        idx.commit().unwrap();
        let table = || fastsearch_core::FieldValue::Str("table".into());

        // Ne(kind, table) → 排除 chunk 1，留 2/3
        let ne = Filter::Ne("kind".into(), table());
        let mut got: Vec<u64> = idx
            .search("data", Some(&ne), None, 10, false)
            .unwrap()
            .iter()
            .map(|h| h.id.chunk_id)
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec![2, 3]);

        // Not(Eq(kind, table)) 等价
        let not = Filter::Not(Box::new(Filter::Eq("kind".into(), table())));
        let mut got2: Vec<u64> = idx
            .search("data", Some(&not), None, 10, false)
            .unwrap()
            .iter()
            .map(|h| h.id.chunk_id)
            .collect();
        got2.sort_unstable();
        assert_eq!(got2, vec![2, 3]);

        // Not(And(Eq kind=table, page>=1)) → 同样只排除 chunk 1（精确补集）
        let not_and = Filter::Not(Box::new(Filter::And(vec![
            Filter::Eq("kind".into(), table()),
            Filter::Gte("page".into(), fastsearch_core::FieldValue::Int(1)),
        ])));
        let mut got3: Vec<u64> = idx
            .search("data", Some(&not_and), None, 10, false)
            .unwrap()
            .iter()
            .map(|h| h.id.chunk_id)
            .collect();
        got3.sort_unstable();
        assert_eq!(got3, vec![2, 3]);

        // Not(Exists(tenant))：不可精确翻译 → 退化 AllQuery，靠后过滤；所有行无 tenant → 全留
        let not_exists = Filter::Not(Box::new(Filter::Exists("tenant".into())));
        let got4 = idx
            .search("data", Some(&not_exists), None, 10, false)
            .unwrap();
        assert_eq!(got4.len(), 3);
    }

    #[test]
    fn float_filter_values_preserve_superset() {
        // H2 回归（text 端到端）：JSON 数值过滤常带小数点 → Float。Lt/Ne/Eq 对 Float 仍须正确
        // （旧 field_as_u64 把 5.5 截成 5 → Lt 漏 page=5；旧变体严格 Ne 补集会错杀）。现翻译退
        // AllQuery + core 精确后过滤（数值互比）保证正确。
        use fastsearch_core::FieldValue;
        let mut idx = ram(TokenizerKind::Default);
        for (id, page) in [(1u64, 4u32), (2, 5), (3, 6)] {
            idx.upsert(
                "kb",
                &chunk("a.pdf", id, ChunkKind::Paragraph, "data here", page),
            )
            .unwrap();
        }
        idx.commit().unwrap();
        let pages = |f: &Filter| {
            let mut v: Vec<u64> = idx
                .search("data", Some(f), None, 10, false)
                .unwrap()
                .iter()
                .map(|h| h.id.chunk_id)
                .collect();
            v.sort_unstable();
            v
        };
        // Lt(page, 5.5) → page 4,5（不漏 page=5）
        assert_eq!(
            pages(&Filter::Lt("page".into(), FieldValue::Float(5.5))),
            vec![1, 2]
        );
        // Ne(page, 6.0) → page 4,5（排除 6，不误留不误杀）
        assert_eq!(
            pages(&Filter::Ne("page".into(), FieldValue::Float(6.0))),
            vec![1, 2]
        );
        // Eq(page, 6.0) → 仅 page=6
        assert_eq!(
            pages(&Filter::Eq("page".into(), FieldValue::Float(6.0))),
            vec![3]
        );
    }

    #[test]
    fn empty_text_and_modality_fast_field() {
        let mut idx = ram(TokenizerKind::Default);
        // 图 chunk 无 caption → text=""（媒资无文本表示）
        idx.upsert("kb", &chunk("a.pdf", 1, ChunkKind::Image, "", 1))
            .unwrap();
        // 文本 chunk
        idx.upsert(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "data here", 1),
        )
        .unwrap();
        idx.commit().unwrap();
        let img_filter = Filter::Eq(
            "modality".into(),
            fastsearch_core::FieldValue::Str("image".into()),
        );
        // text="" 不产 term：keyword 查 "data" 命中文本 chunk，不命中空文本图
        let kw = idx.search("data", None, None, 10, false).unwrap();
        assert_eq!(kw.len(), 1);
        assert_eq!(kw[0].id.chunk_id, 2);
        // 空查询 + modality=image（fast field 真预过滤）→ 命中空文本图
        let img = idx.search("", Some(&img_filter), None, 10, false).unwrap();
        assert_eq!(img.len(), 1);
        assert_eq!(img[0].id.chunk_id, 1);
        // modality=text → 仅文本 chunk
        let txt_filter = Filter::Eq(
            "modality".into(),
            fastsearch_core::FieldValue::Str("text".into()),
        );
        let txt = idx.search("", Some(&txt_filter), None, 10, false).unwrap();
        assert_eq!(txt.len(), 1);
        assert_eq!(txt[0].id.chunk_id, 2);
    }

    #[test]
    fn acl_blocks_unauthorized() {
        let mut idx = ram(TokenizerKind::Default);
        let mut c1 = chunk("a.pdf", 1, ChunkKind::Paragraph, "secret data", 1);
        c1.tenant = Some("acme".into());
        c1.acl = vec!["team-a".into()];
        let mut c2 = chunk("a.pdf", 2, ChunkKind::Paragraph, "secret data", 1);
        c2.tenant = Some("acme".into());
        c2.acl = vec!["team-b".into()];
        idx.upsert("kb", &c1).unwrap();
        idx.upsert("kb", &c2).unwrap();
        idx.commit().unwrap();
        // 调用者 acme / team-a → 只能看到 chunk 1
        let acl = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let hits = idx.search("secret", None, Some(&acl), 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn upsert_overwrites() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "oldword", 1))
            .unwrap();
        idx.commit().unwrap();
        idx.upsert("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "newword", 1))
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(
            idx.search("oldword", None, None, 10, false).unwrap().len(),
            0
        );
        assert_eq!(
            idx.search("newword", None, None, 10, false).unwrap().len(),
            1
        );
    }

    #[test]
    fn delete_by_doc_removes_all() {
        let mut idx = ram(TokenizerKind::Default);
        idx.upsert("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "term", 1))
            .unwrap();
        idx.upsert("kb", &chunk("a.pdf", 2, ChunkKind::Paragraph, "term", 2))
            .unwrap();
        idx.upsert("kb", &chunk("b.pdf", 1, ChunkKind::Paragraph, "term", 1))
            .unwrap();
        idx.commit().unwrap();
        idx.delete_by_doc("kb", "a.pdf").unwrap();
        idx.commit().unwrap();
        let hits = idx.search("term", None, None, 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.doc_id, "b.pdf");
    }

    #[test]
    fn deterministic_results() {
        let mut idx = ram(TokenizerKind::Default);
        for i in 1..=5 {
            idx.upsert(
                "kb",
                &chunk("a.pdf", i, ChunkKind::Paragraph, "same text", 1),
            )
            .unwrap();
        }
        idx.commit().unwrap();
        let a = idx.search("same", None, None, 10, false).unwrap();
        let b = idx.search("same", None, None, 10, false).unwrap();
        let ga: Vec<_> = a.iter().map(|h| h.id.chunk_id).collect();
        let gb: Vec<_> = b.iter().map(|h| h.id.chunk_id).collect();
        assert_eq!(ga, gb);
    }
}
