//! fastsearch CLI 入口（四张脸之一）——server 的**纯 REST 客户端**。
//! 逻辑在 lib，本文件只做命令解析 + I/O。检索/嵌入/落盘归 server。

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use fastsearch_cli::{
    cmd_eval, cmd_index, cmd_index_dir, cmd_search, cmd_similar, EvalOpts, IndexDirOpts, IndexOpts,
    SearchOpts, SimilarOpts,
};
use fastsearch_core::SearchMode;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "fastsearch",
    version,
    about = "混合检索引擎 CLI（server 的 REST 客户端；检索/嵌入/落盘归 server）"
)]
struct Cli {
    /// server 基址（默认 env FASTSEARCH_SERVER 或 http://localhost:8642）。
    #[arg(long, global = true)]
    server: Option<String>,
    /// API Key（默认 env FASTSEARCH_KEY）。作 `Authorization: Bearer`。
    #[arg(long, global = true)]
    key: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 解析文件（客户端 docparse 多格式：PDF/DOCX/HTML/MD/CSV/XLSX/PPTX/SRT/EML）→ POST /v1/index
    /// （需 `--features parse`）。
    #[cfg(feature = "parse")]
    Ingest {
        /// 待解析文件路径（按扩展名自动选解析器）。
        file: PathBuf,
        #[arg(long, default_value = "default")]
        collection: String,
        #[arg(long)]
        doc_id: String,
        #[arg(long)]
        tenant: Option<String>,
    },
    /// 灌入 docparse chunks（JSON 数组或 NDJSON；省略 INPUT 读 stdin）→ POST /v1/index。
    Index {
        #[arg(long, default_value = "default")]
        collection: String,
        #[arg(long)]
        doc_id: String,
        /// 输入文件；省略或 `-` 读 stdin。
        input: Option<PathBuf>,
    },
    /// 喂一个文件夹：递归 .md/.txt（每文件一 doc，doc_id=相对路径）客户端分块 → POST /v1/index。
    IndexDir {
        #[arg(long, default_value = "default")]
        collection: String,
        /// 并发上传文件数（大文件夹提速）。
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// 资料文件夹路径。
        dir: PathBuf,
    },
    /// 检索（POST /v1/search）。默认 hybrid（server 有嵌入器则混合，否则自动退化关键词）。
    Search {
        #[arg(long, default_value = "default")]
        collection: String,
        #[arg(long)]
        query: String,
        #[arg(long, value_enum, default_value_t = Mode::Hybrid)]
        mode: Mode,
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
    /// more_like_this：按 citation_id 反查相似（POST /v1/similar）。
    Similar {
        #[arg(long)]
        citation_id: String,
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        #[arg(long)]
        json: bool,
    },
    /// 相关性评测：golden 语料灌入其 collection → 逐查询经 server 检索算 nDCG/recall/MRR；
    /// 给 --baseline 则做回归门禁。**注**：会把 golden 语料写入目标 server（用专用/临时集合）。
    Eval {
        #[arg(long)]
        golden: PathBuf,
        #[arg(long)]
        baseline: Option<PathBuf>,
        #[arg(long, default_value_t = 0.02)]
        tol: f64,
        #[arg(long, default_value_t = 10)]
        k: usize,
        #[arg(long, value_enum, default_value_t = Mode::Keyword)]
        mode: Mode,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Keyword,
    Vector,
    Hybrid,
}

impl From<Mode> for SearchMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Keyword => SearchMode::Keyword,
            Mode::Vector => SearchMode::Vector,
            Mode::Hybrid => SearchMode::Hybrid,
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

/// 打印命中（人读 / `--json` 原样透传 server 字段，便于脚本/agent）。
fn print_hits(hits: &[Value], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(hits).unwrap_or_else(|_| "[]".into())
        );
        return;
    }
    for (i, h) in hits.iter().enumerate() {
        let cid = h.get("citation_id").and_then(|v| v.as_str()).unwrap_or("?");
        let score = h.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let page = h.get("page").and_then(|v| v.as_u64()).unwrap_or(0);
        let hp = h
            .get("heading_path")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" › ")
            })
            .unwrap_or_default();
        println!("{:>2}. [{score:.4}] {cid} p{page} {hp}", i + 1);
    }
    if hits.is_empty() {
        eprintln!("(no hits)");
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (server, key) = (cli.server, cli.key);
    match cli.command {
        #[cfg(feature = "parse")]
        Command::Ingest {
            file,
            collection,
            doc_id,
            tenant,
        } => {
            let opts = fastsearch_cli::ingest::IngestOpts {
                file,
                server,
                key,
                collection,
                doc_id,
                tenant,
                acl: vec!["public".to_string()],
            };
            let n = fastsearch_cli::ingest::cmd_ingest(&opts)?;
            eprintln!("indexed {n} chunk(s) for doc '{}'", opts.doc_id);
        }
        Command::Index {
            collection,
            doc_id,
            input,
        } => {
            let bytes = read_input(&input)?;
            let opts = IndexOpts {
                server,
                key,
                collection,
                doc_id,
            };
            let n = cmd_index(&opts, &bytes)?;
            eprintln!("indexed {n} chunk(s) for doc '{}'", opts.doc_id);
        }
        Command::IndexDir {
            collection,
            concurrency,
            dir,
        } => {
            let opts = IndexDirOpts {
                server,
                key,
                collection,
                concurrency,
            };
            let (ok, failed, chunks) = cmd_index_dir(&opts, &dir)?;
            eprintln!(
                "indexed {ok} file(s){}, {chunks} chunk(s) from {}",
                if failed > 0 {
                    format!("（{failed} 失败）")
                } else {
                    String::new()
                },
                dir.display()
            );
            if failed > 0 {
                std::process::exit(1);
            }
        }
        Command::Search {
            collection,
            query,
            mode,
            top_k,
            kind,
            page_min,
            page_max,
            json,
        } => {
            let opts = SearchOpts {
                server,
                key,
                collection,
                query,
                mode: mode.into(),
                top_k,
                kind,
                page_min,
                page_max,
            };
            let hits = cmd_search(&opts)?;
            print_hits(&hits, json);
        }
        Command::Similar {
            citation_id,
            top_k,
            json,
        } => {
            let opts = SimilarOpts {
                server,
                key,
                citation_id,
                top_k,
            };
            let hits = cmd_similar(&opts)?;
            print_hits(&hits, json);
        }
        Command::Eval {
            golden,
            baseline,
            tol,
            k,
            mode,
        } => {
            let opts = EvalOpts {
                server,
                key,
                golden,
                baseline,
                tol,
                k,
                mode: mode.into(),
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
