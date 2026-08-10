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

  // AI panel settings (persisted). Chat itself arrives in Phase 5.
  let model = $state(
    typeof localStorage !== "undefined"
      ? (localStorage.getItem("pp.model") ?? "claude-sonnet-4-5")
      : "claude-sonnet-4-5",
  );
  let autonomy = $state(
    typeof localStorage !== "undefined"
      ? (localStorage.getItem("pp.autonomy") ?? "ask-first")
      : "ask-first",
  );
  let chatText = $state("");
  let chatLog = $state<Array<{ role: string; text: string }>>([]);

  $effect(() => {
    localStorage.setItem("pp.model", model);
  });
  $effect(() => {
    localStorage.setItem("pp.autonomy", autonomy);
  });

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

  function sendChat() {
    const text = chatText.trim();
    if (!text) return;
    chatLog = [...chatLog, { role: "user", text }];
    chatText = "";
    chatLog = [...chatLog, { role: "ai", text: "(AI chat arrives in Phase 5)" }];
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
          Model
          <select bind:value={model}>
            <option value="claude-sonnet-4-5">Claude Sonnet 4.5</option>
            <option value="claude-opus-4-1">Claude Opus 4.1</option>
            <option value="claude-haiku-4-5">Claude Haiku 4.5</option>
          </select>
        </label>
        <label>
          Autonomy
          <select bind:value={autonomy}>
            <option value="ask-first">Ask first (default)</option>
            <option value="read-only-auto">Read-only auto</option>
          </select>
        </label>
      </div>
      <div class="chat-log">
        {#if chatLog.length === 0}
          <p class="muted">Chat is wired up in Phase 5.</p>
        {/if}
        {#each chatLog as m, i (i)}
          <div class="msg {m.role}">{m.text}</div>
        {/each}
      </div>
      <div class="chat-input">
        <input
          placeholder="Ask the AI to act on the active host…"
          bind:value={chatText}
          onkeydown={(e) => {
            if (e.key === "Enter") sendChat();
          }}
        />
        <button onclick={sendChat} disabled={!chatText.trim()}>Send</button>
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
  .chat-input input:focus {
    border-color: #1f6feb;
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
