import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { readFile } from "node:fs/promises";
import { homedir, platform } from "node:os";
import { join } from "node:path";
import { allowDevelopmentProxy } from "./src/dev-proxy-policy";

function dataDirectory() {
  if (process.env.HARNESS_DATA_DIR) return process.env.HARNESS_DATA_DIR;
  if (platform() === "darwin")
    return join(homedir(), "Library", "Application Support", "self-agent");
  if (platform() === "win32")
    return join(
      process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local"),
      "self-agent",
    );
  return join(
    process.env.XDG_DATA_HOME ?? join(homedir(), ".local", "share"),
    "self-agent",
  );
}

// The token stays in the Vite process, never in a browser module or response.
function daemonProxy(): Plugin {
  return {
    name: "private-daemon-proxy",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (!req.url?.startsWith("/api/v1/")) return next();
        if (
          !allowDevelopmentProxy(
            req.method ?? "",
            req.url,
            req.headers.origin,
            req.headers["sec-fetch-site"]?.toString(),
            req.headers["content-type"],
          )
        ) {
          res.statusCode = 403;
          res.setHeader("Content-Type", "application/json");
          res.end(
            JSON.stringify({ error: "开发代理仅接受受支持的本机同源请求。" }),
          );
          return;
        }
        try {
          const info = JSON.parse(
            await readFile(join(dataDirectory(), "daemon.json"), "utf8"),
          ) as { base_url: string; token: string };
          const target = new URL(info.base_url);
          if (
            target.protocol !== "http:" ||
            target.hostname !== "127.0.0.1" ||
            target.username ||
            target.password ||
            target.pathname !== "/" ||
            target.search ||
            target.hash
          )
            throw new Error("Invalid daemon endpoint");
          const chunks: Buffer[] = [];
          let size = 0;
          for await (const chunk of req) {
            size += Buffer.byteLength(chunk);
            if (size > 1_048_576) throw new Error("Request too large");
            chunks.push(Buffer.from(chunk));
          }
          const upstream = await fetch(new URL(req.url, target), {
            method: req.method,
            headers: {
              Authorization: `Bearer ${info.token}`,
              "Content-Type": "application/json",
            },
            body: chunks.length ? Buffer.concat(chunks) : undefined,
            signal: AbortSignal.timeout(req.url === "/api/v1/settings/check" ? 35_000 : 15_000),
            redirect: "error",
          });
          res.statusCode = upstream.status;
          res.setHeader("Content-Type", "application/json");
          res.setHeader("Cache-Control", "no-store");
          res.end(await upstream.text());
        } catch {
          res.statusCode = 503;
          res.setHeader("Content-Type", "application/json");
          res.end(
            JSON.stringify({
              error: "后台尚未连接。请先启动 harness-daemon，再重试。",
            }),
          );
        }
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), tailwindcss(), daemonProxy()],
  clearScreen: false,
  build: {
    rollupOptions: {
      output: {
        manualChunks: {
          flow: ["@xyflow/react"],
        },
      },
    },
  },
  server: { host: "127.0.0.1", port: 1420, strictPort: true },
  envPrefix: ["VITE_"],
});
