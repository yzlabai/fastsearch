import { useState } from "react";
import { Database, Sparkles } from "lucide-react";
import { Chat } from "./components/Chat.tsx";
import { DocumentsPanel } from "./components/DocumentsPanel.tsx";

export function App() {
  // 喂入新文档后 bump，让左侧列表刷新。
  const [docsVersion, setDocsVersion] = useState(0);

  return (
    <div className="flex h-screen flex-col bg-background">
      <header className="flex items-center gap-2 border-b px-5 py-3">
        <Sparkles className="h-5 w-5 text-primary" />
        <h1 className="text-sm font-semibold">fastsearch · 知识库 Agent</h1>
        <span className="ml-2 text-xs text-muted-foreground">
          Hono · Drizzle/SQLite · Vercel AI SDK · React/shadcn — 检索走
          fastsearch REST
        </span>
      </header>

      <div className="grid min-h-0 flex-1 grid-cols-[340px_1fr]">
        <aside className="flex min-h-0 flex-col border-r">
          <div className="flex items-center gap-2 px-4 py-3 text-sm font-medium">
            <Database className="h-4 w-4" />
            知识库
          </div>
          <DocumentsPanel
            version={docsVersion}
            onIngested={() => setDocsVersion((v) => v + 1)}
          />
        </aside>

        <main className="min-h-0">
          <Chat />
        </main>
      </div>
    </div>
  );
}
