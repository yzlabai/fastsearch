# 2026-06-11 · G6 生态接入:Python 薄客户端 + LangChain/LlamaIndex 适配器

## TL;DR

`clients/python/` 落地 `docparse-client` 包:**零运行时依赖**——`DocparseClient` 子进程包 CLI、`DocparseHttpClient` 用 urllib 包 REST(手写 multipart),两传输同形输出(json/chunks 返回解码对象,markdown/text 返回 str)。LangChain `DocparseLoader` 与 LlamaIndex `DocparseReader` 各 ~60 行,宿主惰性导入(`pip install docparse-client[langchain]`),每 chunk 一个 Document、metadata 带 `page`/`bbox`/`heading_path`/`kind`——RAG 引用回源所需、多数 loader 丢掉的那部分。验收("LangChain 五行代码加载 PDF 带引用 metadata")经真 langchain-core venv 实测通过。PyPI/crates.io 发布候账号决策。

## 设计

- **零依赖是身份的延伸**:Rust 侧"单二进制零依赖"的卖点不能在 Python 侧被 requests/pydantic 破掉——subprocess + urllib 足够,连测试都 stdlib-only(unittest)。
- **两传输一接口**:`parse(path, format, ocr)` / `chunks(path)` 在子进程与 REST 间同形;loader/reader 构造时二选一(`binary=` / `url=`),框架层不感知传输。
- **惰性导入宿主**:基础包不携带 langchain/llamaindex;缺宿主时报清晰 ImportError(测试锁定该行为),装了 extras 即得真 Document。
- 二进制解析顺序:显式参数 > `$DOCPARSE_BIN` > `$PATH`。

## 验证

- stdlib 测试 5/5:json/chunks/text 解析、错误干净(DocparseError 带 stderr)、未知格式拒绝、loader 缺宿主行为、**REST e2e**(测试自启 `docparse serve` 于随机端口,HTTP chunks 与子进程 chunks 全等——四接口逐字节一致的 Python 侧佐证);
- 真 langchain-core(一次性 venv):OTSL pg9 PDF → 6 个 `langchain_core.documents.Document`,metadata `page: 1`、bbox 坐标齐全。

## 边界与余项

- LlamaIndex 适配器与 LangChain 同构但未装真宿主实测(llama-index-core 依赖树大)——结构风险低,留待真实使用回填;
- PyPI 发布、crates.io workspace 发布、MCP registry 收录:候用户账号/署名决策;
- HTTP 客户端不做重试/流式(薄客户端定位);大文件经 REST 受服务端 multipart 限额约束(N5b)。
