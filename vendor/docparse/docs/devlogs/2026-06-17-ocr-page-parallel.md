# devlog · OCR 增强遍页级并行 + 解锁 rec_cache 串行点(2026-06-17)

一句话:**研究"识别速度还能不能提升"时实测发现,OCR `enhance::apply` 这一遍是串行的、且 `recognize()` 的 `rec_cache` 锁持有到推理结束把 rec 全局串行化——朴素并行只到 3×,取出 `Arc` 释放锁后到 ~10×;落地页级并行 + 锁修复,多页扫描吞吐 18 核 10.2×,单页延迟不变。**

计划:[plans/ocr-page-parallel.md](../plans/ocr-page-parallel.md)。背景研究承接 Phase 8(PP-OCRv6 内嵌),见 [status.md](../status.md) Phase 8b。

## 缘起

用户问"研究是否可以在识别速度和质量上有提升"。先核对 status.md §4:便宜旋钮(max_tokens/重采样/分辨率/det 参数/medium OCR/表模型天花板)已被逐一证伪,不再重提。读 OCR 管线代码找**结构性、还没碰过**的杠杆,定位到两处串行:

1. 确定性 PDF 解析早已 rayon 页并行([pdf/lib.rs](../../crates/docparse-pdf/src/lib.rs) `inputs.par_iter()`),但 **OCR 增强这一遍是串行 `for page` 循环**([core/enhance.rs](../../crates/docparse-core/src/enhance.rs) `apply()`);
2. 更隐蔽:[ocr/lib.rs](../../crates/docparse-ocr/src/lib.rs) `recognize()` 把 `rec_cache` 的 `MutexGuard` **持有到 `model.run()` 结束**(`model` 借自 guard),所有线程的 rec 推理被这把锁全局串行化。

## Spike 先量,再实施

诊断脚手架(`examples/bench_parallel.rs`,用完即删未入 repo):把 chinese_scan 扫描页内存复制成 18 页,对比串行 vs `par_iter` 跑 `enhance_page`,扫不同并行度。

| 页级并行度 | 朴素并行(仅改页循环) | + rec_cache 锁修复 |
|---|---|---|
| 串行基线 | 5.03s(0.28s/页) | 同 |
| par×2 | 1.57×(79%) | **1.99×(100%)** |
| par×4 | 2.34× | **3.50×(88%)** |
| par×8 | 2.90×(36%) | **5.50×(69%)** |
| par×18 | **3.01×(封顶)** | **10.22×(57%)** |

**关键判读**:原以为瓶颈是"tract 已多核、并行会过订阅"。实测**不是**——par×2 修锁后是完美 2.0×/100%,证明 tract 对小 rec crop 单次推理本就近单线程,3× 封顶**纯粹是那把锁**(朴素并行只有 det 能并行、rec 被锁死)。

## 做了什么

一个 commit,三处改动 + 单测:

### 1. `enhance::apply` 页级并行(保序、限内存)
- 串行 `for page` → 每页算 `(Page, Option<PageRoute>)` 的纯函数 `process`,经 scoped rayon `ThreadPool`(`min(cores, MAX_PAGE_PARALLELISM=8)`)并行 `map`。
- **为什么是 8 不是核数**:扫描 buffer ~100MB/页,**内存是闸,不是核数**;效率拐点实测在 8 附近(8→18 只把 5.5×→10×)。常量留 `MAX_PAGE_PARALLELISM`,标 TODO 留作"按可用内存自适应"。
- **保序**:索引化 `par_iter().collect()` 天然按页序;`report` 只收 `Some` 的路由,顺序与串行一致 → 输出字节一致(差异化记分牌硬约束)。
- **少克隆**:重构 `Document` 时只 clone `source`/`provenance` 标量字段,`process` 已把每页 clone 一次,避免 `doc.clone()` 把 ~100MB buffer 二次复制。
- 单页 / 无核信息 / `threads<=1` 走串行分支(零池开销)。

### 2. `recognize()` 解锁 rec_cache
- 锁块内 `entry(bucket).or_insert(...).clone()` 取出 `Arc<TypedRunnableModel>` 句柄→**块结束即释放锁**→锁外 `model.run()`。tract plan 不可变、`run(&self)` 并发安全,共享 `Arc` 跨线程 sound。

### 3. core 加 `rayon` 依赖
- 通用并行库,非 PDF 专属,**不破"core 不 use 任何 PDF 库"分层不变量**。

### 4. 单测
- `parallel_apply_preserves_order_and_is_deterministic`:20 页(超过并行度上限,多波次)、交替数字/扫描,断言**并行结果 == 串行预期(逐页 + report 顺序)+ 跨运行确定性**,用现有 `StubOcr`。

## 验证

- `cargo test` 全过(含新单测)、`cargo clippy --all-targets` 零 warning、touched 文件 `rustfmt --check` 过;
- 三件套 born-digital(lorem/1901.03003/bialetti)字节不变——本改动不碰确定性路径,天然不变;
- chinese_scan `--ocr -f text` 逐字不变(`上海、深圳`、14 行),锁修复无正确性回归。

## 关键收获 / lesson

1. **"过订阅"的直觉要量**:本以为 tract 多线程会让页级并行无效,实测 par×2=100% 反而揭穿真凶是一把锁。**性能瓶颈先量再下结论**(呼应 status §4 lesson 6/8)。
2. **锁的作用域 = 串行的范围**:`MutexGuard` 借出 `&mut` 句柄会把锁一路持有到函数尾;取 `Arc` clone 立即释放是解并发的标准手法。
3. **并行只解吞吐,不解延迟**:本次只提升多页扫描吞吐;单页延迟(0.28s/页)纹丝不动——那要走页内杠杆(rec 同桶批处理 / 框并行),正交、未做。
4. **`cargo fmt -- <文件>` 不按文件过滤**,会格式化整个 workspace、顺带改既存 drift;只想格式化特定文件用 `rustfmt <文件>`。本次误触的 5 个无关 crate 已还原,保持 diff 聚焦。

## 待办(未做,正交)

- ~~页内杠杆攻单页延迟~~ → **已做(见下)**;
- `MAX_PAGE_PARALLELISM` 按可用内存自适应(现为固定 8 + TODO)。

---

## 迭代 2:页内 rec 并行攻单页延迟(同日,第二 commit)

页级并行只解多页吞吐,**单页延迟**(交互式扫描的常见场景)不变。续研究——先量单页拆分(临时 `DOCPARSE_OCR_TIMING` 计时,用完即删):

| 阶段 | 耗时(单页 chinese_scan,cold) | 占比 |
|---|---|---|
| det_boxes(一次 960×960 DBNet + 连通域) | ~206ms | **57%** |
| cls/orient(≤8 次小 cls 投票) | ~7ms | 2% |
| rec(14 框逐个) | ~148ms | **41%** |

**判读**:det 是单次推理(tract 已内部多核,框级拆不动);**rec 是 14 个独立 crop,可并行**(锁已修、并发安全)。

**A/B 实测**(warm,18 核,同 harness):

| 配置 | 单页(warm) | 多页 18@par×8 |
|---|---|---|
| 页内 rec 并行 | **0.211s** | 0.922s |
| 仅页级(rec 串行) | 0.275s | 0.909s |

→ 页内并行**单页 1.31×**,但**多页 ~1.4% 慢**(嵌套 rayon 开销)。故落地**自适应**:`rayon::current_thread_index().is_some()` 判断是否已在并行池——单页(不在池,走全局池)并行 box;多页(已在页级池)串行 box,**不嵌套**。自适应实测:单页 0.211s(满 1.31×)+ 多页 0.888s(零回归)。

**lesson 续**:并行的"该不该并"取决于上下文——同一段代码在单页该并、在多页池里不该并,`current_thread_index()` 是廉价的"我是否已在并行中"探针,比硬编码标志干净。

**det 是单页的硬底(实测证明,非假设)**:进一步拆 `det_boxes` 内部(临时计时)——`det.run` **~198ms = 96%**,resize 2.7ms / normalize 0.9ms / component_boxes 0.4ms 合计仅 ~4ms。即"标量 CPU 藏在 det 里、可零风险优化"的假设**被证伪**:det 就是纯 tract 推理,且 tract 已多核。降 det 只剩两条**有质量风险**的路——① `DET_SIDE=960→640`(v6 鲁棒但常量全局共享、v4 降级,且 v6 在 640 输出也"变",§6c 已弃);② 换更小 det 模型。**结论:OCR 基础路径(扫描取文)速度到底**——多页 10×、单页 1.31×、det 198ms 是 tract×该模型的地板。

**剩余未试速度杠杆(均在 OCR 基础路径外)**:UniRec int8 量化(表/公式/转写 AR 解码 169 tok/s、表 ~2.5s 的慢 opt-in 路;需离线量化 ONNX + 验 tract int8 支持,大 spike、收益面窄);rec 同桶批处理(冷启每桶 `into_optimized` 编译才是单次 CLI 隐藏成本,收益存疑)。

## 关键文件

- [crates/docparse-core/src/enhance.rs](../../crates/docparse-core/src/enhance.rs)——`apply()` 页并行 + 单测
- [crates/docparse-ocr/src/lib.rs](../../crates/docparse-ocr/src/lib.rs)——`recognize()` rec_cache 解锁 + `ocr_boxes()` 自适应页内并行
- [docs/plans/ocr-page-parallel.md](../plans/ocr-page-parallel.md)——计划与设计决策
