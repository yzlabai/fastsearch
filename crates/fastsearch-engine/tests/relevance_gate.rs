//! 相关性回归门禁（F39 闭环）：固定 golden 集 + 提交的 baseline 指标，跑真实检索算当前
//! 指标，掉点超过容差则失败。keyword 模式确定性、零重依赖，适合 CI。

use fastsearch_core::SearchMode;
use fastsearch_engine::golden;
use fastsearch_eval::{assert_no_regression, GoldenSet, Metrics};
use fastsearch_text::{TextIndexConfig, TokenizerKind};

const GOLDEN: &str = include_str!("golden/zh_finance.json");
const BASELINE: &str = include_str!("golden/zh_finance.baseline.json");
const MM_GOLDEN: &str = include_str!("golden/multimodal.json");
const MM_BASELINE: &str = include_str!("golden/multimodal.baseline.json");
const K: usize = 5;
const TOL: f64 = 0.02;

fn zh_cfg() -> TextIndexConfig {
    TextIndexConfig {
        tokenizer: TokenizerKind::Jieba,
        ..Default::default()
    }
}

/// 重算并打印当前指标，用于刷新 baseline（改了语料/排序后跑
/// `cargo test -p fastsearch-engine --test relevance_gate -- --ignored --nocapture`，
/// 把打印的 JSON 写回 `zh_finance.baseline.json`）。默认 `#[ignore]`，不进 CI。
#[ignore]
#[test]
fn probe_print_metrics() {
    let set = GoldenSet::from_json(GOLDEN).unwrap();
    let m = golden::run(&set, zh_cfg(), SearchMode::Keyword, K).unwrap();
    println!("PROBE_METRICS {}", serde_json::to_string(&m).unwrap());
    // 多模态 golden（图 caption / 音视频转录召回）
    let mm = GoldenSet::from_json(MM_GOLDEN).unwrap();
    let mmm = golden::run(&mm, zh_cfg(), SearchMode::Keyword, K).unwrap();
    println!("PROBE_MM_METRICS {}", serde_json::to_string(&mmm).unwrap());
}

/// 多模态相关性门禁：图 caption / 音视频转录经 keyword（M0 派生文本路线）可召回，nDCG/recall
/// 不掉点。这是 MM7——把多模态召回纳入 CI 回归护栏。
#[test]
fn multimodal_no_regression() {
    let set = GoldenSet::from_json(MM_GOLDEN).unwrap();
    let current = golden::run(&set, zh_cfg(), SearchMode::Keyword, K).unwrap();
    let baseline: Metrics = serde_json::from_str(MM_BASELINE).unwrap();
    assert_no_regression(&baseline, &current, TOL)
        .unwrap_or_else(|e| panic!("multimodal relevance regression: {e}\ncurrent={current:?}"));
}

#[test]
fn no_regression_against_baseline() {
    let set = GoldenSet::from_json(GOLDEN).unwrap();
    let current = golden::run(&set, zh_cfg(), SearchMode::Keyword, K).unwrap();
    let baseline: Metrics = serde_json::from_str(BASELINE).unwrap();
    assert_no_regression(&baseline, &current, TOL)
        .unwrap_or_else(|e| panic!("relevance regression: {e}\ncurrent={current:?}"));
}
