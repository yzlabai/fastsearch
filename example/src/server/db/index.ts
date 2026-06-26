import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import Database from "better-sqlite3";
import { drizzle } from "drizzle-orm/better-sqlite3";
import * as schema from "./schema.ts";

const SQLITE_PATH = process.env.SQLITE_PATH ?? "./data/kb.sqlite";

mkdirSync(dirname(SQLITE_PATH), { recursive: true });

const sqlite = new Database(SQLITE_PATH);
sqlite.pragma("journal_mode = WAL");

// 启动兜底建表，让 example "克隆即跑"，不必先手动 drizzle-kit migrate。
// 真实项目用 `npm run db:generate` + migration 管理 schema 演进。
sqlite.exec(`
  CREATE TABLE IF NOT EXISTS documents (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    source      TEXT NOT NULL DEFAULT 'paste',
    collection  TEXT NOT NULL,
    chunk_count INTEGER NOT NULL DEFAULT 0,
    char_len    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch() * 1000)
  );
  CREATE TABLE IF NOT EXISTS chunks (
    citation_id TEXT PRIMARY KEY,
    doc_id      TEXT NOT NULL,
    chunk_id    INTEGER NOT NULL,
    text        TEXT NOT NULL
  );
  CREATE TABLE IF NOT EXISTS messages (
    id         TEXT PRIMARY KEY,
    role       TEXT NOT NULL,
    content    TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch() * 1000)
  );
`);

export const db = drizzle(sqlite, { schema });
export { schema };
