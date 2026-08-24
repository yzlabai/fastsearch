# fail-closed 默认 + 运行档如实标注

> 日期：2026-08-24 · 状态：**已实施 + 活服务验证 done**
> 上游决策：[职责边界：不承担身份与控制面](../governance/2026-08-24-职责边界-不承担身份与控制面.md)
> 来源：[FastGPT 参考建议](2026-08-24-fastgpt知识库与多模态检索参考建议.md) P0-5 / P0-1
> 相关 spec：[19-server](../specs/19-server.md)、[10-core](../specs/10-core.md)

## 1. 要做什么

两件小而独立的事，都不引入控制面：

- **A（fail-closed）**：身份没接好时**拒绝服务**，而不是替调用方编一个默认身份/默认公开。
- **B（如实标注）**：本实例**有无真源、能否从真源重建**，在启动日志 / introspection / metrics 里如实可见。

## 2. 为什么

刚决定"身份归调用方"（见上游决策）。**正因为 100% 依赖调用方接对，才绝不能在他没接对时替他猜。**
当前是替他猜，且两处猜都朝着"更公开"的方向：

1. `FASTSEARCH_KEYS` 未设 → 自动造 `dev` 密钥，`tenant: None`（`server/src/main.rs`）。
   `AclFilter::visible` 对 `tenant: None` 直接放行租户维度（`core/src/filter.rs`），
   即该密钥可读**所有租户**的 public 行。
2. Principal 没有 tags → 写入时 ACL 落 `["public"]`（`ingest_acl_for`）。

两者叠加的实际后果：**默认部署里写进去的一切都是 public，且任何无 tenant 的密钥都能读到所有租户的
public 行**。配置过的部署同样有口子：`FASTSEARCH_KEYS="k=acme:"`（有租户、无标签）写出的行是
`tenant=acme, acl=[public]`——acme 内部全员可读，且被任何无 tenant 密钥读到。

B 侧：`DATABASE_URL` 未配时不装 source store，此时"PG 是真源、派生索引可重建"（不变量 #2）
**不成立**，但对外完全不可见。诚实记账要求它可见。

## 3. 怎么做

### A1. 未配置 keys → 拒绝启动

`main.rs` 删掉自动 dev 密钥分支，改为打印可直接粘贴的修复命令后 `exit(1)`。
**不引入 profile 开关**：一个"生产档"布尔会立刻产生"默认取哪个"的两难（默认 local ⇒ 忘记设置即 fail-open；
默认 production ⇒ 凭空多一个必设项）。而"必须显式配 keys"本身既简单又已经是**全部文档命令的现状**
（README/CLAUDE.md/example 里每条启动命令都带 `FASTSEARCH_KEYS`），代价只是把 `dev=:` 改成 `dev=:public`。

### A2. 身份无 ACL 标签 → 拒绝写入（403）

`ingest_acl_for` 改为可失败：`tags` 为空 → 403 + 可操作的错误信息（提示配 `key=tenant:tags`，
或显式 `key=:public` 表示确要公开）。覆盖全部三条写路径：`/v1/index`、`/v1/chunks/batch-upsert`、`/v1/images`。

**public 仍然可用，但必须是显式授权动作**——`admin=:public` 这种写法在
[集成指南](../using-fastsearch-in-an-agent.md) 里**本来就是既定惯例**，故这不是新概念，
只是不再把"忘配标签"悄悄等同于"公开"。

用 403 而非 400：请求本身合法，是**该身份没有可赋予的 ACL** ⇒ 语义上是"不被授权执行此写入"。

### B. 如实标注

事实**由运行时推导，不新增配置**（有无 source store 是客观事实，不该由开关声明）：

- 启动日志：一行明确 `source of truth: PostgreSQL` / `local-only（派生索引不可从真源重建）`。
- introspection（既有 `server_vector_info`，随集合端点返回）：加 `source_of_truth`、`rebuildable_from_source`。
- metrics：加 gauge `fastsearch_source_store_configured`（0/1），运维可直接告警。

## 4. 不做什么（明确排除）

- **不做**任何身份/资源控制面（上游决策已定）。
- **不做**身份适配 trait（凭据 → Principal）：等第一个真实调用方给出形状，现在做属于猜。
- **不做** profile/运行档开关，理由见 A1。
- **不改** `AclFilter::visible` 的语义（`tenant: None` 放行仍是管理密钥的正当能力；
  危险的是**自动**造出这种密钥，A1 已断根）。
- 不动检索路径、不动 PG DDL。

## 5. 用户使用例子

```bash
# 修复前：不设 keys 也能起，且一切默认公开
./fastsearch-server                      # → 静默使用 dev 密钥，无租户限制

# 修复后：拒绝启动，并给出可直接粘贴的命令
./fastsearch-server
# error: FASTSEARCH_KEYS is required ...
#   本地开发： FASTSEARCH_KEYS="dev=:public" ...

# 忘了配标签时，写入被拒而不是被悄悄公开
curl -X POST .../v1/index -H 'x-api-key: k'   # k=acme:（无标签）→ 403
```

## 6. 测试用例（验收标准）

1. `parse_keys` 空表/空输入的判定（纯函数）。
2. 无标签 Principal 写 `/v1/index`、`/v1/chunks/batch-upsert`、`/v1/images` → **均 403**，且 PG/索引无副作用。
3. 有标签 Principal 写入不受影响（既有测试全绿）；显式 `:public` 标签可写、可被他人读到。
4. 既有 `ingest_acl_for` 单测按新契约更新（无标签不再返回 `["public"]`）。
5. introspection 在有/无 source store 两种情况下如实报告；metrics 出现对应 gauge。
6. 收口：fmt + clippy `-D warnings` + `cargo test --workspace`（带 `DATABASE_URL`）+ **实跑二进制验证**
   （无 keys 拒启、有 keys 正常、403 路径、introspection 字段）。

## 7. 影响面（需同步更新的文档）

`README.md` / `README.zh-CN.md` / `CLAUDE.md` / `example/README.md`(×2) /
`docs/using-fastsearch-in-an-agent.md`(×2，含 env 表里"unset = 单个 dev 密钥"那行) / `docs/specs/19-server.md`。


## 8. 实际验收结果（2026-08-24）

- 收口：fmt 净、clippy `-D warnings` 0 告警、`cargo test --workspace` **345 passed / 0 failed**
  （带 `DATABASE_URL`，PG/CDC 集成测试实跑）。新增 3 个测试。
- **活服务验证**（实跑二进制）：

  | 场景 | 结果 |
  |---|---|
  | 不配 `FASTSEARCH_KEYS` | 拒绝启动，打印可直接粘贴的两条修复命令 ✅ |
  | `dev=:`（有密钥、无标签） | 可启动；`POST /v1/index` → **403** + 可操作错误信息 ✅ |
  | `dev=:public`（显式公开） | 写入 200、检索命中 ✅ 正常路径未被误伤 |
  | 无 `DATABASE_URL` 启动日志 | `source of truth: 无（local-only / ephemeral）…不可从真源重建` ✅ |
  | introspection | `source_of_truth=none` / `rebuildable_from_source=false` ✅ |
  | `/metrics` | `fastsearch_source_store_configured 0` ✅ |

- **活服务验证抓到一个单测没抓到的真 bug**：metrics 的 gauge 最初写成了畸形输出——
  `# TYPE` 行带 9 空格缩进、`# HELP` 行里混入一串空白（字符串续行写错所致）。
  Prometheus 是**行式**格式，顶格是硬要求。已修，并在测试里把格式钉死：
  断言 `# TYPE` 顶格 + 遍历所有行断言不以空白开头（原来的测试只 `contains` 指标名，抓不到）。
  这条正是"实跑二进制"这一步的价值所在。

## 9. 已知限制 / 下一迭代

- **破坏性变更**：`FASTSEARCH_KEYS="dev=:"` 这类无标签密钥不再能写入，需改为 `dev=:public`
  或配真实标签。全部仓内文档已同步；外部使用者升级时会在启动/写入处得到明确指引，不是静默失败。
- 身份适配 trait（凭据 → `Principal`）**未做**，等真实调用方给出形状（上游决策已记）。
- `tenant: None` 的密钥仍可读所有租户的 public 行——这是管理密钥的正当能力，
  危险的是**自动**造出这种密钥，本次已断根。
