import { defineConfig, type Plugin } from "vite";
import { fetchDaemonCompatibility } from "./scripts/daemon_compatibility";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { readFile } from "node:fs/promises";
import { once } from "node:events";
import { homedir, platform } from "node:os";
import { join } from "node:path";
import { allowDevelopmentProxy, eventProxyPath } from "./src/dev-proxy-policy";

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
        const streaming = eventProxyPath(req.url);
        const controller = new AbortController();
        const abort = () => controller.abort();
        res.once("close", abort);
        req.once("aborted", abort);
        const timeout = setTimeout(abort, req.url === "/api/v1/settings/check" ? 35_000 : 15_000);
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
          const negotiated=await fetchDaemonCompatibility(target,info.token,controller.signal);
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
              "X-Self-Agent-Protocol": String(negotiated.protocol),
            },
            body: chunks.length ? Buffer.concat(chunks) : undefined,
            signal: controller.signal,
            redirect: "error",
          });
          res.statusCode = upstream.status;
          res.setHeader("Cache-Control", "no-store");
          if (streaming && upstream.ok && upstream.body) {
            if (upstream.headers.get("content-type")?.split(";")[0].trim() !== "text/event-stream") throw new Error("Invalid event stream");
            clearTimeout(timeout); // SSE stays open until the browser disconnects.
            res.setHeader("Content-Type", "text/event-stream");
            res.setHeader("X-Accel-Buffering", "no");
            res.flushHeaders();
            const reader = upstream.body.getReader();
            try {
              while (!controller.signal.aborted) {
                const { done, value } = await reader.read();
                if (done) break;
                if (!res.write(value)) await once(res, "drain", { signal: controller.signal });
              }
            } finally {
              await reader.cancel().catch(() => {});
              reader.releaseLock();
            }
            res.end();
          } else {
            res.setHeader("Content-Type", "application/json");
            res.end(await upstream.text());
          }
        } catch {
          if (res.headersSent || res.destroyed) { res.destroy(); return; }
          res.statusCode = 503;
          res.setHeader("Content-Type", "application/json");
          res.end(
            JSON.stringify({
              error: "后台尚未连接。请先启动 harness-daemon，再重试。",
            }),
          );
        } finally {
          clearTimeout(timeout);
          controller.abort();
          res.removeListener("close", abort);
          req.removeListener("aborted", abort);
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
