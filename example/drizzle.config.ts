import { defineConfig } from "drizzle-kit";

// 仅用于 `npm run db:generate` / `db:studio`。
// 注意：服务启动时还会用 `CREATE TABLE IF NOT EXISTS` 兜底建表（见 src/server/db/index.ts），
// 所以首次跑不需要先迁移；这里保留 drizzle-kit 工作流给你做正经 migration。
export default defineConfig({
  schema: "./src/server/db/schema.ts",
  out: "./drizzle",
  dialect: "sqlite",
  dbCredentials: {
    url: process.env.SQLITE_PATH ?? "./data/kb.sqlite",
  },
});
