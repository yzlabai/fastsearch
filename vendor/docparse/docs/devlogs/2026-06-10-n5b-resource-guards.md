# Devlog · N5b 资源防护:zip-bomb 预检 + 页数早停(零依赖)

> 日期:2026-06-10 · plan:[plans/n5-security-precheck.md](../plans/n5-security-precheck.md) §N5b · 新模块 `core::limits`

## 做了什么

N2 服务化把解析暴露成对外接口后,恶意构造文档从"用户自伤"变成 DoS 面。N5b 加两道**确定性、零依赖、纯元数据**的资源守卫——阈值远高于任何真实文档,只拦病态构造,且只读元数据不做重活。

**`core::limits`**(格式无关常量 + `LimitError`):
- `check_zip_bomb(bytes)`:**手写解析 ZIP 中央目录**(EOCD → 中央目录文件头),只读各条目声明的压缩/解压尺寸,**不解压任何条目**。绝对上限 `MAX_UNCOMPRESSED_BYTES` 2 GiB + 压缩比上限 `MAX_COMPRESSION_RATIO` 250x(真实 DOCX 实测 8–23x;经典 zip bomb 10³–10¹¹x)。非 ZIP 缓冲直接放行(它是守卫不是校验器)。ZIP64 标记(0xFFFFFFFF)无法预检 → 放行交给解析器。
- `check_page_count(n)`:`MAX_PAGES` 50000,超则在任何逐页工作前早停。

**接入点**:
- `docparse-docx::parse_bytes` 在 `read_docx` 解压前先 `check_zip_bomb`;
- `docparse-pdf` 在 `get_pages()` 后、逐页解释前 `check_page_count`。
- 两者皆经 `?` 上抛 `anyhow`,**产生可追踪错误,不 panic 不挂起**(对齐"不静默吞")。

## 验收

- `core::limits` 单测 6:页数守卫、非 ZIP 放行、真实比例(8x)通过、比例炸弹拒绝、绝对尺寸(4 GiB)拒绝。
- `docparse-docx` 集成测试:手工构造 bomb 形 ZIP(1KB→1GB,~10⁶x)→ `parse_bytes` 拒绝且报"ratio"错误;**真实 fixture(23x)仍正常解析**。
- 端到端:构造 143 字节 bomb.docx(声明 900MB / 实际 11 字节,8 千万倍)→ CLI 报 `compression ratio 81818181x exceeds the 250x guard`,exit 1,无挂起。
- 73 单测全绿(+6 limits + 1 docx 集成),clippy 零 warning,三件套零回归,双记分牌不动。

## 边界 / 下一步

- 未做(N5c,暂缓):复杂度画像(页级路由信号)——quality 已有雏形,待 N3 真实 enhancer 需要时再扩。
- ZIP64 大档案的精确预检按需再加;当前放行交解析器,不是安全漏洞(absolute body 限制 REST 侧 256MB 仍在)。
- **模块 9(安全预检)隐藏文本 N5a + 资源防护 N5b 已落地**;剩 N3 真实 enhancer(需 OCR 选型征询)。
