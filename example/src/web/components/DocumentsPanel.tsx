import { useEffect, useState } from "react";
import { FileText, Loader2, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Badge } from "@/components/ui/badge";

interface DocRow {
  id: string;
  title: string;
  chunkCount: number;
  charLen: number;
}

export function DocumentsPanel({
  version,
  onIngested,
}: {
  version: number;
  onIngested: () => void;
}) {
  const [docs, setDocs] = useState<DocRow[]>([]);
  const [title, setTitle] = useState("");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetch("/api/documents")
      .then((r) => r.json())
      .then((d) => setDocs(d.documents ?? []))
      .catch(() => {});
  }, [version]);

  async function ingest() {
    setBusy(true);
    setError(null);
    try {
      const resp = await fetch("/api/documents", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ title, text }),
      });
      const data = await resp.json();
      if (!resp.ok) throw new Error(data.error ?? `HTTP ${resp.status}`);
      setTitle("");
      setText("");
      onIngested();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  const canIngest = title.trim() && text.trim() && !busy;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="space-y-2 border-b px-4 pb-4">
        <Input
          placeholder="文档标题，如：2025 年报"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
        />
        <Textarea
          placeholder="粘贴正文（会被切块后喂进 fastsearch）…"
          className="min-h-[120px] resize-none"
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
        {error && <p className="text-xs text-destructive">{error}</p>}
        <Button
          className="w-full"
          disabled={!canIngest}
          onClick={ingest}
          size="sm"
        >
          {busy ? (
            <Loader2 className="h-4 w-4 animate-spin" />
          ) : (
            <Plus className="h-4 w-4" />
          )}
          喂入知识库
        </Button>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
        {docs.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            还没有文档。先粘贴一篇上面，再去右边提问。
          </p>
        ) : (
          <ul className="space-y-2">
            {docs.map((d) => (
              <li
                key={d.id}
                className="flex items-start gap-2 rounded-md border px-3 py-2"
              >
                <FileText className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" />
                <div className="min-w-0">
                  <div className="truncate text-sm font-medium">{d.title}</div>
                  <div className="mt-1 flex gap-1.5">
                    <Badge variant="secondary">{d.chunkCount} 块</Badge>
                    <Badge variant="outline">{d.charLen} 字</Badge>
                  </div>
                </div>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
