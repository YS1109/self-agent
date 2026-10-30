import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ChangesReportView } from "./task-changes";
import type { ChangesReport } from "../types";
const report:ChangesReport={version:1,partial:true,attribution:"not_exclusive",files:[{path:"中文<script>.txt",previous_path:"old.txt",status:"renamed",binary:false,before_bytes:10,after_bytes:11,before_readonly:false,after_readonly:false,before_mode:420,after_mode:420,diff_state:"available",diff:"- old\n+ <script>new</script>"}],baseline_git:{state:"available",entries:[{path:"user.txt",index:"M",worktree:" ",untracked:false}]},current_git:{state:"not_git",entries:[]},skipped:[{path:"large.txt",reason:"too_large"}]};
describe("只读改动报告",()=>{
 it("区分预先存在的 Git 修改、观察区间与外部并发写入",()=>{
  const html=renderToStaticMarkup(<ChangesReportView report={report}/>);
  for(const label of ["执行前 Git 概览", "非任务归因", "不能独占归因", "外部并发写入", "不会暂存", "此报告不完整", "重命名", "未检测到 Git 标记", "文件超出读取预算"]) expect(html).toContain(label);
  expect(html).toContain("&lt;script&gt;");expect(html).not.toContain("<script>");expect(html).toContain("user.txt");
 });
 it("过期和不可用不伪装成无改动，二进制和预算明确说明",()=>{
  expect(renderToStaticMarkup(<ChangesReportView report={{version:1,state:"expired"}}/>)).toContain("报告已过期");
  expect(renderToStaticMarkup(<ChangesReportView report={{version:1,state:"unavailable"}}/>)).toContain("报告不可用");
  const files=[{...report.files![0],binary:true,diff_state:"binary_not_shown" as const},{...report.files![0],path:"big",diff_state:"diff_budget_exceeded" as const}];
  const html=renderToStaticMarkup(<ChangesReportView report={{...report,files}}/>);
  expect(html).toContain("二进制文件");expect(html).toContain("文本差异超出预算");expect(html).not.toContain("&lt;script&gt;new");
 });
 it("Git 安全禁用原因显式展示，不把空状态解释为干净或已采集",()=>{
  const unavailable={state:"unavailable" as const,reason:"repository_filter_execution_disabled",entries:[]};
  const html=renderToStaticMarkup(<ChangesReportView report={{...report,baseline_git:unavailable,current_git:unavailable}}/>);
  expect(html).toContain("Git 暂存区、工作树与未跟踪状态未采集");
  expect(html).toContain("不能解释为没有改动或工作区干净");
  expect(html).toContain("repository_filter_execution_disabled");
  expect(html).toContain("已禁用 Git 状态命令");
  expect(html).toContain("重命名");expect(html).not.toContain("索引列表示已暂存状态");
 });

 it("有界 Git 元数据明确区分暂存状态与原始文件字节，不猜测过滤及忽略语义",()=>{
  const partial={state:"partial" as const,head_comparison:"unknown" as const,worktree_comparison:"raw_bytes_only" as const,entries:[{path:"new.txt",index:"?",worktree:"?",untracked:false,raw_worktree:"not_in_index_ignore_unknown" as const},{path:"tracked.txt",index:"M",worktree:"?",untracked:false,raw_worktree:"different_bytes" as const}]};
  const html=renderToStaticMarkup(<ChangesReportView report={{...report,baseline_git:partial}}/>);
  for(const label of ["仅索引列比较 HEAD 与暂存区", "字节不同", "忽略状态未知", "HEAD 不可完整读取", "字节相同也不能认定 Git 干净"]) expect(html).toContain(label);
 });
});
