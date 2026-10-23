import { spawn } from "node:child_process";
import { prepareSidecar, root } from "./prepare-sidecar.mjs";

prepareSidecar();
const command = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const child = spawn(command, ["exec", "tauri", "dev"], {
  cwd: root,
  stdio: "inherit",
  shell: process.platform === "win32",
});
child.on("error", (error) => {
  console.error(error.message);
  process.exitCode = 1;
});
child.on("exit", (code) => {
  process.exitCode = code ?? 1;
});
