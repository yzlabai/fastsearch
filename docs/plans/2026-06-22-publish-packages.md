# 发布到 GitHub Release / crates.io / PyPI / npm / MCP-registry

> 计划稿（复杂需求，按 CLAUDE.md §0 复杂路径写）。原始一句话需求："PyPI/crates.io/MCP-registry 发布"，本轮**补充 npm 渠道**，并把"先有二进制再有 client"的发布拓扑理清。
>
> 状态：**已评审，按推荐执行**（2026-06-22）。crates.io × tract 取舍定为 **§3.2.1 (A) 带 caveat 发布 + PPV2 缺补丁护栏**。落地后过程与结果回写本篇下方"实施记录"。

## 0. 原始需求

把 docparse-rs 发布到公共包管理器，让用户不必 `git clone` 即可安装使用：

- **crates.io** —— Rust 生态 `cargo install` / 库依赖
- **PyPI** —— Python 生态 `pip install`
- **MCP-registry** —— agent 通过官方 MCP 注册表发现并接入
- **npm**（本轮补充）—— Node/TS 生态 `npm install`

## 1. 需求三件套

**要解决什么**：当前唯一分发渠道是 GitHub（`cargo install --git` 或 README 里的 `curl | sh`，依赖 cargo-dist 产出的 Release 二进制）。git 标签从未打过（`git tag` 为空），即 **prebuilt 二进制其实也还没真正发布过一次**。各语言的 client 包（`clients/python`、`clients/typescript`）代码已就绪、版本都是 `0.1.0`，但从未 publish。MCP server（`docparse mcp`）能跑，但没有进官方注册表。

**给谁用**：
- Rust 开发者 / CLI 用户 → crates.io、Release 二进制
- Python RAG/LangChain/LlamaIndex 用户 → PyPI 的 `docparse-client`
- Node/TS（LangChain.js、Vercel AI SDK）用户 → npm 的 `docparse-client`
- Agent / MCP host → MCP registry

**成功长这样**：在干净机器上，下面任一条命令可用且行为正确（见 §6 测试用例）：

```bash
cargo install docparse-cli                 # crates.io
pip install docparse-client                # PyPI
npm install docparse-client                # npm
curl -LsSf .../docparse-cli-installer.sh | sh   # GitHub Release（已有 workflow，缺打标签）
```

MCP host 能在 registry.modelcontextprotocol.io 搜到 `io.github.yzlabai/docparse` 并一键接入。

## 2. 发布拓扑（关键认知：二进制是地基）

```
        ┌─────────────────────────────────────────────┐
        │  GitHub Release（cargo-dist，已 wired）        │  ← 地基：prebuilt `docparse` 二进制
        │  curl|sh / .ps1 installer + checksums         │     4 targets, tag `v*` 触发
        └───────────────┬─────────────────────────────┘
                        │ client 包在 PATH 上调用 `docparse` 二进制（subprocess）
        ┌───────────────┴───────────────┬───────────────────┐
   ┌────▼─────┐                    ┌─────▼──────┐      ┌──────▼──────┐
   │ crates.io│                    │   PyPI     │      │     npm     │
   │ docparse-*│                   │docparse-   │      │docparse-    │
   │ (源码/库) │                    │client(thin)│      │client(thin) │
   └──────────┘                    └────────────┘      └─────────────┘
   ┌──────────────────────────────────────────────────────────────┐
   │  MCP registry：server.json 指向 `docparse mcp`（依赖二进制在 PATH）│
   └──────────────────────────────────────────────────────────────┘
```

**核心结论**：`clients/python`、`clients/typescript` 都是 **thin client**——subprocess 包裹 `docparse` 二进制，或 HTTP 调 `docparse serve`，**零运行时依赖**（见两个 manifest 的注释）。它们 *不* 内嵌 Rust，**装了 client 仍需 `docparse` 二进制在 PATH**。因此发布顺序天然是：**先打二进制 Release → 再发各 client**。这是必须在文档和 README 里讲清的一条用户认知，否则 `pip install docparse-client` 后"找不到 docparse"会被当 bug。

> **范围内的可选增强（§7 列为后续）**：让 client 在 `postinstall` / 首次运行时按平台自动下载对应 Release 二进制（esbuild / ruff 的套路），消除"需手动装二进制"的台阶。本轮**不做**，先把"client + 手动二进制"链路跑通。

## 3. 各渠道现状与阻塞点

### 3.1 GitHub Release（地基，几乎就绪）
- `.github/workflows/release.yml`（cargo-dist 0.32 生成）已存在：push 形如 `v0.1.0` 的 tag → 4 个 target 编译 → 建 GitHub Release + `curl|sh`/`.ps1` installer + checksums。CI 从源码构建，**vendored tract patch 正常生效**（PPV2 可用）。
- 阻塞：**从未打过 tag**，所以 README 里 `releases/latest/download/...` 链接目前 404。
- 待办：打第一个 `v0.1.0` tag（或先 dry-run 验证 workflow）。

### 3.2 crates.io（最大阻塞在 vendored tract patch）
17 个 crate，version 全 `0.1.0`，license `Apache-2.0`。阻塞三项：

1. **path 依赖**：crate 间全是 `{ path = "../docparse-xxx" }`，cargo publish 要求 path 依赖**同时带 `version`** 才能发布（否则 crates.io 拿不到版本）。需给每个内部依赖补 `version = "0.1.0"`（path + version 并存，本地走 path、发布走 version）。
2. **元数据不齐**：`docparse-core`、`docparse-ocr` 等缺 `repository`，crates.io 强烈建议每 crate 有 `description`+`repository`+`license`（license 已由 workspace 继承）。需逐 crate 补 `repository.workspace = true`、`readme`、`keywords`、`categories`。
3. **⚠️ vendored tract patch —— crates.io 的核心取舍**：详见下方 **§3.2.1**（本计划最需要决策的一点）。
4. **发布顺序**：crates.io 要求被依赖者先发。拓扑序大致 `core → 各叶子格式 crate / pdf → raster → ocr → vlm/...→ cli`。用 `cargo publish -p` 按序，或引入 `release-plz` / `cargo workspaces publish` 自动定序。

### 3.2.1 crates.io × vendored tract patch（详细取舍）

这是整个发布里**唯一需要产品级决策**的点，单列详述。

**背景（已核对 [vendor/README.md](../../vendor/README.md) §3 + CLAUDE.md §4）**：
根 `Cargo.toml` 的 `[patch.crates-io]` 把 `tract-hir 0.23.1` / `tract-core 0.23.1` 解析到 `vendor/` 的本地副本，各带一处最小修复——
(1) `tract-hir` GatherNd 形状推断，(2) `tract-core` TopK 接受 TDim 输入。两处**只为 PP-DocLayoutV2（RT-DETR）版面后端**跑通而存在。

**问题的技术本质**（必须讲清，否则容易误判可发性）：

- **`[patch.crates-io]` 是 manifest-local 的，不随 crate 发布、下游消费者完全忽略它。** 它只在"以本 workspace 为根"的构建里生效。
- 谁拉 tract？经核对 Cargo.toml：**仅 `docparse-ocr`**（`tract-onnx` → 间接 `tract-hir`/`tract-core`）。`docparse-raster` 只依赖 `hayro`，不碰 tract。`docparse-cli` 经由 `docparse-ocr` 间接拉入。
- 于是：一旦 `docparse-ocr` / `docparse-cli` 发到 crates.io，**下游 `cargo install docparse-cli` 或 `docparse-ocr = "0.1"` 会解析到 crates.io 上未打补丁的上游 tract**——补丁不会跟过去。

**影响面到底有多大（关键，决定了能否带病发布）**：

- 补丁是**运行期两个算子的行为修复，不是编译修复**。vendored 是完整副本+改动，缺了它**仍能编译**——只是 PPV2 在 eval 阶段出错/产错形状。所以 `cargo install docparse-cli` 从 crates.io **能装、能编译、能跑**。
- vendor/README.md §3 已证明：现役 PP-OCR / UniRec / SLANet / TATR **都不用** GatherNd/TopK 这两条路径；DocLayout-YOLO 的 TopK 走非-TDim 原路径。**因此未打补丁时，唯一坏掉的是 PP-DocLayoutV2 后端**（`--layout-model .../PP-DoclayoutV2_*.onnx`）；默认 DocLayout-YOLO 版面、OCR、表/公式/转写、全部基础解析 **均不受影响**。
- 对照其它渠道：**cargo-dist 的 prebuilt 二进制 与 `cargo install --git` 都从本 workspace 源码构建，`[patch]` 正常生效 → PPV2 完整可用**。即"功能完整版"始终经由 Release 二进制 / git 安装提供。

**三条出路（含真实代价）**：

- **(A) 带 caveat 发布（推荐 v0.1.0）**
  照常发布全部可发 crate（含 ocr/cli）。文档明确写一句：
  > 经 crates.io 安装的 docparse 支持除 **PP-DocLayoutV2 版面后端**外的全部能力；该后端需 prebuilt 二进制（`curl|sh`）或源码安装（`cargo install --git …`），原因是其依赖一处尚未进上游 tract 的修复（见 vendor/README.md）。
  - 代价：**零额外工程**。唯一风险：未打补丁时 PPV2 若**静默产错**（而非显式报错）会误导用户。
  - **必做的护栏**：在 `layout.rs` 选到 PPV2 后端而补丁缺失/输出异常时**显式报错并提示改用 Release 二进制**，杜绝静默错版面。这条无论选哪个方案都该加（low-cost 健壮性）。

- **(B) 改名 fork 整棵 tract 子树发 crates.io（彻底但重）**
  让 crates.io 消费者也拿到修复 = 必须把带补丁的 tract 发上 crates.io。但**不能重发 `tract-hir`/`tract-core` 0.23.1**（版本号归上游所有），只能改名（如 `docparse-tract-hir`）。
  - **真实代价远不止两个 crate**：上游 `tract-onnx`（crates.io）按名硬依赖 `tract-hir`，而 `[patch]` 无法注入到"已发布的 tract-onnx"里。要让 `docparse-ocr` 用上改名后的 tract-hir，就得**连 `tract-onnx` 一起 fork 改名**（`docparse-tract-onnx` → `docparse-tract-hir` → `docparse-tract-core`，可能还牵连 `tract-nnef`/`tract-pulse` 等）。即**fork 一整棵子树并长期维护**。
  - 直接**违背 2026-06-15"vendored 长期留 main、不发上游、代价有界"的决策**，把"有界的 vendored 维护"换成"无界的 fork 维护"。**不推荐**，除非社区明确需要"crates.io 上的 PPV2"。

- **(C) crates.io 只发"纯" crate（保守）**
  只发不依赖 tract 的库：`docparse-core` + `docparse-pdf` + 各确定性格式 crate（docx/html/xlsx/pptx/md/csv/srt/tex/eml/img/adoc）。**`docparse-ocr` / `docparse-raster` / `docparse-cli` 不上 crates.io**，CLI 安装仍靠 Release 二进制 / `cargo install --git`。
  - 代价：用户**装不到 `cargo install docparse-cli`**（crates.io 渠道少了最常用的入口），但**避免了"crates.io 版 PPV2 不可用"这个易混淆点**。库使用者（嵌入 docparse-core 做二次开发）照常受益。

**推荐**：
- **v0.1.0 选 (A)**——工程量为零、功能损失仅限一个 opt-in 后端、且"完整版"在 Release 二进制/git 渠道始终可得；**配套加 PPV2-缺补丁的显式护栏**（避免静默错）。
- **(B) 列入 §7 后续**，触发条件="社区明确要在 crates.io 直接用 PP-DocLayoutV2"或"上游 tract 已合并这两处修复（届时可删 vendored，(A) 的 caveat 自动消失，见 vendor/README.md §5）"。
- (C) 仅在"不愿意接受 crates.io 版有任何能力缺口"时才退守——但代价（失去 `cargo install docparse-cli`）通常比 (A) 的 caveat 更伤。

> **决策（2026-06-22，已确认）：取 (A)** —— 带 caveat 发布全部可发 crate，并落 **PPV2 缺补丁护栏**（`layout.rs` 选到 PPV2 后端而补丁缺失/输出异常时显式报错、提示改用 Release 二进制）。(B) 改名 fork、(C) 只发纯库均列入 §7 后续触发条件。

### 3.3 PyPI（`docparse-client`，基本就绪）
- `clients/python/pyproject.toml` 完整：setuptools 后端、`name="docparse-client"`、`0.1.0`、零依赖、`langchain`/`llamaindex` 可选 extra。纯 Python、无 C 扩展 → **纯 wheel + sdist，无需 maturin/cibuildwheel**。
- 阻塞：**名字 `docparse-client` 在 PyPI 的可用性未核实**（preflight 必查）；缺发布 CI；`__pycache__` 不应进包（已被 `.gitignore` 覆盖，打包用 `build` 自然排除）。
- 待办：`python -m build` → `twine check` → `twine upload`（先传 TestPyPI 验证）。

### 3.4 npm（`docparse-client`，本轮补充重点）
- `clients/typescript/package.json` 完整：ESM、`exports` 暴露 `.`/`./langchain`/`./ai`、`files:["dist","README.md"]`、零运行时依赖、peer 为可选。`dist/` 已 `tsc` 产出。
- 阻塞 / 清理项：
  1. **名字 `docparse-client` 在 npm 的可用性未核实**（preflight 必查）；若被占，回退 scoped 名 `@yzlabai/docparse-client`。
  2. **缺 `version` 校验、缺 `repository`/`homepage`/`keywords` 字段**（npm 页面与可发现性需要）。建议补 `"repository"`, `"homepage"`, `"bugs"`, `"keywords"`。
  3. **`clients/typescript/node_modules/` 与 `dist/` 似乎被 git 跟踪**（`.gitignore` 未含 `node_modules`）。应：把 `node_modules/` 加入 `.gitignore` 并 `git rm -r --cached`；`dist/` 由发布时 `prepublishOnly: npm run build` 重建，可不入库（或保留，二选一，本计划建议**不入库、发布前构建**）。
  4. **`prepublishOnly` / `prepack` 脚本缺失**：加 `"prepublishOnly": "npm run build && npm test"` 保证发出去的 `dist/` 是最新且测试通过。
- 待办：`npm pack --dry-run` 核对包内容 → `npm publish --access public`（scoped 名需 `--access public`）。

### 3.5 MCP registry（需新建 server.json）
- 现状：`docparse mcp` 是手写 JSON-RPC stdio server（`crates/docparse-cli/src/mcp.rs`），能跑；但仓库**无 `server.json`**，未进官方注册表。
- 官方注册表（registry.modelcontextprotocol.io）流程：仓库根写 `server.json`（schema：`https://static.modelcontextprotocol.io/schemas/.../server.json`），用 `mcp-publisher` CLI（GitHub OIDC 鉴权）发布；namespace 用 `io.github.yzlabai/*`（与仓库 owner 绑定校验）。
- server.json 需声明：name `io.github.yzlabai/docparse`、描述、**安装方式**。难点：MCP server 依赖 `docparse` 二进制——registry 条目应指向"通过 npm/PyPI 的 client 或 Release 二进制安装后 `docparse mcp` 启动"。最干净的形态：发布一个 **npm runner**（`npx docparse-mcp` 或复用 `docparse-client`）作为 registry 的 package 引用，runner 内部定位/下载二进制再 `exec docparse mcp`。
- **建议**：MCP registry 放在 §7 第二批——它依赖"client 自动拿二进制"能力（3.4 增强）才能给出零摩擦的 `npx` 接入；v0.1.0 先在 README/`docs/agent-integration.md` 文档化手动接入，registry 条目随增强一起上。

## 4. 范围与"不做什么"

**本轮做**：
1. 补齐 crates.io 发布所需元数据（path+version、repository/keywords/readme/categories），按 §3.2 (A) 带 caveat 发布全部可发 crate。
2. 打通 PyPI `docparse-client` 发布（先 TestPyPI）。
3. 打通 npm `docparse-client` 发布（含 §3.4 清理：node_modules 出库、补元数据、prepublishOnly）。
4. 打第一个 `v0.1.0` tag，验证 cargo-dist Release 真正产出二进制（修好 README 的 404 链接）。
5. 三处 CI 自动化（tag 触发：crates.io publish、PyPI upload、npm publish），或至少写清手动 runbook。
6. README / capabilities / status 更新安装矩阵 + "client 需二进制在 PATH"的明确说明。

**本轮不做（→ §7 后续）**：
- client 自动下载/绑定 prebuilt 二进制（postinstall pattern）。
- MCP registry 正式条目（先文档化手动接入）。
- maturin/napi 原生绑定（in-process，无需外部二进制）——是更大工程，另立计划。
- crates.io 上 PP-DocLayoutV2 可用（即 §3.2 (B) 改名 fork）。
- Homebrew / AUR / conda 等额外渠道。

## 5. 用户使用例子（发布后预期体验）

```bash
# Rust 用户
cargo install docparse-cli
docparse paper.pdf -f chunks

# Python RAG 用户
pip install docparse-client[langchain]   # 另需 docparse 二进制在 PATH（Release 安装）
python -c "from docparse_client.langchain import DocparseLoader; print(DocparseLoader('p.pdf').load()[0].metadata)"

# Node / Vercel AI 用户
npm install docparse-client              # 另需 docparse 二进制在 PATH
node -e "import('docparse-client').then(async m => console.log((await new m.DocparseClient().chunks('p.pdf')).length))"

# 任何人（无工具链）
curl -LsSf https://github.com/yzlabai/docparse-rs/releases/latest/download/docparse-cli-installer.sh | sh
```

## 6. 测试 / 验收用例

每条都要在**尽量干净的环境**（新 venv / 临时 node prefix / `cargo install` 到临时 root）跑通：

| # | 渠道 | 验收命令 | 通过标准 |
|---|---|---|---|
| T1 | Release 二进制 | push `v0.1.0` → workflow 绿 → `curl\|sh` 装 | `docparse --version` 打印 0.1.0；`docparse lorem.pdf -f text` 出文本 |
| T2 | crates.io | `cargo install docparse-cli`（临时 `--root`） | 同 T1 基础解析 + OCR + 默认 YOLO layout 工作；PPV2 后端报错可接受且文档已注明 |
| T3 | crates.io 库 | 新建 crate `depend docparse-core="0.1"` 编译 | 编译通过、能 `use docparse_core::...` |
| T4 | PyPI | 新 venv `pip install docparse-client`（先 TestPyPI 源） | `import docparse_client` 成功；二进制在 PATH 时 `DocparseClient().chunks()` 返回非空；client 测试 `clients/python/tests` 绿 |
| T5 | npm | 临时目录 `npm install docparse-client`（先 `npm pack` 本地 tarball） | `import` 三个 entry 成功；`npm test`（含 `clients/typescript/test`）绿；包内**不含** node_modules/src |
| T6 | 跨样例回归 | §1 三件套 `lorem`/`bialetti`/`1901.03003` | 文本/解码无回归（发布不应改解析，仅防元数据改动误伤构建） |
| T7 | 文档 | README 安装链接、capabilities 安装矩阵 | 链接不 404、四渠道命令准确、"需二进制在 PATH"已写明 |

**Preflight（动手前必做）**：核名可用性——`cargo search docparse-cli` / PyPI `docparse-client` / npm `docparse-client` / MCP namespace。任一被占，按 §3 的 scoped 回退方案改名并回写本计划。

## 7. 后续（明确排出本轮）
1. **client 自动绑定二进制**：npm `postinstall` / Python entry-point 按 `os.platform+arch` 拉对应 Release 二进制并校验 checksum——消除"手动装二进制"台阶（参考 esbuild/ruff）。
2. **MCP registry 正式条目**：依赖 (1)，`server.json` + `mcp-publisher`（GitHub OIDC），`npx` 零摩擦接入。
3. **crates.io 上 PPV2**：改名 fork tract 整棵子树（§3.2.1 (B)），或等上游 tract 合并这两处修复、可删 vendored patch 时（见 vendor/README.md §5）。
4. **maturin/napi 原生绑定**：in-process 解析，免外部二进制——独立大计划。
5. 额外渠道：Homebrew tap / conda-forge / AUR。

## 8. 落点（按 CLAUDE.md §7）
- 元数据：各 `crates/*/Cargo.toml` + 根 `Cargo.toml`（workspace.package 已有 repo/homepage）。
- npm 清理：`clients/typescript/package.json`、`.gitignore`、`git rm --cached node_modules`。
- CI：`.github/workflows/`（新增 `publish-crates.yml`/`publish-pypi.yml`/`publish-npm.yml`，或扩 release.yml）。
- MCP：仓库根 `server.json`（后续批）。
- 文档：`README.md` / `README.zh.md` 安装节、`docs/capabilities.md` 安装矩阵、`docs/status.md` 记分牌、本 devlog/计划。

---

## 实施记录
（动手后回写：preflight 核名结果、各渠道首发版本与遇到的坑、CI 是否绿、最终验收 T1–T7 结果。）
