// 副作用模块：在任何其它模块求值前加载 .env（Node 20.12+）。
// 必须作为 server/index.ts 的第一个 import，确保 KB_MODEL / SQLITE_PATH /
// FASTSEARCH_* 等在被读取前就已注入 process.env。
try {
  process.loadEnvFile(".env");
} catch {
  /* 无 .env：用进程已有环境变量 */
}
