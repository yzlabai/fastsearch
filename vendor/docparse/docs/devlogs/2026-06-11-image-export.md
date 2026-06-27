# 2026-06-11 · 图片导出面(`--image-dir`)—— G9 零头清掉,对齐 ODL external 模式

## TL;DR

`docparse doc.pdf --image-dir imgs/`:导出 PDF 嵌入光栅图为 JPEG/PNG 文件,JSON 的 image 元素带 `file` 路径,Markdown 输出 `![image pN](path)` 引用。对齐 ODL `image_output="external"`。默认关闭,零默认开销;记分牌/三件套零变化;100 测试、clippy 0。

## 设计

抽图早已就位(G4/N3:`XImage` 惰性解码,JPEG 直通 + Flate 位图),缺的只是"全量解码 + 写盘 + 引用"三件:

1. **全量解码开关**:`PdfParser { decode_images }`(默认 false)→ `PageInput`/`Ctx` 线程穿透到 `Do` 处理器。原解码门只放行整页覆盖 ≥0.5 的扫描候选;开关打开后 ≥16px 见方的图全解(16px 以下是项目符/图标,跳过——近似,已注释)。Form XObject 内的图随递归上下文同享开关。
2. **写盘**(`cli/main.rs::export_images`):JPEG 直写 `.jpg`(原字节,零转码);Rgb8/Gray8 走 `docparse_vlm::encode_png_rgb`(转 pub 复用,存储式 deflate,无新依赖;Gray8 展开成 RGB——TODO 灰度 PNG color-type 0 可省 2/3 体积)。命名 `p{页}-{序}.{ext}`。
3. **引用**:IR `ImageChunk.file: Option<String>`(serde skip-none,`data` 照旧不序列化);Markdown 在每页表格之后追加 `![]()`(页级定位,未按块插点——首增量)。

## 验证

- `picture_classification.pdf`:2 张 JPEG 直通,`file` 校验 JFIF 有效,Markdown 引用就位;
- `2206.01062.pdf`:三种编码全覆盖(jpeg 直通/gray8→PNG/rgb8→PNG),肉眼复核两张内容正确(店面照片、标注界面截图);
- 默认路径(不带旗标)记分牌逐字未变(NID 0.792/MHS 0.685/TEDS 0.419)、三件套不变——开关零默认影响。

## 边界(诚实标注)

- 仅 PDF 后端(DOCX/PPTX 媒体抽取另立);MCP/REST 未透出(写盘语义对服务面需另设计,如 base64 embedded 模式——ODL 的 `embedded` 对应物,留待 G8b/服务面迭代);
- JBIG2/CCITT/JPX 仍是位置占位(`ImageKind::None`),不写文件,JSON 保留 bbox 可审计;
- 一张 CMYK JPEG(components 4)按原字节直写——多数查看器可开,未做色彩转换(TODO 标在 images.rs 的直通注释里)。
