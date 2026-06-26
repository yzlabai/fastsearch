//! Tantivy schema 构建与字段句柄。

use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED, STORED, STRING,
};

/// 分词器选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizerKind {
    /// Tantivy 内置 default（英文/unicode + 小写）。
    Default,
    /// 中文 jieba。
    Jieba,
}

impl TokenizerKind {
    pub fn name(self) -> &'static str {
        match self {
            TokenizerKind::Default => "default",
            TokenizerKind::Jieba => "jieba",
        }
    }
}

/// 索引配置。
#[derive(Debug, Clone, Copy)]
pub struct TextIndexConfig {
    pub k1: f32,
    pub b: f32,
    pub tokenizer: TokenizerKind,
    pub heading_boost: f32,
}

impl Default for TextIndexConfig {
    fn default() -> Self {
        TextIndexConfig {
            k1: 1.2,
            b: 0.75,
            tokenizer: TokenizerKind::Default,
            heading_boost: 2.0,
        }
    }
}

/// schema 字段句柄集合。
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub gid: Field,
    pub collection: Field,
    pub doc_id: Field,
    pub text: Field,
    pub heading: Field,
    pub kind: Field,
    pub modality: Field,
    pub page: Field,
    pub section_id: Field,
    pub chunk_id: Field,
    pub tenant: Field,
    pub acl: Field,
    pub heading_path: Field,
    pub bbox: Field,
}

/// 构建 schema + 字段句柄。text/heading 用给定分词器。
pub fn build_schema(tokenizer: TokenizerKind) -> (Schema, Fields) {
    let mut sb = Schema::builder();

    let text_indexing = || {
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer.name())
            .set_index_option(IndexRecordOption::WithFreqsAndPositions)
    };
    // text 额外 STORED：高亮（SnippetGenerator）需要原文。heading 不存（省空间）。
    let text_opts = TextOptions::default()
        .set_indexing_options(text_indexing())
        .set_stored();
    let heading_opts = TextOptions::default().set_indexing_options(text_indexing());

    // 过滤/分面字段额外加 STORED，便于检索后精确 post-filter（core Filter::eval）。
    // TextOptions/NumericOptions 非 Copy，每次取一份。
    let str_fast_stored = || STRING | FAST | STORED;
    let u64_fast_stored = || FAST | INDEXED | STORED;

    let fields = Fields {
        gid: sb.add_text_field("gid", STRING | STORED),
        collection: sb.add_text_field("collection", str_fast_stored()),
        doc_id: sb.add_text_field("doc_id", str_fast_stored()),
        text: sb.add_text_field("text", text_opts),
        heading: sb.add_text_field("heading", heading_opts),
        kind: sb.add_text_field("kind", str_fast_stored()),
        modality: sb.add_text_field("modality", str_fast_stored()),
        page: sb.add_u64_field("page", u64_fast_stored()),
        section_id: sb.add_u64_field("section_id", u64_fast_stored()),
        chunk_id: sb.add_u64_field("chunk_id", STORED),
        tenant: sb.add_text_field("tenant", str_fast_stored()),
        acl: sb.add_text_field("acl", str_fast_stored()), // 多值：add_text 多次
        heading_path: sb.add_text_field("heading_path", STORED),
        bbox: sb.add_text_field("bbox", STORED),
    };

    (sb.build(), fields)
}
