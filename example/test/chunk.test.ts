// 朴素切块器 chunkText 的纯函数单测——零依赖，不需要起任何服务。
// 跑法：npm test（node --import tsx --test）。
import { test } from "node:test";
import assert from "node:assert/strict";
import { chunkText } from "../src/server/lib/chunk.ts";

test("空/纯空白文本 → 0 块", () => {
  assert.deepEqual(chunkText("d", ""), []);
  assert.deepEqual(chunkText("d", "   \n\n  \n\t"), []);
});

test("单段短文本 → 1 块，字段对齐 core::Chunk", () => {
  const chunks = chunkText("doc-1", "毛利率下降，因为成本上升。");
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

test("多段未超 targetLen → 聚合成 1 块", () => {
  const chunks = chunkText("d", "甲段。\n\n乙段。\n\n丙段。");
  assert.equal(chunks.length, 1);
  assert.equal(chunks[0].text, "甲段。\n\n乙段。\n\n丙段。");
});

test("超过 targetLen → 强制分块，chunk_id 顺序递增", () => {
  const a = "A".repeat(300);
  const b = "B".repeat(300);
  const c = "C".repeat(300);
  const d = "D".repeat(300);
  const chunks = chunkText("d", [a, b, c, d].join("\n\n"), 500);
  // 任两段拼接 600 > 500，故每段各自成块。
  assert.equal(chunks.length, 4);
  assert.deepEqual(
    chunks.map((x) => x.chunk_id),
    [0, 1, 2, 3],
  );
  assert.deepEqual(
    chunks.map((x) => x.text),
    [a, b, c, d],
  );
  for (const x of chunks) assert.equal(x.char_len, 300);
});

test("超长单段直接成块，避免无限增长", () => {
  const big = "y".repeat(2000);
  const chunks = chunkText("d", big, 900);
  assert.equal(chunks.length, 1);
  assert.equal(chunks[0].char_len, 2000);
});

test("段内换行被规整，前后空白被裁掉", () => {
  const chunks = chunkText("d", "  第一行  \n  第二行  ");
  assert.equal(chunks.length, 1);
  // 行内的尾随空白 + 段首尾空白被清掉（\s\n → \n，再 trim）。
  assert.equal(chunks[0].text, "第一行\n  第二行");
});
