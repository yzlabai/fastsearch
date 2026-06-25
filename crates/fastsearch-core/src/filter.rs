//! 过滤 AST + 求值器。
//!
//! 过滤对"一行的字段取值"求值，通过 [`FieldSource`] 抽象解耦具体存储
//! （Tantivy fast field / Postgres 行 / 测试桩都可实现它）。求值无副作用、
//! 类型不匹配返回 false 而非 panic（健壮性约定）。

use serde::{Deserialize, Serialize};

/// 标量字段值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl FieldValue {
    /// 数值视图：Int/Float 统一成 f64 便于互比；非数值返回 None。
    fn as_f64(&self) -> Option<f64> {
        match self {
            FieldValue::Int(i) => Some(*i as f64),
            FieldValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// 偏序比较：数值之间按数值比；字符串之间按字典序；其余（含类型不匹配）None。
    fn partial_cmp_val(&self, other: &FieldValue) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (FieldValue::Str(a), FieldValue::Str(b)) => Some(a.cmp(b)),
            _ => match (self.as_f64(), other.as_f64()) {
                (Some(a), Some(b)) => a.partial_cmp(&b),
                _ => None,
            },
        }
    }
}

/// 一行的字段来源。`get` 返回标量字段；`heading_path`/`acl` 是多值专用通道。
pub trait FieldSource {
    fn get(&self, field: &str) -> Option<FieldValue>;
    fn heading_path(&self) -> &[String];
    fn acl(&self) -> &[String];
}

/// 过滤表达式（可嵌套布尔）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
    Eq(String, FieldValue),
    Ne(String, FieldValue),
    Gt(String, FieldValue),
    Gte(String, FieldValue),
    Lt(String, FieldValue),
    Lte(String, FieldValue),
    In(String, Vec<FieldValue>),
    Exists(String),
    /// heading_path 以给定序列为前缀。
    HeadingPrefix(Vec<String>),
}

impl Filter {
    /// 对一行求值。类型不匹配/字段缺失 → false（不 panic）。
    pub fn eval(&self, row: &dyn FieldSource) -> bool {
        use std::cmp::Ordering::*;
        match self {
            // 空 And = true（恒真），空 Or = false（恒假）—— 标准布尔单位元。
            Filter::And(fs) => fs.iter().all(|f| f.eval(row)),
            Filter::Or(fs) => fs.iter().any(|f| f.eval(row)),
            Filter::Not(f) => !f.eval(row),
            Filter::Eq(k, v) => row.get(k).as_ref() == Some(v),
            Filter::Ne(k, v) => row.get(k).as_ref() != Some(v),
            Filter::Gt(k, v) => matches!(cmp(row, k, v), Some(Greater)),
            Filter::Gte(k, v) => matches!(cmp(row, k, v), Some(Greater | Equal)),
            Filter::Lt(k, v) => matches!(cmp(row, k, v), Some(Less)),
            Filter::Lte(k, v) => matches!(cmp(row, k, v), Some(Less | Equal)),
            Filter::In(k, vs) => row.get(k).is_some_and(|val| vs.contains(&val)),
            Filter::Exists(k) => row.get(k).is_some(),
            Filter::HeadingPrefix(prefix) => {
                let hp = row.heading_path();
                prefix.len() <= hp.len() && hp[..prefix.len()] == prefix[..]
            }
        }
    }

    /// 把强制 ACL 过滤 AND 进当前过滤（服务端注入用，客户端不可绕过）。
    pub fn and_acl(self, acl_filter: Filter) -> Filter {
        Filter::And(vec![acl_filter, self])
    }
}

fn cmp(row: &dyn FieldSource, field: &str, v: &FieldValue) -> Option<std::cmp::Ordering> {
    row.get(field).and_then(|got| got.partial_cmp_val(v))
}

/// "调用者可见性"判定：tenant 匹配 **且**（acl 含 `public` 或与授权标签有交集）。
///
/// ACL 是多值字段、需"集合相交"语义，与 [`Filter`] 的标量谓词不同，故单列为
/// 一个纯数据谓词。由服务端在过滤期对每行调用 [`AclFilter::visible`]，客户端
/// 无法绕过（强制注入，见产品设计 §3.6 / 需求 F44）。各后端（Tantivy fast
/// field、Postgres WHERE）据此翻译成等价的索引侧过滤。
pub struct AclFilter {
    pub tenant: Option<String>,
    pub allowed_tags: Vec<String>,
}

impl AclFilter {
    /// 对一行判定可见性：tenant 一致（或本行无 tenant 限制时放行需调用方决定，
    /// 这里采用"行 tenant 必须等于调用者 tenant"严格语义）+ 标签相交或 public。
    pub fn visible(&self, row: &dyn FieldSource) -> bool {
        // tenant 维度
        let tenant_ok = match (&self.tenant, row.get("tenant")) {
            (Some(t), Some(FieldValue::Str(rt))) => *t == rt,
            (Some(_), None) => false, // 调用者有 tenant，行无 → 不可见（严格隔离）
            (None, _) => true,        // 调用者无 tenant 限制（如管理员）→ 放行
            _ => false,
        };
        if !tenant_ok {
            return false;
        }
        // 标签维度：public 公开，或调用者授权标签与行 acl 有交集
        let acl = row.acl();
        acl.iter().any(|a| a == "public")
            || acl.iter().any(|a| self.allowed_tags.iter().any(|t| t == a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 测试桩：用 map 当一行。
    struct Row {
        fields: HashMap<String, FieldValue>,
        heading: Vec<String>,
        acl: Vec<String>,
    }
    impl Row {
        fn new() -> Self {
            Row {
                fields: HashMap::new(),
                heading: vec![],
                acl: vec!["public".into()],
            }
        }
        fn with(mut self, k: &str, v: FieldValue) -> Self {
            self.fields.insert(k.into(), v);
            self
        }
    }
    impl FieldSource for Row {
        fn get(&self, field: &str) -> Option<FieldValue> {
            self.fields.get(field).cloned()
        }
        fn heading_path(&self) -> &[String] {
            &self.heading
        }
        fn acl(&self) -> &[String] {
            &self.acl
        }
    }

    #[test]
    fn eq_ne_and_type_mismatch() {
        let row = Row::new().with("kind", FieldValue::Str("table".into()));
        assert!(Filter::Eq("kind".into(), FieldValue::Str("table".into())).eval(&row));
        assert!(Filter::Ne("kind".into(), FieldValue::Str("image".into())).eval(&row));
        // 类型不匹配 → Eq false
        assert!(!Filter::Eq("kind".into(), FieldValue::Int(3)).eval(&row));
        // 字段缺失 → false
        assert!(!Filter::Eq("missing".into(), FieldValue::Int(1)).eval(&row));
    }

    #[test]
    fn numeric_cross_type_compare() {
        let row = Row::new().with("page", FieldValue::Int(10));
        // Int vs Float 互比
        assert!(Filter::Gte("page".into(), FieldValue::Float(9.5)).eval(&row));
        assert!(Filter::Lt("page".into(), FieldValue::Float(10.5)).eval(&row));
        assert!(!Filter::Gt("page".into(), FieldValue::Int(10)).eval(&row));
        assert!(Filter::Gte("page".into(), FieldValue::Int(10)).eval(&row));
        // 与字符串比 → false（不 panic）
        assert!(!Filter::Gt("page".into(), FieldValue::Str("x".into())).eval(&row));
    }

    #[test]
    fn bool_nesting_units() {
        let row = Row::new().with("a", FieldValue::Int(1));
        // 空 And = true，空 Or = false
        assert!(Filter::And(vec![]).eval(&row));
        assert!(!Filter::Or(vec![]).eval(&row));
        let f = Filter::And(vec![
            Filter::Eq("a".into(), FieldValue::Int(1)),
            Filter::Not(Box::new(Filter::Eq("a".into(), FieldValue::Int(2)))),
        ]);
        assert!(f.eval(&row));
        let f2 = Filter::Or(vec![
            Filter::Eq("a".into(), FieldValue::Int(2)),
            Filter::Eq("a".into(), FieldValue::Int(1)),
        ]);
        assert!(f2.eval(&row));
    }

    #[test]
    fn in_exists_heading_prefix() {
        let mut row = Row::new().with("kind", FieldValue::Str("table".into()));
        row.heading = vec!["第3章".into(), "方法".into(), "3.1".into()];
        assert!(Filter::In(
            "kind".into(),
            vec![
                FieldValue::Str("table".into()),
                FieldValue::Str("image".into())
            ]
        )
        .eval(&row));
        assert!(Filter::Exists("kind".into()).eval(&row));
        assert!(!Filter::Exists("nope".into()).eval(&row));
        assert!(Filter::HeadingPrefix(vec!["第3章".into(), "方法".into()]).eval(&row));
        assert!(!Filter::HeadingPrefix(vec!["第4章".into()]).eval(&row));
        // 空前缀恒真
        assert!(Filter::HeadingPrefix(vec![]).eval(&row));
        // 前缀比 heading 长 → false
        assert!(!Filter::HeadingPrefix(vec![
            "第3章".into(),
            "方法".into(),
            "3.1".into(),
            "x".into()
        ])
        .eval(&row));
    }

    #[test]
    fn acl_filter_enforces_visibility() {
        // 调用者：tenant=acme，授权标签 [team-a]
        let acl = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };

        // 公开行（默认 public）→ 但 tenant 不符则不可见
        let mut pub_row = Row::new();
        pub_row
            .fields
            .insert("tenant".into(), FieldValue::Str("acme".into()));
        assert!(acl.visible(&pub_row)); // public + tenant 匹配

        // 私有行：tenant=acme，acl=[team-a] → 可见
        let mut ok = Row::new();
        ok.acl = vec!["team-a".into()];
        ok.fields
            .insert("tenant".into(), FieldValue::Str("acme".into()));
        assert!(acl.visible(&ok));

        // 越权行：tenant=acme，acl=[team-b] → 不可见
        let mut deny = Row::new();
        deny.acl = vec!["team-b".into()];
        deny.fields
            .insert("tenant".into(), FieldValue::Str("acme".into()));
        assert!(!acl.visible(&deny));

        // 跨租户：tenant=other → 不可见
        let mut other = Row::new();
        other.acl = vec!["public".into()];
        other
            .fields
            .insert("tenant".into(), FieldValue::Str("other".into()));
        assert!(!acl.visible(&other));
    }
}
