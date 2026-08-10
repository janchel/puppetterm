<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { Terminal } from "xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { onMount } from "svelte";

  // ---- reactive state ----------------------------------------------------
  let hosts = $state<string[]>([]);
  let statuses = $state<Record<string, boolean>>({});
  let activeHost = $state<string | null>(null);
  let sessionId = $state<number | null>(null);
  let busy = $state(false);

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

  // ---- non-reactive terminal objects --------------------------------------
  let term: Terminal | null = null;
  let fit: FitAddon | null = null;
  let container: HTMLDivElement;
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

  async function loadHosts() {
    try {
      hosts = await invoke<string[]>("list_ssh_hosts");
      for (const h of hosts) checkStatus(h);
    } catch (e) {
      console.error("loadHosts", e);
    }
  }

  async function checkStatus(h: string) {
    statuses[h] = await invoke<boolean>("check_host", { host: h });
  }

  async function openSession(host: string) {
    if (busy) return;
    if (host === activeHost && sessionId !== null) return;
    if (sessionId !== null) {
      await invoke("stop_ssh_session", { id: sessionId });
      sessionId = null;
    }
    activeHost = host;
    busy = true;
    term?.reset();
    term?.write(`\x1b[33m[puppetterm] connecting to ${host}...\x1b[0m\r\n`);
    try {
      const id = await invoke<number>("start_ssh_session", { host });
      sessionId = id;
      fitTerminal();
    } catch (e) {
      term?.write(`\r\n\x1b[31m[puppetterm] failed to connect: ${e}\x1b[0m\r\n`);
      activeHost = null;
    } finally {
      busy = false;
    }
  }

  async function closeSession() {
    if (sessionId !== null) {
      await invoke("stop_ssh_session", { id: sessionId });
      sessionId = null;
    }
    activeHost = null;
    term?.reset();
  }

  function fitTerminal() {
    fit?.fit();
    if (sessionId !== null && term) {
      invoke("resize_ssh_pty", {
        id: sessionId,
        cols: term.cols,
        rows: term.rows,
      });
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
    term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'JetBrains Mono','Fira Code',monospace",
      theme,
      scrollback: 10000,
    });
    fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);
    fit.fit();

    term.onData((data) => {
      if (sessionId !== null) {
        invoke("write_ssh_input", { id: sessionId, data });
      }
    });

    resizeObserver = new ResizeObserver(() => fitTerminal());
    resizeObserver.observe(container);

    // Async setup (listeners + host discovery) — kicked off, not awaited, so
    // the onMount cleanup can stay synchronous.
    (async () => {
      try {
        unlisteners = [
          await listen<{ id: number; data: string }>("pty-output", (e) => {
            if (e.payload.id === sessionId) term?.write(e.payload.data);
          }),
          await listen<{ id: number }>("pty-exit", (e) => {
            if (e.payload.id === sessionId) {
              term?.write("\r\n\x1b[90m[puppetterm] connection closed\x1b[0m\r\n");
              sessionId = null;
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
      if (sessionId !== null) {
        invoke("stop_ssh_session", { id: sessionId });
        sessionId = null;
      }
      term?.dispose();
    };
  });
</script>

<div class="shell">
  <!-- Left: agent list -->
  <aside class="pane agents">
    <div class="pane-title">
      <span>Agents</span>
      <button class="icon-btn" onclick={loadHosts} title="Refresh hosts">↻</button>
    </div>
    <div class="agent-list">
      {#if hosts.length === 0}
        <p class="muted">No hosts found in<br />~/.ssh/config</p>
      {:else}
        {#each hosts as h (h)}
          <button
            class="agent {activeHost === h ? 'active' : ''}"
            onclick={() => openSession(h)}
            title={statuses[h] ? 'reachable' : 'unreachable'}
          >
            <span class="dot {statuses[h] ? 'up' : 'down'}"></span>
            <span class="host">{h}</span>
          </button>
        {/each}
      {/if}
    </div>
  </aside>

  <!-- Center: terminal -->
  <section class="pane terminal">
    <div class="term-bar">
      {#if activeHost}
        <span class="term-host-label">
          <span class="dot up"></span>{activeHost}
          {#if busy}<span class="busy">connecting…</span>{/if}
        </span>
        <button class="close-btn" onclick={closeSession}>✕</button>
      {:else}
        <span class="muted">Select an agent to connect</span>
      {/if}
    </div>
    <div class="term-host" bind:this={container}></div>
  </section>

  <!-- Right: AI panel -->
  <aside class="pane ai">
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
        placeholder="Ask the AI to act on {activeHost ?? 'the active host'}…"
        bind:value={chatText}
        onkeydown={(e) => {
          if (e.key === "Enter") sendChat();
        }}
      />
      <button onclick={sendChat} disabled={!chatText.trim()}>Send</button>
    </div>
  </aside>
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: 220px 1fr 320px;
    height: 100vh;
    background: #0d1117;
  }

  .pane {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
  }

  .pane-title {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 12px;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: #8b949e;
    border-bottom: 1px solid #21262d;
  }

  .agents {
    border-right: 1px solid #21262d;
    background: #010409;
  }

  .agent-list {
    flex: 1;
    overflow-y: auto;
    padding: 6px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .agent {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 8px 10px;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: #e6edf3;
    font-size: 13px;
    text-align: left;
    cursor: pointer;
  }
  .agent:hover {
    background: #161b22;
  }
  .agent.active {
    background: #1f6feb26;
    outline: 1px solid #1f6feb;
  }
  .host {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    flex: none;
  }
  .dot.up {
    background: #3fb950;
    box-shadow: 0 0 6px #3fb95088;
  }
  .dot.down {
    background: #484f58;
  }

  .icon-btn,
  .close-btn {
    border: none;
    background: transparent;
    color: #8b949e;
    cursor: pointer;
    font-size: 14px;
    border-radius: 4px;
    padding: 2px 6px;
  }
  .icon-btn:hover,
  .close-btn:hover {
    background: #21262d;
    color: #e6edf3;
  }

  .terminal {
    background: #0d1117;
  }

  .term-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 38px;
    padding: 0 12px;
    border-bottom: 1px solid #21262d;
    font-size: 13px;
    background: #010409;
  }
  .term-host-label {
    display: flex;
    align-items: center;
    gap: 8px;
    color: #e6edf3;
  }
  .busy {
    color: #d29922;
    font-size: 12px;
  }
  .term-host {
    flex: 1;
    min-height: 0;
    padding: 4px 6px;
  }

  .ai {
    border-left: 1px solid #21262d;
    background: #010409;
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
