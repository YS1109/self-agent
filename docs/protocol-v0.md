# v0 integration contract

This is the shared contract for the first implementation. All field names are snake_case. IDs and RFC3339 timestamps are strings. Optional values are null or omitted.

## Ownership

- Backend agent: `crates/harness-daemon/**`.
- Frontend agent: `src/**`, `index.html`, `package.json`, `tsconfig*.json`, `vite.config.ts`, `components.json`, `public/**`.
- Platform agent: `crates/harness-core/**`, `src-tauri/**`.
- Primary agent: workspace manifest, documentation, CI, scripts, integration and verification.

## Runtime

Daemon binds `127.0.0.1:47371` by default. All `/api/v1` routes require `Authorization: Bearer TOKEN`. `/health` is public and contains no private data.

`HARNESS_DATA_DIR` overrides the platform local application data directory joined with `self-agent`. `daemon.json` inside this directory contains `{ "base_url": "...", "token": "...", "pid": 123 }`; it must be private to the local user.

The Tauri shell reads that metadata, forwards API requests through the Rust command `api_request(method, path, body?)` and returns JSON. The frontend calls it using `@tauri-apps/api/core`. Browser development uses the same API paths through a Vite development proxy; no daemon bearer token is shipped in the frontend bundle.

The Tauri shell launches the bundled daemon if needed. It must not terminate the daemon when the last desktop window closes. The production sidecar name is `harness-daemon`; target-triple suffixed binaries are prepared by a root-owned script.

## REST routes

- `GET /health` -> `{ "status": "ok", "version": "0.1.0" }`
- `GET /api/v1/snapshot` -> Snapshot
- `POST /api/v1/commands` -> `{ "command_id": "...", "status": "accepted", "session_id": "...?" , "task_id": "...?" }`
- `GET /api/v1/events` -> SSE RunEvent records, best-effort live wakeups; snapshot is authoritative.
- `GET /api/v1/settings` -> `{ "base_url": "...", "model": "...", "configured": false, "api_key_configured": false, "execution_mode": "native" }`
- `PUT /api/v1/settings` accepts base_url, model, optional api_key, execution_mode. API keys are memory-only for v0; environment configuration is supported. Never return a key.

Errors: non-2xx `{ "error": "human-readable message" }`.

## Snapshot

```json
{
  "sessions": [],
  "tasks": [],
  "deliveries": [],
  "events": [],
  "capabilities": [],
  "selections": []
}
```

Session: `{ id, title, project_path, created_at, updated_at, messages: Message[] }`.

Message: `{ id, role: "user"|"assistant"|"system", content, created_at, task_id? }`.

TaskRun: `{ id, session_id, title, status: "queued"|"running"|"waiting_input"|"completed"|"failed"|"cancelled", execution_mode: "native"|"demo", created_at, updated_at, result?, error?, source: SourceContext, steps: TaskStep[] }`.

TaskStep: `{ id, title, status: "queued"|"running"|"completed"|"failed"|"cancelled", detail? }`.

SourceContext: `{ channel_id, actor_id, conversation_id }`. Desktop source uses `"desktop","owner","main"`. Simulated remote source uses `"simulator","owner","remote-1"`.

Delivery: `{ id, task_id, session_id, target: SourceContext, status: "pending"|"sending"|"sent"|"failed", payload, attempts: integer, created_at, updated_at, error? }`.

RunEvent: `{ id, session_id?, task_id?, kind, message, created_at }`.

Capability: `{ id, name, kind: "tool"|"agent"|"skill"|"mcp"|"executor", description, enabled: boolean }`.

SessionSelection: `{ source: SourceContext, session_id }`.

## Commands

CommandEnvelope: `{ command_id, source: SourceContext, command: Command }`.

Commands use an internally tagged `type`:

- `{ type: "create_session", title, project_path }`
- `{ type: "select_session", session_id }`
- `{ type: "send_message", session_id?, content }`
- `{ type: "start_task", session_id?, title }`
- `{ type: "cancel_task", task_id }`
- `{ type: "retry_delivery", delivery_id }`

All callers use a new UUID per user intent. Retrying the same envelope must return the same receipt without repeating effects. No implicit session guessing: missing session uses the exact SourceContext selection or returns a validation error.

`send_message` persists a user message and immediately adds a short management acknowledgment with current task counts. It must remain responsive while task workers wait. Coding execution is explicitly `start_task` in v0, exposed as a composer action. Full model-driven planning is a subsequent milestone.

The dispatch transaction persists task, steps, event and receipt before background execution. Task terminal state, completion event and callback outbox record commit together. Callback target is copied at dispatch, independent of later selection changes.

## Execution and callback behavior

Native is the default execution mode. Without a configured provider, task dispatch returns a clear configuration error. Demo mode is explicitly selectable and labeled in tasks/UI; it runs staged synthetic work only, never claims to modify a project or pass real tests.

The backend owns a `provider` module: first implementation supports gateway Chat Completions with tool calling and a bounded model/tool loop. Workspace tools are scoped to the selected canonical project path. Command execution must be explicitly enabled using `HARNESS_ALLOW_COMMANDS=1`; expose capability readiness to UI.

Remote simulator callbacks can be marked sent by a built-in sink while preserving delivery records. Desktop callbacks are delivered to the durable local session/event stream. No actual communications platform or public inbound server is part of this milestone.

## Core Rust types

Core exports the types above plus `now() -> String`, `new_id() -> String`, `data_dir() -> PathBuf`, and `DaemonInfo { base_url, token, pid }`. Use String for statuses/modes/kinds in v0 to simplify adapter evolution. Command is a serde-tagged enum. Clone/Debug/Serialize/Deserialize are supported for shared records.
