// Frontend ↔ backend bridge.
//
// Under Tauri this calls the real Rust commands via IPC. In a plain browser
// (vite dev/preview) it falls back to a lightweight mock so the UI can be
// iterated on without rebuilding the native app.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type { UnlistenFn };

const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Call a Rust command (or its mock in the browser). */
export async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri) return invoke<T>(cmd, args);
  return mockCall<T>(cmd, args ?? {});
}

/** Subscribe to a backend event (or the mock emitter in the browser). */
export async function on<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  if (isTauri) return listen<T>(event, (e) => handler(e.payload));
  mockHandlers[event] = handler as (p: unknown) => void;
  return () => {
    delete mockHandlers[event];
  };
}

// ---- browser mock (layout / UX iteration only) ---------------------------
const mockHandlers: Record<string, (p: unknown) => void> = {};
const mockSessions = new Map<number, string>();
let mockNextId = 1;

function emitMock(event: string, payload: unknown) {
  mockHandlers[event]?.(payload);
}

function mockCall<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  switch (cmd) {
    case "list_ssh_hosts":
      return Promise.resolve(["server1", "local-lab", "staging"] as T);
    case "check_host":
      // server1 + local-lab up, "staging" down (shows both dot states)
      return Promise.resolve((args.host !== "staging") as T);
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
    case "get_ai_config":
      return Promise.resolve({
        base_url: "http://192.168.5.52:20128/v1",
        model: "jandelcombo",
        has_api_key: true,
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
