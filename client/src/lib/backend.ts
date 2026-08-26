// Frontend ↔ backend bridge.
//
// Three transports, selected automatically:
//   ipc  — under Tauri: calls the real Rust commands via IPC.
//   web  — plain browser: POSTs to /api/<cmd> and streams events over /ws
//          (the puppetterm-server deployment).
//   mock — UI iteration only, enabled with VITE_PUPPETTERM_MOCK=1.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type { UnlistenFn };

const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
const useMock = import.meta.env.VITE_PUPPETTERM_MOCK === "1";

type Handler = (p: unknown) => void;

/** Call a backend command (IPC, HTTP or mock depending on the runtime). */
export async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri) return invoke<T>(cmd, args);
  if (useMock) return mockCall<T>(cmd, args ?? {});
  return webCall<T>(cmd, args ?? {});
}

/** Subscribe to a backend event stream. */
export async function on<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  if (isTauri) return listen<T>(event, (e) => handler(e.payload));
  const h = handler as (p: unknown) => void;
  if (useMock) {
    mockHandlers[event] = h;
    return () => {
      delete mockHandlers[event];
    };
  }
  ensureWebsocket();
  handlersByEvent.set(event, (handlersByEvent.get(event) ?? new Set()).add(h));
  return () => {
    const set = handlersByEvent.get(event);
    set?.delete(h);
    if (set && set.size === 0) handlersByEvent.delete(event);
  };
}

// ---- web transport ---------------------------------------------------------

async function webCall<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`/api/${encodeURIComponent(cmd)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(args),
    });
  } catch (e) {
    throw new Error(`backend unreachable (${String(e)})`);
  }
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const body = await res.json();
      if (body?.error) msg = String(body.error);
    } catch {
      /* non-JSON error body */
    }
    throw new Error(msg);
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

const handlersByEvent = new Map<string, Set<Handler>>();

let ws: WebSocket | null = null;
let wsBackoff = 500;

function dispatchEvent(event: string, payload: unknown) {
  handlersByEvent.get(event)?.forEach((h) => {
    try {
      h(payload);
    } catch (e) {
      console.error(`handler for ${event} failed`, e);
    }
  });
}

function connectWebsocket() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(`${proto}//${location.host}/ws`);

  ws.onmessage = (msg) => {
    try {
      const { event, payload } = JSON.parse(String(msg.data));
      if (typeof event === "string") dispatchEvent(event, payload);
    } catch {
      /* malformed frame */
    }
  };

  ws.onopen = () => {
    wsBackoff = 500;
  };

  // Auto-reconnect with capped backoff. Subscriptions live in
  // handlersByEvent, so nothing needs re-registering after a reconnect.
  ws.onclose = () => {
    ws = null;
    setTimeout(connectWebsocket, wsBackoff);
    wsBackoff = Math.min(wsBackoff * 2, 8000);
  };
}

function ensureWebsocket() {
  if (ws || useMock || isTauri) return;
  connectWebsocket();
}

// Reconnect promptly when the tab becomes visible again.
if (typeof document !== "undefined") {
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      if (!ws && !isTauri && !useMock) {
        wsBackoff = 0;
        connectWebsocket();
      }
    }
  });
}

// ---- browser mock (layout / UX iteration only) ---------------------------
const mockHandlers: Record<string, Handler> = {};
const mockSessions = new Map<number, string>();
let mockNextId = 1;

function emitMock(event: string, payload: unknown) {
  mockHandlers[event]?.(payload);
}

function mockCall<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  switch (cmd) {
    case "list_ssh_hosts":
      return Promise.resolve(["host-a", "host-b", "host-c"] as T);
    case "check_host":
      // host-a + host-b up, "host-c" down (shows both dot states)
      return Promise.resolve((args.host !== "host-c") as T);
    case "start_ssh_session": {
      const id = mockNextId++;
      const host = String(args.host);
      mockSessions.set(id, host);
      setTimeout(() => {
        emitMock("pty-output", {
          id,
          data: `\r\n\x1b[32m(mock shell) ${host}\x1b[0m\r\n$ `,
        });
      }, 250);
      return Promise.resolve(id as T);
    }
    case "start_local_session": {
      const id = mockNextId++;
      mockSessions.set(id, "local");
      setTimeout(() => {
        emitMock("pty-output", {
          id,
          data: `\r\n\x1b[32m(local shell)\x1b[0m\r\n$ `,
        });
      }, 250);
      return Promise.resolve(id as T);
    }
    case "write_ssh_input": {
      const id = Number(args.id);
      const data = String(args.data ?? "");
      if (data === "\r") {
        emitMock("pty-output", { id, data: "\r\n$ " });
      } else if (data === "\u0003") {
        emitMock("pty-output", { id, data: "^C\r\n$ " });
      } else if (!data.startsWith("\x1b")) {
        emitMock("pty-output", { id, data });
      }
      return Promise.resolve(undefined as T);
    }
    case "resize_ssh_pty":
      return Promise.resolve(undefined as T);
    case "stop_ssh_session": {
      const id = Number(args.id);
      mockSessions.delete(id);
      emitMock("pty-exit", { id });
      return Promise.resolve(undefined as T);
    }
    case "stop_agent_action":
      return Promise.resolve(true as T);
    case "check_agent":
      // host-a has the agent; others don't (demo the install hint)
      return Promise.resolve((args.host === "host-a") as T);
    case "install_agent_on_host": {
      const host = String(args.host ?? "");
      const steps = [
        `==> puppetterm-agent install on ${host} (x86_64)`,
        "==> installing binary (user-space)",
        "    authorized_keys updated (command-locked entry)",
        "    agent responded OK",
        `==> done: agent installed on ${host}`,
      ];
      steps.forEach((data, i) => setTimeout(() => emitMock("install-output", { host, data }), 200 * (i + 1)));
      return new Promise((resolve) =>
        setTimeout(
          () =>
            resolve({
              host,
              arch: "amd64",
              agent_path: "~/.puppetterm/bin/puppetterm-agent",
              mode: "user",
              sudoers: false,
            } as T),
          200 * (steps.length + 1),
        ),
      );
    }
    case "audit_recent":
      return Promise.resolve([
        {
          id: 1,
          ts: "2026-08-10T10:00:01Z",
          host: "host-a",
          source: "ai",
          action: "snapshot",
          params: "{}",
          approval: "auto",
          exit: 0,
          result: '{"exit":0}',
        },
        {
          id: 2,
          ts: "2026-08-10T10:01:33Z",
          host: "host-a",
          source: "ai",
          action: "service",
          params: '{"unit":"nginx","op":"restart"}',
          approval: "approved",
          exit: 0,
          result: '{"exit":0}',
        },
        {
          id: 3,
          ts: "2026-08-10T10:02:10Z",
          host: "host-a",
          source: "user",
          action: "run",
          params: '{"cmd":"whoami"}',
          approval: "auto",
          exit: 0,
          result: '{"exit":0}',
        },
      ] as T);
    case "get_ai_config":
      // Fresh-install state: no custom provider configured. The real app reads
      // ~/.config/puppetterm/ai.json (outside the repo); the browser mock must
      // NOT contain the developer's private endpoint/model.
      return Promise.resolve({
        base_url: "",
        model: "",
        provider: "openai",
        has_api_key: false,
      } as T);
    case "set_ai_config":
      return Promise.resolve(undefined as T);
    case "ai_chat": {
      const messages = (args.messages ?? []) as { role?: string; content?: string }[];
      const last = messages.filter((m) => m.role === "user").pop()?.content ?? "";
      return Promise.resolve({
        id: "mock-chat",
        choices: [
          {
            index: 0,
            finish_reason: "stop",
            message: {
              role: "assistant",
              content: `(mock AI) got: ${String(last).slice(0, 80)} — run the Tauri app to use the real endpoint.`,
            },
          },
        ],
        usage: null,
      } as T);
    }
    default:
      return Promise.resolve(undefined as T);
  }
}
