import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// 前端 dev server（:5173）把 /api 反代到 Hono 后端（:8787）。
// 生产里你会把 vite build 的静态产物交给任意静态托管，后端单独部署。
// Tailwind v4 用官方 Vite 插件（CSS-first，无需 tailwind.config / postcss）。
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src/web"),
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8787",
        changeOrigin: true,
      },
    },
  },
});
