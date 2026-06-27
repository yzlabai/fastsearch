# devlog · PP-DocLayoutV2 内嵌 tract(2026-06-14)

执行 [plans/ppv2-layout-via-tract-patch.md](../plans/ppv2-layout-via-tract-patch.md) 路 A。一句话:**把"质量更好但跑不了"的 PP-DocLayoutV2 用两处 tract 补丁跑通,内嵌为 `--layout` 的第二后端,与 DocLayout-YOLO 共存。**

## 做了什么(P0–P6)

1. **根因 → 补丁(P1/P2)**:之前判的"墙"实为 tract 0.23.1 两个具体问题:
   - `tract-hir` GatherNd 推断规则把输出尾维约束到 indices 而非 data(一行 bug);
   - `tract-core` TopK 不接受 TDim 输入(tract-onnx 把 `Cast(to=INT64)` 映射成 TDim,RT-DETR 的 int64 掩码喂进 TopK 即炸)。
   两处补丁 vendored 进 `vendor/`(`[patch.crates-io]`),记 [vendor/PATCHES.md](../../vendor/PATCHES.md)。修完 optimize + eval 端到端通,无更多坎(闸门 G 仅 2/6)。
2. **正确性(P4)**:patched tract == ONNX Runtime,5/5 页逐框一致(class 100%、score Δ<1.3e-6、box Δ<5e-4px、阅读顺序相同)。复现:`scripts/spike/ppv2/{prepare,golden,compare}.py` + `examples/ppv2_run.rs`。
3. **速度(P5)**:tract 2391ms/页 = YOLO 1738ms 的 1.38×(过 1.5× 门)。tract 比 ORT 慢约 4×(两模型皆然),相对比才是落点。
4. **依赖安全**:vendored 补丁对现有模型**可证明零影响**——PP-OCR/UniRec/SLANet/TATR 不含这两算子;YOLO 含 TopK 但输入是 float(非 TDim),走不变的原路径。全单测 + 文本三件套 + YOLO 版面回归通过。详见 [analysis/2026-06-14-vendored-tract-patch-on-main.md](../analysis/2026-06-14-vendored-tract-patch-on-main.md)。
5. **内嵌(P6)**:[layout.rs](../../crates/docparse-ocr/src/layout.rs)
   - 双后端,`LayoutModel::new` **按 ONNX 输入数自动识别**(1=YOLO,3=PPV2),无需新 CLI 线穿过 main/mcp/server;
   - `RegionKind` 统一语义枚举,`map_yolo`(10 类)/`map_ppv2`(25 类)归一;
   - `Region` 增 `kind` + `order`;PPV2 detect:resize 800²、`/255`、3 输入、解析 `[N,8]`、坐标映射回 PDF、携带原生 `order`;
   - `region_rank` 全部区域有 `order` 时按原生序排(否则 XY-cut);
   - `assign_groups` 把标题类 `kind` 写入 `TextChunk.tag`(H1/H2),语义流入输出;
   - `seed_table_regions` 用 `kind.is_table()`;
   - [formula.rs](../../crates/docparse-ocr/src/formula.rs)/[transcribe.rs](../../crates/docparse-ocr/src/transcribe.rs) 迁移到 `kind`(顺带让这两个特性也支持 PPV2 的类别)。

## 验证

- 单测 25/25(layout RegionKind/native-order);clippy 零 warning。
- e2e:`--layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx` → 区域数 22/12/15/12/23/45(与 spike 一致),标题出 heading;YOLO 默认路径区域数 16 不变。

## 待办(P7)

- 上游 PR:GatherNd(明确 bug)+ TopK(应支持 TDim 输入);合并发版后回退 `vendor/` 为版本依赖。
- 记分牌全回归(OmniDocBench)+ 更新 [status.md](../status.md)。
- 可选 UX:`--layout-model` 现为路径(自动识别后端);如需 `{yolo|ppv2}` 别名再加。
- 模型分发:`models/layout-ppv2/PP-DoclayoutV2_simp.onnx` 由 `scripts/spike/ppv2/prepare.py` 从官方权重一次性生成(gitignored)。

## 关键文件

- 补丁:`vendor/tract-hir`、`vendor/tract-core`、`vendor/PATCHES.md`、根 `Cargo.toml` `[patch.crates-io]`
- 代码:`crates/docparse-ocr/src/{layout,formula,transcribe}.rs`
- 复现:`scripts/spike/ppv2/*.py`、`crates/docparse-ocr/examples/ppv2_run.rs`
- 文档:plan、analysis×2、testresults(2026-06-14)
</content>
