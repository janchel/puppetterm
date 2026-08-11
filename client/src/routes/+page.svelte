<script lang="ts">
  import { call, on, type UnlistenFn } from "$lib/backend";
  import { Terminal } from "xterm";
  import "xterm/css/xterm.css";
  import { FitAddon } from "@xterm/addon-fit";
  import { writeText as tauriWriteText, readText as tauriReadText } from "@tauri-apps/plugin-clipboard-manager";
  import { onMount, tick } from "svelte";

  const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

  type Tab = {
    id: number;
    host: string; // remote target ("" = local shell); updated when `ssh <target>` is detected
    sessionId: number | null;
    connecting: boolean;
    buf: string; // input buffer used to detect `ssh <target>` commands
  };

  // ---- reactive state ----------------------------------------------------
  let hosts = $state<string[]>([]);
  let statuses = $state<Record<string, boolean>>({});
  let tabs = $state<Tab[]>([]);
  let activeTabId = $state<number | null>(null);
  let showHostMenu = $state(false);

  // The host the AI chat binds to (the active session's host).
  let activeHost = $derived(tabs.find((t) => t.id === activeTabId)?.host ?? null);

  // Pinned at send time: the terminal + host a chat task is acting on. Kept so
  // switching tabs mid-task can never redirect the AI to a different server.
  let chatTarget = $state<{ host: string; tabId: number } | null>(null);

  // ---- agent install (in-terminal, approval-style: install the agent on the connected host) ---
  let installPrompt = $state<{ tabId: number; host: string } | null>(null); // awaiting y/n in the terminal
  let installBusy = $state(false);
  let installTabId = $state<number | null>(null); // route install-output events here
  let agentChecked = $state<Set<string>>(new Set()); // hosts we've already hinted about

  // AI panel width (persisted; draggable splitter).
  let aiWidth = $state(
    typeof localStorage !== "undefined"
      ? (Number(localStorage.getItem("pp.aiWidth")) || 320)
      : 320,
  );
  $effect(() => {
    localStorage.setItem("pp.aiWidth", String(aiWidth));
  });
  let resizing = $state(false);

  // ---- AI integration (multi-provider) ----------------------------------
  let aiBaseUrl = $state("");
  let aiModel = $state("");
  let aiProvider = $state("openai");
  let aiKey = $state("");
  let aiHasKey = $state(false);
  let aiReady = $state(false);
  // The user's own OpenAI-compatible endpoint (the `openai` provider has no
  // preset URL). Remembered so switching to DeepSeek/Claude and back restores
  // it — otherwise the model switcher leaves a stale preset URL behind.
  let customBaseUrl = $state("");
  // Lightweight toast for quick confirmations (e.g. "AI settings saved").
  let toast = $state<{ msg: string; kind: "ok" | "err" } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;
  function notify(msg: string, kind: "ok" | "err" = "ok") {
    toast = { msg, kind };
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 3500);
  }

  // Provider presets: predefined endpoint + default models.
  const AI_PROVIDERS: Record<
    string,
    { label: string; baseUrl: string; model: string; models: string[] }
  > = {
    openai: {
      label: "Custom (OpenAI-compatible)",
      baseUrl: "",
      model: "",
      models: ["gpt-4o", "gpt-4o-mini", "gpt-4.1"],
    },
    deepseek: {
      label: "DeepSeek",
      baseUrl: "https://api.deepseek.com/v1",
      model: "deepseek-chat",
      models: ["deepseek-chat", "deepseek-reasoner"],
    },
    anthropic: {
      label: "Claude (Anthropic)",
      baseUrl: "https://api.anthropic.com/v1",
      model: "claude-sonnet-4-20250514",
      models: ["claude-sonnet-4-20250514", "claude-3-5-haiku-latest", "claude-opus-4-20250514"],
    },
  };

  function applyAiProvider(p: string) {
    aiProvider = p;
    const preset = AI_PROVIDERS[p];
    if (preset && preset.baseUrl) aiBaseUrl = preset.baseUrl;
    else if (p === "openai") aiBaseUrl = customBaseUrl; // restore the custom endpoint
    if (preset?.model) aiModel = preset.model;
  }

  // Every model across all provider presets, plus whatever is currently saved.
  let allModels = $derived.by(() => {
    const set = new Set<string>();
    for (const p of Object.values(AI_PROVIDERS)) for (const m of p.models) set.add(m);
    if (aiModel) set.add(aiModel);
    return [...set];
  });

  /** Pick a model in the chat panel. If it belongs to a provider preset,
   *  switch the provider + endpoint too (the custom/openai preset restores the
   *  remembered custom endpoint); then persist. */
  function applyAiModel(m: string) {
    aiModel = m;
    for (const [key, p] of Object.entries(AI_PROVIDERS)) {
      if (p.models.includes(m)) {
        aiProvider = key;
        if (p.baseUrl) aiBaseUrl = p.baseUrl;
        else if (key === "openai") aiBaseUrl = customBaseUrl;
        break;
      }
    }
    aiReady = true;
    call("set_ai_config", {
      baseUrl: aiBaseUrl,
      model: aiModel,
      provider: aiProvider,
    }).catch((e) => console.error("save model", e));
  }
  let chatBusy = $state(false);
  let aiThinking = $state(false);
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
    danger?: boolean;
    explain?: string;
    resolve: (ok: boolean) => void;
  } | null>(null);
  let abortRequested = $state(false); // user hit Abort — stop starting new tool calls
  let currentRequestId = $state<string | null>(null); // in-flight agent request (for abort)
  let activity = $state<any[]>([]); // recent audit entries (what the AI did)
  let showActivity = $state(false);

  $effect(() => {
    localStorage.setItem("pp.autonomy", autonomy);
  });

  // ---- settings modal + theme ----------------------------------------------
  let showSettings = $state(false);
  // Single theme for now (dark); kept as a setting so more can be added later.
  let themeName = $state(
    typeof localStorage !== "undefined"
      ? (localStorage.getItem("pp.theme") ?? "dark")
      : "dark",
  );
  $effect(() => {
    localStorage.setItem("pp.theme", themeName);
    document.documentElement.dataset.theme = themeName;
  });

  // Guardrails: destructive commands get a red-flagged approval and are never
  // auto-run. The AI still CAN run them, but only with an explicit Approve.
  const DANGEROUS_PATTERNS = [
    // rm ... --no-preserve-root (anywhere in the command)
    /\brm\b[^;&|]*--?no-preserve-root\b/,
    // rm -rf / or rm -rf /*
    /\brm\s+(-[a-zA-Z]*[rf][a-zA-Z]*\s+)+(?:\/|\/\*)\s*$/,
    // mkfs / mkfs.ext4
    /\bmkfs(\.\w+)?\b/,
    // dd of=/dev/...
    /\bdd\b[^;&|]*\bof=\/dev\//,
    // write directly to a block device
    /\b>\s*\/dev\/(?:sd|vd|nvme)/,
    // power state changes
    /\b(?:shutdown|reboot|halt|poweroff)\b/,
    // fork bomb
    /:\s*\(\s*\)\s*\{\s*:\s*\|\s*:&\s*\}/,
    // chmod -R 777 /
    /\bchmod\b[^;&|]*-R\s+777\s+\//,
    // mv /
    /\bmv\s+\/\s+/,
    // init 0 / init 6
    /\binit\s+[06]\b/,
  ];

  function isDangerous(tool: string, args: Record<string, unknown>): boolean {
    if (tool === "run_command" || tool === "terminal") {
      const cmd =
        tool === "run_command"
          ? String(args.cmd ?? "")
          : String(args.command ?? "");
      return DANGEROUS_PATTERNS.some((re) => re.test(cmd));
    }
    return false;
  }

  const SYSTEM_PROMPT =
    "You are puppetterm, an AI assistant inside a terminal app. You manage the ACTIVE host " +
    "using the provided tools.\n\n" +
    "ANSWER QUESTIONS FIRST. When the user asks a question, answer it directly from your own " +
    "knowledge in text BEFORE calling any tool. Only run a command when you genuinely need LIVE " +
    "system state (current disk/memory, a service's real status, today's logs) or when the user " +
    "asked you to take an action. For general-knowledge questions, just give the answer and DO " +
    "NOT run a command at all.\n\n" +
    "TO RUN A COMMAND, ALWAYS use the `terminal` tool first: it types the command into the user's " +
    "LIVE terminal (which is already logged in) and returns the output — it works on ANY host, " +
    "including password-only ones, and needs no key or agent. The structured tools " +
    "(run_command/snapshot/service/log/config) open a SEPARATE ssh connection and only work on " +
    "key-based hosts with the puppetterm-agent installed; if one fails with an SSH permission " +
    "error, the app retries in the live terminal automatically.\n\n" +
    "Before running anything, explain in text what you'll run and why — the user sees your " +
    "explanation before the approval prompt. Use `read_terminal` to see the current terminal " +
    "screen — the live view of the active session, NOT the shell history file (~/.bash_history " +
    "is a separate concern and does not reflect the current terminal).\n\n" +
    "State-changing actions are approved by the user before execution; you will be told if one " +
    "is rejected. Be concise and summarize tool results for the user.";

  const AGENT_TOOLS = [
    { type: "function", function: { name: "terminal", description: "Run a command by typing it into the user's LIVE, already-connected terminal (like a human) and wait for the output to settle. Works on ANY host — key or password — and needs no agent. THIS IS THE PREFERRED WAY to run commands; the user sees the command run live. Returns the terminal output after the command.", parameters: { type: "object", properties: { command: { type: "string", description: "the full command line to type and execute" } }, required: ["command"] } } },
    { type: "function", function: { name: "read_terminal", description: "Read the current content of the active terminal (what is on screen plus recent scrollback). Use this whenever the user asks about the current terminal.", parameters: { type: "object", properties: {} } } },
    { type: "function", function: { name: "run_command", description: "Run a command over a SEPARATE ssh connection via the installed puppetterm-agent. ONLY works on key-based hosts with the agent installed; on password-only hosts it FAILS with 'Permission denied'. Prefer `terminal` (works everywhere). Use only when you need structured, audited agent results on a key host.", parameters: { type: "object", properties: { cmd: { type: "string" }, dir: { type: "string" } }, required: ["cmd"] } } },
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

  function tabLabel(t: Tab): string {
    return t.host || "local";
  }

  /** Reconstruct the visible text a user actually typed. `term.onData` gives
   *  RAW keystrokes — including backspaces and arrow-key escape sequences — so
   *  a corrected typo would otherwise leave a stray character in the buffer
   *  (e.g. `ssh user2<BS>@host` must parse as `user@host`, NOT `user2@host`). */
  function cleanTyped(line: string): string {
    const out: string[] = [];
    let i = 0;
    while (i < line.length) {
      const c = line[i];
      if (c === "\x7f" || c === "\x08") {
        out.pop(); // backspace: erase the previous character
        i++;
      } else if (c === "\x1b") {
        // Skip an ANSI escape sequence (arrow keys etc.): CSI `ESC [ ... final`,
        // otherwise the two-char form `ESC X`.
        i++;
        if (line[i] === "[") {
          i++;
          while (i < line.length && !/[A-Za-z@~]/.test(line[i])) i++;
          if (i < line.length) i++; // final byte
        } else {
          i++;
        }
      } else if (c === "\t") {
        out.push(" "); // keep tabs as a separator (the target regex needs \s)
        i++;
      } else if (c >= " " && c < "\x7f") {
        out.push(c); // printable ASCII
        i++;
      } else {
        i++; // drop other control bytes
      }
    }
    return out.join("");
  }

  /** Extract the target host from a line like `ssh -p 2222 user@host`. */
  function parseSshTarget(line: string): string | null {
    const m = line.trim().match(/^ssh(?:2)?\s+(.+)$/i);
    if (!m) return null;
    const tokens = m[1].trim().split(/\s+/).filter(Boolean);
    let i = 0;
    while (i < tokens.length) {
      const tok = tokens[i];
      if (tok === "-p" || tok === "-i" || tok === "-l" || tok === "-o" || tok === "-J") {
        i += 2; // option + its value
        continue;
      }
      if (tok.startsWith("-")) {
        i += 1;
        continue;
      }
      // Strip control/whitespace characters (a stray newline/tab makes OpenSSH
      // reject the username with 'remote username contains invalid characters').
      const host = tok.replace(/[\s\x00-\x1f\x7f]/g, "");
      return host.length > 0 ? host : null;
    }
    return null;
  }

  async function openTab(host?: string) {
    // Quick-connect to a named host reuses its existing tab.
    if (host) {
      const existing = tabs.find((t) => t.host === host);
      if (existing) {
        showHostMenu = false;
        await activateTab(existing.id);
        return;
      }
    }

    const id = nextTabId++;
    tabs = [...tabs, { id, host: host ?? "", sessionId: null, connecting: false, buf: "" }];
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
      // In-terminal approval for installing the agent (consumes the keypress,
      // does NOT forward it to the shell).
      if (installPrompt && installPrompt.tabId === id) {
        const ch = data.trim().toLowerCase();
        if (ch === "y") {
          const { host } = installPrompt;
          installPrompt = null;
          runInstall(id, host);
        } else if (ch === "n" || ch === "\r" || ch === "\u0003") {
          term.write("\r\n\x1b[90m[puppetterm] install cancelled\x1b[0m\r\n");
          installPrompt = null;
        }
        return;
      }
      if (t?.sessionId != null) call("write_ssh_input", { id: t.sessionId, data });
      // Track `ssh <target>` so the tab shows the remote connection (like a
      // normal terminal: open local, type `ssh user@host` to connect).
      if (t) {
        t.buf += data;
        const nl = t.buf.search(/[\r\n]/);
        if (nl >= 0) {
          const line = t.buf.slice(0, nl).trim();
          t.buf = t.buf.slice(nl + 1);
          // Apply backspaces / drop arrow-key sequences BEFORE parsing, so a
          // corrected typo (e.g. `user2<BS>@host`) detects as `user@host`.
          const target = parseSshTarget(cleanTyped(line));
          if (target && target !== t.host) {
            // Set the host immediately, but DON'T check for the agent here —
            // on password-only remotes the check would run before the user has
            // connected (no ControlMaster yet) and fail. The prompt-gated
            // buffer scan (maybeDetectSshFromBuffer) does the check once the
            // connection is actually established.
            t.host = target;
          }
        } else if (t.buf.length > 4096) {
          t.buf = "";
        }
      }
    });

    termByTab.set(id, { term, fit });
    wireCopyPaste(term);
    startSession(id, host);
  }

  async function startSession(id: number, host?: string) {
    const t = tabById(id);
    if (!t) return;
    t.connecting = true;
    if (host) {
      termByTab.get(id)?.term.write(`\x1b[33m[puppetterm] connecting to ${host}...\x1b[0m\r\n`);
    }
    try {
      const sessionId = host
        ? await call<number>("start_ssh_session", { host })
        : await call<number>("start_local_session");
      t.sessionId = sessionId;
      fitTab(id);
    } catch (e) {
      termByTab
        .get(id)
        ?.term.write(`\r\n\x1b[31m[puppetterm] failed to start session: ${e}\x1b[0m\r\n`);
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

  // ---- clipboard helpers (terminal copy/paste) -----------------------------
  async function copyText(text: string) {
    if (isTauri) {
      try {
        await tauriWriteText(text);
        return;
      } catch (e) {
        console.error("tauri clipboard write failed", e);
      }
    }
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      try {
        const ta = document.createElement("textarea");
        ta.value = text;
        document.body.appendChild(ta);
        ta.select();
        document.execCommand("copy");
        document.body.removeChild(ta);
      } catch {
        /* ignore */
      }
    }
  }

  async function readClipboard(): Promise<string> {
    if (isTauri) {
      try {
        return await tauriReadText();
      } catch (e) {
        console.error("tauri clipboard read failed", e);
      }
    }
    try {
      return await navigator.clipboard.readText();
    } catch {
      return "";
    }
  }

  async function pasteIntoTerminal(t: Terminal) {
    const text = await readClipboard();
    if (text) t.paste(text);
  }

  function wireCopyPaste(term: Terminal) {
    term.attachCustomKeyEventHandler((e) => {
      const mod = e.ctrlKey || e.metaKey;
      if (mod && e.shiftKey && (e.key === "C" || e.key === "c")) {
        const sel = term.getSelection();
        if (sel) copyText(sel);
        return false;
      }
      if (mod && e.shiftKey && (e.key === "V" || e.key === "v")) {
        pasteIntoTerminal(term);
        return false;
      }
      if (mod && e.key === "Insert") {
        const sel = term.getSelection();
        if (sel) copyText(sel);
        return false;
      }
      if (e.shiftKey && e.key === "Insert") {
        pasteIntoTerminal(term);
        return false;
      }
      return true;
    });
    term.onSelectionChange(() => {
      const sel = term.getSelection();
      if (sel) copyText(sel); // auto-copy on selection (best-effort)
    });
    // Right-click copy/paste: xterm's built-in rightClickHandler already
    // populates its hidden textarea with the current selection and focuses it,
    // so the webview's NATIVE context menu's "Copy"/"Paste" operate on the
    // terminal selection. We must NOT preventDefault here, or the native menu
    // is suppressed and right-click copy breaks in WebKitGTK (Tauri on Linux).
    // (Instant copy/paste is provided by Ctrl+Shift+C / Ctrl+Shift+V and
    // onSelectionChange auto-copy, which go through the native clipboard plugin.)
  }

  // ---- splitter (resizable AI panel) ---------------------------------------
  function startResize(e: PointerEvent) {
    resizing = true;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }
  function onResizeMove(e: PointerEvent) {
    if (!resizing) return;
    const w = Math.min(
      Math.max(window.innerWidth - e.clientX, 260),
      Math.round(window.innerWidth * 0.5),
    );
    aiWidth = w;
  }
  function endResize() {
    resizing = false;
  }

  function pushChat(role: string, text: string) {
    chatLog = [...chatLog, { role, text }];
  }

  /** Start a fresh conversation: reset history to just the system prompt and
   *  clear the visible chat log. History is in-memory (not persisted) and is
   *  already bounded by compaction while a task runs. */
  function newChat() {
    if (chatBusy) return; // don't clear mid-task
    history = [{ role: "system", content: SYSTEM_PROMPT }];
    chatLog = [];
    chatText = "";
    pushChat("ai", "(new chat started — earlier conversation cleared)");
    notify("New chat started");
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
    if (tool === "snapshot" || tool === "log" || tool === "read_terminal") return true;
    if (tool === "service") {
      const op = String(args.op ?? "");
      return op === "status" || op === "is-active" || op === "is-enabled";
    }
    if (tool === "config") return args.op === "read";
    if (tool === "terminal") {
      const cmd = String(args.command ?? "").trim().replace(/^sudo\s+/, "");
      const first = cmd.split(/\s+/)[0] ?? "";
      return READONLY_CMDS.includes(first);
    }
    return false; // run_command and anything else asks
  }

  /** Map a structured agent tool call to a plain shell command, used to fall
   *  back to the live terminal when the agent route is unavailable (e.g. a
   *  password-only host without the agent). Returns null when there's no clean
   *  shell equivalent (e.g. config writes). */
  function structuredToolToShell(name: string, args: Record<string, unknown>): string | null {
    switch (name) {
      case "run_command":
        return String(args.cmd ?? "").trim() || null;
      case "snapshot":
        return "df -h; echo; free -h; echo; uptime";
      case "service": {
        const unit = String(args.unit ?? "");
        const op = String(args.op ?? "status");
        if (!unit) return null;
        const needsSudo = ["start", "stop", "restart", "enable", "disable"].includes(op);
        return `${needsSudo ? "sudo " : ""}systemctl ${op} ${unit}`;
      }
      case "log": {
        const path = String(args.path ?? "");
        if (!path) return null;
        const lines = Number(args.lines) || 50;
        return `tail -n ${lines} ${path}`;
      }
      case "config":
        if (args.op === "read") return `cat ${String(args.path ?? "")}`;
        return null; // writes need the agent's structured path
      default:
        return null;
    }
  }

  // Commands that don't change state — the `terminal` tool auto-runs these.
  const READONLY_CMDS = [
    "ls", "cat", "pwd", "whoami", "echo", "head", "tail", "grep", "df", "free",
    "ps", "uptime", "uname", "hostname", "date", "id", "which", "find", "stat",
    "du", "env", "printenv", "true", "ip", "ss", "mount", "history",
  ];

  async function saveAiConfig() {
    try {
      if (aiProvider === "openai") customBaseUrl = aiBaseUrl; // remember the custom endpoint
      await call("set_ai_config", {
        baseUrl: aiBaseUrl,
        model: aiModel,
        provider: aiProvider,
        apiKey: aiKey,
      });
      aiKey = "";
      const v = await call<any>("get_ai_config");
      aiBaseUrl = v.base_url;
      aiModel = v.model;
      aiProvider = v.provider ?? "openai";
      aiHasKey = v.has_api_key;
      aiReady = true;
      if (aiProvider === "openai") customBaseUrl = v.base_url || customBaseUrl;
      pushChat("ai", "(AI settings saved)");
      notify(`AI settings saved — ${AI_PROVIDERS[aiProvider]?.label ?? "Custom"} · ${aiModel}`);
    } catch (e) {
      pushChat("ai", `(failed to save AI settings: ${e})`);
      notify(`Failed to save AI settings: ${e}`, "err");
    }
  }

  /** Save everything from the Settings modal and close it. */
  async function saveSettings() {
    await saveAiConfig();
    showSettings = false;
  }

  async function sendChat() {
    const text = chatText.trim();
    if (!text) return;
    if (!aiReady) {
      pushChat("ai", "(AI not configured — set the endpoint/model in settings)");
      return;
    }
    // Pin the target now: the whole task runs against THIS host and streams
    // into THIS terminal, even if the user switches tabs mid-task. The host may
    // be empty (local tab) — read_terminal still works, agent tools will say so.
    const target = { host: activeHost ?? "", tabId: activeTabId ?? -1 };
    chatTarget = target;
    abortRequested = false;
    chatText = "";
    pushChat("user", text);
    pushChat("ai", `(acting on ${target.host || "the local terminal"})`);
    history = [...history, { role: "user", content: text }];
    chatBusy = true;
    try {
      await runAiLoop();
    } finally {
      chatBusy = false;
      aiThinking = false;
      chatTarget = null;
      currentRequestId = null;
      loadActivity();
    }
  }

  /** Take back control: stop the AI loop and kill the in-flight remote action. */
  async function abortAi() {
    abortRequested = true;
    const rid = currentRequestId;
    currentRequestId = null;
    if (rid) {
      try {
        // The backend kills the local ssh AND issues a remote `pkill` so the
        // actual command on the server stops too (sshd alone won't).
        await call("stop_agent_action", {
          requestId: rid,
          host: chatTarget?.host ?? activeHost,
        });
      } catch (e) {
        console.error("stop_agent_action", e);
      }
    }
  }

  /** Load the recent audit log (what the AI/user did on each host). */
  async function loadActivity() {
    try {
      activity = await call<any[]>("audit_recent", { limit: 20 });
    } catch {
      /* audit may be unavailable in the browser mock — leave empty */
    }
  }

  // ---- agent install (in-terminal, approval-style) --------------------------------
  /** Ask in the terminal: install the agent on the active host? [y/N] */
  function promptInstall() {
    const host = activeHost;
    const tabId = activeTabId;
    const term = activeTerm();
    if (!host || !tabId || !term || installBusy) return;
    installPrompt = { tabId, host };
    term.write(
      `\r\n\x1b[33m[puppetterm]\x1b[0m Install puppetterm-agent on \x1b[1m${host}\x1b[0m ` +
        `(user-space, no sudo — reuses your SSH connection)? [y/N] `,
    );
  }

  /** Stream the install into the terminal for the given tab. */
  async function runInstall(id: number, host: string) {
    const term = termByTab.get(id)?.term;
    installTabId = id;
    installBusy = true;
    term?.write(`\r\n\x1b[35m[puppetterm install] starting on ${host}…\x1b[0m\r\n`);
    try {
      const res = await call<any>("install_agent_on_host", { host });
      term?.write(
        `\r\n\x1b[32m[puppetterm install] ${res?.already ? "already present —" : "done —"} ` +
          `${res?.mode ?? "user"} agent at ${res?.agent_path ?? "~/.puppetterm/bin/puppetterm-agent"}` +
          `\x1b[0m\r\n`,
      );
      loadActivity();
    } catch (e) {
      term?.write(`\r\n\x1b[31m[puppetterm install] failed: ${e}\x1b[0m\r\n`);
    } finally {
      installBusy = false;
      installTabId = null;
    }
  }

  /** After ssh detection, quietly check whether the agent is present; if not,
   *  print a one-time hint offering to install it. */
  async function checkAndHintAgent(id: number, host: string) {
    if (agentChecked.has(host)) return;
    agentChecked.add(host);
    try {
      const ok = await call<boolean>("check_agent", { host });
      if (!ok) {
        termByTab.get(id)?.term.write(
          `\r\n\x1b[90m[puppetterm] agent not detected on ${host} — ` +
            `click "Install agent" to set it up (no sudo, reuses your SSH connection).\x1b[0m\r\n`,
        );
      }
    } catch {
      /* ignore — hint is best-effort */
    }
  }

  /** Find an `ssh <target>` inside a DISPLAYED terminal line, e.g. the echo of
   *  a command recalled from history: `user@box:~$ ssh user@host`.
   *  Returns the last match so the most recent command wins.
   *  `ssh` must appear right after a shell prompt ($, #, >) or at line start —
   *  prose like "…close the ssh connection…" must NOT be parsed as a target
   *  (that made the tab label become garbage like "connection"). */
  function detectSshFromLine(line: string): string | null {
    const re = /(?:^|[$#>]\s*)(?:ssh(?:2)?)[\s]+/gi;
    let m: RegExpExecArray | null;
    let target: string | null = null;
    while ((m = re.exec(line)) !== null) {
      const rest = line.slice(m.index + m[0].length).split(/[;|&]/)[0].trim();
      const t = parseSshTarget("ssh " + rest);
      if (t) target = t;
    }
    return target;
  }

  /** Detect an ssh target from what's on screen. This catches commands that
   *  never passed through onData — e.g. recalled with the up-arrow — because
   *  the shell echoes them into the pty output. Only scans when the shell is
   *  back at a prompt (i.e. the connection has been established / command done).
   *  This is ALSO where the agent check/hint happens (once per host per
   *  session), so on password-only remotes the check runs only after the user's
   *  interactive ssh has authenticated and created the ControlMaster socket. */
  function maybeDetectSshFromBuffer(id: number) {
    const t = tabs.find((x) => x.id === id);
    const term = termByTab.get(id)?.term;
    if (!t || !term) return;
    const lines = terminalText(term, 60).split("\n");
    // The shell prompt is the last non-empty line (a trailing blank line can
    // follow the prompt, e.g. after a redraw — skip it before gating).
    let li = lines.length - 1;
    while (li >= 0 && lines[li].trim() === "") li--;
    const last = (lines[li] ?? "").trim();
    if (!/[\$#>] ?$/.test(last)) return; // only when at a shell prompt
    for (let i = li; i >= 0; i--) {
      const target = detectSshFromLine(lines[i]);
      if (target) {
        if (t.host !== target) t.host = target;
        checkAndHintAgent(id, t.host); // idempotent per host per session
        return;
      }
    }
  }

  // Keep the conversation bounded so long investigations don't blow the
  // model's context window: drop the middle, keep system + original request +
  // the most recent turns.
  const MAX_HISTORY = 40;
  const MAX_CONTEXT_CHARS = 80000;

  function compactHistory(h: any[]): any[] {
    const chars = h.reduce(
      (n, m) => n + (typeof m.content === "string" ? m.content.length : 0),
      0,
    );
    if (h.length <= MAX_HISTORY && chars <= MAX_CONTEXT_CHARS) return h;
    const head = h.slice(0, 2); // system + original request
    const tail = h.slice(-24);
    const dropped = h.length - head.length - tail.length;
    return [
      ...head,
      {
        role: "system",
        content: `(Note: ${dropped} earlier messages and ~${Math.max(0, chars - MAX_CONTEXT_CHARS)} chars of tool output were compacted to keep the conversation bounded. Continue based on the latest tool results and terminal state.)`,
      },
      ...tail,
    ];
  }

  async function runAiLoop() {
    try {
      let guard = 0;
    while (guard++ < 25) {
      if (abortRequested) {
        pushChat("ai", "(aborted by user)");
        return;
      }
      history = compactHistory(history);
      aiThinking = true;
      let resp: any;
      try {
        resp = await call<any>("ai_chat", { messages: history, tools: AGENT_TOOLS });
      } finally {
        aiThinking = false;
      }
      const msg = resp?.choices?.[0]?.message;
      if (!msg) {
        pushChat("ai", "(no response from the model)");
        return;
      }
      if (msg.tool_calls && msg.tool_calls.length > 0) {
        // Show the AI's explanation even when it also calls a tool — otherwise
        // the user only sees the approval dialog and never the answer/plan.
        const explain = (msg.content ?? "").trim();
        if (explain) pushChat("ai", explain);
        history = [
          ...history,
          { role: "assistant", content: msg.content ?? null, tool_calls: msg.tool_calls },
        ];
        for (const tc of msg.tool_calls) {
          const ok = await requestApproval(tc, explain || undefined);
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
    } catch (e) {
      pushChat("ai", `(AI error: ${e})`);
      console.error("ai_chat", e);
    }
  }

  function requestApproval(
    tc: { id: string; function: { name: string; arguments: string } },
    explain?: string,
  ): Promise<boolean> {
    const name = tc.function.name;
    const args = safeParse(tc.function.arguments);
    // Reading the terminal screen is invisible and changes nothing — never prompt.
    if (name === "read_terminal") return Promise.resolve(true);
    // Propose-first: the AI answers the question in text, then asks before
    // executing ANY command (even read-only ones) — full human-in-the-loop.
    const proposeFirst = autonomy === "propose-first";
    if (!proposeFirst) {
      // Read-only tools auto-run in the other modes.
      if (toolReadOnly(name, args)) return Promise.resolve(true);
      // Read-only mode: state-changing actions are blocked, not silently approved.
      if (autonomy === "read-only-auto") {
        pushChat("ai", `(blocked in read-only mode: ${name} would change state)`);
        return Promise.resolve(false);
      }
    }
    // Ask-first / propose-first: prompt, flagging dangerous commands so the
    // user can't miss them.
    const danger = isDangerous(name, args);
    return new Promise((resolve) => {
      pendingApproval = { id: tc.id, tool: name, args, danger, explain, resolve };
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

  function terminalText(term: Terminal, maxLines = 200): string {
    const buf = term.buffer.active;
    const total = buf.length;
    const start = Math.max(0, total - maxLines);
    const lines: string[] = [];
    for (let y = start; y < total; y++) {
      lines.push(buf.getLine(y)?.translateToString(true) ?? "");
    }
    return lines.join("\n");
  }

  /** Type a command into the LIVE terminal (like a human) and wait for the
   *  output to settle, then hand the visible result back to the AI. Works on
   *  any connection — key or password — because it rides the user's real pty. */
  async function runInTerminal(host: string | null, term: Terminal | null, cmd: string) {
    const tabId = chatTarget?.tabId ?? activeTabId;
    const t = tabId != null ? tabs.find((x) => x.id === tabId) : null;
    if (!term || !t || t.sessionId == null) {
      return { error: "no active terminal session to type into" };
    }
    // Show what the AI is doing, then type the command + Enter into the pty.
    term.write(`\r\n\x1b[36m[puppetterm] AI types: ${cmd}\x1b[0m\r\n`);
    await call("write_ssh_input", { id: t.sessionId, data: cmd });
    await call("write_ssh_input", { id: t.sessionId, data: "\r" });
    // Wait for output to stop changing (or a 30s cap), then return the tail.
    const deadline = Date.now() + 30000;
    let last = terminalText(term, 2000);
    let stableSince = Date.now();
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 250));
      const nowText = terminalText(term, 2000);
      if (nowText === last) {
        if (Date.now() - stableSince > 800) break;
      } else {
        last = nowText;
        stableSince = Date.now();
      }
    }
    const lines = terminalText(term, 2000).split("\n");
    const output = lines.slice(-60).join("\n").slice(-6000);
    return {
      host: host || null,
      note: "typed into the live terminal and waited for the output to settle",
      command: cmd,
      output,
    };
  }

  async function executeTool(tc: { id: string; function: { name: string; arguments: string } }) {
    const name = tc.function.name;
    const args = safeParse(tc.function.arguments);
    // Act on the pinned target (set when the chat was sent), falling back to
    // the current active tab. Never chase a tab switch mid-task.
    const host = chatTarget?.host ?? activeHost;
    const term =
      chatTarget?.tabId != null && chatTarget.tabId >= 0
        ? termByTab.get(chatTarget.tabId)?.term ?? null
        : activeTerm();

    // Client-local tools (no SSH round-trip) — work on local OR remote tabs.
    if (name === "read_terminal") {
      if (!term) return { error: "no active terminal" };
      const text = terminalText(term, 200);
      term.write("\r\n\x1b[36m[puppetterm] AI read the active terminal…\x1b[0m\r\n");
      return {
        host: host || null,
        note: "live terminal screen (not shell history)",
        terminal: text.slice(-8000),
      };
    }

    // Type a command into the live terminal and wait for output. Works on any
    // host (key OR password) because it uses the user's actual pty session.
    if (name === "terminal") {
      const cmd = String(args.command ?? "").trim();
      if (!cmd) return { error: "no command given" };
      return await runInTerminal(host, term, cmd);
    }

    // Tools that run on the remote host need an ssh target.
    if (!host) {
      return {
        error:
          "no remote connection in this tab — type `ssh user@host` to connect, then I can act on it",
      };
    }

    const action = TOOL_TO_ACTION[name] ?? "run";
    if (term) {
      term.write(`\r\n\x1b[36m[puppetterm] AI → ${name} ${JSON.stringify(args)}\x1b[0m\r\n`);
    }
    const request = { action, params: args, request_id: tc.id };
    currentRequestId = tc.id;
    let res: any;
    let agentErr: string | null = null;
    try {
      res = await call<any>("run_agent_action", {
        host,
        request: JSON.stringify(request),
        source: "ai",
        approved: true,
      });
    } catch (e) {
      agentErr = String(e ?? "");
    } finally {
      currentRequestId = null;
    }
    // SSH-layer failure (e.g. password-only host without the agent, or no key
    // auth) — fall back to typing an equivalent command into the user's live
    // terminal, which is ALREADY logged in. This is the whole point: the AI
    // rides the connection the user opened, so password remotes just work.
    const agentProblem =
      agentErr != null ||
      /permission denied|publickey|connection (?:refused|timed out)|no route to host|could not resolve hostname/i.test(
        res?.error ?? "",
      );
    if (agentProblem) {
      const cmd = structuredToolToShell(name, args);
      if (cmd) {
        const why = (agentErr ?? res?.error ?? "ssh failure").slice(0, 140);
        term?.write(
          `\r\n\x1b[33m[puppetterm] agent route unavailable (${why}) — running in your live terminal instead\x1b[0m\r\n`,
        );
        const fb = await runInTerminal(host, term, cmd);
        return { ...fb, fallback: true, from: name };
      }
    }
    if (agentErr) throw new Error(agentErr);
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
      host,
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
            if (t) {
              const term = termByTab.get(t.id)?.term;
              // xterm.write() parses data ASYNCHRONOUSLY, so scanning the
              // buffer synchronously here reads a stale snapshot. The ssh
              // command echo + MOTD + remote prompt often land in the SAME
              // chunk, so we'd miss them entirely. Scan inside the write
              // callback (runs once this chunk is rendered) instead.
              term?.write(p.data, () => maybeDetectSshFromBuffer(t.id));
            }
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
          await on<{ host: string; data: string }>("install-output", (p) => {
            const term =
              installTabId != null
                ? termByTab.get(installTabId)?.term
                : activeTerm();
            term?.write(`\r\n\x1b[35m[puppetterm install]\x1b[0m ${p.data}\r\n`);
          }),
        ];
      } catch (e) {
        console.warn("event listeners unavailable:", e);
      }
      await loadHosts();
      loadActivity();
      try {
        const v = await call<any>("get_ai_config");
        aiBaseUrl = v.base_url;
        aiModel = v.model;
        aiProvider = v.provider ?? "openai";
        aiHasKey = v.has_api_key;
        aiReady = true;
        if (aiProvider === "openai") customBaseUrl = v.base_url || "";
        history = [{ role: "system", content: SYSTEM_PROMPT }];
      } catch (e) {
        console.warn("ai config unavailable:", e);
      }
      // Land in a working LOCAL shell on launch (like a normal terminal)
      // instead of the empty "No open sessions" state — the local-terminal-first
      // flow is then one keystroke away: type `ssh user@host` to connect.
      if (tabs.length === 0) {
        await openTab();
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

<div class="app" class:resizing={resizing}>
  {#if toast}
    <div class="toast {toast.kind === 'err' ? 'err' : ''}" role="status">
      {toast.msg}
    </div>
  {/if}
  <header class="topbar">
    <div class="brand">puppetterm</div>
    <nav class="tabs">
      {#each tabs as t (t.id)}
        <div
          class="tab {t.id === activeTabId ? 'active' : ''}"
          role="button"
          tabindex="0"
          title={tabLabel(t)}
          onclick={() => activateTab(t.id)}
          onkeydown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              activateTab(t.id);
            }
          }}
        >
          <span class="dot {t.connecting ? 'busy' : t.sessionId != null ? 'up' : 'down'}"></span>
          <span class="tab-host">{tabLabel(t)}</span>
          <button
            class="tab-close"
            type="button"
            aria-label={`close ${tabLabel(t)}`}
            onclick={(e) => {
              e.stopPropagation();
              closeTab(t.id);
            }}
          >×</button>
        </div>
      {/each}

      <span class="new-wrap">
        <button class="new-host" onclick={() => openTab()} title="Open a local terminal">+ New</button>
        <button
          class="new-chevron"
          onclick={() => (showHostMenu = !showHostMenu)}
          title="Connect to a saved host"
        >
          ▾
        </button>
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
      <button class="settings-btn" onclick={() => (showSettings = true)} title="Settings">⚙</button>
    </nav>
  </header>

  <main class="body">
    <section class="term-area" bind:this={terminalArea}>
      {#if tabs.length === 0}
        <div class="placeholder">
          No open sessions — click <b>+ New</b> to open a local terminal, then
          <code>ssh user@host</code> to connect to a server.
        </div>
      {/if}
      {#each tabs as t (t.id)}
        <div
          class="term-viewport {t.id === activeTabId ? 'active' : ''}"
          bind:this={viewports[t.id]}
        ></div>
      {/each}
    </section>

    <div
      class="splitter"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize AI panel"
      onpointerdown={startResize}
      onpointermove={onResizeMove}
      onpointerup={endResize}
      onpointercancel={endResize}
    ></div>

    <aside class="ai-panel" style={`width: ${aiWidth}px`}>
      <div class="pane-title">
        AI
        <button class="ai-settings-link" onclick={() => (showSettings = true)} title="AI settings">
          ⚙ settings
        </button>
        <button
          class="ai-settings-link"
          onclick={newChat}
          disabled={chatBusy}
          title="Start a new chat (clear this conversation)"
        >
          ＋ new chat
        </button>
      </div>

      <div class="ai-model-row" title="Switch the AI model (persisted)">
        <span class="ai-provider-tag">{AI_PROVIDERS[aiProvider]?.label ?? "Custom"}</span>
        <select
          value={aiModel}
          onchange={(e) => applyAiModel((e.currentTarget as HTMLSelectElement).value)}
        >
          {#if !aiModel}
            <option value="">(no model)</option>
          {/if}
          {#each allModels as m (m)}
            <option value={m}>{m}</option>
          {/each}
        </select>
      </div>

      {#if !aiReady}
        <div class="ai-unconfigured">
          AI not configured — open <button onclick={() => (showSettings = true)}>⚙ Settings</button> to set the endpoint &amp; model.
        </div>
      {/if}

      {#if pendingApproval}
        <div class="approval {pendingApproval.danger ? 'danger' : ''}">
          <div class="approval-label">
            {pendingApproval.danger ? '⚠ Dangerous action' : 'Approve action?'}
          </div>
          {#if pendingApproval.explain}
            <div class="approval-explain">{pendingApproval.explain}</div>
          {/if}
          <div class="approval-cmd">
            {pendingApproval.tool} {JSON.stringify(pendingApproval.args)}
            <div class="approval-host">
              {pendingApproval.danger ? 'on ' : 'on '}{chatTarget?.host ?? activeHost}
            </div>
          </div>
          <div class="approval-btns">
            <button onclick={reject}>Reject</button>
            <button class="primary" onclick={approve}>Approve</button>
          </div>
        </div>
      {/if}

      <div class="ai-target">
        <span class="dot {activeHost ? 'up' : 'down'}"></span>
        {#if activeHost}
          acting on <b>{activeHost}</b>
          {#if chatBusy && chatTarget && chatTarget.host !== activeHost}
            <span class="warn">(pinned — you switched tabs)</span>
          {/if}
        {:else}
          local — ssh to a remote first
        {/if}
        {#if activeHost && !installBusy}
          <button
            class="install-agent"
            onclick={promptInstall}
            title="Install puppetterm-agent on {activeHost} (no sudo, reuses your SSH connection)"
          >
            Install agent
          </button>
        {/if}
      </div>

      <button class="activity-toggle" onclick={() => (showActivity = !showActivity)}>
        Activity ({activity.length}) {showActivity ? '▾' : '▸'}
      </button>
      {#if showActivity}
        <div class="activity">
          {#if activity.length === 0}
            <p class="muted">No recorded actions yet.</p>
          {:else}
            {#each activity as a (a.id)}
              <div class="activity-row">
                <span class="a-time">{a.ts.slice(11, 19)}</span>
                <span class="a-host">{a.host}</span>
                <span class="a-action">{a.action}</span>
                <span class="a-exit {a.exit === 0 ? 'ok' : 'bad'}">
                  {a.exit == null ? '-' : 'exit ' + a.exit}
                </span>
              </div>
            {/each}
          {/if}
        </div>
      {/if}

      <div class="chat-log">
        {#if chatLog.length === 0}
          <p class="muted">Ask the AI to inspect or change the active host.</p>
        {/if}
        {#each chatLog as m, i (i)}
          <div class="msg {m.role}">{m.text}</div>
        {/each}
        {#if aiThinking}
          <div class="msg ai thinking">
            <span class="spinner"></span> thinking…
          </div>
        {/if}
      </div>
      <div class="chat-input">
        <input
          placeholder={activeHost
            ? `Ask the AI to act on ${activeHost}…`
            : "Ask the AI to act on a remote — ssh to it first…"}
          bind:value={chatText}
          onkeydown={(e) => {
            if (e.key === "Enter") sendChat();
          }}
        />
        <button onclick={sendChat} disabled={!chatText.trim() || chatBusy}>
          {chatBusy ? "…" : "Send"}
        </button>
        {#if chatBusy}
          <button class="abort-btn" onclick={abortAi} title="Stop the AI and kill the running command">
            Abort
          </button>
        {/if}
      </div>
    </aside>
  </main>

  {#if showSettings}
    <div
      class="modal-backdrop"
      role="button"
      tabindex="-1"
      aria-label="Close settings"
      onclick={() => (showSettings = false)}
      onkeydown={(e) => {
        if (e.key === "Escape") showSettings = false;
      }}
    >
      <div
        class="modal"
        role="dialog"
        aria-label="Settings"
        tabindex="-1"
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => e.stopPropagation()}
      >
        <div class="modal-title">Settings</div>

        <div class="modal-section">AI model</div>
        <label class="modal-field">
          AI provider
          <select
            value={aiProvider}
            onchange={(e) => applyAiProvider((e.currentTarget as HTMLSelectElement).value)}
          >
            {#each Object.entries(AI_PROVIDERS) as [key, p] (key)}
              <option value={key}>{p.label}</option>
            {/each}
          </select>
        </label>
        <label class="modal-field">
          Endpoint
          <input
            bind:value={aiBaseUrl}
            placeholder="http://host:port/v1"
            oninput={() => {
              if (aiProvider === "openai") customBaseUrl = aiBaseUrl;
            }}
          />
        </label>
        <label class="modal-field">
          Model
          <input bind:value={aiModel} placeholder="model-name" list="ai-model-list" />
          <datalist id="ai-model-list">
            {#each AI_PROVIDERS[aiProvider]?.models ?? [] as m (m)}
              <option value={m}></option>
            {/each}
          </datalist>
        </label>
        <label class="modal-field">
          API key
          <input
            bind:value={aiKey}
            type="password"
            placeholder={aiHasKey ? "••• (set — encrypted) — type to replace" : "sk-…"}
          />
        </label>
        <label class="modal-field">
          Autonomy
          <select bind:value={autonomy}>
            <option value="ask-first">Ask first (default)</option>
            <option value="propose-first">Propose first (approve every command)</option>
            <option value="read-only-auto">Read-only auto</option>
          </select>
        </label>

        <div class="modal-section">Appearance</div>
        <label class="modal-field">
          Theme
          <select bind:value={themeName}>
            <option value="dark">Dark (default)</option>
          </select>
        </label>

        <div class="modal-btns">
          <button onclick={() => (showSettings = false)}>Cancel</button>
          <button
            class="primary"
            onclick={saveSettings}
            disabled={!aiBaseUrl.trim() || !aiModel.trim()}
          >
            Save
          </button>
        </div>
      </div>
    </div>
  {/if}
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
  .new-chevron,
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
  .new-chevron {
    margin-left: 4px;
    padding: 0 6px;
    border-top-left-radius: 0;
    border-bottom-left-radius: 0;
  }
  .new-host:hover,
  .new-chevron:hover,
  .refresh:hover {
    background: #21262d;
  }
  .settings-btn {
    height: 26px;
    width: 34px;
    border: 1px solid #30363d;
    border-radius: 6px;
    background: #0d1117;
    color: #8b949e;
    font-size: 15px;
    cursor: pointer;
    margin-left: 6px;
  }
  .settings-btn:hover {
    background: #21262d;
    color: #e6edf3;
  }
  .ai-settings-link {
    border: none;
    background: transparent;
    color: #8b949e;
    font-size: 11.5px;
    cursor: pointer;
  }
  .ai-settings-link:hover {
    color: #e6edf3;
  }
  .ai-model-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0 12px 8px;
    flex: none;
  }
  .ai-provider-tag {
    font-size: 11px;
    color: #8b949e;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ai-model-row select {
    flex: 1;
    min-width: 0;
    background: #010409;
    border: 1px solid #30363d;
    border-radius: 6px;
    color: #e6edf3;
    padding: 4px 8px;
    font-size: 12px;
    color-scheme: dark; /* keep the native dropdown dark (WebKitGTK) */
  }
  .ai-model-row select:focus {
    outline: 1px solid #1f6feb;
    border-color: #1f6feb;
  }
  .toast {
    position: fixed;
    bottom: 18px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 1000;
    max-width: 90vw;
    background: #1c2128;
    border: 1px solid #3d444d;
    border-left: 3px solid #2f81f7;
    color: #e6edf3;
    border-radius: 8px;
    padding: 8px 14px;
    font-size: 13px;
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45);
    animation: toast-in 0.15s ease-out;
  }
  .toast.err {
    border-left-color: #f85149;
  }
  @keyframes toast-in {
    from {
      opacity: 0;
      transform: translate(-50%, 8px);
    }
    to {
      opacity: 1;
      transform: translate(-50%, 0);
    }
  }
  .ai-unconfigured {
    margin: 8px 12px;
    padding: 8px 10px;
    font-size: 12px;
    color: #d29922;
    background: #161b22;
    border: 1px solid #30363d;
    border-radius: 6px;
  }
  .ai-unconfigured button {
    border: none;
    background: transparent;
    color: #58a6ff;
    cursor: pointer;
    text-decoration: underline;
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

  .splitter {
    width: 5px;
    flex: none;
    cursor: col-resize;
    background: #21262d;
    touch-action: none;
  }
  .splitter:hover,
  .splitter:active {
    background: #1f6feb;
  }
  .app.resizing {
    user-select: none;
    cursor: col-resize;
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
    min-width: 260px;
    border-left: 1px solid #21262d;
    background: #010409;
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
  .chat-input input {
    background: #0d1117;
    border: 1px solid #30363d;
    border-radius: 6px;
    color: #e6edf3;
    padding: 6px 8px;
    font-size: 13px;
    outline: none;
  }
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
  .approval-explain {
    font-size: 12px;
    color: #e6edf3;
    background: #0d1117;
    border-radius: 6px;
    padding: 6px 8px;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .approval-host {
    margin-top: 6px;
    color: #d29922;
    font-weight: 600;
  }
  .approval.danger {
    border-color: #f85149;
    background: #2d1517;
  }
  .approval.danger .approval-label {
    color: #f85149;
  }
  .approval.danger .approval-host {
    color: #f85149;
  }
  .activity-toggle {
    margin: 0 12px 8px;
    padding: 5px 10px;
    border: 1px solid #21262d;
    background: #161b22;
    color: #8b949e;
    border-radius: 6px;
    cursor: pointer;
    font-size: 12px;
    text-align: left;
    flex: none;
  }
  .activity-toggle:hover {
    background: #21262d;
    color: #e6edf3;
  }
  .activity {
    margin: 0 12px 8px;
    padding: 6px;
    max-height: 160px;
    overflow-y: auto;
    border: 1px solid #21262d;
    border-radius: 6px;
    background: #0d1117;
    font-size: 11.5px;
    flex: none;
  }
  .activity-row {
    display: flex;
    gap: 6px;
    align-items: baseline;
    padding: 3px 4px;
    border-bottom: 1px solid #161b22;
    font-family: monospace;
  }
  .activity-row:last-child {
    border-bottom: none;
  }
  .a-time {
    color: #484f58;
  }
  .a-host {
    color: #58a6ff;
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .a-action {
    color: #e6edf3;
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .a-exit.ok {
    color: #3fb950;
  }
  .a-exit.bad {
    color: #f85149;
  }
  .abort-btn {
    border: 1px solid #f85149;
    background: transparent;
    color: #f85149;
    border-radius: 6px;
    padding: 0 10px;
    cursor: pointer;
    font-weight: 700;
    flex: none;
  }
  .abort-btn:hover {
    background: #f85149;
    color: #fff;
  }

  /* ---- settings modal ---- */
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(1, 4, 9, 0.7);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal {
    width: 420px;
    max-width: 92vw;
    max-height: 86vh;
    overflow-y: auto;
    background: #0d1117;
    border: 1px solid #30363d;
    border-radius: 10px;
    padding: 18px 20px;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
  }
  .modal-title {
    font-size: 16px;
    font-weight: 700;
    margin-bottom: 14px;
  }
  .modal-section {
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: #8b949e;
    margin: 14px 0 8px;
    border-top: 1px solid #21262d;
    padding-top: 10px;
  }
  .modal-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12.5px;
    color: #8b949e;
    margin-bottom: 10px;
  }
  .modal-field input,
  .modal-field select {
    background: #010409;
    border: 1px solid #30363d;
    border-radius: 6px;
    color: #e6edf3;
    padding: 7px 10px;
    font-size: 13px;
  }
  /* Force dark form controls + dark dropdown panel. Without `color-scheme`
     WebKitGTK renders the native <select> popup with the LIGHT system theme,
     so the white option text becomes invisible on a white panel. */
  .modal-field select {
    color-scheme: dark;
  }
  .modal-field option {
    background-color: #0d1117;
    color: #e6edf3;
  }
  .modal-field input:focus,
  .modal-field select:focus {
    outline: 1px solid #1f6feb;
    border-color: #1f6feb;
  }
  .modal-btns {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .modal-btns button {
    border: 1px solid #30363d;
    background: #21262d;
    color: #e6edf3;
    border-radius: 6px;
    padding: 7px 16px;
    cursor: pointer;
    font-weight: 600;
  }
  .modal-btns button.primary {
    background: #1f6feb;
    border-color: #1f6feb;
    color: #fff;
  }
  .modal-btns button:hover {
    filter: brightness(1.1);
  }

  .ai-target {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 0 12px 8px;
    padding: 6px 10px;
    font-size: 12px;
    color: #8b949e;
    background: #161b22;
    border: 1px solid #21262d;
    border-radius: 6px;
    flex: none;
  }
  .ai-target b {
    color: #e6edf3;
    font-family: monospace;
  }
  .ai-target .warn {
    color: #d29922;
  }
  .install-agent {
    margin-left: auto;
    border: 1px solid #238636;
    background: transparent;
    color: #3fb950;
    border-radius: 6px;
    padding: 2px 8px;
    font-size: 11.5px;
    cursor: pointer;
    flex: none;
  }
  .install-agent:hover {
    background: #238636;
    color: #fff;
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
  .msg.thinking {
    color: #8b949e;
    font-style: italic;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .spinner {
    display: inline-block;
    width: 11px;
    height: 11px;
    border: 2px solid #8b949e;
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
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
