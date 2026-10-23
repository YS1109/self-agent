export interface SourceContext {
  channel_id: string;
  actor_id: string;
  conversation_id: string;
}
export interface Message {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  created_at: string;
  task_id?: string;
}
export interface Session {
  id: string;
  title: string;
  project_path: string;
  created_at: string;
  updated_at: string;
  messages: Message[];
}
export interface TaskStep {
  id: string;
  title: string;
  status: string;
  detail?: string;
}
export interface TaskRun {
  id: string;
  session_id: string;
  title: string;
  status: string;
  execution_mode: "native" | "demo";
  created_at: string;
  updated_at: string;
  result?: string;
  error?: string;
  source: SourceContext;
  steps: TaskStep[];
}
export interface Delivery {
  id: string;
  task_id: string;
  session_id: string;
  target: SourceContext;
  status: string;
  payload: unknown;
  attempts: number;
  created_at: string;
  updated_at: string;
  error?: string;
}
export interface RunEvent {
  id: string;
  session_id?: string;
  task_id?: string;
  kind: string;
  message: string;
  created_at: string;
}
export interface Capability {
  id: string;
  name: string;
  kind: string;
  description: string;
  enabled: boolean;
}
export interface Snapshot {
  sessions: Session[];
  tasks: TaskRun[];
  deliveries: Delivery[];
  events: RunEvent[];
  capabilities: Capability[];
  selections: { source: SourceContext; session_id: string }[];
}
export interface Settings {
  api_key_configured?: boolean;
  base_url: string;
  model: string;
  configured: boolean;
  execution_mode: "native" | "demo";
}
export type Command =
  | { type: "create_session"; title: string; project_path: string }
  | { type: "select_session"; session_id: string }
  | { type: "send_message"; session_id?: string; content: string }
  | { type: "start_task"; session_id?: string; title: string }
  | { type: "cancel_task"; task_id: string }
  | { type: "retry_delivery"; delivery_id: string };
export interface Receipt {
  command_id: string;
  status: string;
  session_id?: string;
  task_id?: string;
}
export const DESKTOP_SOURCE: SourceContext = {
  channel_id: "desktop",
  actor_id: "owner",
  conversation_id: "main",
};
export const REMOTE_SOURCE: SourceContext = {
  channel_id: "simulator",
  actor_id: "owner",
  conversation_id: "remote-1",
};
export const EMPTY_SNAPSHOT: Snapshot = {
  sessions: [],
  tasks: [],
  deliveries: [],
  events: [],
  capabilities: [],
  selections: [],
};
export const activeTask = (status: string) =>
  status === "queued" || status === "running" || status === "waiting_input";
export const sameSource = (a: SourceContext, b: SourceContext) =>
  a.channel_id === b.channel_id &&
  a.actor_id === b.actor_id &&
  a.conversation_id === b.conversation_id;
