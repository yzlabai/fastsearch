//! core `Filter`/`AclFilter` ↔ Tantivy 查询的映射，以及检索后精确 post-filter。
//!
//! 策略（兼顾召回与精确，呼应"真预过滤"目标）：
//! - **预过滤**：把过滤翻译成一个 **SUPERSET** Tantivy 查询（不可翻译的子谓词→
//!   match-all，保证不漏召回），用于在索引侧先缩小候选。
//! - **后过滤**：对取回的文档用 core `Filter::eval` + `AclFilter::visible` 做
//!   **精确**判定（保证不越权、不误纳）。over-fetch 抵消后过滤的截断。

use fastsearch_core::{AclFilter, BBox, FieldSource, FieldValue, Filter};
use std::ops::Bound;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, RangeQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use crate::schema::Fields;

fn term_str(field: tantivy::schema::Field, v: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, v),
        IndexRecordOption::Basic,
    ))
}

fn u64_range(field: tantivy::schema::Field, lo: Bound<u64>, hi: Bound<u64>) -> Box<dyn Query> {
    let map = |b: Bound<u64>| match b {
        Bound::Included(v) => Bound::Included(Term::from_field_u64(field, v)),
        Bound::Excluded(v) => Bound::Excluded(Term::from_field_u64(field, v)),
        Bound::Unbounded => Bound::Unbounded,
    };
    Box::new(RangeQuery::new(map(lo), map(hi)))
}

fn field_as_u64(v: &FieldValue) -> Option<u64> {
    match v {
        FieldValue::Int(i) if *i >= 0 => Some(*i as u64),
        FieldValue::Float(f) if *f >= 0.0 => Some(*f as u64),
        _ => None,
    }
}

/// 把 core Filter 翻译成 SUPERSET Tantivy 查询。不可翻译→AllQuery（不约束）。
pub fn translate(filter: &Filter, f: &Fields) -> Box<dyn Query> {
    match filter {
        Filter::And(subs) => {
            let clauses = subs
                .iter()
                .map(|s| (Occur::Must, translate(s, f)))
                .collect();
            Box::new(BooleanQuery::new(clauses))
        }
        Filter::Or(subs) => {
            let clauses = subs
                .iter()
                .map(|s| (Occur::Should, translate(s, f)))
                .collect();
            Box::new(BooleanQuery::new(clauses))
        }
        // Not/Ne/Exists/HeadingPrefix：保守不约束，交给后过滤精确判定。
        Filter::Not(_) | Filter::Ne(_, _) | Filter::Exists(_) | Filter::HeadingPrefix(_) => {
            Box::new(AllQuery)
        }
        Filter::Eq(field, val) => eq_query(field, val, f),
        Filter::In(field, vals) => {
            let clauses = vals
                .iter()
                .map(|v| (Occur::Should, eq_query(field, v, f)))
                .collect();
            Box::new(BooleanQuery::new(clauses))
        }
        Filter::Gt(field, val) => {
            range_query(field, val, f, |v| (Bound::Excluded(v), Bound::Unbounded))
        }
        Filter::Gte(field, val) => {
            range_query(field, val, f, |v| (Bound::Included(v), Bound::Unbounded))
        }
        Filter::Lt(field, val) => {
            range_query(field, val, f, |v| (Bound::Unbounded, Bound::Excluded(v)))
        }
        Filter::Lte(field, val) => {
            range_query(field, val, f, |v| (Bound::Unbounded, Bound::Included(v)))
        }
    }
}

fn eq_query(field: &str, val: &FieldValue, f: &Fields) -> Box<dyn Query> {
    match (field, val) {
        ("kind", FieldValue::Str(s)) => term_str(f.kind, s),
        ("doc_id", FieldValue::Str(s)) => term_str(f.doc_id, s),
        ("collection", FieldValue::Str(s)) => term_str(f.collection, s),
        ("tenant", FieldValue::Str(s)) => term_str(f.tenant, s),
        ("page", v) => match field_as_u64(v) {
            Some(n) => u64_range(f.page, Bound::Included(n), Bound::Included(n)),
            None => Box::new(AllQuery),
        },
        ("section_id", v) => match field_as_u64(v) {
            Some(n) => u64_range(f.section_id, Bound::Included(n), Bound::Included(n)),
            None => Box::new(AllQuery),
        },
        _ => Box::new(AllQuery),
    }
}

fn range_query(
    field: &str,
    val: &FieldValue,
    f: &Fields,
    mk: impl Fn(u64) -> (Bound<u64>, Bound<u64>),
) -> Box<dyn Query> {
    let tf = match field {
        "page" => f.page,
        "section_id" => f.section_id,
        _ => return Box::new(AllQuery),
    };
    match field_as_u64(val) {
        Some(n) => {
            let (lo, hi) = mk(n);
            u64_range(tf, lo, hi)
        }
        None => Box::new(AllQuery),
    }
}

/// ACL 预过滤查询：tenant 命中（若调用者有 tenant）+ acl 命中（public 或授权标签之一）。
pub fn acl_query(acl: &AclFilter, f: &Fields) -> Box<dyn Query> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![];
    if let Some(t) = &acl.tenant {
        clauses.push((Occur::Must, term_str(f.tenant, t)));
    }
    // acl ∈ ({public} ∪ allowed_tags)
    let mut acl_should: Vec<(Occur, Box<dyn Query>)> =
        vec![(Occur::Should, term_str(f.acl, "public"))];
    for tag in &acl.allowed_tags {
        acl_should.push((Occur::Should, term_str(f.acl, tag)));
    }
    clauses.push((Occur::Must, Box::new(BooleanQuery::new(acl_should))));
    Box::new(BooleanQuery::new(clauses))
}

/// 取回文档的字段视图，供 core `Filter::eval` / `AclFilter::visible` 精确后过滤，
/// 并携带组装 citation 所需的全部字段。
pub struct StoredRow {
    pub kind: String,
    pub doc_id: String,
    pub collection: String,
    pub tenant: Option<String>,
    pub page: u64,
    pub section_id: u64,
    pub chunk_id: u64,
    pub bbox: BBox,
    pub heading: Vec<String>,
    pub acl: Vec<String>,
}

impl FieldSource for StoredRow {
    fn get(&self, field: &str) -> Option<FieldValue> {
        match field {
            "kind" => Some(FieldValue::Str(self.kind.clone())),
            "doc_id" => Some(FieldValue::Str(self.doc_id.clone())),
            "collection" => Some(FieldValue::Str(self.collection.clone())),
            "tenant" => self.tenant.clone().map(FieldValue::Str),
            "page" => Some(FieldValue::Int(self.page as i64)),
            "section_id" => Some(FieldValue::Int(self.section_id as i64)),
            _ => None,
        }
    }
    fn heading_path(&self) -> &[String] {
        &self.heading
    }
    fn acl(&self) -> &[String] {
        &self.acl
    }
}

fn first_str(doc: &TantivyDocument, field: tantivy::schema::Field) -> Option<String> {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn first_u64(doc: &TantivyDocument, field: tantivy::schema::Field) -> u64 {
    doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// 从取回文档构造 [`StoredRow`]。
pub fn stored_row(doc: &TantivyDocument, f: &Fields) -> StoredRow {
    let acl: Vec<String> = doc
        .get_all(f.acl)
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    let heading: Vec<String> = first_str(doc, f.heading_path)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let bbox: BBox = first_str(doc, f.bbox)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or(BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 0.0,
            y1: 0.0,
        });
    StoredRow {
        kind: first_str(doc, f.kind).unwrap_or_default(),
        doc_id: first_str(doc, f.doc_id).unwrap_or_default(),
        collection: first_str(doc, f.collection).unwrap_or_default(),
        tenant: first_str(doc, f.tenant),
        page: first_u64(doc, f.page),
        section_id: first_u64(doc, f.section_id),
        chunk_id: first_u64(doc, f.chunk_id),
        bbox,
        heading,
        acl,
    }
}
