import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export function prepareSidecar({ release = false } = {}) {
  const args = ["build", "-p", "harness-daemon"];
  if (release) args.push("--release");
  execFileSync("cargo", args, { cwd: root, stdio: "inherit" });
  const version = execFileSync("rustc", ["-vV"], {
    cwd: root,
    encoding: "utf8",
  });
  const target = version.match(/^host:\s*(\S+)$/m)?.[1];
  if (!target) throw new Error("Cannot determine the Rust host target.");
  const extension = process.platform === "win32" ? ".exe" : "";
  const source = resolve(
    process.env.CARGO_TARGET_DIR || resolve(root, "target"),
    release ? "release" : "debug",
    `harness-daemon${extension}`,
  );
  if (!existsSync(source)) throw new Error(`Daemon binary missing: ${source}`);
  const destination = resolve(
    root,
    "src-tauri",
    "binaries",
    `harness-daemon-${target}${extension}`,
  );
  mkdirSync(dirname(destination), { recursive: true });
  copyFileSync(source, destination);
  console.log(`Prepared desktop sidecar for ${target}.`);
  return destination;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  prepareSidecar({ release: process.argv.includes("--release") });
}
