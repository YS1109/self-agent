import {describe,it,expect,vi} from "vitest";
import {renderToStaticMarkup} from "react-dom/server";
import {PairForm,QuestionCard,TaskCard} from "./MobileApp";
const task={id:"task",session_id:"session",title:"<img src=x onerror=alert(1)>",status:"waiting_input",waiting_request_id:"question",execution_cleanup:"pending" as const,stream_output:"<script>secret()</script>"};
describe("移动端权限与纯文本显示",()=>{
 it("配对码只在password DOM输入，不预填、不存储或渲染secret",()=>{const html=renderToStaticMarkup(<PairForm disabled={false} onPair={vi.fn()} onCheck={vi.fn()}/>);expect(html).toContain('type="password"');expect(html).not.toMatch(/type="password"[^>]*value=/);expect(html).toContain("本机还需要核对");expect(html).toContain("重启后需要重新配对");});
 it("任务内容转义，清理pending不等于副作用停止，无授权不显示动作",()=>{const html=renderToStaticMarkup(<TaskCard task={task} disabled={false} canRun={false} canCancel={false} canInspect={false} onCancel={vi.fn()} onContinue={vi.fn()} onInspect={vi.fn()}/>);expect(html).toContain("&lt;script&gt;");expect(html).not.toContain("<script>");expect(html).toContain("不代表副作用已停止");expect(html).not.toContain("取消任务</button>");});
 it("approval永远没有远程批准或回答按钮",()=>{const html=renderToStaticMarkup(<QuestionCard question={{id:"question",session_id:"session",task_id:"task",kind:"approval",status:"pending",revision:3,prompt:"write a file",expires_at_ms:Date.now()+10000}} task={task} disabled={false} onAnswer={vi.fn()}/>);expect(html).toContain("不是执行授权");expect(html).not.toContain("提交这次回答");expect(html).not.toContain("<textarea");});
 it("过期或非当前请求回答不可提交",()=>{const html=renderToStaticMarkup(<QuestionCard question={{id:"question",session_id:"session",task_id:"task",kind:"question",status:"pending",revision:3,prompt:"question",expires_at_ms:0}} task={task} disabled={false} onAnswer={vi.fn()}/>);expect(html).toMatch(/textarea[^>]*disabled/);expect(html).toContain("提交这次回答");});
});
