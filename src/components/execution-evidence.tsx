import type { ExecutionEvidence as Evidence } from "../types";

export function ExecutionEvidence({ evidence }: { evidence?: Evidence[] }) {
  if (!evidence?.length) return null;
  return (
    <details className="execution-evidence">
      <summary>真实命令记录 · {evidence.length} 条</summary>
      <p className="help-text">来自执行器的命令与退出码。验收通过还需匹配节点指定的验收命令。</p>
      {evidence.map((item, index) => (
        <div className="evidence-item" key={`${index}-${item.command}`}>
          <div><code>{item.command}</code><span className={item.exit_code === 0 ? "evidence-pass" : "error-text"}>退出码 {item.exit_code}</span></div>
          <pre>{item.output || "命令未产生输出"}</pre>
        </div>
      ))}
    </details>
  );
}
