// SDK 共享切块器 chunkText 的仓库内集成单测——不需要起任何服务。
// 跑法：npm test（node --import tsx --test）。
import { test } from "node:test";
import assert from "node:assert/strict";
import { chunkText } from "fastsearch-client";

test("空/纯空白文本 → 0 块", () => {
  assert.deepEqual(chunkText("", { docId: "d" }), []);
  assert.deepEqual(chunkText("   \n\n  \n\t", { docId: "d" }), []);
});

test("单段短文本 → 1 块，字段对齐 core::Chunk", () => {
  const chunks = chunkText("毛利率下降，因为成本上升。", {
    docId: "doc-1",
    acl: ["public"],
  });
  assert.equal(chunks.length, 1);
  const c = chunks[0];
  assert.equal(c.doc_id, "doc-1");
  assert.equal(c.chunk_id, 0);
  assert.equal(c.kind, "paragraph");
  assert.equal(c.page, 1);
  assert.deepEqual(c.bbox, { x0: 0, y0: 0, x1: 0, y1: 0 });
  assert.deepEqual(c.acl, ["public"]);
  assert.equal(c.text, "毛利率下降，因为成本上升。");
  assert.equal(c.char_len, c.text.length);
});

test("多段未超 targetChars → 聚合成 1 块", () => {
  const chunks = chunkText("甲段。\n\n乙段。\n\n丙段。", { docId: "d" });
  assert.equal(chunks.length, 1);
  assert.equal(chunks[0].text, "甲段。\n\n乙段。\n\n丙段。");
});

test("达到 targetChars → 强制分块，chunk_id 顺序递增", () => {
  const a = "A".repeat(300);
  const b = "B".repeat(300);
  const c = "C".repeat(300);
  const d = "D".repeat(300);
  const chunks = chunkText([a, b, c, d].join("\n\n"), {
    docId: "d",
    targetChars: 500,
  });
  assert.equal(chunks.length, 2);
  assert.deepEqual(
    chunks.map((x) => x.chunk_id),
    [0, 1],
  );
  assert.deepEqual(
    chunks.map((x) => x.text),
    [`${a}\n\n${b}`, `${c}\n\n${d}`],
  );
  for (const x of chunks) assert.equal(x.char_len, 602);
});

test("超长单段直接成块，避免无限增长", () => {
  const big = "y".repeat(2000);
  const chunks = chunkText(big, { docId: "d", targetChars: 900 });
  assert.equal(chunks.length, 1);
  assert.equal(chunks[0].char_len, 2000);
});

test("markdown 标题独立成块并进入 heading_path", () => {
  const chunks = chunkText("# 财报\n\n正文\n\n## 风险\n\n汇率波动", { docId: "d" });
  assert.deepEqual(
    chunks.map((chunk) => [chunk.kind, chunk.text, chunk.heading_path]),
    [
      ["heading", "财报", ["财报"]],
      ["paragraph", "正文", ["财报"]],
      ["heading", "风险", ["财报", "风险"]],
      ["paragraph", "汇率波动", ["财报", "风险"]],
    ],
  );
});
