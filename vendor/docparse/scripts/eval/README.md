# 评测脚本 · eval harness（N1）

把 docparse-rs 放到与 Docling 同尺的质量记分牌上（roadmap §6）：阅读顺序 **NID**、表格结构 **TEDS**、标题层级 **MHS**，对标 OpenDataLoader benchmark（Docling 综合 0.882）。

## 流水线

```bash
# 1) 我方输出 → 评测输入格式
docparse <file.pdf> -f chunks | scripts/eval/extract.py > pred.json

# 2) 与 ground truth 评分
scripts/eval/score.py pred.json gt.json      # → NID / TEDS / MHS / composite

# 自检（无需数据）
scripts/eval/score.py --selftest
```

差异化记分牌（无需 GT，可直接跑）：

```bash
scripts/metrics.sh > docs/devlogs/<date>-differentiation-metrics.md
```

## 当前状态

- ✅ **评分算法 + 提取器就绪**，合成自检通过。
- ✅ **差异化指标已回填**（体积/延迟/吞吐/确定性/引用率），见 `docs/devlogs/`。
- ⛔ **质量记分牌（NID/TEDS/MHS）未回填**，受阻于：
  1. **无 Docling 实例**（Python + 模型下载）做对照；
  2. **无 born-digital 标注集**（阅读顺序/表格/标题 ground truth）。

提供任一即可推进：给一个 `gt.json`（本格式）就能立刻给出我方分数；装上 Docling 就能同台。

## 数据格式（pred.json / gt.json）

单文档或文档列表：

```json
{ "reading_order": ["block text", "..."],
  "tables": [ [["a","b"],["c","d"]] ],
  "headings": [ [1,"Intro"], [2,"Methods"] ] }
```

## 注意

- **TEDS 当前是结构代理**（网格形状 + 单元格内容对齐），非完整 tree-edit-distance/APTED。标注格式定下后换成精确 TEDS（`score.py` 内 TODO）。
- 标题 level 由面包屑深度代理（`heading_path` 长度 + 1）。
- 评测集**只含 born-digital**、显式不含扫描件——那不是本项目战场（roadmap §2）。
