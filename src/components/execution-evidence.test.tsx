import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ExecutionEvidence } from "./execution-evidence";

describe("真实命令证据", () => {
  it("不为空的执行器记录展示实际命令、退出码和输出，并安全转义命令输出", () => {
    const html = renderToStaticMarkup(<ExecutionEvidence evidence={[
      { command: "pnpm test", exit_code: 1, output: "<script>失败</script>" },
      { command: "pnpm test", exit_code: 0, output: "2 个测试通过" },
    ]} />);
    expect(html).toContain("真实命令记录 · 2 条");
    expect(html).toContain("退出码 1");
    expect(html).toContain("退出码 0");
    expect(html).toContain("&lt;script&gt;失败&lt;/script&gt;");
    expect(html).not.toContain("<script>");
    expect(html).toContain("2 个测试通过");
  });
  it("没有工具证据时不把模型文字或空记录展示为验收结果", () => {
    expect(renderToStaticMarkup(<ExecutionEvidence />)).toBe("");
    expect(renderToStaticMarkup(<ExecutionEvidence evidence={[]} />)).toBe("");
  });
});
