<script lang="ts">
  import { call, on, type UnlistenFn } from "$lib/backend";
  import { Terminal } from "xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { onMount, tick } from "svelte";

  type Tab = {
    id: number;
    host: string;
    sessionId: number | null;
    connecting: boolean;
  };

  // ---- reactive state ----------------------------------------------------
  let hosts = $state<string[]>([]);
  let statuses = $state<Record<string, boolean>>({});
  let tabs = $state<Tab[]>([]);
  let activeTabId = $state<number | null>(null);
  let showHostMenu = $state(false);

  // The host the AI chat binds to (the active session's host).
  let activeHost = $derived(tabs.find((t) => t.id === activeTabId)?.host ?? null);

  // ---- AI integration (OpenAI-compatible) --------------------------------
  let aiBaseUrl = $state("");
  let aiModel = $state("");
  let aiKey = $state("");
  let aiHasKey = $state(false);
  let aiReady = $state(false);
  let chatBusy = $state(false);
  let chatText = $state("");
  let chatLog = $state<Array<{ role: string; text: string }>>([]);
  let history = $state<any[]>([]);
  let autonomy = $state(
    typeof localStorage !== "undefined"
      ? (localStorage.getItem("pp.autonomy") ?? "ask-first")
      : "ask-first",
  );
  let pendingApproval = $state<{
    id: string;
    tool: string;
    args: Record<string, unknown>;
    resolve: (ok: boolean) => void;
  } | null>(null);

  $effect(() => {
    localStorage.setItem("pp.autonomy", autonomy);
  });

  const SYSTEM_PROMPT =
    "You are puppetterm, an AI assistant inside a terminal app. You manage the ACTIVE host " +
    "using the provided tools. Prefer the structured tools (service/log/config/snapshot) over " +
    "run_command. State-changing actions are approved by the user before execution; you will be " +
    "told if one is rejected. Be concise and summarize tool results for the user.";

  const AGENT_TOOLS = [
    { type: "function", function: { name: "run_command", description: "Run an arbitrary shell command on the active host (always approved first).", parameters: { type: "object", properties: { cmd: { type: "string" }, dir: { type: "string" } }, required: ["cmd"] } } },
    { type: "function", function: { name: "snapshot", description: "System snapshot of the active host: CPU, memory, disk, uptime.", parameters: { type: "object", properties: {} } } },
    { type: "function", function: { name: "service", description: "Control a systemd service on the active host.", parameters: { type: "object", properties: { unit: { type: "string" }, op: { type: "string", enum: ["status", "is-active", "is-enabled", "start", "stop", "restart", "enable", "disable"] } }, required: ["unit", "op"] } } },
    { type: "function", function: { name: "log", description: "Tail a log file on the active host (allow-listed paths).", parameters: { type: "object", properties: { path: { type: "string" }, lines: { type: "number" }, follow: { type: "boolean" } }, required: ["path"] } } },
    { type: "function", function: { name: "config", description: "Read or write a config file on the active host (allow-listed paths).", parameters: { type: "object", properties: { path: { type: "string" }, op: { type: "string", enum: ["read", "write"] }, content: { type: "string" } }, required: ["path", "op"] } } },
  ];

  const TOOL_TO_ACTION: Record<string, string> = {
    run_command: "run",
    snapshot: "snapshot",
    service: "service",
    log: "log",
    config: "config",
  };

  // ---- terminal plumbing (per-tab, non-reactive) --------------------------
  let viewports = $state<Record<number, HTMLDivElement>>({});
  const termByTab = new Map<number, { term: Terminal; fit: FitAddon }>();
  let nextTabId = 1;

  let terminalArea: HTMLElement;
  let resizeObserver: ResizeObserver | null = null;
  let unlisteners: UnlistenFn[] = [];

  const theme = {
    background: "#0d1117",
    foreground: "#e6edf3",
    cursor: "#58a6ff",
    selectionBackground: "#264f78",
    black: "#0d1117",
    brightBlack: "#484f58",
    red: "#ff7b72",
    brightRed: "#ffa198",
    green: "#3fb950",
    brightGreen: "#56d364",
    yellow: "#d29922",
    brightYellow: "#e3b341",
    blue: "#58a6ff",
    brightBlue: "#79c0ff",
    magenta: "#bc8cff",
    brightMagenta: "#d2a8ff",
    cyan: "#39c5cf",
    brightCyan: "#56d4dd",
    white: "#b1bac4",
    brightWhite: "#f0f6fc",
  };

  function tabById(id: number): Tab | undefined {
    return tabs.find((t) => t.id === id);
  }

  async function loadHosts() {
    try {
      hosts = await call<string[]>("list_ssh_hosts");
      for (const h of hosts) checkStatus(h);
    } catch (e) {
      console.error("loadHosts", e);
    }
  }

  async function checkStatus(h: string) {
    statuses[h] = await call<boolean>("check_host", { host: h });
  }

  async function openTab(host: string) {
    // Reuse an existing tab for the same host instead of duplicating.
    const existing = tabs.find((t) => t.host === host);
    if (existing) {
      showHostMenu = false;
      await activateTab(existing.id);
      return;
    }

    const id = nextTabId++;
    tabs = [...tabs, { id, host, sessionId: null, connecting: false }];
    activeTabId = id;
    showHostMenu = false;
    await tick();

    const el = viewports[id];
    if (!el) return;

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'JetBrains Mono','Fira Code',monospace",
      theme,
      scrollback: 10000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    fit.fit();

    term.onData((data) => {
      const t = tabById(id);
      if (t?.sessionId != null) call("write_ssh_input", { id: t.sessionId, data });
    });

    termByTab.set(id, { term, fit });
    startSession(id, host);
  }

  async function startSession(id: number, host: string) {
    const t = tabById(id);
    if (!t) return;
    t.connecting = true;
    termByTab.get(id)?.term.write(`\x1b[33m[puppetterm] connecting to ${host}...\x1b[0m\r\n`);
    try {
      const sessionId = await call<number>("start_ssh_session", { host });
      t.sessionId = sessionId;
      fitTab(id);
    } catch (e) {
      termByTab
        .get(id)
        ?.term.write(`\r\n\x1b[31m[puppetterm] failed to connect: ${e}\x1b[0m\r\n`);
    } finally {
      t.connecting = false;
    }
  }

  async function activateTab(id: number) {
    if (activeTabId === id) return;
    activeTabId = id;
    await tick();
    fitTab(id);
  }

  function fitTab(id: number) {
    const entry = termByTab.get(id);
    if (!entry) return;
    entry.fit.fit();
    const t = tabById(id);
    if (t?.sessionId != null) {
      call("resize_ssh_pty", {
        id: t.sessionId,
        cols: entry.term.cols,
        rows: entry.term.rows,
      });
    }
  }

  async function closeTab(id: number) {
    const t = tabById(id);
    if (t?.sessionId != null) await call("stop_ssh_session", { id: t.sessionId });
    termByTab.get(id)?.term.dispose();
    termByTab.delete(id);
    delete viewports[id];

    const idx = tabs.findIndex((x) => x.id === id);
    tabs = tabs.filter((x) => x.id !== id);

    if (activeTabId === id) {
      const next = tabs[idx] ?? tabs[idx - 1] ?? null;
      activeTabId = next ? next.id : null;
      if (next) {
        await tick();
        fitTab(next.id);
      }
    }
  }

  function pushChat(role: string, text: string) {
    chatLog = [...chatLog, { role, text }];
  }

  function safeParse(s: string): Record<string, unknown> {
    try {
      const v = JSON.parse(s);
      return v && typeof v === "object" ? v : {};
    } catch {
      return {};
    }
  }

  function activeTerm(): Terminal | null {
    if (activeTabId == null) return null;
    return termByTab.get(activeTabId)?.term ?? null;
  }

  function toolReadOnly(tool: string, args: Record<string, unknown>): boolean {
    if (tool === "snapshot" || tool === "log") return true;
    if (tool === "service") {
      const op = String(args.op ?? "");
      return op === "status" || op === "is-active" || op === "is-enabled";
    }
    if (tool === "config") return args.op === "read";
    return false; // run_command and anything else asks
  }

  async function saveAiConfig() {
    try {
      await call("set_ai_config", { baseUrl: aiBaseUrl, model: aiModel, apiKey: aiKey });
      aiKey = "";
      const v = await call<any>("get_ai_config");
      aiBaseUrl = v.base_url;
      aiModel = v.model;
      aiHasKey = v.has_api_key;
      aiReady = true;
      pushChat("ai", "(AI settings saved)");
    } catch (e) {
      pushChat("ai", `(failed to save AI settings: ${e})`);
    }
  }

  async function sendChat() {
    const text = chatText.trim();
    if (!text) return;
    if (!activeHost) {
      pushChat("ai", "(open a session first — the AI acts on the active host)");
      return;
    }
    if (!aiReady) {
      pushChat("ai", "(AI not configured — set the endpoint/model in settings)");
      return;
    }
    chatText = "";
    pushChat("user", text);
    history = [...history, { role: "user", content: text }];
    chatBusy = true;
    try {
      await runAiLoop();
    } finally {
      chatBusy = false;
    }
  }

  async function runAiLoop() {
    let guard = 0;
    while (guard++ < 10) {
      const resp = await call<any>("ai_chat", { messages: history, tools: AGENT_TOOLS });
      const msg = resp?.choices?.[0]?.message;
      if (!msg) {
        pushChat("ai", "(no response from the model)");
        return;
      }
      if (msg.tool_calls && msg.tool_calls.length > 0) {
        history = [
          ...history,
          { role: "assistant", content: msg.content ?? null, tool_calls: msg.tool_calls },
        ];
        for (const tc of msg.tool_calls) {
          const ok = await requestApproval(tc);
          const content = ok
            ? JSON.stringify(await executeTool(tc))
            : JSON.stringify({ status: "rejected", reason: "user rejected the action" });
          history = [...history, { role: "tool", tool_call_id: tc.id, content }];
        }
        continue;
      }
      const text = msg.content ?? "(done)";
      pushChat("ai", text);
      history = [...history, { role: "assistant", content: text }];
      return;
    }
    pushChat("ai", "(stopped after too many tool steps)");
  }

  function requestApproval(tc: { id: string; function: { name: string; arguments: string } }): Promise<boolean> {
    const name = tc.function.name;
    const args = safeParse(tc.function.arguments);
    if (autonomy === "read-only-auto" || toolReadOnly(name, args)) {
      return Promise.resolve(true);
    }
    return new Promise((resolve) => {
      pendingApproval = { id: tc.id, tool: name, args, resolve };
    });
  }

  function approve() {
    pendingApproval?.resolve(true);
    pendingApproval = null;
  }

  function reject() {
    pendingApproval?.resolve(false);
    pendingApproval = null;
  }

  async function executeTool(tc: { id: string; function: { name: string; arguments: string } }) {
    const name = tc.function.name;
    const args = safeParse(tc.function.arguments);
    const action = TOOL_TO_ACTION[name] ?? "run";
    const term = activeTerm();
    if (term) {
      term.write(`\r\n\x1b[36m[puppetterm] AI → ${name} ${JSON.stringify(args)}\x1b[0m\r\n`);
    }
    const request = { action, params: args, request_id: tc.id };
    const res = await call<any>("run_agent_action", {
      host: activeHost,
      request: JSON.stringify(request),
    });
    for (const ev of res?.events ?? []) {
      if (ev?.type === "output" && term) term.write(ev.data ?? "");
    }
    if (res?.error && term) {
      term.write(`\r\n\x1b[31m[puppetterm] action error: ${res.error}\x1b[0m\r\n`);
    }
    const resultEvent = [...(res?.events ?? [])].reverse().find((e: any) => e?.type === "result");
    const outputs = (res?.events ?? [])
      .filter((e: any) => e?.type === "output")
      .map((e: any) => e.data ?? "")
      .join("")
      .slice(-4000);
    return {
      host: activeHost,
      exit: resultEvent?.exit ?? res?.exit ?? null,
      outputs,
      structured: resultEvent?.structured ?? null,
      error: res?.error ?? null,
    };
  }

  onMount(() => {
    resizeObserver = new ResizeObserver(() => {
      if (activeTabId != null) fitTab(activeTabId);
    });
    if (terminalArea) resizeObserver.observe(terminalArea);

    // Async setup (listeners + host discovery) — kicked off, not awaited, so
    // the onMount cleanup can stay synchronous.
    (async () => {
      try {
        unlisteners = [
          await on<{ id: number; data: string }>("pty-output", (p) => {
            const t = tabs.find((x) => x.sessionId === p.id);
            if (t) termByTab.get(t.id)?.term.write(p.data);
          }),
          await on<{ id: number }>("pty-exit", (p) => {
            const t = tabs.find((x) => x.sessionId === p.id);
            if (t) {
              t.sessionId = null;
              termByTab
                .get(t.id)
                ?.term.write("\r\n\x1b[90m[puppetterm] connection closed\x1b[0m\r\n");
            }
          }),
        ];
      } catch (e) {
        console.warn("event listeners unavailable:", e);
      }
      await loadHosts();
      try {
        const v = await call<any>("get_ai_config");
        aiBaseUrl = v.base_url;
        aiModel = v.model;
        aiHasKey = v.has_api_key;
        aiReady = true;
        history = [{ role: "system", content: SYSTEM_PROMPT }];
      } catch (e) {
        console.warn("ai config unavailable:", e);
      }
    })();

    return () => {
      unlisteners.forEach((u) => u());
      resizeObserver?.disconnect();
      for (const t of tabs) {
        if (t.sessionId != null) call("stop_ssh_session", { id: t.sessionId });
        termByTab.get(t.id)?.term.dispose();
      }
      termByTab.clear();
    };
  });
</script>

<div class="app">
  <header class="topbar">
    <div class="brand">puppetterm</div>
    <nav class="tabs">
      {#each tabs as t (t.id)}
        <div
          class="tab {t.id === activeTabId ? 'active' : ''}"
          role="button"
          tabindex="0"
          title={t.host}
          onclick={() => activateTab(t.id)}
          onkeydown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              activateTab(t.id);
            }
          }}
        >
          <span class="dot {t.connecting ? 'busy' : t.sessionId != null ? 'up' : 'down'}"></span>
          <span class="tab-host">{t.host}</span>
          <button
            class="tab-close"
            type="button"
            aria-label={`close ${t.host}`}
            onclick={(e) => {
              e.stopPropagation();
              closeTab(t.id);
            }}
          >×</button>
        </div>
      {/each}

      <span class="new-wrap">
        <button class="new-host" onclick={() => (showHostMenu = !showHostMenu)}>+ New</button>
        {#if showHostMenu}
          <div class="host-menu">
            {#if hosts.length === 0}
              <div class="menu-item muted">No hosts in ~/.ssh/config</div>
            {:else}
              {#each hosts as h (h)}
                <button class="menu-item" onclick={() => openTab(h)}>
                  <span class="dot {statuses[h] ? 'up' : 'down'}"></span>{h}
                </button>
              {/each}
            {/if}
          </div>
        {/if}
      </span>

      <button class="refresh" onclick={loadHosts} title="Refresh hosts">↻</button>
    </nav>
  </header>

  <main class="body">
    <section class="term-area" bind:this={terminalArea}>
      {#if tabs.length === 0}
        <div class="placeholder">
          No open sessions — click <b>+ New</b> to connect.
        </div>
      {/if}
      {#each tabs as t (t.id)}
        <div
          class="term-viewport {t.id === activeTabId ? 'active' : ''}"
          bind:this={viewports[t.id]}
        ></div>
      {/each}
    </section>

    <aside class="ai-panel">
      <div class="pane-title">AI</div>
      <div class="ai-opts">
        <label>
          Endpoint
          <input bind:value={aiBaseUrl} placeholder="http://host:port/v1" />
        </label>
        <label>
          Model
          <input bind:value={aiModel} placeholder="model-name" />
        </label>
        <label>
          API key
          <input
            bind:value={aiKey}
            type="password"
            placeholder={aiHasKey ? "••• (set) — type to replace" : "sk-…"}
          />
        </label>
        <label>
          Autonomy
          <select bind:value={autonomy}>
            <option value="ask-first">Ask first (default)</option>
            <option value="read-only-auto">Read-only auto</option>
          </select>
        </label>
        <button
          class="save-btn"
          onclick={saveAiConfig}
          disabled={!aiBaseUrl.trim() || !aiModel.trim()}
        >
          Save AI settings
        </button>
      </div>

      {#if pendingApproval}
        <div class="approval">
          <div class="approval-label">Approve action?</div>
          <div class="approval-cmd">
            {pendingApproval.tool} {JSON.stringify(pendingApproval.args)}
          </div>
          <div class="approval-btns">
            <button onclick={reject}>Reject</button>
            <button class="primary" onclick={approve}>Approve</button>
          </div>
        </div>
      {/if}

      <div class="chat-log">
        {#if chatLog.length === 0}
          <p class="muted">Ask the AI to inspect or change the active host.</p>
        {/if}
        {#each chatLog as m, i (i)}
          <div class="msg {m.role}">{m.text}</div>
        {/each}
      </div>
      <div class="chat-input">
        <input
          placeholder="Ask the AI to act on {activeHost ?? 'the active host'}…"
          bind:value={chatText}
          onkeydown={(e) => {
            if (e.key === "Enter") sendChat();
          }}
        />
        <button onclick={sendChat} disabled={!chatText.trim() || chatBusy}>
          {chatBusy ? "…" : "Send"}
        </button>
      </div>
    </aside>
  </main>
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
    background: #0d1117;
    color: #e6edf3;
  }

  /* ---- top bar with tabs ---- */
  .topbar {
    display: flex;
    align-items: stretch;
    flex: none;
    position: relative;
    z-index: 10;
    height: 40px;
    background: #010409;
    border-bottom: 1px solid #21262d;
  }
  .brand {
    display: flex;
    align-items: center;
    padding: 0 14px;
    font-weight: 800;
    letter-spacing: 0.02em;
    color: #58a6ff;
    border-right: 1px solid #21262d;
    white-space: nowrap;
  }
  .tabs {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 0 8px;
    overflow-x: auto;
    flex: 1;
    min-width: 0;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 7px;
    height: 26px;
    padding: 0 8px 0 10px;
    border: 1px solid transparent;
    border-radius: 6px;
    background: transparent;
    color: #8b949e;
    font-size: 12.5px;
    cursor: pointer;
    white-space: nowrap;
  }
  .tab:hover {
    background: #161b22;
    color: #e6edf3;
  }
  .tab.active {
    background: #161b22;
    border-color: #30363d;
    color: #e6edf3;
  }
  .tab-host {
    max-width: 160px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .tab-close {
    border: none;
    background: transparent;
    cursor: pointer;
    border-radius: 4px;
    padding: 0 4px;
    font-size: 14px;
    line-height: 1;
    color: #8b949e;
  }
  .tab-close:hover {
    background: #da3633;
    color: #fff;
  }
  .new-wrap {
    position: relative;
    display: inline-flex;
  }
  .new-host,
  .refresh {
    height: 26px;
    border: 1px solid #30363d;
    border-radius: 6px;
    background: #0d1117;
    color: #e6edf3;
    font-size: 12.5px;
    cursor: pointer;
    padding: 0 10px;
  }
  .new-host:hover,
  .refresh:hover {
    background: #21262d;
  }
  .host-menu {
    position: absolute;
    top: 30px;
    left: 0;
    min-width: 200px;
    background: #161b22;
    border: 1px solid #30363d;
    border-radius: 8px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
    z-index: 20;
    padding: 4px;
    display: flex;
    flex-direction: column;
  }
  .menu-item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 7px 10px;
    border: none;
    border-radius: 5px;
    background: transparent;
    color: #e6edf3;
    font-size: 13px;
    text-align: left;
    cursor: pointer;
  }
  .menu-item:hover {
    background: #1f6feb;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }
  .dot.up {
    background: #3fb950;
  }
  .dot.down {
    background: #484f58;
  }
  .dot.busy {
    background: #d29922;
    animation: pulse 1s infinite alternate;
  }
  @keyframes pulse {
    from {
      opacity: 0.4;
    }
    to {
      opacity: 1;
    }
  }

  /* ---- body: terminal + AI ---- */
  .body {
    display: flex;
    flex: 1;
    min-height: 0;
    position: relative;
  }

  .term-area {
    position: relative;
    flex: 1;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
    background: #0d1117;
  }
  .term-viewport {
    position: absolute;
    inset: 0;
    display: none;
    padding: 6px;
  }
  .term-viewport.active {
    display: block;
  }
  .placeholder {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #8b949e;
    font-size: 14px;
  }

  /* ---- AI panel (right) ---- */
  .ai-panel {
    display: flex;
    flex-direction: column;
    position: relative;
    z-index: 5;
    width: 300px;
    min-width: 260px;
    border-left: 1px solid #21262d;
    background: #010409;
  }
  .pane-title {
    padding: 10px 12px;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: #8b949e;
    border-bottom: 1px solid #21262d;
  }
  .ai-opts {
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    border-bottom: 1px solid #21262d;
  }
  .ai-opts label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
    color: #8b949e;
    font-weight: 600;
  }
  .ai-opts select,
  .ai-opts input,
  .chat-input input {
    background: #0d1117;
    border: 1px solid #30363d;
    border-radius: 6px;
    color: #e6edf3;
    padding: 6px 8px;
    font-size: 13px;
    outline: none;
  }
  .ai-opts select:focus,
  .ai-opts input:focus,
  .chat-input input:focus {
    border-color: #1f6feb;
  }
  .save-btn {
    background: #21262d;
    border: 1px solid #30363d;
    border-radius: 6px;
    color: #e6edf3;
    padding: 6px 8px;
    font-size: 12.5px;
    font-weight: 600;
    cursor: pointer;
  }
  .save-btn:hover:not(:disabled) {
    background: #1f6feb;
  }
  .save-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .approval {
    margin: 8px 12px;
    border: 1px solid #d29922;
    border-radius: 8px;
    background: #161b22;
    padding: 10px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .approval-label {
    font-size: 12px;
    font-weight: 700;
    color: #d29922;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .approval-cmd {
    font-family: monospace;
    font-size: 12px;
    background: #0d1117;
    border-radius: 6px;
    padding: 6px 8px;
    word-break: break-all;
    white-space: pre-wrap;
  }
  .approval-btns {
    display: flex;
    gap: 8px;
  }
  .approval-btns button {
    flex: 1;
    border: 1px solid #30363d;
    background: #21262d;
    color: #e6edf3;
    border-radius: 6px;
    padding: 6px 0;
    cursor: pointer;
    font-weight: 600;
  }
  .approval-btns button.primary {
    background: #1f6feb;
    border-color: #1f6feb;
    color: #fff;
  }
  .approval-btns button:hover {
    filter: brightness(1.1);
  }
  .chat-log {
    flex: 1;
    overflow-y: auto;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    font-size: 13px;
  }
  .msg {
    border-radius: 8px;
    padding: 8px 10px;
    line-height: 1.4;
    white-space: pre-wrap;
  }
  .msg.user {
    background: #1f6feb26;
    align-self: flex-end;
    max-width: 90%;
  }
  .msg.ai {
    background: #161b22;
    align-self: flex-start;
    max-width: 90%;
  }
  .chat-input {
    display: flex;
    gap: 6px;
    padding: 10px 12px;
    border-top: 1px solid #21262d;
  }
  .chat-input input {
    flex: 1;
  }
  .chat-input button {
    background: #1f6feb;
    border: none;
    border-radius: 6px;
    color: #fff;
    font-weight: 600;
    padding: 0 14px;
    cursor: pointer;
  }
  .chat-input button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .muted {
    color: #8b949e;
    font-size: 12px;
    line-height: 1.5;
  }
</style>
