import { Check, Loader2 } from "lucide-react";
import { STATUS } from "../labels";

export function Status({ status }: { status: string }) {
  return (
    <span className={`status status-${status}`}>
      {["running", "sending", "reviewing"].includes(status) ? (
        <Loader2 size={11} className="spin" />
      ) : ["completed", "sent"].includes(status) ? (
        <Check size={11} />
      ) : (
        <span className="status-dot" />
      )}
      {STATUS[status] ?? "状态更新中"}
    </span>
  );
}
