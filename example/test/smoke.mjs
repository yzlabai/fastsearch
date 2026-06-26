// 端到端冒烟测试：打已经在跑的栈（fastsearch + 本例服务），验证
//   ingest → /v1/index → SQLite  和  /api/chat 的 Agent 工具循环（DeepSeek）。
//
// 跑法（另开两个终端先把栈起好）：
//   1) 仓库根：FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:" cargo run -p fastsearch-server --bin fastsearch-server
//   2) example/：npm run dev     （需 .env 里有 DEEPSEEK_API_KEY）
//   3) example/：npm run test:e2e
//
// 零依赖：只用 Node 内置 fetch。失败时退出码 1。

const BASE = process.env.KB_BASE_URL ?? `http://127.0.0.1:${process.env.PORT ?? 8787}`;
const DOC = {
  title: "冒烟测试文档",
  text:
    "FastSearch 以托管 Postgres(pgvector) 为真源。\n\n" +
    "它只需 pgvector + 逻辑复制，不在 PG 装任何原生扩展，因此能跑在任意托管 PG。",
};
const QUESTION = "FastSearch 为什么能跑在任意托管 PG 上？";

let passed = 0;
let failed = 0;
function check(name, ok, detail = "") {
  if (ok) {
    passed++;
    console.log(`  ✓ ${name}`);
  } else {
    failed++;
    console.error(`  ✗ ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

async function main() {
  console.log(`▸ 目标服务 ${BASE}\n`);

  // 0) 健康检查
  try {
    const h = await fetch(`${BASE}/api/health`).then((r) => r.json());
    check("健康检查 /api/health", h?.ok === true);
  } catch (e) {
    check("健康检查 /api/health", false, `连不上服务，先 npm run dev：${e.message}`);
    return finish();
  }

  // 1) 喂入文档
  let ingest;
  try {
    const r = await fetch(`${BASE}/api/documents`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(DOC),
    });
    ingest = await r.json();
    check(
      "喂入文档 /api/documents",
      r.ok && ingest.chunkCount >= 1,
      ingest.error ?? `chunkCount=${ingest?.chunkCount}`,
    );
  } catch (e) {
    check("喂入文档 /api/documents", false, e.message);
    return finish();
  }

  // 2) 文档出现在列表
  try {
    const list = await fetch(`${BASE}/api/documents`).then((r) => r.json());
    check(
      "文档登记到 SQLite",
      Array.isArray(list.documents) && list.documents.some((d) => d.id === ingest.id),
    );
  } catch (e) {
    check("文档登记到 SQLite", false, e.message);
  }

  // 3) Agent 聊天：解析 UI message 流
  let toolCalled = false;
  let toolGotHits = false;
  let answer = "";
  let finishReason = null;
  let streamError = null;
  try {
    const resp = await fetch(`${BASE}/api/chat`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        messages: [
          { id: "u1", role: "user", parts: [{ type: "text", text: QUESTION }] },
        ],
      }),
    });
    if (!resp.ok || !resp.body) throw new Error(`HTTP ${resp.status}`);

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const lines = buf.split("\n");
      buf = lines.pop() ?? "";
      for (const line of lines) {
        if (!line.startsWith("data:")) continue;
        const payload = line.slice(5).trim();
        if (!payload || payload === "[DONE]") continue;
        let ev;
        try {
          ev = JSON.parse(payload);
        } catch {
          continue;
        }
        if (ev.type === "tool-input-available" && ev.toolName === "searchKnowledgeBase")
          toolCalled = true;
        if (ev.type === "tool-output-available" && ev.output?.hits?.length)
          toolGotHits = true;
        if (ev.type === "text-delta" && typeof ev.delta === "string")
          answer += ev.delta;
        if (ev.type === "finish") finishReason = ev.finishReason ?? "(none)";
        if (ev.type === "error") streamError = ev.errorText ?? JSON.stringify(ev);
      }
    }
  } catch (e) {
    check("Agent /api/chat 可流式响应", false, e.message);
    return finish();
  }

  check("Agent /api/chat 可流式响应", true);
  check("Agent 调用了 searchKnowledgeBase 工具", toolCalled);
  check("工具从 fastsearch 拿到命中", toolGotHits);
  check("Agent 产出了文本回答", answer.trim().length > 0, `len=${answer.length}`);
  check("回答带引用 [kb:...]", /\[kb:/.test(answer) || /kb:[^\s]+:\d+/.test(answer));
  check("正常结束（finishReason=stop）", finishReason === "stop", `finishReason=${finishReason}`);
  check("流中无 error 事件", streamError === null, streamError ?? "");

  if (answer.trim()) {
    console.log("\n— Agent 回答预览 —\n" + answer.trim().slice(0, 400) + "\n");
  }

  finish();
}

function finish() {
  console.log(`\n结果：${passed} 通过 / ${failed} 失败`);
  process.exit(failed > 0 ? 1 : 0);
}

main();
