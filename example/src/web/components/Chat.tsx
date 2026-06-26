import { useEffect, useRef, useState } from "react";
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";
import { Loader2, Search, Send } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

// 从一条消息的 parts 里抽出：纯文本 + 检索工具命中的 citation_id 列表。
function readParts(parts: ReadonlyArray<{ type: string } & Record<string, any>>) {
  let text = "";
  const citations = new Set<string>();
  let searching = false;
  for (const p of parts) {
    if (p.type === "text") {
      text += p.text;
    } else if (p.type === "tool-searchKnowledgeBase") {
      if (p.state === "output-available" && p.output?.hits) {
        for (const h of p.output.hits as Array<{ citation_id: string }>) {
          citations.add(h.citation_id);
        }
      } else if (p.state !== "output-error") {
        searching = true;
      }
    }
  }
  return { text, citations: [...citations], searching };
}

export function Chat() {
  const { messages, sendMessage, status, error } = useChat({
    transport: new DefaultChatTransport({ api: "/api/chat" }),
  });
  const [input, setInput] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [messages]);

  const busy = status === "submitted" || status === "streaming";

  function submit(e: React.FormEvent) {
    e.preventDefault();
    const text = input.trim();
    if (!text || busy) return;
    sendMessage({ text });
    setInput("");
  }

  return (
    <div className="flex h-full flex-col">
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-2xl space-y-5 px-4 py-6">
          {messages.length === 0 && (
            <div className="rounded-lg border border-dashed p-6 text-center text-sm text-muted-foreground">
              先在左侧喂一篇文档，然后问点什么——
              <br />
              Agent 会先用 <code>searchKnowledgeBase</code> 检索 fastsearch，
              再带引用作答。
            </div>
          )}

          {messages.map((m) => {
            const { text, citations, searching } = readParts(m.parts);
            const isUser = m.role === "user";
            return (
              <div
                key={m.id}
                className={cn("flex", isUser ? "justify-end" : "justify-start")}
              >
                <div
                  className={cn(
                    "max-w-[85%] rounded-2xl px-4 py-2.5 text-sm",
                    isUser
                      ? "bg-primary text-primary-foreground"
                      : "bg-muted text-foreground",
                  )}
                >
                  {searching && !text && (
                    <div className="flex items-center gap-1.5 text-muted-foreground">
                      <Search className="h-3.5 w-3.5 animate-pulse" />
                      检索知识库…
                    </div>
                  )}
                  {text && (
                    <div className="whitespace-pre-wrap leading-relaxed">
                      {text}
                    </div>
                  )}
                  {citations.length > 0 && (
                    <div className="mt-2 flex flex-wrap gap-1 border-t border-border/40 pt-2">
                      <span className="text-xs text-muted-foreground">
                        来源：
                      </span>
                      {citations.map((c) => (
                        <Badge key={c} variant="outline" className="font-mono">
                          {c}
                        </Badge>
                      ))}
                    </div>
                  )}
                </div>
              </div>
            );
          })}

          {error && (
            <div className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
              出错了：{error.message}
            </div>
          )}
        </div>
      </div>

      <form onSubmit={submit} className="border-t p-3">
        <div className="mx-auto flex max-w-2xl gap-2">
          <Input
            placeholder="问知识库点什么…"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            disabled={busy}
          />
          <Button type="submit" size="icon" disabled={busy || !input.trim()}>
            {busy ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <Send className="h-4 w-4" />
            )}
          </Button>
        </div>
      </form>
    </div>
  );
}
