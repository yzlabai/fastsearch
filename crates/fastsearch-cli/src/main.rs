//! fastsearch CLI 入口（四张脸之一）。逻辑在 lib，本文件只做命令解析 + I/O。

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use fastsearch_cli::{cmd_eval, cmd_index, cmd_search, EvalOpts, IndexOpts, SearchOpts};
use fastsearch_core::SearchMode;
use fastsearch_text::TokenizerKind;
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "fastsearch",
    version,
    about = "混合检索引擎 CLI（以 Postgres 为真源；本 CLI 演示落盘全文检索）"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 解析 PDF（in-process docparse）→ 适配 → 索引（需 `--features parse`）。
    #[cfg(feature = "parse")]
    Ingest {
        /// 待解析的 PDF 路径。
        pdf: PathBuf,
        #[arg(long)]
        data: PathBuf,
        #[arg(long, default_value = "default")]
        collection: String,
        #[arg(long)]
        doc_id: String,
        #[arg(long, value_enum, default_value_t = Tok::Jieba)]
        tokenizer: Tok,
        #[arg(long)]
        tenant: Option<String>,
    },
    /// 灌入 docparse chunks（JSON 数组或 NDJSON；省略 INPUT 读 stdin）。
    Index {
        #[arg(long)]
        data: PathBuf,
        #[arg(long, default_value = "default")]
        collection: String,
        #[arg(long)]
        doc_id: String,
        #[arg(long, value_enum, default_value_t = Tok::Jieba)]
        tokenizer: Tok,
        /// 输入文件；省略或 `-` 读 stdin。
        input: Option<PathBuf>,
    },
    /// 检索（落盘 keyword）。
    Search {
        #[arg(long)]
        data: PathBuf,
        #[arg(long, default_value = "default")]
        collection: String,
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        page_min: Option<u32>,
        #[arg(long)]
        page_max: Option<u32>,
        /// 以 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 相关性评测：对 golden 集跑检索算 nDCG/recall/MRR/precision；给 --baseline 则做回归门禁。
    Eval {
        /// golden 集 JSON 路径。
        #[arg(long)]
        golden: PathBuf,
        /// baseline 指标 JSON；给定则掉点超容差时以非零退出。
        #[arg(long)]
        baseline: Option<PathBuf>,
        #[arg(long, default_value_t = 0.02)]
        tol: f64,
        #[arg(long, default_value_t = 10)]
        k: usize,
        #[arg(long, value_enum, default_value_t = Tok::Jieba)]
        tokenizer: Tok,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Tok {
    Default,
    Jieba,
}

impl From<Tok> for TokenizerKind {
    fn from(t: Tok) -> Self {
        match t {
            Tok::Default => TokenizerKind::Default,
            Tok::Jieba => TokenizerKind::Jieba,
        }
    }
}

fn read_input(input: &Option<PathBuf>) -> Result<Vec<u8>> {
    match input {
        Some(p) if p.as_os_str() != "-" => {
            std::fs::read(p).with_context(|| format!("reading {}", p.display()))
        }
        _ => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        #[cfg(feature = "parse")]
        Command::Ingest {
            pdf,
            data,
            collection,
            doc_id,
            tokenizer,
            tenant,
        } => {
            let opts = fastsearch_cli::ingest::IngestOpts {
                pdf,
                data,
                collection,
                doc_id,
                tokenizer: tokenizer.into(),
                tenant,
                acl: vec!["public".to_string()],
            };
            let n = fastsearch_cli::ingest::cmd_ingest(&opts)?;
            eprintln!("ingested {n} chunk(s) from pdf for doc '{}'", opts.doc_id);
        }
        Command::Index {
            data,
            collection,
            doc_id,
            tokenizer,
            input,
        } => {
            let bytes = read_input(&input)?;
            let opts = IndexOpts {
                data,
                collection,
                doc_id,
                tokenizer: tokenizer.into(),
            };
            let n = cmd_index(&opts, &bytes)?;
            eprintln!("indexed {n} chunk(s) for doc '{}'", opts.doc_id);
        }
        Command::Search {
            data,
            collection,
            query,
            top_k,
            kind,
            page_min,
            page_max,
            json,
        } => {
            let opts = SearchOpts {
                data,
                collection,
                query,
                top_k,
                kind,
                page_min,
                page_max,
            };
            let hits = cmd_search(&opts)?;
            if json {
                let arr: Vec<_> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "citation_id": h.citation.citation_id(),
                            "score": h.score,
                            "page": h.citation.page,
                            "bbox": h.citation.bbox,
                            "heading_path": h.citation.heading_path,
                            "doc_id": h.id.doc_id,
                            "chunk_id": h.id.chunk_id,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                for (i, h) in hits.iter().enumerate() {
                    println!(
                        "{:>2}. [{:.4}] {} p{} {}",
                        i + 1,
                        h.score,
                        h.citation.citation_id(),
                        h.citation.page,
                        h.citation.heading_path.join(" › ")
                    );
                }
                if hits.is_empty() {
                    eprintln!("(no hits)");
                }
            }
        }
        Command::Eval {
            golden,
            baseline,
            tol,
            k,
            tokenizer,
        } => {
            let opts = EvalOpts {
                golden,
                baseline,
                tol,
                k,
                tokenizer: tokenizer.into(),
                mode: SearchMode::Keyword,
            };
            let (m, gate) = cmd_eval(&opts)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
            if let Some(res) = gate {
                match res {
                    Ok(()) => eprintln!("gate: OK (no regression within tol)"),
                    Err(e) => {
                        eprintln!("gate: FAIL — {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
    Ok(())
}
