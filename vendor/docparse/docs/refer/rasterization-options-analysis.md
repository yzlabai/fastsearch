# 调研 · enhancer 光栅来源选型(G2/G3/G8c/G8d 共用决策)

> 日期:2026-06-10 · 背景:[G2 spike](../devlogs/2026-06-10-g2-layout-spike.md) 证明版面模型可用,但 born-digital 页喂任何"吃图"的神经增强(版面/表结构/公式/VLM)都需要**真实页面光栅**;"IR 文本框草图"已实验否决(模型判整页为 figure)。
> 本文带实测数据。spike 代码 `tmp/hayro-spike`(不合入)。

## 0. 前提:这个决策影响什么

| 里程碑 | born-digital 输入 | 扫描页输入 |
|---|---|---|
| G2 版面 enhancer | **需要光栅** | 已有(嵌入位图) |
| G3 表结构 | **需要光栅**(表区域图) | 已有 |
| G8c 公式/图片内嵌 | 矢量公式**需要光栅**;光栅图片已有 | 已有 |
| G8d/G8b VLM | **需要光栅**(页面图) | 已有 |

不解决 → 记分牌剩余 gap(CJK 0.12–0.22)与 G8 的 born-digital 半场全部不可达。

## 1. 选项与实测

### A. 外部光栅进程(运行时可选子进程)

`pdftoppm`(poppler)/ `sips`(macOS 内置)/ `mutool`(mupdf)按 PATH 探测,难页按需调用——同 tesseract-CLI 模式(N3 已验证该模式的工程形态)。

- ✅ 二进制零新依赖;渲染保真度最高(成熟引擎);
- ✅ 许可无忧:GPL(poppler)/AGPL(mupdf)经**子进程调用不构成衍生作品**;
- ⚠️ 用户须自装工具(macOS 有 sips 兜底,Linux 服务器通常有 poppler);跨平台行为有差异;
- ⚠️ 渲染不可信 PDF 的子进程需超时/隔离(poppler 历史 CVE 多);
- 实测(sips,1600px):~0.4s/页(含进程启动)。

### B. hayro:纯 Rust PDF 渲染器(**spike 实测,推荐**)

[LaurenzV/hayro](https://github.com/LaurenzV/hayro)(typst/krilla 作者),"目前功能最全的纯 Rust PDF 渲染器",0.7.1,**Apache-2.0/MIT 双许可**,1400+ 回归 PDF(PDFBOX/pdf.js 套件),vello_cpu 光栅化。

**实测(本机,normal_4pages 韩文信息图页)**:

- 解析 0.7ms + **渲染 99ms/页**(scale 2.0 → 1224×1718)——比预期"实验阶段性能未优化"好得多;
- **CJK 嵌入字体渲染完全正确**(目检字形/配色/版面无缺),喂 DocLayout-YOLO 出合理区域;
- API 干净:`Pdf::new(bytes)` + `render(page, cache, interp, settings)` → RGBA pixmap;标准 14 字体有内置数据兜底,CJK 非嵌入字体可经 `font_resolver` 配 Noto(可选模型文件同款思路)。

| 维度 | 评估 |
|---|---|
| 身份 | **纯 Rust 保持**;"不光栅化"改述为"**主流程**不光栅化;enhancer 按页 opt-in 光栅(纯 Rust)" |
| 许可 | Apache-2.0/MIT,无忧 |
| 风险 | 上游自述"实验/WIP"(API 演进、长尾 PDF 渲染缺陷);**已知缺口:非嵌入 CID 字体**(我方测试页均嵌入,未踩) |
| 体积 | 依赖树较大(9 个 hayro crate + vello_cpu);作**独立可选 crate**(`docparse-raster`)隔离,主二进制不受影响可 feature-gate |
| 红利 | hayro-jbig2 / hayro-ccitt / hayro-jpeg2000 子 crate **顺手解掉 G4 的扫描编码 TODO**(JBIG2/CCITT/JPX 解码可独立复用,无须整渲染器) |

### C. 仅扫描页(不解 born-digital)

CJK gap 与 G8 born-digital 半场放弃。VLM 对 born-digital 同样要页面图,问题不消失。**否决**——除非 A/B 均不可行。

### (D. 自研 text-only 渲染器——被 B 取代)

hayro 存在后,自研无理由(数周工程 vs 现成 Apache/MIT crate)。

## 2. 推荐

**B 为主,A 为备**:

1. 新可选 crate `docparse-raster`(包 hayro),仅 enhancer 路由的难页调用;不开 enhancer 的构建/运行路径零变化;
2. 失败兜底链:hayro 渲染失败(WIP 长尾)→ 若配置了外部工具(`--raster-cmd` 或 PATH 探测)走 A → 再不行该页跳过增强,确定性结果照常交付(M7 哲学);
3. 身份约束改述(roadmap §1):"纯 Rust · 主流程不光栅化——确定性解析永不渲染;神经 enhancer 可对路由难页按需光栅(纯 Rust hayro,opt-in)";
4. 顺手:用 hayro-jbig2/ccitt 补 G4 扫描编码(独立小项,不依赖本决策)。

**决议(2026-06-10,用户)**:① 改述通过——"主流程不渲染像素(该快的地方照样快);只有被判定为难页、要请 AI 帮忙时,才用纯 Rust 工具按需画那一页(默认关闭)",并将产品定位定为"速度快、质量好";② hayro 批准(独立 `docparse-raster` crate);③ 先只做 B,外部兜底链按需后补。
